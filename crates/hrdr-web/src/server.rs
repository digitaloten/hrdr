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
    if let Err(resp) = check_auth(&state, peer.ip(), &headers, &query) {
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
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "60")],
            "rate limit exceeded",
        )
            .into_response();
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

    let ok = match hash_opt {
        Some(hash) => crate::users::verify_password(&body.password, &hash),
        None => {
            // Burn same argon2 work so timing can't leak "user exists".
            let _ = crate::users::verify_password(&body.password, crate::users::DUMMY_HASH);
            false
        }
    };

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

#[allow(clippy::result_large_err)]
fn check_auth(
    state: &AppState,
    peer: IpAddr,
    headers: &HeaderMap,
    query: &std::collections::HashMap<String, String>,
) -> Result<(), Response> {
    let client_ip = auth::extract_client_ip(peer, headers);
    if !auth::check_rate_limit(&state.auth, client_ip) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "60")],
            "rate limit exceeded",
        )
            .into_response());
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

// ── WS handler ─────────────────────────────────────────────────────────────

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
    let snap_json = serde_json::to_string(&snapshot).unwrap();
    if sender.send(Message::Text(snap_json.into())).await.is_err() {
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
                            let json = serde_json::to_string(&frame).unwrap();
                            if sender.send(Message::Text(json.into())).await.is_err() {
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
                            let json = serde_json::to_string(&snap).unwrap();
                            if sender.send(Message::Text(json.into())).await.is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                direct = direct_rx.recv() => {
                    match direct {
                        Some(frame) => {
                            let json = serde_json::to_string(&frame).unwrap();
                            if sender.send(Message::Text(json.into())).await.is_err() {
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
