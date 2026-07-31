//! Axum HTTP+WS server with authentication, config-gated binding, TLS support,
//! and the full refuse-to-bind matrix from §6.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, Query, State, ws::Message, ws::WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::auth::{self, AuthState};
use crate::config::{AuthMode, WebConfig};
use crate::session::SharedSession;

/// Shared state for all routes.
#[derive(Clone)]
pub struct AppState {
    pub session: SharedSession,
    pub auth: Arc<AuthState>,
    /// Whether this server terminates TLS itself. Gates the `Secure` cookie
    /// attribute: on plain HTTP (the default loopback deployment) a `Secure`
    /// cookie is dropped by the browser and login silently fails.
    pub tls_enabled: bool,
}

/// Configuration for `serve()` — the resolved config after all precedence layers.
pub struct ServeConfig {
    pub bind: IpAddr,
    pub port: u16,
}

/// A running server — address + task handle.
pub struct RunningServer {
    pub addr: SocketAddr,
    handle: tokio::task::JoinHandle<()>,
}

impl RunningServer {
    pub async fn wait(self) {
        let _ = self.handle.await;
    }
    pub fn shutdown(self) {
        self.handle.abort();
    }
}

/// Validate and start the server.
pub async fn serve(
    session: SharedSession,
    cfg: ServeConfig,
    web_cfg: &WebConfig,
    auth_state: AuthState,
) -> anyhow::Result<RunningServer> {
    let loopback = cfg.bind.is_loopback();

    // Refuse-to-bind matrix.
    if !loopback && !web_cfg.allow_remote {
        anyhow::bail!(
            "refusing to bind {}: pass --allow-remote (plus auth and TLS)",
            cfg.bind
        );
    }
    if !loopback && web_cfg.auth == AuthMode::Token {
        anyhow::bail!("token mode is loopback-only; use --auth basic or users for remote access");
    }
    if !loopback
        && web_cfg.auth == AuthMode::Basic
        && (web_cfg.basic_user.is_none() || web_cfg.basic_password_hash.is_none())
    {
        anyhow::bail!("Basic auth requires basic_user and basic_password_hash in [web] config");
    }
    if !loopback && web_cfg.tls_cert_path.is_none() {
        anyhow::bail!(
            "TLS is required for non-loopback access. Set tls_cert_path and tls_key_path, or bind loopback behind a reverse proxy."
        );
    }

    let addr = SocketAddr::new(cfg.bind, cfg.port);

    let tls = match (&web_cfg.tls_cert_path, &web_cfg.tls_key_path) {
        (Some(cert), Some(key)) => Some((cert.clone(), key.clone())),
        _ => None,
    };

    let state = AppState {
        session,
        auth: Arc::new(auth_state),
        tls_enabled: tls.is_some(),
    };

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(index))
        .route("/ws", get(ws_handler))
        .route("/login", post(login_handler))
        .route("/logout", post(logout_handler))
        .with_state(state);

    // TLS or plain. Both paths serve with connect info so handlers see the real
    // peer address — the rate limiter keys on it.
    if let Some((cert, key)) = tls {
        use axum_server::tls_rustls::RustlsConfig;
        let tls_cfg = RustlsConfig::from_pem_file(cert, key).await?;

        // `axum_server` binds inside `serve()`, so the real bound address (which
        // differs from `addr` whenever port 0 was requested) is only reachable
        // through a `Handle`. A oneshot carries the serve error back out so a
        // bind failure is reported instead of swallowed.
        let bind_handle = axum_server::Handle::new();
        let serve_handle = bind_handle.clone();
        let (err_tx, err_rx) = tokio::sync::oneshot::channel::<std::io::Error>();
        let handle = tokio::spawn(async move {
            if let Err(e) = axum_server::bind_rustls(addr, tls_cfg)
                .handle(serve_handle)
                .serve(app.into_make_service_with_connect_info::<SocketAddr>())
                .await
                && let Err(e) = err_tx.send(e)
            {
                // Nobody is waiting on the bind result any more: the server was
                // already running and has now died.
                eprintln!("hrdr web: TLS server error: {e}");
            }
        });

        // Resolves once the listener is bound, or `None` if binding failed.
        match bind_handle.listening().await {
            Some(bound_addr) => Ok(RunningServer {
                addr: bound_addr,
                handle,
            }),
            None => {
                let reason = match err_rx.await {
                    Ok(e) => e.to_string(),
                    Err(_) => "unknown error".to_string(),
                };
                anyhow::bail!("failed to bind {addr} with TLS: {reason}");
            }
        }
    } else {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let bound_addr = listener.local_addr()?;
        let handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            {
                eprintln!("hrdr web: server error: {e}");
            }
        });
        Ok(RunningServer {
            addr: bound_addr,
            handle,
        })
    }
}

