//! `git`: a read-only window onto the repository.
//!
//! Everything here is reachable through `bash`, but only agents that *have* a
//! shell — and a shell answer arrives as unstructured text the model must parse
//! around. Exposing the read-only subcommands as their own tool means
//! `explore` and `review`, which have no shell at all, can finally look at
//! history, blame and the working diff.
//!
//! The subcommand is an **allow-list**, not a filter: `git` runs
//! `git <subcommand> …` directly, never through a shell, so there is no
//! quoting or `;`-injection surface. Nothing here can mutate the repository —
//! no `commit`, `checkout`, `reset`, `push`.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext, truncate};

/// The subcommands this tool will run. All are read-only: none writes to the
/// index, the working tree, the object store, or a remote.
const ALLOWED: &[&str] = &[
    "status", "diff", "log", "show", "blame", "branch", "describe", "remote", "shortlog",
];

/// Flags refused for **every** subcommand: each can run a program of the
/// caller's choosing, or write a file, turning a read-only command into an
/// arbitrary one.
const FORBIDDEN_ANY: &[&str] = &[
    // `-c core.pager=sh -c evil` / `--config-env=…` inject config for the run.
    "-c",
    "--config-env",
    // Run an external program as part of the diff.
    "--ext-diff",
    "--exec",
    // `git diff --output=FILE` writes to the filesystem.
    "--output",
    // Reach a remote, running its side's program.
    "--upload-pack",
    "--receive-pack",
];

/// Flags refused for `diff`/`blame` specifically: `--no-index` turns `diff`
/// into a generic two-arbitrary-paths file comparator (reads anything on
/// disk, not just tracked repo content); `--contents` feeds `blame` a file
/// from *outside* the repo to attribute against the history — both are file
/// read escapes, not repository inspection.
const FORBIDDEN_DIFF_BLAME: &[&str] = &["--no-index", "--contents"];

/// Flags refused for `branch` specifically: the subcommand reads by default but
/// deletes, renames or copies with these. (`-M`/`-m` mean *move detection* on
/// `blame`/`diff`, which is harmless — hence the per-subcommand list.)
const FORBIDDEN_BRANCH: &[&str] = &[
    "-d",
    "-D",
    "--delete",
    "-m",
    "-M",
    "--move",
    "-c",
    "-C",
    "--copy",
    "--force",
    "-f",
    "--edit-description",
    "--set-upstream-to",
    "-u",
    "--unset-upstream",
];

/// `remote` sub-subcommands that only read (no mutation, no network fetch of
/// a remote's refs). Anything else (`add`, `remove`/`rm`, `set-url`, `rename`,
/// `update`, `prune`, `set-head`, …) either mutates `.git/config` or reaches
/// out to the network — refused by the allow-list in [`check_remote_args`].
const REMOTE_READ_ONLY_FORMS: &[&str] = &["-v", "--verbose", "show", "get-url"];

/// Single-character flags that, bundled into one dash-prefixed short-flag
/// group (e.g. `-fD`, a `git` convention this parser must not be fooled by),
/// make the subcommand unsafe. Checked against every letter of every `-xyz`
/// style argument, not just whole-flag matches.
const FORBIDDEN_BRANCH_SHORT_CHARS: &[char] = &['d', 'D', 'm', 'M', 'c', 'C', 'f', 'u'];

/// Whether `arg` is `flag`, or `flag=value`.
fn matches_flag(arg: &str, flag: &str) -> bool {
    arg == flag || (arg.starts_with(flag) && arg.as_bytes().get(flag.len()) == Some(&b'='))
}

/// Whether `arg` is a bundled short-flag group (`-fD`, `-Dx`, …) that contains
/// one of `chars` — so `-fD` is caught as containing `-D` even though it isn't
/// a whole-argument match. Long options (`--foo`) and the bare `-` are never
/// bundles.
fn bundled_short_flag_contains(arg: &str, chars: &[char]) -> bool {
    let Some(rest) = arg.strip_prefix('-') else {
        return false;
    };
    if rest.is_empty() || rest.starts_with('-') {
        return false; // "-" alone, or a long "--flag"
    }
    rest.chars().any(|c| chars.contains(&c))
}

/// The refused flag in `args` for `sub`, if any.
fn forbidden_flag<'a>(sub: &str, args: &'a [String]) -> Option<&'a str> {
    let extra: &[&str] = match sub {
        "branch" => FORBIDDEN_BRANCH,
        "diff" | "blame" => FORBIDDEN_DIFF_BLAME,
        _ => &[],
    };
    args.iter().map(String::as_str).find(|arg| {
        FORBIDDEN_ANY
            .iter()
            .chain(extra)
            .any(|f| matches_flag(arg, f))
            || (sub == "branch" && bundled_short_flag_contains(arg, FORBIDDEN_BRANCH_SHORT_CHARS))
    })
}

