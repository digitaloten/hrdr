//! Starting a session fetches the models.dev catalog, so the `/model` selector
//! has models to list.
//!
//! The selector reads the cache synchronously — it builds its list on a keypress
//! and cannot await a fetch — and every other consumer fetched only as a *side
//! effect* of needing one model's context window. So whether the catalog ever
//! landed on disk depended on which code path happened to run: an endpoint that
//! answered `/v1/models` with a window, or a window already known from config or
//! a previous cache, meant nothing fetched and the selector offered nothing but
//! the configured model — on a fresh install, indefinitely.
//!
//! Served from a local socket rather than models.dev: this asserts hrdr's
//! behaviour, not a third party's uptime.

extern crate hrdr_test_support;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// A one-shot HTTP server for `/api.json`, returning `(url, join handle)`.
fn serve_catalog(body: &'static str) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    });
    (url, handle)
}

/// `warm()` writes the catalog to the cache, so a later synchronous
/// `load_cached()` — the selector's only source — can see it.
#[test]
fn warming_the_catalog_populates_the_cache_the_selector_reads() {
    const BODY: &str = r#"{"opencode":{"name":"opencode zen","models":{"grok-code":{"name":"Grok Code","limit":{"context":256000}}}}}"#;
    let (url, server) = serve_catalog(BODY);

    // SAFETY: single-threaded test binary, before any other thread exists. The
    // sandbox ctor disables fetching for every test binary; this one is about the
    // fetch, so it opts back in — against its own socket.
    unsafe {
        std::env::remove_var("HRDR_DISABLE_MODELS_FETCH");
        std::env::remove_var("HRDR_MODELS_PATH");
        std::env::set_var("HRDR_MODELS_URL", &url);
    }

    // Nothing cached to begin with: this is the fresh-install state.
    assert!(
        hrdr_llm::catalog::load_cached().is_none(),
        "the sandboxed cache starts empty"
    );

    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    rt.block_on(hrdr_llm::catalog::warm());
    let _ = server.join();

    // The selector's synchronous read now finds the provider's models.
    let cached = hrdr_llm::catalog::load_cached().expect("warm() wrote the cache");
    let (label, models) =
        hrdr_llm::catalog::provider_models(&cached, "opencode").expect("the provider is listed");
    assert_eq!(label, "opencode zen");
    assert_eq!(
        models,
        vec![("grok-code".to_string(), "Grok Code".to_string())]
    );

    // And it is a real file, so the next process starts warm.
    let started = Instant::now();
    while hrdr_llm::catalog::load_cached().is_none() && started.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(hrdr_llm::catalog::load_cached().is_some());
}
