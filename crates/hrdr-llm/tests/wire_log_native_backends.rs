//! `HRDR_LOG_REQUESTS` covers the native backends too: a request that never gets
//! a reply is still in the log.
//!
//! The Anthropic and Codex paths used to be logged by `Client::chat_stream`
//! *after* their `chat_stream` returned `Ok` — but the send and the status check
//! happen inside that call, so a 401 (or a refused connection) propagated and the
//! turn left no trace at all: the exact failure the wire log exists to diagnose
//! was invisible on 2 of 3 backends. Both now log the body before the send.
//!
//! Its own integration binary because it sets a process-global env var, which
//! would race the unit tests in the library's test binary. One test, not two, for
//! the same reason: cargo runs a binary's tests on parallel threads.

// This is its own test binary: it does NOT get the library's `#[cfg(test)]` code, so it
// links the sandbox ctor itself. Without this line the test would run against the
// developer's real `$HOME`. Every `tests/*.rs` in the workspace carries it, and
// `every_test_binary_is_sandboxed` fails the build for one that does not.
extern crate hrdr_test_support;

use hrdr_llm::{ChatMessage, Client};

/// Both native backends write a `request` record for a send that fails, and
/// neither writes the credential.
///
/// The failure is manufactured without any network: the port is out of range, so
/// `reqwest` rejects the URL when the request is built and `send()` returns the
/// stored parse error — no DNS lookup, no socket, no reachable-host assumption in
/// CI. Backend detection keys on the *host*, which is still `api.anthropic.com` /
/// `chatgpt.com`, so the native paths are the ones exercised.
#[tokio::test]
async fn a_failed_send_is_logged_for_anthropic_and_codex() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("wire.log");

    // SAFETY: this binary runs only this test, and nothing else writes this var.
    unsafe { std::env::set_var("HRDR_LOG_REQUESTS", &log) };

    const KEY: &str = "sk-must-never-be-logged";
    let messages = [ChatMessage::user("marker-prompt")];

    for base_url in [
        "http://api.anthropic.com:99999/v1",
        "http://chatgpt.com:99999/backend-api/codex",
    ] {
        let client = Client::new(base_url, Some(KEY.to_string()), "claude-sonnet-4-5");
        assert!(
            client.chat_stream(&messages, &[]).await.is_err(),
            "{base_url} must fail to send (that is the point of the test)"
        );
    }

    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&log)
        .expect("the wire log was created")
        .lines()
        .map(|l| serde_json::from_str(l).expect("each line is one JSON object"))
        .collect();

    let requests: Vec<&serde_json::Value> =
        lines.iter().filter(|r| r["kind"] == "request").collect();
    assert_eq!(
        requests.len(),
        2,
        "one `request` record per failed send, Anthropic and Codex: {lines:#?}"
    );
    assert_eq!(
        requests[0]["url"],
        "http://api.anthropic.com:99999/v1/messages"
    );
    assert_eq!(
        requests[1]["url"],
        "http://chatgpt.com:99999/backend-api/codex/responses"
    );
    // The body is the whole reason to log: it must be the real one, not a stub.
    for req in &requests {
        assert!(
            req["body"].to_string().contains("marker-prompt"),
            "the logged body must carry the conversation: {req:#?}"
        );
    }

    // The credential is a header on both backends (`x-api-key` / `Bearer`) and
    // never reaches the body builders. Assert it on the file, not the parsed
    // records, so a future field that leaks one fails here too.
    let raw = std::fs::read_to_string(&log).unwrap();
    assert!(
        !raw.contains(KEY),
        "the wire log must never contain a credential"
    );
}
