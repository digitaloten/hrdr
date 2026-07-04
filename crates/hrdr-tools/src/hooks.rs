//! Post-edit hooks: user-configured shell commands that run after `edit` /
//! `write` mutate a matching file — formatters, mostly (`cargo fmt`,
//! `prettier --write`). Mechanical like the guardrails: a config rule the
//! model can't forget. The mutating tool re-reads the file *after* hooks run,
//! so the diff the model sees (and the text its next `old_string` must match)
//! is the post-hook content.

use std::path::Path;
use std::time::Duration;

/// One configured hook: run `run` (with `{path}` substituted) after tool `on`
/// successfully mutates a file matching `glob`.
#[derive(Debug, Clone)]
pub struct Hook {
    /// Tool that triggers it: `edit` or `write` (`*` for both).
    pub on: String,
    /// File filter, matched against the file name and the cwd-relative path;
    /// `None` matches everything.
    pub glob: Option<glob::Pattern>,
    /// Shell command template; every `{path}` becomes the (quoted) file path.
    pub run: String,
    /// Kill the hook after this long (default [`DEFAULT_HOOK_TIMEOUT_MS`]).
    pub timeout_ms: u64,
}

/// Default per-hook timeout: formatters are fast; anything slower is stuck.
pub const DEFAULT_HOOK_TIMEOUT_MS: u64 = 30_000;

impl Hook {
    /// Whether this hook applies to `tool` mutating `path` (relative to `cwd`).
    fn matches(&self, tool: &str, path: &Path, cwd: &Path) -> bool {
        if self.on != "*" && self.on != tool {
            return false;
        }
        let Some(pat) = &self.glob else {
            return true;
        };
        let name_hit = path
            .file_name()
            .map(|n| pat.matches(&n.to_string_lossy()))
            .unwrap_or(false);
        let rel = path.strip_prefix(cwd).unwrap_or(path);
        name_hit || pat.matches_path(rel)
    }
}

/// Substitute `{path}` with the shell-quoted file path.
fn render_command(template: &str, path: &Path) -> String {
    let quoted = if cfg!(windows) {
        format!("\"{}\"", path.display())
    } else {
        // POSIX single-quote escaping: ' -> '\''.
        format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
    };
    template.replace("{path}", &quoted)
}

/// Run every hook matching (`tool`, `path`) sequentially, returning one
/// warning line per hook that failed or timed out (empty = all quiet).
/// Success output is discarded — the caller re-reads the file and diffs, so
/// the model sees the effect, not the chatter.
pub async fn run_file_hooks(hooks: &[Hook], tool: &str, path: &Path, cwd: &Path) -> Vec<String> {
    let mut notes = Vec::new();
    for hook in hooks.iter().filter(|h| h.matches(tool, path, cwd)) {
        let cmd_line = render_command(&hook.run, path);
        let mut cmd = if cfg!(windows) {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/C", &cmd_line]);
            c
        } else {
            let mut c = tokio::process::Command::new("bash");
            c.arg("-c").arg(&cmd_line);
            c
        };
        cmd.current_dir(cwd);
        cmd.kill_on_drop(true);
        let timeout = Duration::from_millis(hook.timeout_ms);
        match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(out)) if out.status.success() => {}
            Ok(Ok(out)) => {
                let mut detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
                if detail.is_empty() {
                    detail = String::from_utf8_lossy(&out.stdout).trim().to_string();
                }
                let detail = crate::truncate_inline(&detail, 300);
                notes.push(format!(
                    "[hook `{}` failed ({}){}]",
                    hook.run,
                    out.status,
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {detail}")
                    }
                ));
            }
            Ok(Err(e)) => notes.push(format!("[hook `{}` couldn't run: {e}]", hook.run)),
            Err(_) => notes.push(format!(
                "[hook `{}` timed out after {}ms; killed]",
                hook.run, hook.timeout_ms
            )),
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(on: &str, glob: Option<&str>, run: &str) -> Hook {
        Hook {
            on: on.to_string(),
            glob: glob.map(|g| glob::Pattern::new(g).unwrap()),
            run: run.to_string(),
            timeout_ms: DEFAULT_HOOK_TIMEOUT_MS,
        }
    }

    #[test]
    fn matching_by_tool_and_glob() {
        let cwd = Path::new("/proj");
        let h = hook("edit", Some("*.rs"), "true");
        assert!(h.matches("edit", Path::new("/proj/src/main.rs"), cwd));
        assert!(!h.matches("write", Path::new("/proj/src/main.rs"), cwd));
        assert!(!h.matches("edit", Path::new("/proj/README.md"), cwd));
        // `*` tool matches both; no glob matches every file.
        let any = hook("*", None, "true");
        assert!(any.matches("edit", Path::new("/proj/x"), cwd));
        assert!(any.matches("write", Path::new("/proj/x"), cwd));
        // Path-shaped globs match against the cwd-relative path.
        let nested = hook("edit", Some("src/**/*.rs"), "true");
        assert!(nested.matches("edit", Path::new("/proj/src/a/b.rs"), cwd));
        assert!(!nested.matches("edit", Path::new("/proj/tests/a.rs"), cwd));
    }

    #[test]
    fn command_rendering_quotes_path() {
        let cmd = render_command("fmt {path} && check {path}", Path::new("/tmp/a b.rs"));
        if cfg!(windows) {
            assert_eq!(cmd, "fmt \"/tmp/a b.rs\" && check \"/tmp/a b.rs\"");
        } else {
            assert_eq!(cmd, "fmt '/tmp/a b.rs' && check '/tmp/a b.rs'");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hooks_run_fail_and_time_out() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "x").unwrap();
        // A hook that mutates the file runs quietly…
        let ok = hook("edit", None, "printf y >> {path}");
        let notes = run_file_hooks(&[ok], "edit", &file, dir.path()).await;
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "xy");
        // …a failing hook reports, with its stderr…
        let bad = hook("edit", None, "echo broken >&2; exit 3");
        let notes = run_file_hooks(&[bad], "edit", &file, dir.path()).await;
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("failed") && notes[0].contains("broken"),
            "{}",
            notes[0]
        );
        // …and a hung hook is killed at its timeout.
        let mut slow = hook("edit", None, "sleep 5");
        slow.timeout_ms = 100;
        let notes = run_file_hooks(&[slow], "edit", &file, dir.path()).await;
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("timed out"), "{}", notes[0]);
    }
}
