use std::path::Path;

use anyhow::{Context, Result};

use crate::ToolContext;

pub struct FileChange {
    pub content_after: String,
    pub notes: Vec<String>,
}

/// Checkpoint the file, write `content`, run post-edit hooks, re-read if hooks
/// ran, then collect LSP diagnostics for the final content. Returns the
/// post-hook content plus hook/diagnostic notes (if any).
pub async fn apply_file_change(
    ctx: &ToolContext,
    path: &Path,
    hook_event: &str,
    content: &str,
) -> Result<FileChange> {
    ctx.checkpoint(path);
    tokio::fs::write(path, content)
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    let mut notes = crate::run_file_hooks(&ctx.hooks, hook_event, path, &ctx.cwd).await;
    let content_after = if !ctx.hooks.is_empty() {
        tokio::fs::read_to_string(path)
            .await
            .unwrap_or_else(|_| content.to_string())
    } else {
        content.to_string()
    };
    // Diagnostics run on the *post-hook* content — what's actually on disk.
    if let Some(lsp) = &ctx.lsp
        && let Some(note) = lsp.diagnostics_note(path, &content_after).await
    {
        notes.push(note);
    }
    Ok(FileChange {
        content_after,
        notes,
    })
}