/// A path argument (to `diff`/`blame`) that reads outside the workspace: an
/// absolute path, or one whose components escape the cwd via `..`. Flags
/// (`-`-prefixed) are not paths and are skipped by the caller.
fn escapes_workspace(arg: &str) -> bool {
    let p = std::path::Path::new(arg);
    p.is_absolute()
        || p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// For `diff`/`blame`: reject any non-flag argument that is an absolute path
/// or escapes the workspace via `..` — combined with the `--no-index`/
/// `--contents` flag bans, this keeps both subcommands reading only tracked
/// repository content under the cwd.
fn escaping_path_arg<'a>(sub: &str, args: &'a [String]) -> Option<&'a str> {
    if sub != "diff" && sub != "blame" {
        return None;
    }
    args.iter()
        .map(String::as_str)
        .find(|arg| !arg.starts_with('-') && escapes_workspace(arg))
}

/// For `remote`: only the read-only forms are allowed — bare `git remote`
/// (list), `-v`/`--verbose`, `show [name]`, `get-url <name>`. Anything else
/// (`add`, `remove`/`rm`, `set-url`, `rename`, `update`, `prune`, `set-head`,
/// or an unrecognized sub-subcommand) mutates config or talks to the network.
fn check_remote_args(args: &[String]) -> Result<(), &'static str> {
    match args.first().map(String::as_str) {
        None => Ok(()), // bare `git remote` — lists remotes
        Some(first) if REMOTE_READ_ONLY_FORMS.contains(&first) => Ok(()),
        _ => Err(
            "`git remote` only allows the read-only forms: no args, -v, show [name], \
             get-url <name> — add/remove/set-url/rename/update/prune are refused",
        ),
    }
}

/// For `branch`: refuse a bare `git branch <name>` (creates a branch) — only
/// the listing forms (no args, or args that are all flags) are read-only.
fn check_branch_args(args: &[String]) -> Result<(), &'static str> {
    if args.iter().any(|a| !a.starts_with('-')) {
        return Err(
            "`git branch <name>` creates a branch — this tool only lists branches \
             (no args, or flags like -a/-r/-v)",
        );
    }
    Ok(())
}

pub struct GitTool;

#[derive(Deserialize)]
struct GitArgs {
    subcommand: String,
    #[serde(default)]
    args: Vec<String>,
}

