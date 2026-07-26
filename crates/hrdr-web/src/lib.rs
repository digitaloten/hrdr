//! `hrdr-web` — the web server and headless session host for hrdr.

// Every test in this crate runs sandboxed — see hrdr-test-support for the
// life-before-main ctor that redirects $HOME and the XDG roots.
#[cfg(test)]
extern crate hrdr_test_support;

pub mod auth;
pub mod config;
pub mod convert;
pub mod host;
pub mod server;
pub mod session;
pub mod users;

pub use session::{SharedSession, WebSession};

#[cfg(feature = "ui")]
mod assets {
    #[derive(rust_embed::Embed)]
    #[folder = "../hrdr-ui/dist"]
    pub struct Assets;
}

/// Serve the SPA index.html when the `ui` feature is enabled.
#[cfg(feature = "ui")]
pub fn spa_index_html() -> Option<String> {
    use assets::Assets;
    Assets::get("index.html").map(|f| String::from_utf8_lossy(&f.data).to_string())
}

#[cfg(not(feature = "ui"))]
pub fn spa_index_html() -> Option<String> {
    None
}
