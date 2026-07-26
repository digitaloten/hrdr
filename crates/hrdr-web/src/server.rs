//! Axum HTTP+WS server with authentication, config-gated binding, TLS support,
//! and the full refuse-to-bind matrix from §6.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State, ws::Message, ws::WebSocketUpgrade};
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

    let state = AppState {
        session,
        auth: Arc::new(auth_state),
    };

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(index))
        .route("/ws", get(ws_handler))
        .route("/login", post(login_handler))
        .route("/logout", post(logout_handler))
        .with_state(state);

    // TLS or plain.
    if let (Some(cert), Some(key)) = (&web_cfg.tls_cert_path, &web_cfg.tls_key_path) {
        use axum_server::tls_rustls::RustlsConfig;
        let tls_cfg = RustlsConfig::from_pem_file(cert, key).await?;
        let handle = tokio::spawn(async move {
            axum_server::bind_rustls(addr, tls_cfg)
                .serve(app.into_make_service())
                .await
                .ok();
        });
        Ok(RunningServer { addr, handle })
    } else {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let bound_addr = listener.local_addr()?;
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
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
    headers: HeaderMap,
    query: Query<std::collections::HashMap<String, String>>,
) -> Response {
    match check_auth(&state, &headers, &query) {
        Ok(()) => axum::response::Html(INDEX_HTML).into_response(),
        Err(resp) => resp,
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Err(resp) = check_auth(&state, &headers, &query) {
        return resp;
    }
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    if let Err(status) = auth::check_ws_origin(origin, host) {
        return status.into_response();
    }
    let session = state.session.clone();
    ws.on_upgrade(move |socket| handle_socket(socket, session))
}

// ── login / logout ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

async fn login_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<LoginBody>,
) -> Response {
    let client_ip = auth::extract_client_ip(&headers);
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

    let db = state.auth.users_db.lock().unwrap();
    let ok = match &*db {
        Some(conn) => crate::users::verify(conn, &body.username, &body.password).unwrap_or(false),
        None => false,
    };
    drop(db);

    if !ok {
        auth::rate_limit_record(&state.auth, client_ip);
        return (StatusCode::UNAUTHORIZED, "bad credentials").into_response();
    }

    // Mint session cookie.
    let cookie_val = auth::mint_session_cookie(&body.username, &state.auth.cookie_secret[..]);
    let mut cookie =
        format!("hrdr_session={cookie_val}; HttpOnly; SameSite=Strict; Path=/; Max-Age=604800");
    // In tests we can't determine TLS from here. Only add Secure if TLS certs are loaded.
    // (Simplification: add Secure on non-loopback configs.)
    cookie.push_str("; Secure");

    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie.as_str())],
        "ok",
    )
        .into_response()
}

async fn logout_handler() -> Response {
    (
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            "hrdr_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0",
        )],
        "ok",
    )
        .into_response()
}

// ── auth helper ────────────────────────────────────────────────────────────

#[allow(clippy::result_large_err)]
fn check_auth(
    state: &AppState,
    headers: &HeaderMap,
    query: &std::collections::HashMap<String, String>,
) -> Result<(), Response> {
    let client_ip = auth::extract_client_ip(headers);
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
        AuthMode::Users => {
            // Check session cookie.
            if let Some(cookie_header) = headers.get(header::COOKIE)
                && let Ok(cookie_str) = cookie_header.to_str()
            {
                for part in cookie_str.split(';') {
                    let part = part.trim();
                    if let Some(val) = part.strip_prefix("hrdr_session=") {
                        let val = val.trim();
                        let ok = auth::verify_session_cookie(val, &state.auth.cookie_secret[..])
                            .is_some();
                        if ok {
                            return Ok(());
                        } else {
                            return Err((StatusCode::UNAUTHORIZED, "unauthorized").into_response());
                        }
                    }
                }
            }
            false
        }
    };

    if !authed {
        auth::rate_limit_record(&state.auth, client_ip);
        return Err((StatusCode::UNAUTHORIZED, "unauthorized").into_response());
    }

    Ok(())
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

    let (snapshot, mut broadcast_rx) = {
        let s = session.lock().await;
        s.subscribe()
    };
    let snap_json = serde_json::to_string(&snapshot).unwrap();
    if sender.send(Message::Text(snap_json.into())).await.is_err() {
        return;
    }

    let forward_handle = tokio::spawn(async move {
        loop {
            match broadcast_rx.recv().await {
                Ok(frame) => {
                    let json = serde_json::to_string(&frame).unwrap();
                    if sender.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("broadcast lagged by {n} messages, client may need reconnect");
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            Message::Text(text) => {
                let client_msg: hrdr_protocol::ClientMsg = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("invalid client message: {e}");
                        continue;
                    }
                };
                handle_client_msg(client_msg, &session).await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    forward_handle.abort();
}

async fn handle_client_msg(msg: hrdr_protocol::ClientMsg, session: &SharedSession) {
    match msg {
        hrdr_protocol::ClientMsg::Submit { pane, text } => {
            session.lock().await.submit(pane, text).await;
        }
        hrdr_protocol::ClientMsg::Command { .. } => {
            let mut s = session.lock().await;
            let seq = s.next_seq_internal();
            let frame = crate::convert::build_notice(seq, "commands land in a later slice".into());
            s.emit_internal(frame);
        }
        hrdr_protocol::ClientMsg::Cancel { pane } => {
            session.lock().await.cancel(pane);
        }
        hrdr_protocol::ClientMsg::SwitchPane { pane } => {
            session.lock().await.switch_pane(pane);
        }
        hrdr_protocol::ClientMsg::Resume { seq } => {
            let mut s = session.lock().await;
            match s.replay_after(seq) {
                Some(frames) => {
                    let resumed = hrdr_protocol::ServerFrame {
                        seq: s.next_seq_internal(),
                        msg: hrdr_protocol::ServerMsg::Resumed {},
                    };
                    s.emit_internal(resumed);
                    for frame in frames {
                        s.emit_internal(frame);
                    }
                }
                None => {
                    let snap = s.build_snapshot();
                    s.emit_internal(snap);
                }
            }
        }
    }
}