// ── routes ─────────────────────────────────────────────────────────────────

async fn healthz() -> &'static str {
    "ok"
}

async fn index(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    query: Query<std::collections::HashMap<String, String>>,
) -> Response {
    // Users mode is the one mode where a browser cannot supply a credential on
    // this request: the only thing `check_auth` accepts is the `hrdr_session`
    // cookie, and that cookie is minted by `POST /login`. A 401 here is
    // therefore a dead end — no unauthenticated route serves a form, so the
    // mode has no browser entry point at all. Answer the navigation with the
    // login page instead, and let the SPA (or the placeholder index) wait until
    // the request is authenticated: serving the app to a logged-out user is the
    // same dead end wearing a nicer page.
    //
    // A *missing or unusable* cookie is deliberately not recorded as a failed
    // auth attempt. The rate limiter still gates the route — a bucket already
    // locked by real failures gets its 429 below, before anything is served —
    // but merely fetching the login form must not consume the budget the user
    // needs to submit it. This is not the paranoid choice it looks like: the
    // cookie secret is regenerated on every start (`AuthState::from_config`),
    // so after a restart every browser in the fleet presents a cookie that no
    // longer verifies, and counting those would lock each of them out of the
    // page that fixes it. The credential check that can actually be
    // brute-forced is `POST /login`, and that one does record.
    if state.auth.mode == AuthMode::Users {
        let client_ip = auth::extract_client_ip(peer.ip(), &headers);
        if !auth::check_rate_limit(&state.auth, client_ip) {
            return rate_limited();
        }
        let authed = session_cookie(&headers)
            .map(|val| auth::verify_session_cookie(val, &state.auth.cookie_secret[..]).is_some())
            .unwrap_or(false);
        if !authed {
            return axum::response::Html(LOGIN_HTML).into_response();
        }
    } else if let Err(resp) = check_auth(&state, peer.ip(), &headers, &query) {
        return resp;
    }
    if let Some(spa) = crate::spa_index_html() {
        return axum::response::Html(spa).into_response();
    }
    axum::response::Html(INDEX_HTML).into_response()
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    query: Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Err(resp) = check_auth(&state, peer.ip(), &headers, &query) {
        return resp;
    }
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    if let Err(status) = auth::check_ws_origin(origin, host) {
        return status.into_response();
    }
    let session = state.session.clone();
    ws.max_frame_size(16 * 1024 * 1024) // 16 MiB per frame
        .max_message_size(16 * 1024 * 1024) // 16 MiB per message
        .on_upgrade(move |socket| handle_socket(socket, session))
}

// ── login / logout ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

async fn login_handler(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<LoginBody>,
) -> Response {
    let client_ip = auth::extract_client_ip(peer.ip(), &headers);
    if !auth::check_rate_limit(&state.auth, client_ip) {
        return rate_limited();
    }

    if state.auth.mode != AuthMode::Users {
        auth::rate_limit_record(&state.auth, client_ip);
        return (StatusCode::NOT_FOUND, "users auth not enabled").into_response();
    }

    // Fetch the stored hash under the mutex, then run argon2 outside it.
    let db = state.auth.users_db.lock().unwrap();
    let hash_opt = match &*db {
        Some(conn) => crate::users::get_password_hash(conn, &body.username).unwrap_or(None),
        None => None,
    };
    drop(db);

    let ok = crate::users::password_matches(hash_opt, &body.password);

    if !ok {
        auth::rate_limit_record(&state.auth, client_ip);
        return (StatusCode::UNAUTHORIZED, "bad credentials").into_response();
    }

    // Mint session cookie.
    let cookie_val = auth::mint_session_cookie(&body.username, &state.auth.cookie_secret[..]);
    let mut cookie =
        format!("hrdr_session={cookie_val}; HttpOnly; SameSite=Strict; Path=/; Max-Age=604800");
    if state.tls_enabled {
        cookie.push_str("; Secure");
    }

    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie.as_str())],
        "ok",
    )
        .into_response()
}

