//! `hrdr-web` — the web server and headless session host for hrdr.
//!
//! `WebSession` owns the agent, folds panes, and emits `ServerFrame` deltas
//! on a broadcast channel. `serve()` (slice 3) wraps it in an axum HTTP+WS
//! server; this slice only delivers the session engine.

// Every test in this crate runs sandboxed — see hrdr-test-support for the
// life-before-main ctor that redirects $HOME and the XDG roots.
#[cfg(test)]
extern crate hrdr_test_support;

pub mod convert;
pub mod session;

pub use session::{SharedSession, WebSession};
