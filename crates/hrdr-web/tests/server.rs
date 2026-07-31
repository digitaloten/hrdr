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

/// A bad `hrdr_session` cookie is counted by the rate limiter like any other
/// auth failure: ten of them lock the bucket, so the eleventh request is 429
/// even though it carries a *valid* cookie.
///
/// The ten failures go through `/ws`, the surface where a bad cookie is still
/// an auth failure. `GET /` answers an unusable cookie with the login page and
/// records nothing (see `users_mode_index_does_not_lock_out_a_stale_cookie`),
/// but it does still consult the limiter — which is exactly what the final
/// assertion proves, on a request whose cookie is perfectly good.
#[tokio::test]
async fn bad_session_cookie_is_rate_limited() {
    let (server, _session, secret) = start_test_server_users_mode().await;

    for i in 0..10 {
        let status = ws_upgrade_status(server.addr, Some("hrdr_session=not-a-cookie")).await;
        assert_eq!(status, 401, "ws attempt {i} should be 401");
    }

    let cookie = format!(
        "Cookie: hrdr_session={}",
        hrdr_web::auth::mint_session_cookie("alice", &secret[..])
    );
    let response = raw_get(server.addr, "/", &[&cookie]).await;
    assert!(
        response.contains("429 Too Many Requests"),
        "11th request should be rate limited even with a good cookie, got: {response}"
    );

    drop(server);
}