async fn logout_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    // Require authentication when session cookies are in use (AuthMode::Users).
    // Without this an unauthenticated cross-origin POST could clear the
    // victim's hrdr_session cookie — a CSRF logout attack.
    if state.auth.mode == AuthMode::Users {
        let valid = session_cookie(&headers)
            .map(|val| auth::verify_session_cookie(val, &state.auth.cookie_secret[..]).is_some())
            .unwrap_or(false);
        if !valid {
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    }

    let mut cookie = "hrdr_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0".to_string();
    if state.tls_enabled {
        cookie.push_str("; Secure");
    }
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie.as_str())],
        "ok",
    )
        .into_response()
}

// ── auth helper ────────────────────────────────────────────────────────────

/// The Basic-auth challenge sent with a 401 in `AuthMode::Basic`. `charset` is
/// the only parameter RFC 7617 defines, and `UTF-8` is its only legal value —
/// it tells the browser to send non-ASCII credentials as UTF-8 rather than
/// whatever the page's encoding happens to be.
const BASIC_CHALLENGE: &str = r#"Basic realm="hrdr", charset="UTF-8""#;

/// The answer to a request from a locked-out bucket. Every route that consults
/// the limiter — `check_auth`, `login_handler`, and the users-mode arm of `/`
/// that serves the login page without going through `check_auth` — replies with
/// this same 429, so the three cannot drift apart.
fn rate_limited() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, "60")],
        "rate limit exceeded",
    )
        .into_response()
}

#[allow(clippy::result_large_err)]
fn check_auth(
    state: &AppState,
    peer: IpAddr,
    headers: &HeaderMap,
    query: &std::collections::HashMap<String, String>,
) -> Result<(), Response> {
    let client_ip = auth::extract_client_ip(peer, headers);
    if !auth::check_rate_limit(&state.auth, client_ip) {
        return Err(rate_limited());
    }

    let authed = match state.auth.mode {
        AuthMode::Token => auth::verify_token(headers, query, state.auth.token.as_deref()),
        AuthMode::Basic => auth::verify_basic(
            headers,
            state.auth.basic_user.as_deref(),
            state.auth.basic_password_hash.as_deref(),
        ),
        // Every arm must yield a bool into the shared `!authed` tail below —
        // an arm that returns early skips `rate_limit_record` and its failures
        // go uncounted.
        AuthMode::Users => session_cookie(headers)
            .map(|val| auth::verify_session_cookie(val, &state.auth.cookie_secret[..]).is_some())
            .unwrap_or(false),
    };

    if !authed {
        auth::rate_limit_record(&state.auth, client_ip);
        // RFC 9110 §11.6.1 makes `WWW-Authenticate` mandatory on a 401, and a
        // browser uses it to decide whether to offer the credential prompt at
        // all. Without the challenge, `--auth basic` renders the bare
        // "unauthorized" body with no way to supply credentials — which is the
        // one mode `serve()` allows for remote access. Token and users mode
        // have no challenge to offer (a pasted token, and a cookie minted by
        // POST /login), so they answer with a plain 401. Users mode reaching
        // here at all means the request was for `/ws`: `/` serves the login
        // page instead of a 401 in that mode, but `/ws` is an API endpoint a
        // browser never navigates to, so it stays a hard 401 — an HTML login
        // page is not a WebSocket handshake response.
        if state.auth.mode == AuthMode::Basic {
            return Err((
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, BASIC_CHALLENGE)],
                "unauthorized",
            )
                .into_response());
        }
        return Err((StatusCode::UNAUTHORIZED, "unauthorized").into_response());
    }

    Ok(())
}

/// The `hrdr_session` value out of the `Cookie` header, if the header carries one.
fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    let cookie_str = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie_str
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("hrdr_session="))
        .map(str::trim)
}

