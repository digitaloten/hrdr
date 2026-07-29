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
#[tokio::test]
async fn bad_session_cookie_is_rate_limited() {
    let (server, _session, secret) = start_test_server_users_mode().await;

    for i in 0..10 {
        let response = raw_get(server.addr, "/", &["Cookie: hrdr_session=not-a-cookie"]).await;
        assert!(
            response.contains("401 Unauthorized"),
            "attempt {i} should be 401, got: {response}"
        );
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

// ── escalation approvals ───────────────────────────────────────────────────

/// A command a dialog might be tempted to shorten — and whose tail is the whole
/// point, because approving it runs it with no sandbox at all.
const LONG_COMMAND: &str = "git push origin main --force-with-lease --set-upstream \
                            --no-verify --push-option=ci.skip ; rm -rf ~/projects";

/// A connected client is what makes an escalation *askable*, and a disconnected
/// one must stop counting immediately.
///
/// Both directions are bugs with teeth. A leaked registration leaves the gate
/// believing somebody is there, so every later escalation waits out the full 60s
/// before denying itself. A missing one denies requests while a user sits looking
/// at the page. The count is what tells a *second* tab from the last one leaving.
#[tokio::test]
async fn ws_clients_register_and_unregister_with_the_approval_gate() {
    use std::time::{Duration, Instant};
    use tokio_tungstenite::connect_async;

    let (server, session, token) = start_test_server_with_token().await;
    let gate = session
        .lock()
        .await
        .approval_gate()
        .expect("the session's agent has an approval gate");
    let ws_url = format!("ws://{}/ws?token={token}", server.addr);

    // Nobody attached: headless, and the answer is no *now* rather than in a
    // minute.
    assert!(!gate.can_answer(), "no client is connected yet");
    let started = Instant::now();
    assert_eq!(
        gate.request("git push", &["git push".to_string()], "why")
            .await,
        hrdr_tools::ApprovalDecision::Deny
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "with nobody to ask, a request must not wait out the timeout"
    );

    let (ws_a, _) = connect_async(&ws_url).await.expect("client A connect");
    wait_until(|| gate.can_answer())
        .await
        .expect("connecting registers the client as an answerer");

    let (ws_b, _) = connect_async(&ws_url).await.expect("client B connect");
    // Give B's handler the same chance to register that A had.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // A leaves. B is still sitting in front of the page, so the gate must still
    // be answerable — this is the assertion a bare flag (or an unpaired
    // unregister) fails.
    drop(ws_a);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        gate.can_answer(),
        "one tab closing must not stop the other from answering"
    );

    // The last one leaves: instant denial is restored.
    drop(ws_b);
    wait_until(|| !gate.can_answer())
        .await
        .expect("the last client leaving hands the registration back");
    let started = Instant::now();
    assert_eq!(
        gate.request("git push", &["git push".to_string()], "why")
            .await,
        hrdr_tools::ApprovalDecision::Deny
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a leaked registration would cost a full 60s timeout here"
    );

    drop(server);
}

/// The whole round trip over a real socket: a blocked tool call, the request on
/// the wire with the command intact, and an answer that resolves the call.
#[tokio::test]
async fn a_connected_client_sees_the_request_and_answers_it() {
    use std::time::Duration;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    let (server, session, token) = start_test_server_with_token().await;
    let gate = session.lock().await.approval_gate().expect("gate");
    let ws_url = format!("ws://{}/ws?token={token}", server.addr);

    let (ws, _) = connect_async(&ws_url).await.expect("client connect");
    let (mut write, mut read) = ws.split();
    wait_until(|| gate.can_answer()).await.expect("registered");
    drain_quiet(&mut read).await;

    let (id, waiting) = file_request(&session, &gate, LONG_COMMAND).await;

    let frame = next_approval_request(&mut read)
        .await
        .expect("the client is shown the request");
    assert_eq!(frame["id"], serde_json::json!(id));
    assert_eq!(
        frame["command"],
        serde_json::json!(LONG_COMMAND),
        "the command reaches the client whole — a dialog that elides the tail lies \
         about what it is asking for"
    );
    assert_eq!(frame["rules"], serde_json::json!(["git push"]));
    assert!(
        frame["reason"].as_str().is_some_and(|r| !r.is_empty()),
        "the reason is shown as-is"
    );

    let answer = serde_json::json!({
        "type": "answer_approval",
        "id": id,
        "decision": "session",
    });
    write
        .send(Message::Text(answer.to_string().into()))
        .await
        .expect("send answer");

    let decision = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the blocked call is unblocked by the answer")
        .expect("join");
    assert_eq!(decision, hrdr_tools::ApprovalDecision::Session);

    drop(server);
}

/// Two tabs, one question: both are shown it, the first answer is the one that
/// counts, and the loser is told the question is closed rather than left holding
/// a live dialog for a decision already made.
#[tokio::test]
async fn two_clients_both_see_it_and_only_the_first_answer_counts() {
    use std::time::Duration;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    let (server, session, token) = start_test_server_with_token().await;
    let gate = session.lock().await.approval_gate().expect("gate");
    let ws_url = format!("ws://{}/ws?token={token}", server.addr);

    let (ws_a, _) = connect_async(&ws_url).await.expect("client A connect");
    let (ws_b, _) = connect_async(&ws_url).await.expect("client B connect");
    let (mut write_a, mut read_a) = ws_a.split();
    let (mut write_b, mut read_b) = ws_b.split();
    wait_until(|| gate.can_answer()).await.expect("registered");
    tokio::time::sleep(Duration::from_millis(200)).await;
    drain_quiet(&mut read_a).await;
    drain_quiet(&mut read_b).await;

    let (id, waiting) = file_request(&session, &gate, LONG_COMMAND).await;

    for (who, read) in [("A", &mut read_a), ("B", &mut read_b)] {
        let frame = next_approval_request(read)
            .await
            .unwrap_or_else(|| panic!("client {who} was not shown the request"));
        assert_eq!(frame["id"], serde_json::json!(id));
        assert_eq!(frame["command"], serde_json::json!(LONG_COMMAND));
    }

    // A approves once. That is the decision the tool call gets.
    let approve = serde_json::json!({ "type": "answer_approval", "id": id, "decision": "once" });
    write_a
        .send(Message::Text(approve.to_string().into()))
        .await
        .expect("A answers");
    let decision = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("A's answer unblocks the call")
        .expect("join");
    assert_eq!(decision, hrdr_tools::ApprovalDecision::Once);

    // B, whose dialog was still up when it clicked, is told it is closed…
    let closed = next_frame_of_type(&mut read_b, "approval_closed")
        .await
        .expect("B is told the question is over");
    assert_eq!(closed["id"], serde_json::json!(id));

    // …and B's late answer changes nothing: the gate has no such id any more.
    let deny = serde_json::json!({ "type": "answer_approval", "id": id, "decision": "deny" });
    write_b
        .send(Message::Text(deny.to_string().into()))
        .await
        .expect("B answers late");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !gate.is_pending(&id),
        "the request stayed resolved by A's answer"
    );

    drop(server);
}

