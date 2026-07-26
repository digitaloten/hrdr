//! Filesystem confinement: how much of the disk an agent may touch.
//!
//! Two layers share one vocabulary. [`SandboxPolicy`] is the resolved boundary
//! — a mode plus concrete, canonicalized root sets — consulted by the
//! in-process file tools (software guard) and, on Linux, handed to the OS for
//! shell children (bwrap/Landlock). This module owns the vocabulary and the
//! path mechanics; the wiring into `ToolContext`, config, and the shell spawn
//! lands in later slices, so most of what is here has no caller yet.
//!
//! The reason the boundary is *enforced* rather than *asked for*: hrdr runs
//! arbitrary models, and guidance only reaches steerable ones. A delegated
//! sub-agent that `cd`s out of its worktree and commits to the parent repo's
//! `main` is the concrete failure this exists to make impossible.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::{canonicalize_nearest, tool_output_dir};

/// How much of the filesystem an agent may touch. Enforced by the OS for
/// shell children (bwrap/Landlock/Seatbelt) and by a software path-guard for
/// the in-process file tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    /// No confinement — full read/write everywhere. The pre-sandbox behavior.
    None,
    /// Read broadly (builds need /usr, toolchains, ~/.cargo, …); write ONLY
    /// within the writable roots (cwd + temp/scratch + tool-output dir + git
    /// metadata roots for a linked worktree + configured extras).
    Write,
    /// Read ONLY within the readable roots (cwd + scratch + tool-output);
    /// no writes anywhere. For read-only / research agents.
    Read,
}

impl SandboxMode {
    /// The canonical spelling, matching the config/env/flag vocabulary.
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxMode::None => "none",
            SandboxMode::Write => "write",
            SandboxMode::Read => "read",
        }
    }
}

impl std::str::FromStr for SandboxMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "write" => Ok(SandboxMode::Write),
            "read" => Ok(SandboxMode::Read),
            "none" => Ok(SandboxMode::None),
            other => Err(format!(
                "unknown sandbox mode {other:?} — expected write, read, or none"
            )),
        }
    }
}

impl std::fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A resolved confinement policy: the mode plus the concrete, canonicalized
/// root sets. Built once per agent in `Agent::new`; `ToolContext` holds it
/// behind an Arc so tool calls share it cheaply.
#[derive(Debug)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    /// Canonicalized (via [`canonicalize_nearest`]) writable roots. Empty when
    /// mode is `None` (meaning "everything") or `Read` (meaning "nothing").
    pub writable_roots: Vec<PathBuf>,
    /// Canonicalized readable roots; only consulted in `Read` mode.
    pub readable_roots: Vec<PathBuf>,
}

impl SandboxPolicy {
    /// The no-op policy: mode None, no roots. What `ToolContext::new` installs
    /// — the bare constructor stays unconfined on purpose; only `Agent::new`
    /// installs a real policy.
    pub fn unconfined() -> Self {
        Self {
            mode: SandboxMode::None,
            writable_roots: Vec::new(),
            readable_roots: Vec::new(),
        }
    }

    /// Build the policy for an agent working in `cwd`.
    ///
    /// Writable (mode `Write` only): `cwd`, [`std::env::temp_dir`],
    /// [`session_scratch_dir`], [`tool_output_dir`], the git metadata roots a
    /// linked worktree needs to commit (see [`git_metadata_roots`]), then the
    /// caller's configured `extras`. Readable: `cwd`, the scratch dir, the
    /// tool-output dir. Every root is run through [`canonicalize_nearest`] and
    /// deduped (a root already under an earlier root is dropped).
    ///
    /// Non-existent `extras` are skipped silently — a user config typo is not
    /// worth failing a session over, and everything in the default set is
    /// created by its own accessor before canonicalization.
    pub fn for_agent(mode: SandboxMode, cwd: &Path, extras: &[PathBuf]) -> Self {
        if mode == SandboxMode::None {
            return Self::unconfined();
        }
        let scratch = session_scratch_dir().to_path_buf();
        let output = tool_output_dir();
        let readable_roots =
            canonical_roots(vec![cwd.to_path_buf(), scratch.clone(), output.clone()]);
        let writable_roots = if mode == SandboxMode::Read {
            Vec::new()
        } else {
            let mut roots = vec![cwd.to_path_buf(), std::env::temp_dir(), scratch, output];
            roots.extend(git_metadata_roots(cwd));
            roots.extend(extras.iter().filter(|p| p.exists()).cloned());
            canonical_roots(roots)
        };
        Self {
            mode,
            writable_roots,
            readable_roots,
        }
    }