// ── placeholder index ──────────────────────────────────────────────────────

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>hrdr web</title>
    <style>
        body { font-family: system-ui, sans-serif; max-width: 800px; margin: 2rem auto; padding: 0 1rem; }
        code { background: #eee; padding: 0.2em 0.4em; border-radius: 3px; }
    </style>
</head>
<body>
    <h1>hrdr web</h1>
    <p>Connect a WebSocket client to <code>/ws</code>.</p>
    <p>Example: <code>websocat ws://127.0.0.1:9911/ws</code></p>
</body>
</html>
"#;

// ── login page (users mode) ────────────────────────────────────────────────

/// The unauthenticated `/` in `AuthMode::Users`. Deliberately minimal: this is
/// the server's own fallback entry point, not a UI — enough to get a cookie
/// minted so the real client can load.
///
/// Wholly static, and that is a requirement rather than an accident: not one
/// byte of it comes from the request, so there is nothing to escape and no way
/// for a username, a query parameter, or a header to reach the markup. Anything
/// dynamic that appeared here later would need escaping, which is the moment to
/// stop and reconsider instead.
///
/// The `fetch()` exists because `login_handler` takes `axum::Json<LoginBody>`:
/// a native form submit sends `application/x-www-form-urlencoded`, which that
/// extractor rejects with a 415 before ever seeing the credentials.
const LOGIN_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>hrdr web — sign in</title>
    <style>
        body { font-family: system-ui, sans-serif; max-width: 22rem; margin: 4rem auto; padding: 0 1rem; }
        label { display: block; margin: 1rem 0 0.25rem; }
        input { width: 100%; padding: 0.4em; box-sizing: border-box; font: inherit; }
        button { margin-top: 1.25rem; padding: 0.5em 1.2em; font: inherit; }
        #error { color: #a00; min-height: 1.3em; margin-top: 1rem; }
    </style>
</head>
<body>
    <h1>hrdr web</h1>
    <form id="login-form">
        <label for="username">Username</label>
        <input id="username" name="username" autocomplete="username" autocapitalize="none" autofocus required>
        <label for="password">Password</label>
        <input id="password" name="password" type="password" autocomplete="current-password" required>
        <button type="submit">Sign in</button>
    </form>
    <p id="error" role="alert"></p>
    <script>
        const form = document.getElementById('login-form');
        const error = document.getElementById('error');
        form.addEventListener('submit', async (event) => {
            event.preventDefault();
            error.textContent = '';
            let response;
            try {
                response = await fetch('/login', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({
                        username: document.getElementById('username').value,
                        password: document.getElementById('password').value,
                    }),
                });
            } catch (e) {
                error.textContent = 'could not reach the server';
                return;
            }
            if (response.ok) {
                // The Set-Cookie has landed, so the same URL now serves the app.
                window.location.reload();
                return;
            }
            // Failures come back as a plain-text reason (401 "bad credentials",
            // 429 "rate limit exceeded", 404 when users mode is off). Show it —
            // a form that does nothing on a bad password looks broken. Assigned
            // through textContent, never innerHTML: the body is server-authored
            // today, but a login form is the wrong place to trust that forever.
            const reason = await response.text().catch(() => '');
            error.textContent = reason || ('sign-in failed (' + response.status + ')');
        });
    </script>
</body>
</html>
"#;

// ── WS handler ─────────────────────────────────────────────────────────────

/// Serialize `value` and send it down the socket. Returns `false` once the
/// socket is gone, which is the caller's signal to stop.
///
/// A value that will not serialize is dropped rather than fatal. The wire types
/// are all serde-derived so that is unreachable today, but every send here runs
/// inside a detached `tokio::spawn` where a panic kills the connection with no
/// diagnostic at all — losing one frame and staying up is the better failure.
async fn send_json<T: serde::Serialize>(
    sender: &mut futures_util::stream::SplitSink<axum::extract::ws::WebSocket, Message>,
    value: &T,
) -> bool {
    let json = match serde_json::to_string(value) {
        Ok(json) => json,
        Err(e) => {
            eprintln!("hrdr web: dropping unserializable frame: {e}");
            return true;
        }
    };
    sender.send(Message::Text(json.into())).await.is_ok()
}

