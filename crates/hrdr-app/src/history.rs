//! Persisted single-line input history, shared by hrdr's frontends. A newline-
//! delimited file under `$XDG_DATA_HOME/hrdr/history` holds the most recent
//! [`MAX_HISTORY`] submitted lines (oldest first) for Up/Down recall. No UI —
//! just load/save over the XDG data dir.

use std::path::PathBuf;

/// Max input-history entries kept (in memory and on disk).
pub const MAX_HISTORY: usize = 200;

/// Path to the persisted input history (`$XDG_DATA_HOME/hrdr/history`).
fn history_path() -> Option<PathBuf> {
    hjkl_xdg::data_dir("hrdr").ok().map(|d| d.join("history"))
}

/// Load persisted single-line input history (most recent [`MAX_HISTORY`], oldest
/// first). Blank lines are skipped; a missing/unreadable file yields an empty
/// history.
pub fn load_history() -> Vec<String> {
    let Some(path) = history_path() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut v: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    if v.len() > MAX_HISTORY {
        let drop = v.len() - MAX_HISTORY;
        v.drain(0..drop);
    }
    v
}

/// Persist input history (one entry per line; multi-line entries are skipped to
/// keep the line-based file well-formed). Best-effort — filesystem errors are
/// silently ignored.
pub fn persist_history(history: &[String]) {
    let Some(path) = history_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body: String = history
        .iter()
        .filter(|s| !s.contains('\n'))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::write(path, body);
}