/// An approval hands a command the whole machine, so it must be unreachable
/// without credentials: the socket that would carry the answer is refused at the
/// upgrade, and the question stays open for whoever *is* authenticated.
#[tokio::test]
async fn an_unauthenticated_client_cannot_answer_an_approval() {
    use std::time::Duration;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    let (server, session, token) = start_test_server_with_token().await;
    let gate = session.lock().await.approval_gate().expect("gate");
    let ws_url = format!("ws://{}/ws?token={token}", server.addr);

    // One authenticated client, so the gate has somebody to ask.
    let (ws, _) = connect_async(&ws_url).await.expect("client connect");
    let (mut write, mut read) = ws.split();
    wait_until(|| gate.can_answer()).await.expect("registered");
    drain_quiet(&mut read).await;

    let (id, waiting) = file_request(&session, &gate, LONG_COMMAND).await;
    next_approval_request(&mut read)
        .await
        .expect("the authenticated client is shown the request");

    // The intruder has no way in at all: the upgrade is refused, so
    // `answer_approval` has no socket to arrive on.
    let unauth = format!("ws://{}/ws", server.addr);
    match connect_async(&unauth).await {
        Ok((_ws, resp)) => panic!("unauthenticated ws upgrade succeeded: {}", resp.status()),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status(), 401, "unauthenticated upgrade must be 401");
        }
        Err(e) => panic!("unexpected ws error: {e}"),
    }
    // A refused connection also registered nothing, and answered nothing.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        gate.is_pending(&id),
        "the request must still be waiting on the real user"
    );

    // …and the authenticated client's answer is the one that lands.
    let deny = serde_json::json!({ "type": "answer_approval", "id": id, "decision": "deny" });
    write
        .send(Message::Text(deny.to_string().into()))
        .await
        .expect("send answer");
    let decision = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the authenticated answer lands")
        .expect("join");
    assert_eq!(decision, hrdr_tools::ApprovalDecision::Deny);

    drop(server);
}

