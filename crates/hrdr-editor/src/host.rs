//! The hjkl `Host` implementation hrdr's editor runs against.
//!
//! Mirrors the well-trodden sqeel adapter: the host **owns the viewport** (the
//! renderer publishes dimensions, the engine writes scroll offsets). Clipboard
//! writes queue to an outbox flushed on a tick (non-blocking); reads pull the OS
//! clipboard directly (the same synchronous `hjkl_clipboard::get` the TUI's
//! `/paste` uses — a paste is user-initiated and rare, so it needn't be cached).

use std::time::{Duration, Instant};

use hjkl_clipboard::{Clipboard, MimeType, Selection};
use hjkl_engine::types::Viewport;
use hjkl_engine::{CursorShape, Host};

pub struct HrdrHost {
    clipboard: Option<Clipboard>,
    clipboard_outbox: Vec<String>,
    started: Instant,
    viewport: Viewport,
}

impl HrdrHost {
    pub fn new() -> Self {
        Self {
            clipboard: Clipboard::new().ok(),
            clipboard_outbox: Vec::new(),
            started: Instant::now(),
            viewport: Viewport {
                width: 80,
                height: 24,
                ..Viewport::default()
            },
        }
    }

    /// Flush queued clipboard writes to the OS (call once per tick).
    pub fn flush_clipboard(&mut self) {
        let outbox = std::mem::take(&mut self.clipboard_outbox);
        if let Some(cb) = &self.clipboard {
            for text in outbox {
                let _ = cb.set(Selection::Clipboard, MimeType::Text, text.as_bytes());
            }
        }
    }
}

impl Default for HrdrHost {
    fn default() -> Self {
        Self::new()
    }
}

impl Host for HrdrHost {
    type Intent = ();

    fn write_clipboard(&mut self, text: String) {
        self.clipboard_outbox.push(text);
    }

    fn read_clipboard(&mut self) -> Option<String> {
        let bytes = self
            .clipboard
            .as_ref()?
            .get(Selection::Clipboard, MimeType::Text)
            .ok()?;
        String::from_utf8(bytes).ok()
    }

    fn now(&self) -> Duration {
        self.started.elapsed()
    }

    fn should_cancel(&self) -> bool {
        false
    }

    fn prompt_search(&mut self) -> Option<String> {
        None
    }

    fn emit_cursor_shape(&mut self, _shape: CursorShape) {}

    fn emit_intent(&mut self, _intent: Self::Intent) {}

    fn viewport(&self) -> &Viewport {
        &self.viewport
    }

    fn viewport_mut(&mut self) -> &mut Viewport {
        &mut self.viewport
    }
}