/// In `users` mode an unauthenticated `GET /` serves the login form, not a 401.
/// The only credential `check_auth` accepts in that mode is a cookie minted by
/// `POST /login`, so a 401 here leaves a browser with no way in at all.
#[tokio::test]
async fn users_mode_index_serves_the_login_page_without_a_cookie() {
    let (server, _session, _secret) = start_test_server_users_mode().await;

    let response = raw_get(server.addr, "/", &[]).await;
    assert!(
        response.contains("200 OK"),
        "unauthenticated / should serve the login page, got: {response}"
    );
    assert!(
        response.contains(r#"id="login-form""#)
            && response.contains(r#"id="username""#)
            && response.contains(r#"id="password""#),
        "login page must carry the credential form, got: {response}"
    );
    assert!(
        response.contains("fetch('/login'"),
        "the form must post to /login, got: {response}"
    );

    drop(server);
}

/// A *valid* cookie still gets the app page — the login page must not have
/// displaced it for users who are already signed in.
#[tokio::test]
async fn users_mode_index_with_a_valid_cookie_serves_the_app() {
    let (server, _session, secret) = start_test_server_users_mode().await;

    let cookie = format!(
        "Cookie: hrdr_session={}",
        hrdr_web::auth::mint_session_cookie("alice", &secret[..])
    );
    let response = raw_get(server.addr, "/", &[&cookie]).await;
    assert!(response.contains("200 OK"), "expected 200, got: {response}");
    assert!(
        !response.contains(r#"id="login-form""#),
        "an authenticated / must not serve the login page, got: {response}"
    );

    drop(server);
}

/// A cookie the server cannot verify — a stale one from a previous run, since
/// the cookie secret is regenerated on every start — also gets the login page,
/// and burns none of the rate-limit budget. Otherwise every browser in the
/// fleet would lock itself out of the very form that fixes its cookie.
#[tokio::test]
async fn users_mode_index_does_not_lock_out_a_stale_cookie() {
    let (server, _session, secret) = start_test_server_users_mode().await;

    for i in 0..11 {
        let response = raw_get(
            server.addr,
            "/",
            &["Cookie: hrdr_session=stale-from-last-run"],
        )
        .await;
        assert!(
            response.contains("200 OK") && response.contains(r#"id="login-form""#),
            "request {i} should serve the login page, got: {response}"
        );
    }

    // The bucket is untouched, so logging in still works.
    let cookie = format!(
        "Cookie: hrdr_session={}",
        hrdr_web::auth::mint_session_cookie("alice", &secret[..])
    );
    let response = raw_get(server.addr, "/", &[&cookie]).await;
    assert!(
        response.contains("200 OK"),
        "a good cookie after 11 login-page loads should still be 200, got: {response}"
    );

    drop(server);
}

/// `/ws` in `users` mode is unchanged: a hard 401 without a cookie, an upgrade
/// with one. The login page is a page, not a widening of the API surface — a
/// browser never navigates to `/ws`, and an HTML body is not a handshake.
#[tokio::test]
async fn users_mode_ws_still_rejects_without_a_cookie() {
    let (server, _session, secret) = start_test_server_users_mode().await;

    assert_eq!(
        ws_upgrade_status(server.addr, None).await,
        401,
        "ws without a cookie must stay 401"
    );

    let cookie = format!(
        "hrdr_session={}",
        hrdr_web::auth::mint_session_cookie("alice", &secret[..])
    );
    assert_eq!(
        ws_upgrade_status(server.addr, Some(&cookie)).await,
        101,
        "ws with a valid cookie should upgrade"
    );

    drop(server);
}

/// A valid session cookie authenticates and records nothing: eleven of them in
/// a row all succeed.
#[tokio::test]
async fn good_session_cookie_is_not_rate_limited() {
    let (server, _session, secret) = start_test_server_users_mode().await;

    let cookie = format!(
        "Cookie: hrdr_session={}",
        hrdr_web::auth::mint_session_cookie("alice", &secret[..])
    );
    for i in 0..11 {
        let response = raw_get(server.addr, "/", &[&cookie]).await;
        assert!(
            response.contains("200 OK"),
            "request {i} should be 200, got: {response}"
        );
    }

    drop(server);
}

/// In `basic` mode the 401 carries the `WWW-Authenticate` challenge — without
/// it a browser renders the bare body and never offers the credential prompt,
/// which makes the mode unusable from the deployment it exists for.
#[tokio::test]
async fn basic_mode_401_carries_the_challenge() {
    let (server, _session) = start_test_server_basic_mode().await;

    let response = raw_get(server.addr, "/", &[]).await;
    assert!(
        response.contains("401 Unauthorized"),
        "expected 401, got: {response}"
    );
    assert!(
        response.contains(r#"www-authenticate: Basic realm="hrdr", charset="UTF-8""#),
        "401 must carry the Basic challenge, got: {response}"
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

/// The origin the real UI sends — its own `location.host` — is accepted, while
/// the same loopback host on another port (another local app's page) is 403.
#[tokio::test]
async fn ws_origin_loopback_must_match_the_served_port() {
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let (server, _session, token) = start_test_server_with_token().await;

    let ws_url = format!("ws://{}/ws?token={token}", server.addr);
    let with_origin = |origin: String| {
        let mut request = ws_url.clone().into_client_request().expect("build request");
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
        request
    };

    let (_ws, resp) = connect_async(with_origin(format!("http://{}", server.addr)))
        .await
        .expect("same-origin upgrade should succeed");
    assert_eq!(resp.status(), 101);

    let other_port = format!("http://127.0.0.1:{}", server.addr.port().wrapping_add(1));
    match connect_async(with_origin(other_port)).await {
        Ok((_ws, resp)) => panic!("ws upgrade should fail, got status {}", resp.status()),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), 403, "another local port should be forbidden");
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

/// Attempt a WS upgrade, optionally carrying a `Cookie`, and return the HTTP
/// status the server answered the handshake with (101 once it succeeded). A
/// plain `GET /ws` is not usable for this: the `WebSocketUpgrade` extractor runs
/// before the handler's auth check and rejects a non-handshake request outright,
/// so the status under test would never be reached.
async fn ws_upgrade_status(addr: std::net::SocketAddr, cookie: Option<&str>) -> u16 {
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut request = format!("ws://{addr}/ws")
        .into_client_request()
        .expect("build ws request");
    if let Some(cookie) = cookie {
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
    }
    match connect_async(request).await {
        Ok((_ws, resp)) => resp.status().as_u16(),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => resp.status().as_u16(),
        Err(e) => panic!("unexpected ws error: {e}"),
    }
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

/// A server in `basic` auth mode with credentials configured, so the 401 path
/// under test is the one a real Basic deployment takes.
async fn start_test_server_basic_mode() -> (server::RunningServer, SharedSession) {
    let config = AgentConfig {
        cwd: std::path::PathBuf::from("/tmp"),
        api_key: Some("test-key".into()),
        ..Default::default()
    };

    let shared = SharedSession::start(config).await.expect("session");

    let mut web_cfg = WebConfig::load(&Default::default()).0;
    web_cfg.auth = hrdr_web::config::AuthMode::Basic;
    web_cfg.basic_user = Some("alice".into());
    web_cfg.basic_password_hash = Some(hrdr_web::auth::hash_password("s3cret").expect("hash"));
    let auth_state = AuthState::from_config(&web_cfg);

    let cfg = ServeConfig {
        bind: std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        port: 0,
    };
    let running = server::serve(shared.clone(), cfg, &web_cfg, auth_state)
        .await
        .expect("serve");

    (running, shared)
}

/// A server in `users` auth mode, plus the cookie secret so a test can mint a
/// session cookie the server will accept.
async fn start_test_server_users_mode() -> (server::RunningServer, SharedSession, [u8; 32]) {
    let config = AgentConfig {
        cwd: std::path::PathBuf::from("/tmp"),
        api_key: Some("test-key".into()),
        ..Default::default()
    };

    let shared = SharedSession::start(config).await.expect("session");

    let mut web_cfg = WebConfig::load(&Default::default()).0;
    web_cfg.auth = hrdr_web::config::AuthMode::Users;
    let auth_state = AuthState::from_config(&web_cfg);
    let secret = *auth_state.cookie_secret;

    let cfg = ServeConfig {
        bind: std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        port: 0,
    };
    let running = server::serve(shared.clone(), cfg, &web_cfg, auth_state)
        .await
        .expect("serve");

    (running, shared, secret)
}
