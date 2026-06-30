//! The hjkl `Host` implementation hrdr's editor runs against.
//!
//! Mirrors the well-trodden sqeel adapter: the host **owns the viewport**
//! (the renderer publishes dimensions, the engine writes scroll offsets), and
//! clipboard I/O is fully decoupled — writes queue to an outbox flushed on a
//! tick, reads return a cached slot, so the engine never blocks.

use std::time::{Duration, Instant};

use hjkl_clipboard::{Clipboard, MimeType, Selection};
use hjkl_engine::types::Viewport;
use hjkl_engine::{CursorShape, Host};

pub struct HrdrHost {
    last_cursor_shape: CursorShape,
    clipboard: Option<Clipboard>,
    clipboard_cache: Option<String>,
    clipboard_outbox: Vec<String>,
    started: Instant,
    cancel: bool,
    viewport: Viewport,
}

impl HrdrHost {
    pub fn new() -> Self {
        Self {
            last_cursor_shape: CursorShape::Block,
            clipboard: Clipboard::new().ok(),
            clipboard_cache: None,
            clipboard_outbox: Vec::new(),
            started: Instant::now(),
            cancel: false,
            viewport: Viewport {
                width: 80,
                height: 24,
                ..Viewport::default()
            },
        }
    }

    pub fn cursor_shape(&self) -> CursorShape {
        self.last_cursor_shape
    }

    /// Pull the OS clipboard into the read cache (call before a paste key).
    pub fn refresh_clipboard_cache(&mut self) {
        if let Some(cb) = &mut self.clipboard {
            self.clipboard_cache = cb
                .get(Selection::Clipboard, MimeType::Text)
                .ok()
                .and_then(|b| String::from_utf8(b).ok());
        }
    }

    /// Flush queued clipboard writes to the OS (call once per tick).
    pub fn flush_clipboard(&mut self) {
        let outbox = std::mem::take(&mut self.clipboard_outbox);
        if let Some(cb) = &mut self.clipboard {
            for text in outbox {
                let _ = cb.set(Selection::Clipboard, MimeType::Text, text.as_bytes());
            }
        }
    }

    pub fn set_cancel(&mut self, cancel: bool) {
        self.cancel = cancel;
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
        self.clipboard_cache.clone()
    }

    fn now(&self) -> Duration {
        self.started.elapsed()
    }

    fn should_cancel(&self) -> bool {
        self.cancel
    }

    fn prompt_search(&mut self) -> Option<String> {
        None
    }

    fn emit_cursor_shape(&mut self, shape: CursorShape) {
        self.last_cursor_shape = shape;
    }

    fn emit_intent(&mut self, _intent: Self::Intent) {}

    fn viewport(&self) -> &Viewport {
        &self.viewport
    }

    fn viewport_mut(&mut self) -> &mut Viewport {
        &mut self.viewport
    }
}
