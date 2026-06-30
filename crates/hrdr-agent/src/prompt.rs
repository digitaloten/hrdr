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
pub fn render_system(tools: &ToolRegistry, cwd: &Path) -> Result<String> {
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
    })
    .context("rendering system template")
}
