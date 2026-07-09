//! Chat-UI color theme for the terminal.
//!
//! hrdr reuses hjkl's theme system (a theme TOML with a palette + `[ui]`
//! styles). The role mapping — which palette entries feed which chat role —
//! is shared with the GUI via [`hrdr_app::ChatPalette`]; this module only
//! converts the resolved RGB roles to ratatui colors with ANSI fallbacks for
//! anything the theme omits.

use hjkl_markdown_tui::MdTheme;
use hrdr_app::ChatPalette;
use ratatui::style::Color;

/// Resolved colors for hrdr's chat surfaces.
#[derive(Debug, Clone)]
pub struct Theme {
    /// User prompt accent (the `❯` and user text).
    pub user: Color,
    /// User prompt background.
    pub user_bg: Color,
    /// Assistant message text.
    pub assistant: Color,
    /// Dimmed chrome: reasoning, system lines, stats, borders, hints, scrollbar.
    pub dim: Color,
    /// Attention color: tool names, the inference loader, the follow button.
    pub warn: Color,
    /// Tool-call block background.
    pub tool_bg: Color,
    /// Success marks (tool ✓).
    pub success: Color,
    /// Error marks (tool ✗) and the quit-confirm banner.
    pub error: Color,
    /// Secondary accent (blue) — extra variety for status-bar sections.
    pub accent: Color,
    /// Tertiary accent (magenta/purple) — extra variety for status-bar sections.
    pub accent2: Color,
}

impl Theme {
    /// Load a theme from `path` (an hjkl theme TOML), falling back to hjkl's
    /// bundled default if the path is `None` or fails to parse.
    pub fn load(path: Option<&str>) -> Self {
        Self::from_palette(&ChatPalette::load(path))
    }

    /// Apply terminal-appropriate (ANSI) fallbacks to the shared role palette —
    /// the role→palette-entry mapping itself lives in [`ChatPalette`].
    fn from_palette(p: &ChatPalette) -> Self {
        let c = |rgb: Option<(u8, u8, u8)>, fb: Color| {
            rgb.map(|(r, g, b)| Color::Rgb(r, g, b)).unwrap_or(fb)
        };
        Self {
            user: c(p.user, Color::Cyan),
            user_bg: c(p.user_bg, Color::Rgb(0, 48, 60)),
            assistant: c(p.assistant, Color::White),
            dim: c(p.dim, Color::DarkGray),
            warn: c(p.warn, Color::Yellow),
            tool_bg: c(p.tool_bg, Color::Rgb(30, 32, 40)),
            success: c(p.success, Color::Green),
            error: c(p.error, Color::Red),
            accent: c(p.accent, Color::Blue),
            accent2: c(p.accent2, Color::Magenta),
        }
    }

    /// Markdown render theme derived from these chat colors, so assistant
    /// markdown follows the active hjkl theme.
    pub fn md_theme(&self) -> MdTheme {
        MdTheme::new(
            self.assistant, // text
            self.user,      // heading1
            self.warn,      // heading 2-6
            self.success,   // inline code span
            self.success,   // code block
            self.user,      // link
            self.warn,      // list bullet
            self.assistant, // bold
            self.assistant, // italic
            self.dim,       // rule
        )
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::load(None)
    }
}
