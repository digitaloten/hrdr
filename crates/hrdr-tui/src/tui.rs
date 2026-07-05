//! The terminal driver: owns the ratatui `Terminal` + the crossterm event loop,
//! translating input into [`App`] method calls and rendering `App` state. This
//! is the only place tied to the terminal — `App` itself carries no terminal
//! I/O or renderer types, so a GUI frontend can drive the same `App` with its
//! own loop + renderer.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;

use crate::app::{Action, App, run_editor};
use crate::{Tui, resume_terminal, suspend_terminal, ui};

/// Drive `app` against the terminal until it quits: draw, then await terminal
/// input, agent messages, config-file changes, or a spinner tick.
pub(crate) async fn run_loop(app: &mut App, terminal: &mut Tui) -> Result<()> {
    // Probe the endpoint in the background and warn if it's unreachable or
    // doesn't have the configured model — surfaced before the first turn.
    app.spawn_health_check();
    let mut events = EventStream::new();
    let mut rx = app.rx.take().expect("run_loop called once");
    // Periodic wake so the inference spinner animates between tokens.
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    // Shared config watch (OS watcher with polling fallback); pings arrive as
    // TurnMsg::ConfigChanged. Kept alive for the loop.
    let _config_watch = app.start_config_watch();

    loop {
        terminal.draw(|f| ui::draw(f, app))?;
        if app.should_quit {
            break;
        }

        tokio::select! {
            maybe_ev = events.next() => match maybe_ev {
                Some(Ok(Event::Key(key))) => match app.on_key(key) {
                    Action::OpenEditor => open_in_editor(app, terminal)?,
                    Action::OpenFile(path) => open_file_in_editor(app, terminal, &path)?,
                    Action::Redraw => terminal.clear()?,
                    Action::None => {}
                },
                Some(Ok(Event::Mouse(m))) => app.on_mouse(m),
                Some(Ok(Event::Paste(text))) => {
                    app.quit_armed = false;
                    app.editor.paste(&text);
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            Some(msg) = rx.recv() => {
                app.on_turn_msg(msg);
                // Drain any further messages that arrived in the same burst so
                // fast-streaming endpoints don't cause 100+ full redraws/sec —
                // all buffered tokens are folded into state before the next draw.
                while let Ok(msg) = rx.try_recv() {
                    app.on_turn_msg(msg);
                }
            }
            _ = ticker.tick() => {}
        }
    }
    Ok(())
}

/// Hand the input buffer to `$EDITOR`/`$VISUAL`, then read it back.
fn open_in_editor(app: &mut App, terminal: &mut Tui) -> Result<()> {
    let path = std::env::temp_dir().join(format!("hrdr-input-{}.md", std::process::id()));
    std::fs::write(&path, app.editor.content())?;

    suspend_terminal(terminal)?;
    let status = run_editor(&path);
    resume_terminal(terminal)?;
    terminal.clear()?;

    if status.is_ok()
        && let Ok(text) = std::fs::read_to_string(&path)
    {
        // Editors append a trailing newline; drop one so it doesn't submit blank.
        let text = text.strip_suffix('\n').unwrap_or(&text);
        app.editor.set_content(text);
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// Open an arbitrary file in `$EDITOR` (from `/edit <file>`), suspending the TUI
/// for the duration. The file may not exist yet — the editor creates it.
fn open_file_in_editor(app: &mut App, terminal: &mut Tui, path: &std::path::Path) -> Result<()> {
    suspend_terminal(terminal)?;
    let status = run_editor(path);
    resume_terminal(terminal)?;
    terminal.clear()?;
    match status {
        Ok(_) => app.system(format!("edited {}", path.display())),
        Err(e) => app.system(format!("editor failed: {e}")),
    }
    Ok(())
}