// ── helpers ────────────────────────────────────────────────────────────────

/// File an escalation request on the session's own gate and publish it exactly
/// as a running turn does — the turn loop reads the request off the gate's
/// channel and re-emits it as an `AgentEvent`, which is what this hands to the
/// session's frontend hook.
///
/// Returns the id and the handle of the "blocked tool call", so a test can assert
/// what the answer resolved it to.
async fn file_request(
    session: &SharedSession,
    gate: &std::sync::Arc<hrdr_tools::ApprovalGate>,
    command: &str,
) -> (
    String,
    tokio::task::JoinHandle<hrdr_tools::ApprovalDecision>,
) {
    let asked = gate.clone();
    let owned = command.to_string();
    let waiting = tokio::spawn(async move {
        asked
            .request(&owned, &["git push".to_string()], "why this is being asked")
            .await
    });

    // The gate's receiver belongs to the agent, so a test cannot read the id off
    // it — but ids are minted `esc-N` from 1 per gate and nothing else in this
    // session files one. Asserted rather than assumed: if that ever changes, this
    // fails here and loudly instead of testing nothing.
    let id = "esc-1".to_string();
    let pending = gate.clone();
    let probe = id.clone();
    wait_until(move || pending.is_pending(&probe))
        .await
        .expect("the request was filed under the id this test expects");

    session
        .lock()
        .await
        .deliver_agent_event(hrdr_agent::AgentEvent::ApprovalRequested {
            id: id.clone(),
            command: command.to_string(),
            reason: "why this is being asked".to_string(),
            rules: vec!["git push".to_string()],
            allow_session: true,
        });
    (id, waiting)
}

/// Poll `cond` until it holds, or give up after two seconds.
async fn wait_until<F: FnMut() -> bool>(mut cond: F) -> Result<(), &'static str> {
    for _ in 0..200 {
        if cond() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    Err("condition never held")
}

/// The next `approval_requested` frame on this socket, as JSON.
async fn next_approval_request<S>(read: &mut S) -> Option<serde_json::Value>
where
    S: futures_util::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    next_frame_of_type(read, "approval_requested").await
}

/// The next frame of `kind` on this socket, as JSON.
async fn next_frame_of_type<S>(read: &mut S, kind: &str) -> Option<serde_json::Value>
where
    S: futures_util::Stream<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    for _ in 0..200 {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), read.next())
            .await
            .ok()
            .flatten()
            .and_then(|r| r.ok())?;
        let Ok(text) = frame.into_text() else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if v["type"] == kind {
            return Some(v);
        }
    }
    None
}

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
