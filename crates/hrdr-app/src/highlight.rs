//! Shared syntect resources for code highlighting. Both frontends highlight
//! fenced code blocks (and tint tool-output panels) with the same syntax set,
//! theme, and panel background — loading them once here keeps the two visually
//! identical and drops the duplicated setup. The actual span→color rendering
//! stays per-frontend (ratatui `Line`s vs floem `TextLayout`s).

use std::sync::OnceLock;

use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

/// The bundled syntax set, deserialized once.
pub fn syntax_set() -> &'static SyntaxSet {
    static SS: OnceLock<SyntaxSet> = OnceLock::new();
    SS.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// The highlight theme (base16-ocean.dark, falling back to any bundled theme).
pub fn syntect_theme() -> &'static Theme {
    static TH: OnceLock<Theme> = OnceLock::new();
    TH.get_or_init(|| {
        let ts = ThemeSet::load_defaults();
        ts.themes
            .get("base16-ocean.dark")
            .or_else(|| ts.themes.values().next())
            .cloned()
            .expect("syntect ships default themes")
    })
}

/// Panel background RGB for code blocks and tool output: the syntect theme's
/// background with a dark fallback. Frontends convert to their color type.
pub fn panel_bg_rgb() -> (u8, u8, u8) {
    syntect_theme()
        .settings
        .background
        .map(|c| (c.r, c.g, c.b))
        .unwrap_or((30, 32, 40))
}
