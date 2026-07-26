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

/// `/` and `/ws` reject requests that carry no credentials at all.
#[tokio::test]
async fn index_and_ws_reject_without_token() {
    use tokio_tungstenite::connect_async;

    let (server, _session, _token) = start_test_server_with_token().await;

    let response = raw_get(server.addr, "/", &[]).await;
    assert!(
        response.contains("401 Unauthorized"),
        "expected 401, got: {response}"
    );

    let ws_url = format!("ws://{}/ws", server.addr);
    let result = connect_async(&ws_url).await;
    match result {
        Ok((_ws, resp)) => panic!("ws upgrade should fail, got status {}", resp.status()),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), 401, "expected 401 on ws upgrade");
        }
        Err(e) => panic!("unexpected ws error: {e}"),
    }

    drop(server);
}

/// `/` with a valid Bearer token returns 200.
#[tokio::test]
async fn index_accepts_bearer_token() {
    let (server, _session, token) = start_test_server_with_token().await;

    let auth = format!("Authorization: Bearer {token}");
    let response = raw_get(server.addr, "/", &[&auth]).await;
    assert!(response.contains("200 OK"), "expected 200, got: {response}");

    drop(server);
}

/// Ten failed attempts from the same peer lock the bucket: the eleventh request
/// is 429 even when it carries the correct token. Proves the rate limiter keys
/// on the real peer address (loopback here, no `X-Forwarded-For` in play).
#[tokio::test]
async fn wrong_token_is_401_and_rate_limited() {
    let (server, _session, token) = start_test_server_with_token().await;

    for i in 0..10 {
        let response = raw_get(server.addr, "/", &["Authorization: Bearer wrong-token"]).await;
        assert!(
            response.contains("401 Unauthorized"),
            "attempt {i} should be 401, got: {response}"
        );
    }

    let auth = format!("Authorization: Bearer {token}");
    let response = raw_get(server.addr, "/", &[&auth]).await;
    assert!(
        response.contains("429 Too Many Requests"),
        "11th request should be rate limited even with the right token, got: {response}"
    );

    drop(server);
}

/// A WS upgrade from a foreign Origin is 403 even with a valid token.
#[tokio::test]
async fn ws_origin_foreign_is_rejected() {
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let (server, _session, token) = start_test_server_with_token().await;

    let ws_url = format!("ws://{}/ws?token={token}", server.addr);
    let mut request = ws_url.into_client_request().expect("build ws request");
    request
        .headers_mut()
        .insert("origin", "https://evil.example".parse().unwrap());

    match connect_async(request).await {
        Ok((_ws, resp)) => panic!("ws upgrade should fail, got status {}", resp.status()),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), 403, "foreign Origin should be forbidden");
        }
        Err(e) => panic!("unexpected ws error: {e}"),
    }

    drop(server);
}

/// A `Resume` is answered on the requesting socket only: the replay (or the
/// fallback snapshot) must never reach the other connected clients through the
/// shared broadcast.
#[tokio::test]
async fn resume_answers_only_the_requesting_socket() {
    use std::time::Duration;
    use tokio::time::timeout;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    let (server, _session, token) = start_test_server_with_token().await;
    let ws_url = format!("ws://{}/ws?token={token}", server.addr);

    let (ws_a, _) = connect_async(&ws_url).await.expect("client A connect");
    let (ws_b, _) = connect_async(&ws_url).await.expect("client B connect");
    let (_write_a, mut read_a) = ws_a.split();
    let (mut write_b, mut read_b) = ws_b.split();

    // Drain the startup traffic (snapshot + the first panes/status frames the
    // 100ms tick emits) so anything arriving later is attributable to `Resume`.
    drain_quiet(&mut read_a).await;
    drain_quiet(&mut read_b).await;

    let resume = serde_json::json!({ "type": "resume", "seq": 0 });
    write_b
        .send(Message::Text(resume.to_string().into()))
        .await
        .expect("send resume");

    // B is answered on its own socket.
    let mut b_answered = false;
    for _ in 0..10 {
        let frame = timeout(Duration::from_secs(2), read_b.next())
            .await
            .ok()
            .flatten()
            .and_then(|r| r.ok());
        let Some(frame) = frame else { break };
        let Ok(text) = frame.into_text() else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        // Either a replay tail terminated by `resumed`, or a gap snapshot.
        if v["type"] == "resumed" || v["type"] == "snapshot" {
            b_answered = true;
            break;
        }
    }
    assert!(b_answered, "client B should receive its resume response");

    // A must see nothing at all as a result of B's resume.
    let leaked = timeout(Duration::from_millis(500), read_a.next()).await;
    assert!(
        leaked.is_err(),
        "client A must not receive B's resume traffic, got {leaked:?}"
    );

    drop(server);
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Read until the socket goes quiet for 300ms.
async fn drain_quiet<S>(read: &mut S)
where
    S: futures_util::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    while let Ok(Some(Ok(_))) =
        tokio::time::timeout(std::time::Duration::from_millis(300), read.next()).await
    {}
}

/// Issue a single `GET` over a fresh connection and return the raw response.
async fn raw_get(addr: std::net::SocketAddr, path: &str, extra_headers: &[&str]) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    for header in extra_headers {
        request.push_str(header);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    String::from_utf8_lossy(&buf).to_string()
}

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