    /// Err unless `canon` (already run through [`canonicalize_nearest`]) is
    /// under a writable root, or the mode is `None`. `shown` is the path as
    /// the model named it, so the refusal talks about what it asked for.
    pub fn check_write(&self, canon: &Path, shown: &Path) -> anyhow::Result<()> {
        if self.mode == SandboxMode::None || is_under_any(canon, &self.writable_roots) {
            return Ok(());
        }
        anyhow::bail!(
            "sandbox: refusing to write {} — it is outside this agent's writable roots. \
             You may write only under: {}. Keep work inside your working directory; \
             use the scratch dir for throwaway files.",
            shown.display(),
            join_roots(&self.writable_roots)
        )
    }

    /// Err iff the mode is `Read` and `canon` (already canonicalized) is
    /// outside every readable root. A no-op in `Write` and `None` modes: broad
    /// reads under `Write` are a deliberate tradeoff (builds read all over the
    /// filesystem).
    pub fn check_read(&self, canon: &Path, shown: &Path) -> anyhow::Result<()> {
        if self.mode != SandboxMode::Read || is_under_any(canon, &self.readable_roots) {
            return Ok(());
        }
        anyhow::bail!(
            "sandbox: refusing to read {} — this agent is read-only and may read only under: {}.",
            shown.display(),
            join_roots(&self.readable_roots)
        )
    }
}

/// Canonicalize every root and drop the ones already covered by an earlier
/// one, preserving order (the first root is the most meaningful — the cwd —
/// and the refusal message reads in that order).
fn canonical_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(roots.len());
    for root in roots {
        let canon = canonicalize_nearest(&root);
        if !out.iter().any(|kept| canon.starts_with(kept)) {
            out.push(canon);
        }
    }
    out
}

/// Whether `canon` sits under any of `roots`. Both sides have been through
/// [`canonicalize_nearest`], which resolves symlinks and lexical `..` in the
/// not-yet-existing suffix — that is what makes this check escape-proof.
fn is_under_any(canon: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| canon.starts_with(root))
}

/// The roots as the refusal messages list them.
fn join_roots(roots: &[PathBuf]) -> String {
    roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The per-session scratch dir: `<temp_dir>/hrdr-scratch-<pid>-<8 hex rand>`,
/// created 0700 on first use, one per process (a session lives in one process;
/// sub-agents share the process, hence the shared scratch — by design).
///
/// The first call also sweeps stale `hrdr-scratch-<pid>-*` siblings whose pid
/// is no longer alive. That sweep *is* the teardown: there is deliberately no
/// exit handler (a killed process runs none anyway, and the OS tmp reaper is
/// the backstop). Sweep failures are ignored.
pub fn session_scratch_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let temp = std::env::temp_dir();
        let dir = temp.join(format!(
            "hrdr-scratch-{}-{}",
            std::process::id(),
            rand_hex8()
        ));
        sweep_stale_scratch(&temp, &dir);
        crate::ensure_private_dir(&dir);
        dir
    })
    .as_path()
}

/// Eight hex characters of randomness for the scratch-dir name, so a recycled
/// pid cannot land on a directory somebody else created first.
fn rand_hex8() -> String {
    use rand::RngExt as _;
    let mut bytes = [0u8; 4];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Remove `hrdr-scratch-<pid>-*` directories in `temp` whose pid is gone,
/// skipping `keep` (this process's own, freshly named). Best-effort: every
/// error is ignored.
#[cfg(unix)]
fn sweep_stale_scratch(temp: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(temp) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep {
            continue;
        }
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("hrdr-scratch-"))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<i32>().ok())
        else {
            continue;
        };
        // Signal 0 probes for existence without delivering anything. Only
        // ESRCH proves the process is gone — EPERM means it is alive and
        // owned by somebody else, and that directory is not ours to reap.
        let rc = unsafe { libc::kill(pid, 0) };
        let gone = rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Non-unix: no portable liveness probe, so leave stale dirs to the OS.
#[cfg(not(unix))]
fn sweep_stale_scratch(_temp: &Path, _keep: &Path) {}

