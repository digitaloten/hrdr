//! System-prompt assembly via minijinja.
//!
//! hrdr uses Jinja for its *own* prompt templating only — the model wire-format
//! chat template is applied server-side (e.g. by infr). Keep that boundary:
//! we emit structured messages, the server renders the model prompt.

use std::path::Path;

use anyhow::{Context, Result};
use hrdr_tools::ToolRegistry;
use minijinja::{Environment, context};
use serde::Serialize;

const SYSTEM_TEMPLATE: &str = include_str!("templates/system.j2");

#[derive(Serialize)]
struct ToolView {
    name: String,
    description: String,
}

/// Render the agent system prompt for the given tool set and working directory.
/// `instructions` is the gathered AGENTS.md content (see [`gather_agent_docs`]).
pub fn render_system(
    tools: &ToolRegistry,
    cwd: &Path,
    instructions: Option<&str>,
) -> Result<String> {
    let mut env = Environment::new();
    env.add_template("system", SYSTEM_TEMPLATE)
        .context("loading system template")?;
    let tmpl = env.get_template("system")?;

    let views: Vec<ToolView> = tools
        .defs()
        .into_iter()
        .map(|d| ToolView {
            name: d.function.name,
            description: d.function.description,
        })
        .collect();

    tmpl.render(context! {
        cwd => cwd.display().to_string(),
        os => std::env::consts::OS,
        tools => views,
        instructions => instructions,
    })
    .context("rendering system template")
}

/// File name for the open-standard project instructions (https://agents.md).
const AGENTS_FILE: &str = "AGENTS.md";

/// Collect project instructions from `AGENTS.md` files, walking from `cwd` up to
/// the filesystem root, plus an optional global `~/.config/hrdr/AGENTS.md`. Less
/// specific files (global, then ancestors) come first so nearer files override
/// by appearing later. Returns `None` if nothing is found.
pub fn gather_agent_docs(cwd: &Path) -> Option<String> {
    // Walk up from cwd; collect cwd-first (most specific first).
    let mut docs: Vec<String> = Vec::new();
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if let Ok(text) = std::fs::read_to_string(d.join(AGENTS_FILE)) {
            let text = text.trim();
            if !text.is_empty() {
                docs.push(text.to_string());
            }
        }
        dir = d.parent();
    }
    // Reverse to outer-first (root ancestor … cwd).
    docs.reverse();

    // Global personal instructions, least specific of all. Same directory as
    // config.toml (XDG-aware, cross-platform) — see [`crate::config_dir`].
    if let Some(dir) = crate::config_dir()
        && let Ok(text) = std::fs::read_to_string(dir.join(AGENTS_FILE))
    {
        let text = text.trim();
        if !text.is_empty() {
            docs.insert(0, text.to_string());
        }
    }

    if docs.is_empty() {
        None
    } else {
        Some(docs.join("\n\n---\n\n"))
    }
}
