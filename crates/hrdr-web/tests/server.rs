//! Integration tests for the axum HTTP+WS server.

extern crate hrdr_test_support;

use std::net::Ipv4Addr;

use futures_util::{SinkExt, StreamExt};
use hrdr_agent::AgentConfig;
use hrdr_web::SharedSession;
use hrdr_web::auth::AuthState;
use hrdr_web::config::WebConfig;
use hrdr_web::server::{self, ServeConfig};

/// `/healthz` returns 200 OK with body "ok".
#[tokio::test]
async fn healthz_answers_ok() {
    let (server, _session) = start_test_server().await;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(server.addr)
        .await
        .expect("connect");
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let response = String::from_utf8_lossy(&buf);
    assert!(response.contains("200 OK"), "expected 200, got: {response}");
    assert!(
        response.contains("ok"),
        "expected body 'ok', got: {response}"
    );

    drop(server);
}

/// WebSocket: connect with token, get snapshot, submit, get entries frames.
#[tokio::test]
async fn ws_snapshot_then_delta() {
    use tokio_tungstenite::connect_async;

    let (server, _session, token) = start_test_server_with_token().await;

    let ws_url = format!("ws://{}/ws?token={token}", server.addr);
    let (ws, _resp) = connect_async(&ws_url).await.expect("ws connect");
    let (mut write, mut read) = ws.split();

    let first = read.next().await.expect("first frame").expect("ok frame");
    let first_text = first.to_text().unwrap();
    let first_value: serde_json::Value =
        serde_json::from_str(first_text).expect("parse first frame");
    assert_eq!(first_value["type"], "snapshot");
    // Not `== 1`: the 100ms tick task that `SharedSession::start` spawns emits its first
    // panes+status frames as soon as it runs (both `last_*_json` caches start empty), and
    // that can happen before this client connects — under parallel test load it usually
    // does, leaving the snapshot at seq 3. The guarantee is a positive, monotonic seq, not
    // a particular value, so assert only that.
    let first_seq = first_value["seq"]
        .as_u64()
        .expect("snapshot carries a numeric seq");
    assert!(
        first_seq >= 1,
        "snapshot seq must be positive, got {first_seq}"
    );

    let submit = serde_json::json!({
        "type": "submit",
        "pane": "main",
        "text": "hi"
    });
    let msg = tokio_tungstenite::tungstenite::Message::Text(submit.to_string().into());
    write.send(msg).await.unwrap();

    let mut saw_user = false;
    for _ in 0..100 {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), read.next())
            .await
            .ok()
            .flatten()
            .and_then(|r| r.ok());
        let Some(frame) = frame else {
            break;
        };
        let text = frame.to_text().unwrap();
        let v: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v["type"] == "entries" {
            for entry in v["entries"].as_array().unwrap_or(&vec![]) {
                let ek = &entry["entry"]["kind"];
                let ed = &entry["entry"]["data"];
                if ek == "user" && ed == "hi" {
                    saw_user = true;
                }
            }
        }
        if saw_user {
            break;
        }
    }
    assert!(saw_user, "should see user message in transcript");

    drop(server);
}

/// Binding a non-loopback address returns an error (auth not implemented).
#[tokio::test]
async fn serve_refuses_non_loopback() {
    let config = AgentConfig::default();
    let shared = SharedSession::start(config).await.expect("session");

    let web_cfg = WebConfig::load(&Default::default()).0;
    let auth_state = AuthState::from_config(&web_cfg);

    let cfg = ServeConfig {
        bind: std::net::IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
        port: 0,
    };
    let result = server::serve(shared, cfg, &web_cfg, auth_state).await;
    match result {
        Ok(_) => panic!("expected error for non-loopback bind"),
        Err(e) => {
            let err = e.to_string();
            assert!(
                err.contains("allow-remote") || err.contains("TLS"),
                "error should mention remote/TLS: {err}"
            );
        }
    }
}

/// Helpers
async fn start_test_server() -> (server::RunningServer, SharedSession) {
    let (srv, sess, _tok) = start_test_server_with_token().await;
    (srv, sess)
}

async fn start_test_server_with_token() -> (server::RunningServer, SharedSession, String) {
    let config = AgentConfig {
        cwd: std::path::PathBuf::from("/tmp"),
        api_key: Some("test-key".into()),
        ..Default::default()
    };

    let shared = SharedSession::start(config).await.expect("session");

    let web_cfg = WebConfig::load(&Default::default()).0;
    let auth_state = AuthState::from_config(&web_cfg);
    let token = auth_state.token_str().unwrap().to_string();

    let cfg = ServeConfig {
        bind: std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        port: 0,
    };
    let running = server::serve(shared.clone(), cfg, &web_cfg, auth_state)
        .await
        .expect("serve");

    (running, shared, token)
}