/// Extra writable roots a linked git worktree needs to commit, and nothing
/// more. Empty when `<cwd>/.git` is a directory (a normal checkout: `.git` is
/// under cwd, already writable) or absent (not a repo).
///
/// A write sub-agent works in a linked worktree, where `<cwd>/.git` is a
/// *file* pointing at `<repo>/.git/worktrees/<name>/` and `git commit` writes
/// objects into the **parent** repo's `.git/objects` and moves a ref under
/// `.git/refs/heads/hrdr/…`. Without these roots every sub-agent commit dies
/// on EROFS; with the whole parent `.git` writable, `git -C <parent>
/// update-ref refs/heads/main …` re-opens the exact escape this feature
/// closes. So: the worktree's private gitdir, the append-only object store,
/// and the two `hrdr/` ref namespaces — never `common` itself, its `index`,
/// its `packed-refs`, its `config`, or any other branch's refs.
fn git_metadata_roots(cwd: &Path) -> Vec<PathBuf> {
    let dot_git = cwd.join(".git");
    if !dot_git.is_file() {
        return Vec::new();
    }
    // Malformed pointers fail open to "no extras": worse ergonomics for a
    // broken worktree, never a wider boundary.
    let Ok(text) = std::fs::read_to_string(&dot_git) else {
        return Vec::new();
    };
    let Some(target) = text
        .lines()
        .next()
        .and_then(|line| line.trim().strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return Vec::new();
    };
    let gitdir = canonicalize_nearest(&crate::resolve_under(cwd, target));
    if !gitdir.is_dir() {
        return Vec::new();
    }
    let Ok(commondir) = std::fs::read_to_string(gitdir.join("commondir")) else {
        return Vec::new();
    };
    let common = canonicalize_nearest(&crate::resolve_under(&gitdir, commondir.trim()));
    if !common.is_dir() {
        return Vec::new();
    }
    let refs = common.join("refs").join("heads").join("hrdr");
    let logs = common.join("logs").join("refs").join("heads").join("hrdr");
    // They must exist to be bind-mounted (bwrap) and to canonicalize to
    // themselves; git creates them lazily on the first task branch.
    let _ = std::fs::create_dir_all(&refs);
    let _ = std::fs::create_dir_all(&logs);
    vec![gitdir, common.join("objects"), refs, logs]
}

/// One-shot slot for a sandbox degradation notice awaiting delivery through
/// the agent's event stream, plus the set of notices already delivered.
///
/// The seen-set is the difference from `hrdr_llm::take_client_warning`'s plain
/// cell: a degradation is detected on *every* confined shell command, so a
/// bare `Option` would re-fill after each drain and the user would see the
/// same warning once per command. Each distinct message is delivered exactly
/// once per process.
static SANDBOX_NOTICE: OnceLock<Mutex<(HashSet<String>, Option<String>)>> = OnceLock::new();

fn notice_cell() -> &'static Mutex<(HashSet<String>, Option<String>)> {
    SANDBOX_NOTICE.get_or_init(|| Mutex::new((HashSet::new(), None)))
}

/// Take the pending sandbox notice for delivery through the caller's normal
/// event channel (never stderr — a TUI owns the terminal).
pub fn take_sandbox_notice() -> Option<String> {
    notice_cell().lock().ok().and_then(|mut cell| cell.1.take())
}