async fn handle_socket(socket: axum::extract::ws::WebSocket, session: SharedSession) {
    let (mut sender, mut receiver) = socket.split();

    // Line channel: WebHost posts system/diff lines here.
    let (line_tx, mut line_rx) =
        tokio::sync::mpsc::unbounded_channel::<(hrdr_app::LineKind, String)>();

    // Direct channel: frames destined for THIS socket only (resume replays,
    // per-connection snapshots). They are already sequenced, so the forward task
    // serializes them as-is — no session lock, no new seq, no replay-buffer push,
    // and crucially no broadcast to other clients.
    let (direct_tx, mut direct_rx) =
        tokio::sync::mpsc::unbounded_channel::<hrdr_protocol::ServerFrame>();

    let (snapshot, mut broadcast_rx) = {
        let mut s = session.lock().await;
        s.subscribe()
    };
    if !send_json(&mut sender, &snapshot).await {
        return;
    }

    // Forward broadcast frames + line-channel messages.
    let forward_session = session.clone();
    let forward_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = broadcast_rx.recv() => {
                    match frame {
                        Ok(frame) => {
                            if !send_json(&mut sender, &frame).await {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // This client fell behind and the missed frames are
                            // gone from the channel. Resynchronize it with a
                            // fresh snapshot and keep serving — the receiver has
                            // already skipped to the oldest retained frame, so
                            // breaking here would leave the socket open and
                            // permanently silent.
                            let snap = {
                                let mut s = forward_session.lock().await;
                                s.build_snapshot()
                            };
                            if !send_json(&mut sender, &snap).await {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                direct = direct_rx.recv() => {
                    match direct {
                        Some(frame) => {
                            if !send_json(&mut sender, &frame).await {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                line = line_rx.recv() => {
                    match line {
                        Some((kind, text)) => {
                            let mut s = forward_session.lock().await;
                            match kind {
                                hrdr_app::LineKind::System => {
                                    let seq = s.next_seq_internal();
                                    let frame = crate::convert::build_notice(seq, text);
                                    s.emit_internal(frame);
                                }
                                hrdr_app::LineKind::Diff => {
                                    // Push a diff entry and let tick broadcast it.
                                    s.panes_mut().active_pane_mut().transcript_mut()
                                        .push(hrdr_agent::Entry::diff(text));
                                    s.notify_tick();
                                }
                            }
                        }
                        None => break,
                    }
                }
            }
        }
    });

    'recv: while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(text) => {
                let client_msg: hrdr_protocol::ClientMsg = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("invalid client message: {e}");
                        continue;
                    }
                };
                let direct = handle_client_msg(client_msg, &session, &line_tx).await;
                for frame in direct {
                    if direct_tx.send(frame).is_err() {
                        // Forward task is gone — the socket is dead.
                        break 'recv;
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    forward_handle.abort();
}

/// Handle one client message. The returned frames (if any) are destined for the
/// requesting socket ALONE — the caller pushes them down that connection's
/// direct channel. They must never be broadcast: other clients neither asked for
/// them nor can make sense of another client's replay.
async fn handle_client_msg(
    msg: hrdr_protocol::ClientMsg,
    session: &SharedSession,
    line_tx: &tokio::sync::mpsc::UnboundedSender<(hrdr_app::LineKind, String)>,
) -> Vec<hrdr_protocol::ServerFrame> {
    match msg {
        hrdr_protocol::ClientMsg::Submit { pane, text } => {
            session.lock().await.submit(pane, text).await;
        }
        hrdr_protocol::ClientMsg::Command { pane, line } => {
            // Dispatch via WebHost.
            let mut s = session.lock().await;
            let mut host = crate::host::WebHost {
                session: &mut s,
                line_tx: line_tx.clone(),
            };
            if hrdr_app::is_quit_command(&line) {
                let seq = host.session.next_seq_internal();
                let frame =
                    crate::convert::build_notice(seq, "use your browser's close button".into());
                host.session.emit_internal(frame);
            } else if !line.starts_with('/') {
                // Not a command — treat as plain text submit to the pane the
                // client named (not always Main: sub-panes accept steers).
                drop(host);
                drop(s);
                session.lock().await.submit(pane, line).await;
            } else {
                let dispatched = hrdr_app::dispatch(&mut host, &line);
                if !dispatched {
                    let seq = host.session.next_seq_internal();
                    let frame = crate::convert::build_notice(seq, "unknown command — /help".into());
                    host.session.emit_internal(frame);
                }
                // Tick so any state changes (snapshot/panes) are broadcast.
                host.session.tick();
            }
        }
        hrdr_protocol::ClientMsg::Cancel { pane } => {
            session.lock().await.cancel(pane);
        }
        hrdr_protocol::ClientMsg::SwitchPane { pane } => {
            session.lock().await.switch_pane(pane);
        }
        hrdr_protocol::ClientMsg::Resume { seq } => {
            let mut s = session.lock().await;
            return match s.replay_after(seq) {
                Some(frames) => {
                    // The replayed frames keep their ORIGINAL seqs, so `Resumed`
                    // must not take a fresh (higher) one — that would make this
                    // client's seq stream jump forward and then back. It carries
                    // no seq of its own: it reuses the last replayed frame's seq
                    // (or the client's cursor when there is nothing to replay)
                    // and is sent last, so the stream stays monotonic and the
                    // marker means "you are now current at this seq".
                    let resumed_seq = frames.last().map_or(seq, |f| f.seq);
                    let mut out = frames;
                    out.push(hrdr_protocol::ServerFrame {
                        seq: resumed_seq,
                        msg: hrdr_protocol::ServerMsg::Resumed {},
                    });
                    out
                }
                // Gap: the client's cursor is behind the replay buffer. A fresh
                // snapshot (which takes its own seq) resynchronizes it.
                None => vec![s.build_snapshot()],
            };
        }
    }
    Vec::new()
}
