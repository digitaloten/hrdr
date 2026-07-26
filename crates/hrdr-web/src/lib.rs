//! `hrdr-web` — the web server and headless session host for hrdr.

// Every test in this crate runs sandboxed — see hrdr-test-support for the
// life-before-main ctor that redirects $HOME and the XDG roots.
#[cfg(test)]
extern crate hrdr_test_support;

pub mod auth;
pub mod config;
pub mod convert;
pub mod server;
pub mod session;

pub use session::{SharedSession, WebSession};
