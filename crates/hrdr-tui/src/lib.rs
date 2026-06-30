//! `hrdr-tui` — the interactive terminal UI.
//!
//! Layout: a scrolling transcript (assistant text, reasoning, tool calls) above
//! a vim-keybound input pane. The agent runs on a background task; its
//! [`AgentEvent`]s stream over a channel and the UI selects them against
//! crossterm's async `EventStream`, so input stays responsive during a turn.
//!
//! Workflow: type in the input (Insert mode), `Esc` to Normal, `Enter` to send.

mod app;
mod ui;

use std::io::{Stdout, stdout};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use hrdr_agent::AgentConfig;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use app::App;

/// Restores the terminal to a sane state on drop, even on panic.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut out = stdout();
        execute!(
            out,
            EnterAlternateScreen,
            EnableBracketedPaste,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        )?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = stdout();
        let _ = execute!(
            out,
            PopKeyboardEnhancementFlags,
            LeaveAlternateScreen,
            DisableBracketedPaste,
        );
        let _ = disable_raw_mode();
    }
}

type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Launch the interactive TUI against the configured agent.
pub async fn run(config: AgentConfig) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal: Tui = Terminal::new(backend)?;

    let mut app = App::new(config)?;
    app.run(&mut terminal).await?;
    Ok(())
}
