//! Axum HTTP+WS server that wraps a `SharedSession`. Bound to loopback only
//! in this slice — auth and config gating land in slices 4/5.

use std::net::{IpAddr, SocketAddr};

use axum::Router;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;

use crate::session::SharedSession;

/// Configuration for `serve()`.
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
    /// Wait for the server to stop (blocks the calling task).
    pub async fn wait(self) {
        let _ = self.handle.await;
    }

    /// Request graceful shutdown by aborting the server task.
    pub fn shutdown(self) {
        self.handle.abort();
    }
}

/// Start the axum server on the given config. Hardcodes loopback-only:
/// returns an error if `bind` is not loopback (auth lands in slice 4).
pub async fn serve(session: SharedSession, cfg: ServeConfig) -> anyhow::Result<RunningServer> {
    if !cfg.bind.is_loopback() {
        anyhow::bail!("authentication is not implemented yet — bind 127.0.0.1 only");
    }

    let addr = SocketAddr::new(cfg.bind, cfg.port);

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(index))
        .route(
            "/ws",
            get(move |ws: WebSocketUpgrade| ws_handler(ws, session.clone())),
        );

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

// ── routes ─────────────────────────────────────────────────────────────────

async fn healthz() -> &'static str {
    "ok"
}

/// Placeholder index page (replaced by the Dioxus SPA in slice 7).
async fn index() -> impl IntoResponse {
    axum::response::Html(INDEX_HTML)
}

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

async fn ws_handler(ws: WebSocketUpgrade, session: SharedSession) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, session))
}

async fn handle_socket(socket: axum::extract::ws::WebSocket, session: SharedSession) {
    let (mut sender, mut receiver) = socket.split();

    // Send snapshot on connect.
    let (snapshot, mut broadcast_rx) = {
        let s = session.lock().await;
        s.subscribe()
    };
    let snap_json = serde_json::to_string(&snapshot).unwrap();
    if sender.send(Message::Text(snap_json.into())).await.is_err() {
        return;
    }

    // Spawn broadcast forwarder.
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

    // Read loop: parse ClientMsg and dispatch.
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