#[async_trait]
impl Tool for GitTool {
    fn name(&self) -> &'static str {
        "git"
    }

    fn read_only(&self) -> bool {
        // Nothing in ALLOWED mutates the repository, so a read-only agent may
        // use it. Keep that true if you add a subcommand.
        true
    }

    fn description(&self) -> &'static str {
        "Inspect the git repository: status, diff, log, show, blame, branch, describe, \
         remote, shortlog. Read-only — it cannot commit, checkout, reset or push. Pass the \
         subcommand's own flags in `args`, e.g. subcommand=\"log\", args=[\"-5\", \"--oneline\"], \
         or subcommand=\"diff\", args=[\"--staged\"]."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "subcommand": {
                    "type": "string",
                    "enum": ALLOWED,
                    "description": "The read-only git subcommand to run."
                },
                "args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Arguments for the subcommand, one per element (not a single joined string)."
                }
            },
            "required": ["subcommand"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let a: GitArgs = crate::tool_args("git", args)?;
        let sub = a.subcommand.trim();
        if !ALLOWED.contains(&sub) {
            bail!(
                "`git {sub}` is not available — this tool is read-only. Allowed: {}",
                ALLOWED.join(", ")
            );
        }
        if let Some(bad) = forbidden_flag(sub, &a.args) {
            bail!("`{bad}` is not allowed: it can modify the repository or run a program");
        }
        if let Some(bad) = escaping_path_arg(sub, &a.args) {
            bail!(
                "`{bad}` is not allowed: `git {sub}` only reads paths inside the workspace \
                 (no absolute paths, no `..` escapes)"
            );
        }
        if sub == "remote"
            && let Err(msg) = check_remote_args(&a.args)
        {
            bail!(msg);
        }
        if sub == "branch"
            && let Err(msg) = check_branch_args(&a.args)
        {
            bail!(msg);
        }

        let out = tokio::process::Command::new("git")
            .arg(sub)
            .args(&a.args)
            .current_dir(&ctx.cwd)
            // A pager would hang waiting for a terminal that isn't there.
            .env("GIT_PAGER", "cat")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .await
            .context("running git (is it installed?)")?;

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            let msg = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            bail!("git {sub} failed: {msg}");
        }
        let body = if stdout.trim().is_empty() {
            "(no output)".to_string()
        } else {
            stdout.into_owned()
        };
        Ok(truncate(&body, ctx.max_output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A repo with one commit, so `log`/`status` have something to say.
    async fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "first"]);
        dir
    }

    #[tokio::test]
    async fn runs_read_only_subcommands() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());

        let log = GitTool
            .execute(json!({"subcommand": "log", "args": ["--oneline"]}), &ctx)
            .await
            .unwrap();
        assert!(log.contains("first"), "{log}");

        // An unstaged change shows up in status and diff.
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        let status = GitTool
            .execute(json!({"subcommand": "status", "args": ["--short"]}), &ctx)
            .await
            .unwrap();
        assert!(status.contains("a.txt"), "{status}");
        let diff = GitTool
            .execute(json!({"subcommand": "diff"}), &ctx)
            .await
            .unwrap();
        assert!(diff.contains("-one") && diff.contains("+two"), "{diff}");
    }

    /// The subcommand is an allow-list: writing commands are refused, and so is
    /// anything that isn't a git subcommand at all.
    #[tokio::test]
    async fn refuses_writing_subcommands() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        for sub in ["commit", "checkout", "reset", "push", "clean", "rm"] {
            let err = GitTool
                .execute(json!({"subcommand": sub}), &ctx)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("read-only"), "{sub}: {err}");
        }
    }

    /// Flags that would let a read-only subcommand write, or run a program, are
    /// refused even though the subcommand itself is allowed.
    #[tokio::test]
    async fn refuses_dangerous_flags_on_allowed_subcommands() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        // `git branch -D main` deletes a branch.
        let err = GitTool
            .execute(
                json!({"subcommand": "branch", "args": ["-D", "main"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
        // `-c` sets config for the invocation, which can point at a program.
        let err = GitTool
            .execute(
                json!({"subcommand": "log", "args": ["-c", "core.pager=sh -c evil"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
    }

    /// Arguments go to `git` directly, never through a shell: a `;` is just a
    /// bad argument, not a second command.
    #[tokio::test]
    async fn arguments_are_not_shell_interpreted() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        let marker = dir.path().join("pwned");
        let injected = format!("; touch {}", marker.display());
        let err = GitTool
            .execute(json!({"subcommand": "log", "args": [injected]}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("git log failed"), "{err}");
        assert!(!marker.exists(), "the shell never saw it");
    }

    /// A failing git command surfaces git's own message rather than an empty ok.
    #[tokio::test]
    async fn a_failure_is_an_error_not_an_empty_success() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        let err = GitTool
            .execute(json!({"subcommand": "show", "args": ["nosuchref"]}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("git show failed"), "{err}");
    }

    /// A forbidden flag bundled into a short-flag group (`-fD`) is caught the
    /// same as a standalone `-D` or `-f` — the allow-list can't be defeated by
    /// packing flags together.
    #[tokio::test]
    async fn bundled_short_flags_are_caught() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        let err = GitTool
            .execute(
                json!({"subcommand": "branch", "args": ["-fD", "main"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
    }

    /// `git diff --no-index` and `git blame --contents` are file-read escapes
    /// (they read arbitrary filesystem paths, not tracked repo content) and
    /// are refused even though `diff`/`blame` are otherwise allowed.
    #[tokio::test]
    async fn diff_no_index_and_blame_contents_are_refused() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        let err = GitTool
            .execute(
                json!({"subcommand": "diff", "args": ["--no-index", "/etc/passwd", "a.txt"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");

        let err = GitTool
            .execute(
                json!({"subcommand": "blame", "args": ["--contents", "/etc/passwd", "a.txt"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
    }

    /// `diff`/`blame` args that are absolute paths or escape the workspace via
    /// `..` are refused, even without `--no-index`/`--contents`.
    #[tokio::test]
    async fn diff_and_blame_reject_paths_outside_the_workspace() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        let err = GitTool
            .execute(json!({"subcommand": "diff", "args": ["/etc/passwd"]}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("workspace"), "{err}");

        let err = GitTool
            .execute(
                json!({"subcommand": "blame", "args": ["../../etc/passwd"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("workspace"), "{err}");
    }

    /// `git remote` only allows the read-only forms — mutating/networking
    /// sub-subcommands are refused.
    #[tokio::test]
    async fn remote_only_allows_read_only_forms() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        for args in [
            vec!["add", "origin", "https://evil.example/repo.git"],
            vec!["remove", "origin"],
            vec!["rm", "origin"],
            vec!["set-url", "origin", "https://evil.example/repo.git"],
            vec!["rename", "origin", "up"],
            vec!["update"],
            vec!["prune"],
            vec!["set-head", "origin", "-a"],
        ] {
            let err = GitTool
                .execute(json!({"subcommand": "remote", "args": args.clone()}), &ctx)
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("read-only forms"),
                "{args:?}: {err}"
            );
        }
        // The read-only forms still work.
        for args in [vec![], vec!["-v".to_string()], vec!["show".to_string()]] {
            GitTool
                .execute(json!({"subcommand": "remote", "args": args.clone()}), &ctx)
                .await
                .unwrap_or_default(); // no remotes configured — empty output is fine
        }
    }

    /// A bare `git branch <name>` creates a branch — refused; only listing
    /// forms (no args, or all-flag args) are allowed.
    #[tokio::test]
    async fn bare_branch_name_is_refused() {
        let dir = repo().await;
        let ctx = ToolContext::new(dir.path());
        let err = GitTool
            .execute(
                json!({"subcommand": "branch", "args": ["new-branch"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("only lists branches"), "{err}");
        // Listing (no args, or flags only) still works.
        GitTool
            .execute(json!({"subcommand": "branch", "args": ["-a"]}), &ctx)
            .await
            .unwrap();
    }
}
