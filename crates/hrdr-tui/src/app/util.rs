//! Free helper functions with no `App` receiver.

use std::time::SystemTime;

use tokio::sync::mpsc;

/// Set up an OS-level watch on the config file, pinging `()` on the returned
/// channel whenever it changes. Returns `None` if a watcher can't be created
/// (the caller falls back to mtime polling). The watcher must be kept alive for
/// the watch to stay active.
pub(crate) fn setup_config_watcher()
-> Option<(notify::RecommendedWatcher, mpsc::UnboundedReceiver<()>)> {
    use notify::{RecursiveMode, Watcher};
    let path = hrdr_agent::config_file_path()?;
    let dir = path.parent()?.to_path_buf();
    // Watch the parent directory (so atomic saves via rename are caught) and
    // filter to our file. Create the dir so the watch can be established.
    let _ = std::fs::create_dir_all(&dir);
    let file_name = path.file_name()?.to_os_string();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res
            && event
                .paths
                .iter()
                .any(|p| p.file_name() == Some(file_name.as_os_str()))
        {
            let _ = tx.send(());
        }
    })
    .ok()?;
    watcher.watch(&dir, RecursiveMode::NonRecursive).ok()?;
    Some((watcher, rx))
}
/// Modified-time of the user config file, for the hot-reload dedup guard.
pub(super) fn current_config_mtime() -> Option<SystemTime> {
    hrdr_agent::config_file_path()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
}
/// Current local time, for per-message timestamps.
pub(super) fn timestamp_now() -> chrono::DateTime<chrono::Local> {
    chrono::Local::now()
}
/// Run `$VISUAL`/`$EDITOR` (falling back to `vi`) on `path`, inheriting stdio.
/// The command string may carry args (e.g. `code -w`), split on whitespace.
pub(crate) fn run_editor(path: &std::path::Path) -> std::io::Result<std::process::ExitStatus> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    std::process::Command::new(program)
        .args(parts)
        .arg(path)
        .status()
}
