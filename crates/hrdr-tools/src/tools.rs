//! The seven MVP tools.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{TodoItem, Tool, ToolContext, truncate};

/// Hard cap on a rendered source line, so one minified file can't blow context.
const MAX_LINE: usize = 2_000;
const DEFAULT_READ_LIMIT: usize = 2_000;
const DEFAULT_BASH_TIMEOUT_MS: u64 = 120_000;

// ---- read_file ----

pub struct ReadTool;

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read_file"
    }
    fn description(&self) -> &'static str {
        "Read a file from disk. Returns 1-based line-numbered content. Use `offset`/`limit` \
         to page through large files instead of reading the whole thing."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to cwd."},
                "offset": {"type": "integer", "description": "1-based line to start at (default 1)."},
                "limit": {"type": "integer", "description": "Max lines to return (default 2000)."}
            },
            "required": ["path"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: ReadArgs = serde_json::from_value(args).context("invalid read_file args")?;
        let path = ctx.resolve(&a.path);
        let text = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        let start = a.offset.unwrap_or(1).max(1);
        let limit = a.limit.unwrap_or(DEFAULT_READ_LIMIT);
        let mut out = String::new();
        for (i, line) in text.lines().enumerate().skip(start - 1).take(limit) {
            let n = i + 1;
            let line = if line.len() > MAX_LINE {
                &line[..MAX_LINE]
            } else {
                line
            };
            out.push_str(&format!("{n:>6}\t{line}\n"));
        }
        if out.is_empty() {
            out.push_str("(file is empty or offset past end)");
        }
        Ok(truncate(&out, ctx.max_output))
    }
}

// ---- write_file ----

pub struct WriteTool;

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write_file"
    }
    fn description(&self) -> &'static str {
        "Create a new file or overwrite an existing one with `content`. Parent directories \
         are created as needed. Prefer `edit` for changing part of an existing file."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to cwd."},
                "content": {"type": "string", "description": "Full file contents to write."}
            },
            "required": ["path", "content"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: WriteArgs = serde_json::from_value(args).context("invalid write_file args")?;
        let path = ctx.resolve(&a.path);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let bytes = a.content.len();
        tokio::fs::write(&path, a.content)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(format!("Wrote {bytes} bytes to {}", path.display()))
    }
}

// ---- edit ----

pub struct EditTool;

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }
    fn description(&self) -> &'static str {
        "Replace an exact substring in a file. `old_string` must match uniquely unless \
         `replace_all` is set. This is the preferred, token-cheap way to mutate a file."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string", "description": "Exact text to replace (include surrounding context to make it unique)."},
                "new_string": {"type": "string", "description": "Replacement text."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)."}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: EditArgs = serde_json::from_value(args).context("invalid edit args")?;
        let path = ctx.resolve(&a.path);
        let text = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        let count = text.matches(&a.old_string).count();
        if count == 0 {
            bail!("old_string not found in {}", path.display());
        }
        if count > 1 && !a.replace_all {
            bail!(
                "old_string is not unique in {} ({count} matches) — add context or set replace_all",
                path.display()
            );
        }
        let updated = if a.replace_all {
            text.replace(&a.old_string, &a.new_string)
        } else {
            text.replacen(&a.old_string, &a.new_string, 1)
        };
        tokio::fs::write(&path, updated)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(format!(
            "Replaced {count} occurrence(s) in {}",
            path.display()
        ))
    }
}

// ---- bash ----

pub struct BashTool;

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }
    fn description(&self) -> &'static str {
        "Run a shell command via `bash -c` in the working directory. Use for build, test, \
         git, and anything without a dedicated tool. Output is captured and length-bounded."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to run."},
                "timeout_ms": {"type": "integer", "description": "Timeout in ms (default 120000)."}
            },
            "required": ["command"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: BashArgs = serde_json::from_value(args).context("invalid bash args")?;
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(&a.command)
            .current_dir(&ctx.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let timeout = Duration::from_millis(a.timeout_ms.unwrap_or(DEFAULT_BASH_TIMEOUT_MS));
        let output = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| anyhow!("command timed out after {}ms", timeout.as_millis()))?
            .context("spawning bash")?;
        let mut out = String::new();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stdout.is_empty() {
            out.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&stderr);
        }
        if !output.status.success() {
            out.push_str(&format!("\n[exit status: {}]", output.status));
        }
        if out.is_empty() {
            out.push_str("(no output)");
        }
        Ok(truncate(&out, ctx.max_output))
    }
}

// ---- grep ----

pub struct GrepTool;

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }
    fn description(&self) -> &'static str {
        "Search file contents with ripgrep. Returns `path:line:match`. Optionally scope to a \
         `path` and/or filter files with a `glob` (e.g. '*.rs')."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex pattern to search for."},
                "path": {"type": "string", "description": "File or directory to search (default cwd)."},
                "glob": {"type": "string", "description": "Glob to filter files, e.g. '*.rs'."}
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: GrepArgs = serde_json::from_value(args).context("invalid grep args")?;
        let mut cmd = tokio::process::Command::new("rg");
        cmd.arg("--line-number")
            .arg("--no-heading")
            .arg("--color=never")
            .current_dir(&ctx.cwd);
        if let Some(g) = &a.glob {
            cmd.arg("--glob").arg(g);
        }
        cmd.arg("--").arg(&a.pattern);
        if let Some(p) = &a.path {
            cmd.arg(p);
        }
        let output = cmd
            .output()
            .await
            .context("running ripgrep (is `rg` installed?)")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.is_empty() {
            // rg exits 1 with no output when there are no matches.
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.is_empty() {
                bail!("ripgrep: {}", stderr.trim());
            }
            return Ok("(no matches)".to_string());
        }
        Ok(truncate(&stdout, ctx.max_output))
    }
}

// ---- glob ----

pub struct GlobTool;

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }
    fn description(&self) -> &'static str {
        "Find files by glob pattern (supports `**`), relative to cwd. Returns matching paths."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern, e.g. 'src/**/*.rs'."}
            },
            "required": ["pattern"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: GlobArgs = serde_json::from_value(args).context("invalid glob args")?;
        let joined = ctx.cwd.join(&a.pattern);
        let pat = joined.to_string_lossy().to_string();
        let mut paths: Vec<String> = glob::glob(&pat)
            .with_context(|| format!("invalid glob pattern: {pat}"))?
            .filter_map(|r| r.ok())
            .map(|p| {
                p.strip_prefix(&ctx.cwd)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        paths.sort();
        if paths.is_empty() {
            return Ok("(no matches)".to_string());
        }
        Ok(truncate(&paths.join("\n"), ctx.max_output))
    }
}

// ---- todo_write ----

pub struct TodoTool;

#[derive(Deserialize)]
struct TodoArgs {
    todos: Vec<TodoItem>,
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo_write"
    }
    fn description(&self) -> &'static str {
        "Replace the task list for the current work. Use it to plan and track multi-step \
         coding tasks: mark exactly one item `in_progress`, the rest `pending`/`completed`."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: TodoArgs = serde_json::from_value(args).context("invalid todo_write args")?;
        let rendered = render_todos(&a.todos);
        if let Ok(mut todos) = ctx.todos.lock() {
            *todos = a.todos;
        }
        Ok(rendered)
    }
}

fn render_todos(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "(todo list cleared)".to_string();
    }
    let mut out = String::from("Updated task list:\n");
    for t in todos {
        let mark = match t.status.as_str() {
            "completed" => "x",
            "in_progress" => "~",
            _ => " ",
        };
        out.push_str(&format!("[{mark}] {}\n", t.content));
    }
    out
}