/// Record a degradation notice. Only a message seen for the first time this
/// process becomes pending; repeats are dropped.
// No caller until the OS layer lands (slice 6 emits these at each degrade point).
#[allow(dead_code)]
pub(crate) fn set_sandbox_notice(msg: String) {
    if let Ok(mut cell) = notice_cell().lock()
        && cell.0.insert(msg.clone())
    {
        cell.1 = Some(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `check_write` with the canonicalization its callers owe it.
    fn check_write(policy: &SandboxPolicy, path: &Path) -> anyhow::Result<()> {
        policy.check_write(&canonicalize_nearest(path), path)
    }

    /// `check_read` with the canonicalization its callers owe it.
    fn check_read(policy: &SandboxPolicy, path: &Path) -> anyhow::Result<()> {
        policy.check_read(&canonicalize_nearest(path), path)
    }

    #[test]
    fn sandbox_mode_parses_all_spellings_and_rejects_garbage() {
        assert_eq!("write".parse::<SandboxMode>().unwrap(), SandboxMode::Write);
        assert_eq!("READ".parse::<SandboxMode>().unwrap(), SandboxMode::Read);
        assert_eq!("  none ".parse::<SandboxMode>().unwrap(), SandboxMode::None);
        assert_eq!(SandboxMode::Write.to_string(), "write");
        assert_eq!(SandboxMode::Read.to_string(), "read");
        assert_eq!(SandboxMode::None.to_string(), "none");

        let err = "wrote".parse::<SandboxMode>().unwrap_err();
        assert!(err.contains("wrote"), "{err}");
        for valid in ["write", "read", "none"] {
            assert!(err.contains(valid), "{err} should name {valid}");
        }
    }

    #[test]
    fn session_scratch_dir_is_private_stable_and_under_temp() {
        let first = session_scratch_dir();
        let second = session_scratch_dir();
        assert_eq!(first, second, "the scratch dir is per-process and cached");
        assert!(first.is_dir(), "{} should exist", first.display());
        assert!(
            first.starts_with(std::env::temp_dir()),
            "{} should live under the temp dir",
            first.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(first).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "scratch dir must be owner-only");
        }
    }

    #[test]
    fn policy_write_roots_cover_cwd_temp_scratch_and_tool_output() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);

        check_write(&policy, &dir.path().join("out.txt")).unwrap();
        check_write(&policy, &std::env::temp_dir().join("hrdr-write-probe")).unwrap();
        check_write(&policy, &session_scratch_dir().join("probe")).unwrap();
        check_write(&policy, &tool_output_dir().join("probe")).unwrap();

        // The temp dir is a writable root by design, so a *sibling* of the
        // test cwd is allowed — only paths outside the temp tree are refused.
        let sibling = dir.path().parent().unwrap().join("hrdr-sibling-probe");
        check_write(&policy, &sibling).unwrap();

        let err = check_write(&policy, Path::new("/etc/passwd"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to write /etc/passwd"), "{err}");
        assert!(err.contains("You may write only under"), "{err}");
        for root in &policy.writable_roots {
            assert!(
                err.contains(&root.display().to_string()),
                "{err} should name {root:?}"
            );
        }
        check_write(&policy, Path::new("/nonexistent-outside/f")).unwrap_err();
    }

    #[test]
    fn read_mode_refuses_reads_outside_roots_and_allows_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Read, dir.path(), &[]);

        check_read(&policy, &dir.path().join("notes.md")).unwrap();
        check_read(&policy, &session_scratch_dir().join("probe")).unwrap();
        check_read(&policy, &tool_output_dir().join("probe")).unwrap();

        let err = check_read(&policy, Path::new("/etc/passwd"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to read /etc/passwd"), "{err}");
        assert!(err.contains("read-only and may read only under"), "{err}");
        assert!(
            err.contains(&canonicalize_nearest(dir.path()).display().to_string()),
            "{err}"
        );
        // Read mode writes nothing anywhere.
        check_write(&policy, &dir.path().join("out.txt")).unwrap_err();

        // `check_read` is a no-op in the other two modes.
        let write_mode = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        check_read(&write_mode, Path::new("/etc/passwd")).unwrap();
        check_read(&SandboxPolicy::unconfined(), Path::new("/etc/passwd")).unwrap();
        check_write(&SandboxPolicy::unconfined(), Path::new("/etc/passwd")).unwrap();
    }

    #[test]
    fn symlink_and_dotdot_escapes_are_caught() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);

        // Enough `..` to bottom out at `/` no matter how deep the temp dir is.
        let escape = dir
            .path()
            .join("a")
            .join(format!("{}etc/passwd", "../".repeat(40)));
        check_write(&policy, &escape).unwrap_err();

        // The symlink target must sit outside the temp tree: another tempdir
        // would be under the writable `env::temp_dir()` root and allowed.
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink("/etc", &link).unwrap();
            check_write(&policy, &link.join("passwd")).unwrap_err();
        }
    }

    #[test]
    fn git_metadata_roots_for_a_linked_worktree() {
        if which::which("git").is_err() {
            return; // best-effort: exercise the real backend when available
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);
        let wt = dir.path().join("wt");
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "hrdr/task-1",
            wt.to_str().unwrap(),
        ]);

        // A plain checkout needs nothing extra: its `.git` is under the cwd.
        assert!(git_metadata_roots(&repo).is_empty());

        let roots = git_metadata_roots(&wt);
        assert_eq!(roots.len(), 4, "{roots:?}");
        let common = canonicalize_nearest(&repo.join(".git"));
        // Both sides through `canonicalize_nearest`: macOS tempdirs live
        // behind the `/var → /private/var` symlink.
        let expected = [
            canonicalize_nearest(&common.join("worktrees").join("wt")),
            canonicalize_nearest(&common.join("objects")),
            canonicalize_nearest(&common.join("refs").join("heads").join("hrdr")),
            canonicalize_nearest(&common.join("logs").join("refs").join("heads").join("hrdr")),
        ];
        let got: Vec<PathBuf> = roots.iter().map(|r| canonicalize_nearest(r)).collect();
        assert_eq!(got, expected.to_vec());
        for root in &got {
            assert!(root.is_dir(), "{} should exist", root.display());
        }
        // The parent's own index and refs stay outside the boundary.
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: canonical_roots(
                std::iter::once(wt.clone()).chain(roots).collect::<Vec<_>>(),
            ),
            readable_roots: Vec::new(),
        };
        check_write(&policy, &common.join("index")).unwrap_err();
        check_write(&policy, &common.join("refs").join("heads").join("main")).unwrap_err();
        check_write(&policy, &common.join("objects").join("aa").join("bb")).unwrap();
    }

    #[test]
    fn sandbox_notice_is_take_once() {
        let msg = "sandbox: test notice — take once".to_string();
        set_sandbox_notice(msg.clone());
        assert_eq!(take_sandbox_notice().as_deref(), Some(msg.as_str()));
        assert_eq!(take_sandbox_notice(), None);
        // The same message never notices twice per process.
        set_sandbox_notice(msg);
        assert_eq!(take_sandbox_notice(), None);
    }
}
