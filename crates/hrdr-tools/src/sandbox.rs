//! Filesystem confinement: how much of the disk an agent may touch.
//!
//! Two layers share one vocabulary. [`SandboxPolicy`] is the resolved boundary
//! — a mode plus concrete, canonicalized root sets — consulted by the
//! in-process file tools (software guard) and, on Linux, handed to the OS for
//! shell children (bwrap/Landlock). This module owns the vocabulary, the path
//! mechanics, and — via [`sandboxed_shell_command`] — the OS wrapper the
//! `shell` and `watch` tools spawn through.
//!
//! The two layers are not redundant: the software guard sees only the paths a
//! tool is *handed*, and a shell command is opaque to it. Everything a
//! subprocess writes is the OS layer's job.
//!
//! The reason the boundary is *enforced* rather than *asked for*: hrdr runs
//! arbitrary models, and guidance only reaches steerable ones. A delegated
//! sub-agent that `cd`s out of its worktree and commits to the parent repo's
//! `main` is the concrete failure this exists to make impossible.

use std::collections::{HashSet, VecDeque};
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
    ///
    /// Spelled `none`, `yolo` or `off` in config/env/flags; `none` is canonical
    /// and the one this renders back as.
    None,
    /// Read broadly (builds need /usr, toolchains, ~/.cargo, …); write ONLY
    /// within the writable roots (cwd + temp/scratch + tool-output dir + git
    /// metadata roots for a linked worktree + configured extras).
    Write,
    /// Read broadly, write NOWHERE. What a read-only agent gets.
    ///
    /// "Read-only" is a restriction on WRITING, not on reading — the same
    /// meaning Codex gives its `read-only` mode, and for the same reason: a
    /// review agent has to run the tools the user installed, and those live all
    /// over the filesystem (`~/.cargo/bin`, a nvm/fnm node, a Homebrew or Nix
    /// prefix, a mason symlink farm). This mode used to confine reads too, which
    /// left an agent's shell able to see only `/usr` and `/etc` — "command not
    /// found" for tools that are plainly installed. [`Strict`](Self::Strict) is
    /// that behavior, kept and made opt-in.
    Read,
    /// Read only within the readable roots (cwd + scratch + tool-output), write
    /// nowhere. The strongest confinement hrdr has, and **opt-in**
    /// (`sandbox = "strict"`).
    ///
    /// Everything else is not merely unwritable but ABSENT — an outside path is
    /// ENOENT rather than EROFS, so nothing outside the workspace can be read at
    /// all. The price is that the agent's shell has only the system toolchain
    /// (`/usr`, `/etc`): anything installed under `$HOME` is invisible, and a
    /// build that needs a rustup toolchain or a node from a version manager
    /// cannot run. Choose it when confining reads matters more than running the
    /// user's tools.
    Strict,
}

impl SandboxMode {
    /// The canonical spelling, matching the config/env/flag vocabulary.
    pub fn as_str(self) -> &'static str {
        match self {
            SandboxMode::None => "none",
            SandboxMode::Write => "write",
            SandboxMode::Read => "read",
            SandboxMode::Strict => "strict",
        }
    }
}

impl std::str::FromStr for SandboxMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "write" => Ok(SandboxMode::Write),
            "read" => Ok(SandboxMode::Read),
            "strict" => Ok(SandboxMode::Strict),
            // `yolo` is a SPELLING of `none`, not a fourth behavior: turning the
            // sandbox off is already exactly one thing, and two modes that did
            // the same thing under different names would be a bug waiting to be
            // written. It exists because that is the word people reach for, and
            // a mode you cannot name is one you disable some other, worse way.
            // `none` stays canonical — it is what `as_str`/`Display` render.
            "none" | "yolo" | "off" => Ok(SandboxMode::None),
            other => Err(format!(
                "unknown sandbox mode {other:?} — expected write, read, strict, or none \
                 (aka yolo/off)"
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
    /// Paths *inside* a writable root that stay read-only anyway — the
    /// subtraction that [`deny_git_writes`](Self::deny_git_writes) installs for a
    /// write sub-agent. Empty for every other agent.
    ///
    /// Enforced at the OS layer (a `--ro-bind` after the writable binds, a
    /// read-only Landlock rule nested inside the writable one, a trailing SBPL
    /// `deny`) rather than only in the file tools, because the thing being
    /// stopped is `git`, and git runs through `shell`.
    pub readonly_subpaths: Vec<PathBuf>,
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
            readonly_subpaths: Vec::new(),
        }
    }

    /// Build the policy for an agent working in `cwd`.
    ///
    /// Writable (mode `Write` only): `cwd`, [`std::env::temp_dir`],
    /// [`session_scratch_dir`], [`tool_output_dir`], the git metadata roots a
    /// linked worktree needs to commit (see [`git_metadata_roots`]), then the
    /// caller's configured `extras`. Every root is run through
    /// [`canonicalize_nearest`] and deduped (a root already under an earlier
    /// root is dropped).
    ///
    /// Readable roots (`cwd`, scratch, tool-output) are only ever CONSULTED in
    /// [`SandboxMode::Strict`] — the one mode that confines reads. `Read` and
    /// `Write` both read broadly, so they carry the list but never check it.
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
        let writable_roots = if matches!(mode, SandboxMode::Read | SandboxMode::Strict) {
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
            readonly_subpaths: Vec::new(),
        }
    }

    /// Make the repository's git metadata read-only for this agent: it may read
    /// history, run `git log`/`diff`/`status`, and change tracked files, but it
    /// cannot commit, move a ref, stage, or install a hook.
    ///
    /// Installed for **write sub-agents only**, and it is what makes the
    /// delegation model's central claim enforceable rather than merely stated.
    /// The prompt tells a sub-agent not to commit; this is why it cannot. A
    /// commit from a sub-agent would sweep the whole shared tree — the parent's
    /// work in progress, a sibling's half-finished edit — into one commit under
    /// its own message, and the parent would have no way to take it apart
    /// afterwards. Ref writes are worse: nothing else in the harness would notice
    /// a sub-agent moving `refs/heads/main`.
    ///
    /// Two halves, because the writable set is not a single directory:
    ///
    /// 1. **Drop the metadata roots.** When hrdr itself runs inside a linked
    ///    worktree, `for_agent` added the parent repo's `objects`/`refs` dirs so
    ///    that commits work at all ([`git_metadata_roots`]). A sub-agent that
    ///    does not commit needs none of them, and leaving them would be a hole in
    ///    the parent `.git` that the `<cwd>/.git` denial below does not cover.
    /// 2. **Deny `<root>/.git` inside each remaining writable root.** As a path
    ///    rather than a name: the check that matters happens in the kernel, which
    ///    knows nothing about basenames.
    ///
    /// Deliberately NOT read-denied. A sub-agent reviewing its own work wants
    /// `git log`, `git diff`, `git show`; every one of those reads `.git`, and
    /// none of them can write. Codex denies reads too (`.git` is masked as an
    /// empty directory), which also costs it every read-only git command.
    pub fn deny_git_writes(&mut self, cwd: &Path) {
        if self.mode == SandboxMode::None {
            return;
        }
        let metadata = git_metadata_roots(cwd);
        if !metadata.is_empty() {
            let denied = canonical_roots(metadata);
            self.writable_roots
                .retain(|root| !denied.iter().any(|d| root == d));
        }
        let mut denied: Vec<PathBuf> = self
            .writable_roots
            .iter()
            .map(|root| canonicalize_nearest(&root.join(".git")))
            .filter(|dot_git| dot_git.exists())
            .collect();
        denied.sort();
        denied.dedup();
        self.readonly_subpaths = denied;
    }

    /// Err unless `canon` (already run through [`canonicalize_nearest`]) is
    /// under a writable root *and* clear of the protected metadata directories
    /// ([`PROTECTED_METADATA_DIRS`]). `shown` is the path as the model named
    /// it, so the refusal talks about what it asked for.
    ///
    /// Both questions are answered on the *canonical* path, which is what makes
    /// a symlink into `.git` — `link/hooks/pre-commit` — refusable at all.
    ///
    /// Mode `None` answers neither: an agent with no boundary has nothing to
    /// escalate out of, and the unconfined path stays byte-identical to the
    /// pre-sandbox behavior (see the [`ToolContext::new`](crate::ToolContext::new)
    /// rule).
    ///
    /// This guards the **model's file tools** and nothing else — `shell` does not
    /// come through here, because `git commit` legitimately writes `.git/index`
    /// and moves a ref, and the main agent has to be able to do that. So the
    /// metadata rule can be absolute for the file tools (no tool of the model's
    /// ever has business writing `.git`) while git itself still works.
    ///
    /// The shell half is enforceable only at the OS layer, and for a write
    /// sub-agent it now IS enforced there — see
    /// [`deny_git_writes`](Self::deny_git_writes), which subtracts `.git` from
    /// the writable mounts. The main agent keeps it writable, deliberately.
    pub fn check_write(&self, canon: &Path, shown: &Path) -> anyhow::Result<()> {
        if self.mode == SandboxMode::None {
            return Ok(());
        }
        if !is_under_any(canon, &self.writable_roots) {
            anyhow::bail!(
                "sandbox: refusing to write {} — it is outside this agent's writable roots. \
                 You may write only under: {}. Keep work inside your working directory; \
                 use the scratch dir for throwaway files.",
                shown.display(),
                join_roots(&self.writable_roots)
            )
        }
        if let Some(dir) = protected_metadata_dir(canon, &self.writable_roots) {
            anyhow::bail!(
                "sandbox: refusing to write {} — it lands inside `{dir}`, and your file tools \
                 never write repository or agent metadata: a hook placed there runs with the \
                 user's full authority the next time they commit, outside this agent's boundary. \
                 If the change is really wanted, ask the user to make it. To record work, commit \
                 it — `shell` reaches git through git itself, which writes its own \
                 metadata; you do not.",
                shown.display(),
            )
        }
        Ok(())
    }

    /// Err iff the mode is `Strict` and `canon` (already canonicalized) is
    /// outside every readable root. A no-op in every other mode — `Read` means
    /// "writes nowhere", not "reads nowhere", so like `Write` it reads broadly
    /// (builds and review tools read all over the filesystem).
    pub fn check_read(&self, canon: &Path, shown: &Path) -> anyhow::Result<()> {
        if self.mode != SandboxMode::Strict || is_under_any(canon, &self.readable_roots) {
            return Ok(());
        }
        anyhow::bail!(
            "sandbox: refusing to read {} — this agent is strictly confined and may read only \
             under: {}.",
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

/// Directory names a model-supplied write may never land inside, even when the
/// path is comfortably under a writable root. Every one of them is a folder the
/// *harness* later reads back as instruction, or hands to something that
/// executes it — so a write there escalates this agent's privileges past its own
/// boundary. Codex refuses the same shape for the same stated reason (folders
/// whose contents _"could be modified to escalate the privileges of the
/// agent"_), and its list is the ancestor of this one.
///
/// - `.git` — `hooks/pre-commit` runs with the user's full authority on the next
///   commit *in the parent repo*. That is this module's founding incident with
///   one extra step, and it is reachable from inside the boundary because a
///   write sub-agent's cwd **is** its worktree, so the worktree's `.git` is
///   under a writable root.
///
/// Deliberately **only** `.git`, though the harness-config trees (`.hrdr`,
/// `.claude`, `.opencode`) are the same shape of hazard on paper — hrdr's own
/// loaders read `.hrdr/skills` and `.hrdr/agents` back as instruction, so
/// authoring one is the model writing its own next system prompt. Two reasons
/// they are not here:
///
/// - The cost is certain and the gain is not. "Add a project skill for this
///   repo" is an ordinary request, and refusing it buys little: `shell` reaches
///   those paths anyway (the same hole this doc admits for `.git`), so against a
///   model already following hostile instructions the refusal is a speed bump,
///   while against an honest one it is a wall.
/// - `.git` does not have that symmetry. Nothing legitimate makes a model's file
///   tools write git metadata — git writes its own, through git — and
///   `hooks/pre-commit` executes with the user's full authority the next time
///   *they* commit, which no other name on the list can claim.
///
/// Widening this to the config trees is a policy call about what a model may
/// author, recorded in `docs/backlog.md`; it is not this guard's decision to
/// make quietly. Also deliberately not `.agents/`: hrdr reads no such repo
/// directory at all (its global `AGENTS.md` lookup is an XDG config dir), so
/// refusing it would protect nothing and surprise somebody.
const PROTECTED_METADATA_DIRS: [&str; 1] = [".git"];

/// The protected directory a write to `canon` would land inside, if any.
///
/// `.git` is refused wherever it appears in the canonical path, because four
/// roots *inside* the parent `.git` are deliberately writable (see
/// [`git_metadata_roots`] — drop them and every write sub-agent's commit dies on
/// EROFS), and a root-relative test would let those roots launder
/// `objects/…`, or `worktrees/<wt>/hooks/…`, straight back in. The file tools
/// need none of it: git writes its own metadata, through git.
///
/// Any other protected name would be refused only *below* a containing root,
/// never in the root's own path — a write sub-agent's cwd is
/// `<repo>/.hrdr/worktrees/wt-N` and a checkout living under `~/.claude/…` is
/// somebody's real layout, so testing the whole path for those names would refuse
/// every write those agents make. The root-relative arm is kept for that reason:
/// the list is one name today, and the next one added must not silently become
/// whole-path.
///
/// Any containing root refusing is enough (deny beats allow), though
/// [`canonical_roots`] leaves roots mutually non-nested, so in a policy it built
/// exactly one root can contain a given path.
fn protected_metadata_dir(canon: &Path, roots: &[PathBuf]) -> Option<&'static str> {
    if canon.components().any(|c| c.as_os_str() == ".git") {
        return Some(".git");
    }
    roots
        .iter()
        .filter_map(|root| canon.strip_prefix(root).ok())
        .flat_map(Path::components)
        .find_map(|component| {
            PROTECTED_METADATA_DIRS
                .into_iter()
                .find(|name| *name != ".git" && component.as_os_str() == *name)
        })
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

/// One agent's sandbox degradation notices awaiting delivery through **its own**
/// event stream, plus the ones it has already been told.
///
/// Per agent, not per process: a notice is a statement about *this* agent's
/// confinement, and several agents run in one process, each with its own
/// [`SandboxPolicy`]. A single shared queue let whichever turn loop drained
/// first swallow a sibling's notice — the wrong session hearing that its sandbox
/// degraded, the right one never hearing it, and a test
/// (`sandbox_notice_reaches_the_event_stream`) that failed whenever a parallel
/// test drained its seeded notice.
///
/// Lives beside the policy in [`crate::ToolContext`] rather than inside it: the
/// policy is an immutable *description* of a boundary, built once and shared
/// behind an `Arc` (other crates construct one as a plain literal to render it);
/// this is mutable per-session state.
///
/// The seen-set is the difference from `hrdr_llm::take_client_warning`'s plain
/// cell: a degradation is detected on *every* confined shell command, so a
/// bare slot would re-fill after each drain and the user would see the same
/// warning once per command. Each distinct message is delivered exactly once
/// per agent — the recurrence is silenced, the sibling is not.
///
/// Pending is a *queue* rather than a single slot because one command can
/// degrade twice — a read-mode agent on the Landlock fallback both loses its
/// primary backend and loses read confinement — and a single slot would
/// silently drop the first of the two while still marking it seen.
#[derive(Debug, Default)]
pub struct SandboxNotices {
    /// `(already told, awaiting delivery)`. A poisoned lock costs a notice
    /// rather than a panic, exactly as the process-global cell did: a
    /// degradation warning is not worth taking a session down for.
    inner: Mutex<(HashSet<String>, VecDeque<String>)>,
}

impl SandboxNotices {
    /// Record a degradation notice. Only a message *this agent* has not been
    /// told yet is queued; repeats are dropped.
    pub fn set(&self, msg: String) {
        if let Ok(mut cell) = self.inner.lock()
            && cell.0.insert(msg.clone())
        {
            cell.1.push_back(msg);
        }
    }

    /// Take the next pending notice for delivery through this agent's normal
    /// event channel (never stderr — a TUI owns the terminal).
    pub fn take(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|mut cell| cell.1.pop_front())
    }
}

/// Emitted when a confined agent's shell command runs without any OS-level
/// confinement — no bwrap and no Landlock, a macOS whose
/// `/usr/bin/sandbox-exec` is gone, or Windows.
/// Never silently pretend to sandbox: the file tools stay guarded, the shell
/// does not.
const NO_OS_SANDBOX_NOTICE: &str = "sandbox: no OS-level sandbox is available on this system — \
     shell commands are NOT OS-confined; the file tools remain guarded. Use --sandbox none to \
     silence this.";

/// Emitted when `bwrap(1)` is not installed and Landlock catches the fall.
///
/// Linux-only, like the two notices below it: nothing off Linux can fall back
/// to Landlock, and an unused const is a `-D warnings` failure on the other CI
/// runners.
#[cfg(target_os = "linux")]
const BWRAP_MISSING_NOTICE: &str = "sandbox: bwrap not found — falling back to Landlock: writes \
     are still confined, but reads are not, and strict-mode agents degrade to write-only \
     confinement for shell commands. Install bubblewrap for full confinement.";

/// Emitted when bwrap is installed but the kernel/distro forbids unprivileged
/// user namespaces, so it cannot build a mount namespace.
#[cfg(target_os = "linux")]
const USERNS_DISABLED_NOTICE: &str = "sandbox: unprivileged user namespaces are disabled on this \
     system — falling back to Landlock: writes are still confined, but reads are not, and \
     strict-mode agents degrade to write-only confinement for shell commands.";

/// Emitted in addition to the fallback notice when a **strict-mode** agent runs
/// a shell command on Landlock: the ruleset confines writes only, so this agent
/// is quietly weaker than its mode claims — say so, loudly.
///
/// `Read` is deliberately NOT here. It means "read broadly, write nowhere",
/// which is precisely what a Landlock ruleset with no writable roots expresses,
/// so a read-only agent loses nothing on this backend. Only `Strict`, which asks
/// for reads to be confined, is weakened by it.
#[cfg(target_os = "linux")]
const STRICT_DEGRADES_UNDER_LANDLOCK_NOTICE: &str = "sandbox: Landlock cannot confine reads — this \
     strict-mode agent's shell commands are write-confined only, so paths outside its readable \
     roots remain readable.";

/// The OS mechanism available to confine *shell children* on this machine.
///
/// The file tools are guarded in-process regardless; this is only about the
/// subprocesses `shell` and `watch` spawn, which the software guard cannot see
/// inside of. On Linux bwrap is primary because it is the only mechanism there
/// that can confine reads as well as writes; on macOS there is one option.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsSandboxBackend {
    /// `bwrap(1)` — a mount namespace built per command (§3.6.1).
    Bwrap,
    /// Landlock LSM rules the child applies to itself. Writes only.
    Landlock,
    /// macOS `sandbox-exec(1)` with a generated SBPL profile (§3.7).
    Seatbelt,
    /// Nothing available: the shell runs unconfined and says so.
    None,
}

/// What backend detection concluded: the mechanism to use, plus the §5 notice
/// owed to the user when that mechanism is a step down from bwrap.
///
/// The notice is *stored* here rather than emitted during detection, because
/// detection is lazy and global while the notice is about a specific confined
/// command: a session that runs unsandboxed (`--sandbox none`) must never be
/// told its sandbox degraded.
#[derive(Clone, Copy, Debug)]
struct Detection {
    backend: OsSandboxBackend,
    /// `None` when the primary backend was available and nothing was lost.
    ///
    /// Only the Linux Landlock arm consumes it — a step *down* is a Linux-only
    /// concept, since macOS either has Seatbelt or has nothing — so off Linux
    /// it is written and never read, and `-D warnings` on the macOS/Windows CI
    /// runners would otherwise fail the build.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    degraded: Option<&'static str>,
}

/// The detection result for this process, probed once and cached — the probe
/// spawns a process, and a shell tool call must not pay for that every time.
fn detection() -> Detection {
    static DETECTION: OnceLock<Detection> = OnceLock::new();
    *DETECTION.get_or_init(detect_backend_uncached)
}

/// The backend this process uses (§3.5).
pub fn detect_backend() -> OsSandboxBackend {
    detection().backend
}

/// Linux: bwrap if it exists *and* unprivileged user namespaces are usable
/// (a kernel/distro switch — the binary being installed proves nothing), else
/// Landlock if the LSM is enabled, else nothing.
#[cfg(target_os = "linux")]
fn detect_backend_uncached() -> Detection {
    match which::which("bwrap") {
        Ok(bwrap) if userns_probe_succeeds(&bwrap) => Detection {
            backend: OsSandboxBackend::Bwrap,
            degraded: None,
        },
        Ok(_) => without_bwrap(USERNS_DISABLED_NOTICE),
        Err(_) => without_bwrap(BWRAP_MISSING_NOTICE),
    }
}

/// Run the smallest possible sandbox and see whether the kernel allows it.
/// Mirrors Codex's `SYSTEM_BWRAP_PROBE_TIMEOUT` / `_POLL_INTERVAL` loop: spawn
/// with null stdio, poll, kill on the deadline. A hung probe must not hang the
/// session, so a timeout counts as failure.
#[cfg(target_os = "linux")]
fn userns_probe_succeeds(bwrap: &Path) -> bool {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
    const PROBE_POLL: Duration = Duration::from_millis(50);

    let Ok(mut child) = std::process::Command::new(bwrap)
        .args([
            "--unshare-user",
            "--unshare-pid",
            "--ro-bind",
            "/",
            "/",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--",
            "/bin/true",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {}
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        std::thread::sleep(PROBE_POLL);
    }
}

/// Step 3 of §3.5: bwrap is unusable, so Landlock if the kernel has the LSM
/// enabled (the list of active LSMs is the authoritative answer), else
/// nothing.
///
/// `why` explains the bwrap failure, and both of its spellings promise a
/// Landlock fallback — so it is only the right thing to say when Landlock
/// actually catches the fall. With no backend at all, the blunter "not
/// confined" notice supersedes it.
#[cfg(target_os = "linux")]
fn without_bwrap(why: &'static str) -> Detection {
    if std::fs::read_to_string("/sys/kernel/security/lsm")
        .unwrap_or_default()
        .contains("landlock")
    {
        Detection {
            backend: OsSandboxBackend::Landlock,
            degraded: Some(why),
        }
    } else {
        Detection {
            backend: OsSandboxBackend::None,
            degraded: Some(NO_OS_SANDBOX_NOTICE),
        }
    }
}

/// macOS: Seatbelt whenever the system wrapper is where it belongs (§3.7).
///
/// Existence is the whole probe — unlike bwrap there is no kernel switch to
/// discover, and `sandbox-exec` is deprecated-but-present on every macOS that
/// still ships it. A machine that has had it removed falls to the software
/// layer with the blunt "not confined" notice.
#[cfg(target_os = "macos")]
fn detect_backend_uncached() -> Detection {
    if Path::new(SEATBELT_PROGRAM).exists() {
        Detection {
            backend: OsSandboxBackend::Seatbelt,
            degraded: None,
        }
    } else {
        Detection {
            backend: OsSandboxBackend::None,
            degraded: Some(NO_OS_SANDBOX_NOTICE),
        }
    }
}

/// Every other platform: nothing. Windows stays software-layer-only in v1.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn detect_backend_uncached() -> Detection {
    Detection {
        backend: OsSandboxBackend::None,
        degraded: Some(NO_OS_SANDBOX_NOTICE),
    }
}

/// A note to append when a sandboxed command's output looks like the SANDBOX
/// refused a write, rather than the program failing on its own terms.
///
/// The confinement is a read-only bind mount, so a blocked write surfaces as
/// `EROFS` / "Read-only file system" — from deep inside whatever tool was
/// running, describing a path the model never mentioned. That reads as a broken
/// or missing tool, and a model acts on it as one.
///
/// The case this was written for: `npx prettier --write …` on a machine where
/// `prettier` is installed and on `PATH`. `npx` ignored it, tried to fetch the
/// package into `~/.npm/_cacache`, and got `EROFS`. The model concluded
/// "prettier is not available in this environment" — a false statement about the
/// machine — and silently skipped formatting the file it had just written.
///
/// Deliberately narrow. Only `EROFS`/"read-only file system" triggers it: those
/// are all but unheard of on a developer's box outside a sandbox, whereas a
/// bare "Permission denied" is a normal error this must not editorialize over.
/// `None` when unconfined, or when nothing in the output matches.
/// The GPU/compute device nodes present on this host, for the sandbox to bind
/// through.
///
/// `bwrap --dev /dev` mounts a *fresh, minimal* devtmpfs — `null`, `zero`,
/// `random`, `tty` and little else — so every accelerator on the machine
/// disappears inside the sandbox no matter how permissive the mode is. That is
/// not a policy anyone chose: `Write` and `Read` both mount all of `/` and are
/// meant to leave the host as visible as it really is. Without this a ROCm build
/// fails on `/dev/kfd`, a CUDA one on `/dev/nvidiactl`, and the error names a
/// missing device rather than a sandbox — which reads as "this machine has no
/// GPU" and sends the agent off to work around a problem it does not have.
///
/// Matched by name rather than a fixed list because the numbered nodes
/// (`nvidia0`, `nvidia1`, …) depend on how many cards are installed. Read live:
/// a readdir of `/dev` costs microseconds, and a cached answer that missed a
/// device after a driver reload would be a worse bug than the cost it saved.
#[cfg(target_os = "linux")]
pub(crate) fn gpu_device_nodes() -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };
    let mut out: Vec<std::path::PathBuf> = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            // `kfd` is AMD's compute node; `dri` the render/card directory every
            // vendor uses; `nvidia*` covers `nvidiactl`, `nvidia-uvm`,
            // `nvidia-caps` and the per-card numbers.
            name == "kfd" || name == "dri" || name.starts_with("nvidia")
        })
        .map(|e| e.path())
        .collect();
    // Stable order so the argv a test asserts on does not depend on readdir.
    out.sort();
    out
}

/// GPU devices don't exist on non-Linux — the loop is a no-op.
#[cfg(not(target_os = "linux"))]
pub(crate) fn gpu_device_nodes() -> Vec<std::path::PathBuf> {
    Vec::new()
}

pub fn sandbox_denial_note(policy: &SandboxPolicy, output: &str) -> Option<String> {
    if policy.mode == SandboxMode::None {
        return None;
    }
    let lower = output.to_ascii_lowercase();
    // A GPU node missing under `strict` — the one mode that deliberately does not
    // bind them. Checked before the write case because the failure reads nothing
    // like a write refusal: HIP/CUDA report a missing device or "no agents
    // found", which is indistinguishable from a machine that genuinely has no
    // card, and an agent that believes that goes off to work around it.
    if policy.mode == SandboxMode::Strict
        && [
            "/dev/kfd",
            "/dev/nvidia",
            "/dev/dri",
            "hsa_status",
            "no rocm",
            "no cuda",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return Some(
            "\n\n[sandbox] `strict` mode does not bind GPU devices (`/dev/kfd`, `/dev/dri`, \
             `/dev/nvidia*`), so a card that exists on this host is invisible in here. This is \
             not a machine without a GPU and not a broken driver — it is the confinement this \
             mode asks for. `write` and `read` mode both pass the devices through; if this work \
             needs the GPU, say so rather than reporting the hardware as absent."
                .to_string(),
        );
    }
    if !lower.contains("read-only file system") && !lower.contains("erofs") {
        return None;
    }
    // A refused git metadata write is the one EROFS with a specific, correct
    // answer, and the generic note below sends the reader looking for a missing
    // writable root instead. Nothing is broken and nothing is missing: this
    // agent is not the one that commits.
    if !policy.readonly_subpaths.is_empty()
        && [
            ".git/",
            "packed-refs",
            "index.lock",
            "objects",
            "refs/heads",
        ]
        .iter()
        .any(|needle| lower.contains(&needle.to_ascii_lowercase()))
    {
        return Some(format!(
            "\n\n[sandbox] that write was into git's metadata ({}), which is READ-ONLY for a \
             sub-agent — deliberately, and this is not a fault to work around. You may read \
             history freely (`git log`, `diff`, `show`, `status`) and edit tracked files; you \
             may not commit, stage, move a ref, or install a hook. You share this working \
             directory with the agent that delegated to you, so a commit from you would sweep \
             its work and any sibling's into one commit under your message. Leave your changes \
             in the tree, list the files you changed in your report, and let the parent commit \
             them.",
            join_roots(&policy.readonly_subpaths),
        ));
    }
    let where_writable = if policy.writable_roots.is_empty() {
        "nothing is writable for this agent (read-only mode)".to_string()
    } else {
        format!("writable here: {}", join_roots(&policy.writable_roots))
    };
    Some(format!(
        "\n\n[sandbox] the \"read-only file system\" above is hrdr's sandbox refusing a write \
         outside this agent's roots — {where_writable}. The program is installed and working; \
         it tried to write somewhere it may not. If it was a package runner fetching a tool \
         (`npx`, `uvx`, `pipx`), run the copy already on PATH instead of downloading one. If \
         the write is genuinely needed, say so — do not report the tool as missing or broken."
    ))
}

/// The command `shell`/`watch` actually spawn: `cmd_str` run through `shell`,
/// wrapped in whatever OS confinement the policy's mode demands. Mode `None`
/// — or a platform/kernel with no backend — returns exactly what
/// [`crate::Shell::command`] returns today, so the unsandboxed path is
/// byte-identical to the pre-sandbox behavior.
///
/// The caller still owns cwd, stdio, timeouts and process groups: bwrap
/// inherits the cwd, passes stdio through untouched, propagates the child's
/// exit status, and `--die-with-parent` plus the pid-namespace init mean the
/// existing group-kill still reaches every descendant.
///
/// `cwd` is passed explicitly (rather than read off the policy) because the
/// policy holds *roots*, and the roots of a write agent include several
/// directories that are not where the command should start. `notices` is the
/// **calling agent's** channel ([`crate::ToolContext::sandbox_notices`]): every
/// degradation this discovers is owed to that agent and to no other.
pub fn sandboxed_shell_command(
    shell: crate::Shell,
    cmd_str: &str,
    policy: &SandboxPolicy,
    cwd: &Path,
    notices: &SandboxNotices,
) -> tokio::process::Command {
    if policy.mode == SandboxMode::None {
        return shell.command(cmd_str);
    }
    shell_command_with_backend(detect_backend(), shell, cmd_str, policy, cwd, notices)
}

/// [`sandboxed_shell_command`] with the backend chosen for it.
///
/// Split out so the fallback arms are reachable on a machine whose detection
/// would never pick them: on a host with a working bwrap, Landlock is dead
/// code that no test could otherwise execute.
///
/// Every arm that ends up running a command with less confinement than the
/// mode asks for sets its §5 notice *first* — the one rule this layer may
/// never break is pretending to sandbox — and it sets it on the **calling
/// agent's** `notices`, so a sibling that never ran a shell command is not told
/// its own sandbox degraded, and one that did is not silenced by whoever got
/// here first.
///
/// [`detection`] is still cached process-wide: caching the *probe* is right (it
/// spawns a process), and it costs no agent its notice, because every arm reads
/// the cached `degraded` reason again on every call rather than announcing it
/// once at detection time.
fn shell_command_with_backend(
    backend: OsSandboxBackend,
    shell: crate::Shell,
    cmd_str: &str,
    policy: &SandboxPolicy,
    cwd: &Path,
    notices: &SandboxNotices,
) -> tokio::process::Command {
    match backend {
        OsSandboxBackend::Bwrap => {
            let mut cmd = tokio::process::Command::new("bwrap");
            cmd.args(bwrap_args(policy.mode, policy, cwd, shell, cmd_str));
            cmd
        }
        #[cfg(target_os = "linux")]
        OsSandboxBackend::Landlock => {
            // Why we are down here rather than on bwrap (§3.5 defers this to
            // the first command that actually needs a backend).
            if let Some(why) = detection().degraded {
                notices.set(why.to_string());
            }
            // Landlock has no read axis. `Read` does not need one (no writable
            // roots IS the whole mode), but `Strict` does — so only it gets the
            // explicit admission of the gap.
            if policy.mode == SandboxMode::Strict {
                notices.set(STRICT_DEGRADES_UNDER_LANDLOCK_NOTICE.to_string());
            }
            landlock_command(shell, cmd_str, policy)
        }
        // `Landlock` is unreachable off Linux (detection never returns it),
        // but the variant exists on every platform, so the arm must too.
        #[cfg(not(target_os = "linux"))]
        OsSandboxBackend::Landlock => {
            notices.set(NO_OS_SANDBOX_NOTICE.to_string());
            shell.command(cmd_str)
        }
        #[cfg(target_os = "macos")]
        OsSandboxBackend::Seatbelt => {
            let mut cmd = tokio::process::Command::new(SEATBELT_PROGRAM);
            cmd.args(seatbelt_args(policy, shell, cmd_str));
            cmd
        }
        // The macOS twin of the Landlock arm above: unreachable off macOS,
        // still a variant that has to compile there.
        #[cfg(not(target_os = "macos"))]
        OsSandboxBackend::Seatbelt => {
            notices.set(NO_OS_SANDBOX_NOTICE.to_string());
            shell.command(cmd_str)
        }
        OsSandboxBackend::None => {
            notices.set(NO_OS_SANDBOX_NOTICE.to_string());
            shell.command(cmd_str)
        }
    }
}

/// The Landlock fallback: the shell is spawned exactly as it would be
/// unsandboxed, but the child confines *itself* between fork and exec, so the
/// ruleset covers the shell and every descendant it goes on to spawn.
///
/// Weaker than bwrap in two ways, both decided and both noticed: reads are
/// unrestricted (Landlock's read axis cannot express "everything except…"
/// without enumerating the filesystem), and a blocked write surfaces as
/// EACCES rather than a read-only mount.
///
/// In `Read` mode the policy's writable roots are empty, which is exactly
/// right here: the child may then write nowhere at all but `/dev/null`.
#[cfg(target_os = "linux")]
fn landlock_command(
    shell: crate::Shell,
    cmd_str: &str,
    policy: &SandboxPolicy,
) -> tokio::process::Command {
    let mut cmd = shell.command(cmd_str);
    // Resolved and cloned *before* the fork: a path that does not exist makes
    // `path_beneath_rules` fail the whole ruleset, and the closure must not go
    // touching the filesystem or the allocator's slow paths post-fork.
    let writable: Vec<PathBuf> = policy
        .writable_roots
        .iter()
        .filter(|root| root.exists())
        .cloned()
        .collect();
    let readonly: Vec<PathBuf> = policy
        .readonly_subpaths
        .iter()
        .filter(|path| path.exists())
        .cloned()
        .collect();
    // SAFETY: the closure runs in the forked child before `exec`. It issues
    // landlock/prctl syscalls and builds the ruleset from data moved in
    // beforehand; it shares no lock, handle, or global with the parent, and it
    // never spawns a thread.
    unsafe {
        cmd.pre_exec(move || install_landlock_rules(&writable, &readonly));
    }
    cmd
}

/// Codex's `install_filesystem_landlock_rules_on_current_thread`
/// (`linux-sandbox/src/landlock.rs`) minus its seccomp: read everything, write
/// only `/dev/null` and the writable roots.
///
/// `BestEffort` compatibility means an older kernel silently enforces the
/// subset of ABI v5 it understands — but a kernel that enforces *nothing*
/// fails the spawn rather than running the command unconfined.
#[cfg(target_os = "linux")]
fn install_landlock_rules(
    writable_roots: &[PathBuf],
    readonly_subpaths: &[PathBuf],
) -> std::io::Result<()> {
    use landlock::{
        ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, path_beneath_rules,
    };

    let abi = ABI::V5;
    let access_rw = AccessFs::from_all(abi);
    let access_ro = AccessFs::from_read(abi);

    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(access_rw)
        .map_err(std::io::Error::other)?
        .create()
        .map_err(std::io::Error::other)?
        .add_rules(path_beneath_rules(["/"], access_ro))
        .map_err(std::io::Error::other)?
        .add_rules(path_beneath_rules(["/dev/null"], access_rw))
        .map_err(std::io::Error::other)?
        // Same reasoning as the bwrap `--dev-bind` above, for the fallback
        // backend: a GPU node is opened read-write to submit work at all, so
        // read access alone leaves it unusable. Landlock has no `/dev` remount
        // to undo — the nodes are visible — but the ruleset would deny the open.
        .add_rules(path_beneath_rules(gpu_device_nodes(), access_rw))
        .map_err(std::io::Error::other)?
        // Codex calls this `set_no_new_privs`, deprecated since its pin.
        .no_new_privs(true);

    if !writable_roots.is_empty() {
        ruleset = ruleset
            .add_rules(path_beneath_rules(writable_roots, access_rw))
            .map_err(std::io::Error::other)?;
    }
    // Nested inside a writable root, and read-only: Landlock resolves an access
    // request against the MOST SPECIFIC matching hierarchy, so a rule on
    // `<cwd>/.git` overrides the one on `<cwd>`. Added last for readability
    // only — unlike bwrap's argv, rule order carries no meaning here.
    if !readonly_subpaths.is_empty() {
        ruleset = ruleset
            .add_rules(path_beneath_rules(readonly_subpaths, access_ro))
            .map_err(std::io::Error::other)?;
    }

    let status = ruleset.restrict_self().map_err(std::io::Error::other)?;
    if status.ruleset == RulesetStatus::NotEnforced {
        // Half-confined is not confined: refuse the spawn instead.
        return Err(std::io::Error::other("landlock not enforced"));
    }
    Ok(())
}

/// The full bwrap argv (everything after `argv[0]`) for `mode`.
///
/// **Argument order is semantics**: bwrap applies mounts in argv order and a
/// later mount shadows an earlier one. In `Write` mode the read-only root must
/// come first so the writable binds layer on top of it; in `Read` mode the
/// private `/tmp` must come *before* the cwd/scratch/tool-output binds, since
/// those can live under `/tmp` and a tmpfs mounted after them hides them (a
/// cwd under `/tmp` then makes even `--chdir` fail). The environment is
/// inherited whole (no `--clearenv`): PATH/HOME/CARGO_HOME must survive.
fn bwrap_args(
    mode: SandboxMode,
    policy: &SandboxPolicy,
    cwd: &Path,
    shell: crate::Shell,
    cmd_str: &str,
) -> Vec<std::ffi::OsString> {
    /// `flags` verbatim.
    fn push(args: &mut Vec<std::ffi::OsString>, flags: &[&str]) {
        args.extend(flags.iter().map(std::ffi::OsString::from));
    }
    /// `<flag> <path> <path>` — every bwrap mount is source-then-destination,
    /// and every mount here keeps the path it already has.
    fn bind(args: &mut Vec<std::ffi::OsString>, flag: &str, path: &Path) {
        args.push(flag.into());
        args.push(path.into());
        args.push(path.into());
    }

    let mut args: Vec<std::ffi::OsString> = Vec::new();
    push(&mut args, &["--new-session", "--die-with-parent"]);
    match mode {
        SandboxMode::Write | SandboxMode::Read => {
            // Everything readable, nothing writable…
            push(
                &mut args,
                &["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"],
            );
            // The accelerators `--dev` just hid, put back. `--dev-bind` rather
            // than `--ro-bind`: these are opened read-write even to submit work,
            // so a read-only bind is the same as not having them. This is not a
            // writable *root* — no file of the user's is reachable through a
            // GPU character device — so it stands in `Read` mode too, which
            // otherwise has no `--bind` at all.
            for dev in gpu_device_nodes() {
                bind(&mut args, "--dev-bind", &dev);
            }
            // …then punch the writable roots through, in policy order. `Read`
            // has none — that is exactly what makes it read-only — so the loop
            // is a no-op there and the whole filesystem stays read-only. Same
            // shape Codex gives its `read-only` mode.
            for root in policy.writable_roots.iter().filter(|root| root.exists()) {
                bind(&mut args, "--bind", root);
            }
            // …and take back the parts of them that must not be writable, AFTER
            // the binds above — bwrap applies mounts in argv order and the last
            // one wins, so this ordering is the whole mechanism. For a write
            // sub-agent that is `<cwd>/.git`: it can edit tracked files, and
            // `git commit` fails on the object write.
            for path in policy.readonly_subpaths.iter().filter(|p| p.exists()) {
                bind(&mut args, "--ro-bind", path);
            }
        }
        SandboxMode::Strict => {
            // Only the system dirs an interpreter/compiler needs exist at
            // all; `/home`, `/opt`, `/var`, … are simply absent, so reads
            // there fail with ENOENT — stronger than EROFS.
            for system in ["/usr", "/etc"] {
                let path = Path::new(system);
                if path.is_dir() {
                    bind(&mut args, "--ro-bind", path);
                }
            }
            args.extend(usr_merge_compat_args());
            push(&mut args, &["--tmpfs", "/tmp"]);
            for root in policy.readable_roots.iter().filter(|root| root.exists()) {
                bind(&mut args, "--ro-bind", root);
            }
            push(&mut args, &["--dev", "/dev", "--proc", "/proc"]);
        }
        // Unreachable: `sandboxed_shell_command` returns before it gets here.
        SandboxMode::None => {}
    }
    push(&mut args, &["--unshare-user", "--unshare-pid"]);
    args.push("--chdir".into());
    // Canonicalized so the child lands in the directory that was actually
    // bound, even when the inherited cwd reaches it through a symlink alias.
    args.push(canonicalize_nearest(cwd).into());
    args.push("--".into());
    args.push(shell.program().into());
    args.extend(shell.invoke_args().iter().map(std::ffi::OsString::from));
    args.push(cmd_str.into());
    args
}

/// `/bin`, `/sbin`, `/lib`, `/lib64` for the Read-mode mount set.
///
/// On a usr-merged distro these are symlinks into `/usr`, and bind-mounting a
/// symlink source manufactures a real directory at the target — which breaks
/// the merge and hides the real binaries. So: recreate the symlink with
/// `--symlink`, reading its **actual** target rather than guessing
/// `usr/<basename>` (on Arch `/sbin → usr/bin` and `/lib64 → usr/lib`, neither
/// of which the guess produces). A real directory is bound read-only; an
/// absent path contributes nothing.
fn usr_merge_compat_args() -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = Vec::new();
    for compat in ["/bin", "/sbin", "/lib", "/lib64"] {
        let path = Path::new(compat);
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            continue; // absent
        };
        if meta.file_type().is_symlink() {
            let Ok(target) = std::fs::read_link(path) else {
                continue;
            };
            args.push("--symlink".into());
            args.push(target.into());
            args.push(path.into());
        } else if meta.is_dir() {
            args.push("--ro-bind".into());
            args.push(path.into());
            args.push(path.into());
        }
    }
    args
}

// The three Seatbelt items below are the macOS backend's building blocks, and
// they are deliberately **not** `cfg(target_os = "macos")`: profile and argv
// construction is pure string work, so keeping it compiled everywhere is what
// lets the tests that pin its exact text run on a Linux developer machine and
// in Linux CI, where the macOS arm itself cannot. Off macOS nothing but those
// tests calls them, hence the `dead_code` waiver.

/// macOS's sandbox wrapper, by absolute path.
///
/// Pinned rather than looked up on `PATH` — exactly as Codex does
/// (`MACOS_PATH_TO_SEATBELT_EXECUTABLE`) — so a poisoned `PATH` cannot swap the
/// confinement for a same-named no-op. If `/usr/bin/sandbox-exec` itself has
/// been tampered with, whoever did it already had root.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const SEATBELT_PROGRAM: &str = "/usr/bin/sandbox-exec";

/// The full `sandbox-exec` argv (everything after `argv[0]`): the generated
/// profile, then the shell invocation it applies to.
///
/// Unlike bwrap there is no `--chdir` to pass — Seatbelt only filters syscalls,
/// so the child inherits the cwd the caller sets on the `Command`, and stdio,
/// exit status, timeouts and group-kill are untouched.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn seatbelt_args(
    policy: &SandboxPolicy,
    shell: crate::Shell,
    cmd_str: &str,
) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        "-p".into(),
        seatbelt_profile(policy.mode, policy).into(),
        "--".into(),
        shell.program().into(),
    ];
    args.extend(shell.invoke_args().iter().map(std::ffi::OsString::from));
    args.push(cmd_str.into());
    args
}

/// The SBPL profile for `mode` (§4 slice 8, verbatim): deny by default, allow
/// the process machinery a shell needs, then open exactly the file access the
/// mode grants.
///
/// `Write` reads everywhere and writes only under the policy's writable roots;
/// `Read` narrows reads to the system directories an interpreter or compiler
/// needs plus the readable roots, and grants no writes at all. Network stays
/// allowed in both, matching the Linux backends — the network axis is a
/// declared follow-up, not v1.
///
/// **Caveat carried from the spec:** the `Read` variant is author-written and
/// **unvalidated** — no Mac was available when this landed, so it has never
/// been run. The `Write` variant is also a coarsening of Codex's
/// `seatbelt_base_policy.sbpl` (broad `sysctl-read`/`mach-lookup`/`ipc-posix*`
/// in place of its enumerated names, and none of its `pseudo-tty`/`iokit-open`/
/// `user-preference-read` allowances), so real-world macOS runs may need
/// additions here.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn seatbelt_profile(mode: SandboxMode, policy: &SandboxPolicy) -> String {
    /// `(subpath "…")` per root, space-separated — SBPL's "this directory and
    /// everything beneath it" filter.
    ///
    /// A path is a quoted SBPL string, so a literal backslash or quote in it
    /// must be escaped or the profile either changes meaning or fails to
    /// parse (and a profile that fails to parse is a command that does not
    /// run — never a boundary that silently widens).
    fn subpaths(roots: &[PathBuf]) -> String {
        roots
            .iter()
            .map(|root| {
                let escaped = root
                    .to_string_lossy()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"");
                format!("(subpath \"{escaped}\")")
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    let mut profile = String::from(
        "(version 1)\n\
         (deny default)\n\
         (allow process-fork)\n\
         (allow process-exec*)\n\
         (allow signal)\n\
         (allow sysctl-read)\n\
         (allow mach-lookup)\n\
         (allow ipc-posix*)\n",
    );
    match mode {
        SandboxMode::Write | SandboxMode::Read => {
            profile.push_str("(allow file-read*)\n");
            // An empty root list must stay closed: `(allow file-write*)` with
            // no filter allows every write there is, so the line is omitted
            // and `(deny default)` answers instead. `Read` has no writable
            // roots at all, so it always takes that branch — broad reads, no
            // writes anywhere.
            if !policy.writable_roots.is_empty() {
                let writes = subpaths(&policy.writable_roots);
                profile.push_str(&format!("(allow file-write* {writes})\n"));
            }
            // Subtract what must stay read-only inside those roots. SBPL is
            // last-match-wins, so this `deny` after the `allow` above is what
            // makes a write sub-agent's `<cwd>/.git` unwritable.
            if !policy.readonly_subpaths.is_empty() {
                let denied = subpaths(&policy.readonly_subpaths);
                profile.push_str(&format!("(deny file-write* {denied})\n"));
            }
        }
        SandboxMode::Strict => {
            let reads = subpaths(&policy.readable_roots);
            let reads = if reads.is_empty() {
                String::new()
            } else {
                format!(" {reads}")
            };
            profile.push_str(&format!(
                "(allow file-read* (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\")\n  \
                 (subpath \"/System\") (subpath \"/Library\") (subpath \"/private/etc\")\n  \
                 (subpath \"/dev\"){reads})\n"
            ));
        }
        // Unreachable: `sandboxed_shell_command` returns before it gets here.
        SandboxMode::None => {}
    }
    profile.push_str("(allow network*)\n");
    profile
}

/// A [`crate::ToolContext`] rooted at `dir` and confined to it *alone*, for
/// the file-tool guard tests.
///
/// Deliberately a struct literal rather than [`SandboxPolicy::for_agent`]:
/// `for_agent` makes [`std::env::temp_dir`] writable, so a second tempdir
/// would sit *inside* the roots and no "outside" assertion could ever fire.
#[cfg(test)]
pub(crate) fn confined_ctx(dir: &Path, mode: SandboxMode) -> crate::ToolContext {
    let root = canonicalize_nearest(dir);
    let mut ctx = crate::ToolContext::new(dir.to_path_buf());
    ctx.sandbox = std::sync::Arc::new(SandboxPolicy {
        mode,
        writable_roots: vec![root.clone()],
        readable_roots: vec![root],
        readonly_subpaths: Vec::new(),
    });
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A blocked write is reported as the SANDBOX, not as a broken tool.
    ///
    /// Verbatim from a real run: the model wrote a report, then ran
    /// `npx prettier --write docs/code-review.md`. `prettier` was installed and
    /// on `PATH`, but `npx` ignored it and tried to fetch the package into
    /// `~/.npm/_cacache`, which the sandbox binds read-only. The model read the
    /// `EROFS` as "prettier is not available in this environment" — a false claim
    /// about the machine — and skipped formatting.
    #[test]
    fn a_sandboxed_write_denial_is_named_as_the_sandbox() {
        const NPX_EROFS: &str = "npm error code EROFS\n\
             npm error syscall open\n\
             npm error path /home/u/.npm/_cacache/tmp/aa7100ad\n\
             npm error errno EROFS\n\
             npm error rofs Invalid response body while trying to fetch \
             https://registry.npmjs.org/prettier: EROFS: read-only file system";

        let dir = tempfile::tempdir().unwrap();
        let write = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        let note = sandbox_denial_note(&write, NPX_EROFS).expect("the denial is recognized");
        assert!(note.contains("[sandbox]"), "{note}");
        assert!(note.contains("writable here:"), "{note}");
        // It must say the thing the model got wrong, in as many words.
        assert!(
            note.contains("do not report the tool as missing or broken"),
            "{note}"
        );
        assert!(note.contains("run the copy already on PATH"), "{note}");

        // A read-mode agent has no writable root at all — say that, rather than
        // printing an empty list.
        let read = SandboxPolicy::for_agent(SandboxMode::Read, dir.path(), &[]);
        let ro_note = sandbox_denial_note(&read, NPX_EROFS).expect("recognized in read mode too");
        assert!(ro_note.contains("read-only mode"), "{ro_note}");

        // Unconfined: the sandbox did not do this, so it says nothing.
        assert_eq!(
            sandbox_denial_note(&SandboxPolicy::unconfined(), NPX_EROFS),
            None
        );
        // …and NARROW: an ordinary failure is never editorialized over. A bare
        // "Permission denied" is a normal error a program raises for its own
        // reasons, and annotating it would be noise on every one of them.
        for ordinary in [
            "error: could not compile `foo`",
            "cat: /etc/shadow: Permission denied",
            "fatal: not a git repository",
            "",
        ] {
            assert_eq!(sandbox_denial_note(&write, ordinary), None, "{ordinary}");
        }
    }

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
    fn strict_mode_refuses_reads_outside_roots_and_allows_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Strict, dir.path(), &[]);

        check_read(&policy, &dir.path().join("notes.md")).unwrap();
        check_read(&policy, &session_scratch_dir().join("probe")).unwrap();
        check_read(&policy, &tool_output_dir().join("probe")).unwrap();

        let err = check_read(&policy, Path::new("/etc/passwd"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to read /etc/passwd"), "{err}");
        assert!(err.contains("strictly confined and may read only"), "{err}");
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

    /// A real repo with one commit at `<root>/repo` plus a linked worktree at
    /// `<root>/wt` on branch `hrdr/task-1` — the exact shape a write
    /// sub-agent runs in, and the one the metadata roots exist for.
    fn repo_with_linked_worktree(root: &Path) -> (PathBuf, PathBuf) {
        let repo = root.join("repo");
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
        let wt = root.join("wt");
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "hrdr/task-1",
            wt.to_str().unwrap(),
        ]);
        (repo, wt)
    }

    #[test]
    fn git_metadata_roots_for_a_linked_worktree() {
        if which::which("git").is_err() {
            return; // best-effort: exercise the real backend when available
        }
        let dir = tempfile::tempdir().unwrap();
        let (repo, wt) = repo_with_linked_worktree(dir.path());

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
            readonly_subpaths: Vec::new(),
        };
        check_write(&policy, &common.join("index")).unwrap_err();
        check_write(&policy, &common.join("refs").join("heads").join("main")).unwrap_err();
        // `objects` IS a writable root — the OS layer binds it rw or no commit
        // works — but the *file tools* are refused it all the same: git writes
        // its own object store, and nothing the model types needs to.
        let err = check_write(&policy, &common.join("objects").join("aa").join("bb"))
            .unwrap_err()
            .to_string();
        assert!(err.contains(".git"), "{err}");
    }

    /// "Is it under a writable root" cannot be the only question a write has to
    /// answer: a write sub-agent's cwd **is** its worktree, so the worktree's
    /// `.git` is under a writable root and `.git/hooks/pre-commit` would be a
    /// file the model may write and the user's next commit would execute.
    #[test]
    fn metadata_writes_are_refused_inside_a_writable_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonicalize_nearest(dir.path());
        // Struct literal, not `for_agent`: the subject is paths *inside* the
        // root, and a writable `env::temp_dir()` root only adds noise.
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![root.clone()],
            readable_roots: vec![root],
            readonly_subpaths: Vec::new(),
        };

        for refused in [
            ".git/hooks/pre-commit",
            ".git/config",
            ".git/objects/ab/cdef",
            // Not just the leading component: a `.git` several levels down is
            // some other repo's hooks, which is the same escalation.
            "vendor/dep/.git/hooks/post-checkout",
        ] {
            let err = check_write(&policy, &dir.path().join(refused))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("never write repository or agent metadata"),
                "{refused}: {err}"
            );
            assert!(err.contains("ask the user"), "{refused}: {err}");
        }

        // The harness-config trees are the same hazard on paper and are
        // deliberately NOT refused — see `PROTECTED_METADATA_DIRS`. Authoring a
        // project skill is an ordinary request, `shell` reaches these paths
        // regardless, and widening the list is a policy call recorded in the
        // backlog rather than one this guard makes quietly. Asserted so the
        // decision is visible the next time somebody reads the list and assumes
        // an omission.
        for allowed in [
            ".hrdr/skills/helpful.md",
            ".claude/agents/reviewer.md",
            ".opencode/command/ship.md",
        ] {
            check_write(&policy, &dir.path().join(allowed)).unwrap_or_else(|e| {
                panic!("{allowed} is deliberately writable, see PROTECTED_METADATA_DIRS: {e}")
            });
        }

        // A neighbour of `.git` is ordinary work, and the test is on whole
        // components — `.gitignore` and `.github` are not `.git`.
        check_write(&policy, &dir.path().join("src").join("main.rs")).unwrap();
        check_write(&policy, &dir.path().join(".gitignore")).unwrap();
        check_write(&policy, &dir.path().join(".github").join("ci.yml")).unwrap();

        // Write-only: the model must still be able to *read* a config it may
        // not write, or it cannot answer a question about the repo.
        let read_policy = SandboxPolicy {
            mode: SandboxMode::Read,
            writable_roots: Vec::new(),
            readable_roots: vec![canonicalize_nearest(dir.path())],
            readonly_subpaths: Vec::new(),
        };
        check_read(&read_policy, &dir.path().join(".git").join("config")).unwrap();

        // Mode `None` is unaffected: an unconfined agent has no boundary to
        // escalate out of, and that path stays what it was before the sandbox.
        check_write(
            &SandboxPolicy::unconfined(),
            &dir.path().join(".git").join("hooks").join("pre-commit"),
        )
        .unwrap();

        // The symlink shape: the guard decides on canonical paths, which is the
        // only reason a link *into* `.git` is refusable at all.
        #[cfg(unix)]
        {
            let dot_git = dir.path().join(".git");
            std::fs::create_dir(&dot_git).unwrap();
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&dot_git, &link).unwrap();
            check_write(&policy, &link.join("hooks").join("pre-commit")).unwrap_err();
        }
    }

    /// hrdr can itself be launched inside a linked worktree (the user made one,
    /// or another harness did). The MAIN agent must still be able to commit
    /// there, which is what [`git_metadata_roots`] grants — while the file tools
    /// stay refused that same `.git`, because `check_write` answers on where the
    /// write lands and hrdr's git plumbing does not go through it.
    ///
    /// Not the sub-agent case: a delegated writer has `deny_git_writes` applied
    /// on top and cannot commit anywhere (see
    /// `a_subagent_can_edit_files_but_not_commit`).
    #[test]
    fn hrdr_inside_a_linked_worktree_still_commits_under_the_metadata_guard() {
        if which::which("git").is_err() {
            return; // best-effort: exercise the real backend when available
        }
        let dir = tempfile::tempdir().unwrap();
        let (_repo, wt) = repo_with_linked_worktree(dir.path());
        // The sub-agent shape from `for_agent`, minus the temp/scratch roots
        // that would cover the whole test tree.
        let mut roots = vec![wt.clone()];
        roots.extend(git_metadata_roots(&wt));
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: canonical_roots(roots),
            readable_roots: Vec::new(),
            readonly_subpaths: Vec::new(),
        };

        // The model's tools: its own source file yes; the worktree's `.git`
        // pointer, and anything the pointer leads to, no.
        check_write(&policy, &wt.join("f.txt")).unwrap();
        check_write(&policy, &wt.join(".git")).unwrap_err();
        check_write(&policy, &wt.join(".git").join("hooks").join("pre-commit")).unwrap_err();

        // hrdr's plumbing, spelled the way `task_*` spells it.
        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&wt)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        git(&["add", "f.txt"]);
        git(&[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "mine",
        ]);
        assert!(
            git(&["log", "--oneline"]).contains("mine"),
            "the sub-agent's commit did not land"
        );
    }

    /// The argv as strings, for readable assertions.
    fn argv(args: &[std::ffi::OsString]) -> Vec<String> {
        args.iter().map(|a| a.to_string_lossy().into()).collect()
    }

    /// The index of the mount `<flag> <path> <path>` triple, or `None`.
    fn mount_at(args: &[String], flag: &str, path: &Path) -> Option<usize> {
        let shown = path.display().to_string();
        args.windows(3)
            .position(|w| w[0] == flag && w[1] == shown && w[2] == shown)
    }

    #[test]
    fn bwrap_write_args_are_exactly_the_spec() {
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one");
        let two = dir.path().join("two");
        std::fs::create_dir(&one).unwrap();
        std::fs::create_dir(&two).unwrap();
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![one.clone(), two.clone()],
            readable_roots: Vec::new(),
            readonly_subpaths: Vec::new(),
        };
        let args = argv(&bwrap_args(
            SandboxMode::Write,
            &policy,
            dir.path(),
            crate::Shell::Bash,
            "echo hi",
        ));

        let chdir = canonicalize_nearest(dir.path()).display().to_string();
        let expected: Vec<String> = [
            "--new-session",
            "--die-with-parent",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
        ]
        .iter()
        .map(|a| a.to_string())
        // The GPU binds are derived, not hardcoded: which nodes exist is a
        // property of the host, so a literal list would pass on this machine and
        // fail on a runner with no card (or a second one). Their POSITION is
        // still pinned — after the `/dev` remount they undo, before the writable
        // roots — which is the part that has to be right.
        .chain(
            gpu_device_nodes()
                .iter()
                .flat_map(|d| {
                    let p = d.display().to_string();
                    ["--dev-bind".to_string(), p.clone(), p]
                })
                .collect::<Vec<_>>(),
        )
        .chain(
            [
                "--bind",
                &one.display().to_string(),
                &one.display().to_string(),
                "--bind",
                &two.display().to_string(),
                &two.display().to_string(),
                "--unshare-user",
                "--unshare-pid",
                "--chdir",
                &chdir,
                "--",
                "bash",
                "-c",
                "echo hi",
            ]
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>(),
        )
        .collect();
        assert_eq!(args, expected);

        // Order is semantics: the read-only root must be mounted before the
        // writable binds layer over it, or they are shadowed away.
        let ro_root = args
            .windows(3)
            .position(|w| w[0] == "--ro-bind" && w[1] == "/" && w[2] == "/")
            .unwrap();
        assert!(ro_root < mount_at(&args, "--bind", &one).unwrap());
        assert!(ro_root < mount_at(&args, "--bind", &two).unwrap());
    }

    /// `read` is a WRITE restriction, not a read one: the whole filesystem is
    /// bound read-only and nothing is writable anywhere.
    ///
    /// This is the shape Codex gives its own `read-only` mode, and hrdr adopted
    /// it after the previous behavior (mount only `/usr` + `/etc`, the current
    /// [`SandboxMode::Strict`]) left a read-only agent's shell unable to see the
    /// tools the user had installed — `~/.cargo/bin`, a version-managed node, a
    /// Homebrew or Nix prefix — and reporting "command not found" for them.
    #[test]
    fn bwrap_read_args_bind_the_whole_filesystem_read_only_and_nothing_writable() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Read, dir.path(), &[]);
        assert!(
            policy.writable_roots.is_empty(),
            "no writable root is what makes it read-only"
        );
        let args = argv(&bwrap_args(
            SandboxMode::Read,
            &policy,
            dir.path(),
            crate::Shell::Bash,
            "echo hi",
        ));

        // The whole filesystem, read-only — so every PATH entry resolves.
        assert!(
            args.windows(3).any(|w| w == ["--ro-bind", "/", "/"]),
            "{args:?}"
        );
        // …and not one writable bind, anywhere.
        assert!(
            !args.iter().any(|a| a == "--bind"),
            "read mode writes nothing: {args:?}"
        );
        // No private /tmp either: that is strict mode's isolation, and mounting
        // a tmpfs here would hide a real /tmp the tools legitimately read.
        assert!(!args.iter().any(|a| a == "--tmpfs"), "{args:?}");
    }

    /// `--dev` mounts a fresh minimal devtmpfs, which silently deletes every
    /// accelerator on the host from the sandbox's view. `Write` and `Read` both
    /// mount all of `/` and are meant to show the machine as it is, so the GPU
    /// nodes are bound back through — and with `--dev-bind`, because a compute
    /// device is opened read-write to submit work at all, making a read-only
    /// bind identical to not having it.
    ///
    /// Skipped when the host has no GPU: there is nothing to assert about a
    /// machine with no such device, and asserting the *absence* would pass for
    /// the wrong reason on every CI runner.
    #[cfg(target_os = "linux")]
    #[test]
    fn bwrap_binds_gpu_devices_back_through_the_fresh_dev_mount() {
        let devices = gpu_device_nodes();
        if devices.is_empty() {
            return; // no GPU on this host — nothing this test can observe
        }
        let dir = tempfile::tempdir().unwrap();
        for mode in [SandboxMode::Write, SandboxMode::Read] {
            let policy = SandboxPolicy::for_agent(mode, dir.path(), &[]);
            let args = argv(&bwrap_args(
                mode,
                &policy,
                dir.path(),
                crate::Shell::Bash,
                "rocminfo",
            ));
            // `--dev /dev` is still there — this adds to it rather than
            // replacing it, so `/dev/null` and friends keep working.
            assert!(args.windows(2).any(|w| w == ["--dev", "/dev"]), "{args:?}");
            for dev in &devices {
                let d = dev.to_string_lossy().to_string();
                let bound = args
                    .windows(3)
                    .any(|w| w[0] == "--dev-bind" && w[1] == d && w[2] == d);
                assert!(bound, "{mode:?} must bind {d} back through: {args:?}");
            }
        }
        // Strict is the deliberate exception: it confines by leaving things out,
        // and `sandbox_denial_note` explains the absence rather than the mount
        // set quietly papering over it.
        let policy = SandboxPolicy::for_agent(SandboxMode::Strict, dir.path(), &[]);
        let args = argv(&bwrap_args(
            SandboxMode::Strict,
            &policy,
            dir.path(),
            crate::Shell::Bash,
            "rocminfo",
        ));
        assert!(
            !args.iter().any(|a| a == "--dev-bind"),
            "strict binds no devices: {args:?}"
        );
    }

    /// A GPU failure under `strict` reads exactly like a machine with no GPU —
    /// HIP says the device is missing, not that a sandbox hid it — so the note
    /// has to name the cause. Under the modes that DO bind the devices there is
    /// nothing to explain, and saying it anyway would be wrong.
    #[test]
    fn a_missing_gpu_under_strict_is_explained_as_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let strict = SandboxPolicy::for_agent(SandboxMode::Strict, dir.path(), &[]);
        let note = sandbox_denial_note(&strict, "hipErrorNoDevice: failed to open /dev/kfd")
            .expect("strict must explain a hidden GPU");
        assert!(note.contains("does not bind GPU devices"), "{note}");
        assert!(note.contains("not a machine without a GPU"), "{note}");

        for mode in [SandboxMode::Write, SandboxMode::Read] {
            let policy = SandboxPolicy::for_agent(mode, dir.path(), &[]);
            assert!(
                sandbox_denial_note(&policy, "failed to open /dev/kfd").is_none(),
                "{mode:?} binds the devices — a failure there is not the sandbox"
            );
        }
    }

    /// Linux-only: the Strict profile's mount set is built from what the *host*
    /// filesystem really has — `/usr`, `/etc`, and whatever `/bin` turns out to
    /// be — through `.exists()`/`symlink_metadata` filters. Off Linux those
    /// paths are absent (Windows) or shaped differently, so the builder
    /// correctly emits a different argv and there is nothing to assert against:
    /// bwrap only ever runs on Linux. Its Write sibling above stays
    /// cross-platform because every path it names is a tempdir or the literal
    /// `/` bind the builder emits unconditionally.
    #[cfg(target_os = "linux")]
    #[test]
    fn bwrap_strict_args_omit_rw_binds_and_private_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Strict, dir.path(), &[]);
        let args = argv(&bwrap_args(
            SandboxMode::Strict,
            &policy,
            dir.path(),
            crate::Shell::Bash,
            "echo hi",
        ));

        assert!(
            !args.iter().any(|a| a == "--bind"),
            "read mode writes nothing: {args:?}"
        );
        assert!(
            mount_at(&args, "--ro-bind", Path::new("/usr")).is_some(),
            "{args:?}"
        );

        // `--tmpfs /tmp` must precede every readable-root bind: the scratch
        // dir (and the tool-output dir in its fallback location) live under
        // `/tmp`, and a tmpfs mounted after them hides them.
        let tmpfs = args
            .windows(2)
            .position(|w| w[0] == "--tmpfs" && w[1] == "/tmp")
            .expect("read mode gets a private /tmp");
        for root in &policy.readable_roots {
            let at = mount_at(&args, "--ro-bind", root)
                .unwrap_or_else(|| panic!("{} is not bound: {args:?}", root.display()));
            assert!(
                tmpfs < at,
                "--tmpfs /tmp ({tmpfs}) must precede {} ({at})",
                root.display()
            );
        }

        // `/bin` is a symlink on usr-merged distros: recreate it as one,
        // pointing where it really points — a bind would manufacture a real
        // directory and break the merge.
        if let Ok(meta) = std::fs::symlink_metadata("/bin") {
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link("/bin").unwrap().display().to_string();
                assert!(
                    args.windows(3)
                        .any(|w| w[0] == "--symlink" && w[1] == target && w[2] == "/bin"),
                    "expected --symlink {target} /bin in {args:?}"
                );
            } else if meta.is_dir() {
                assert!(
                    mount_at(&args, "--ro-bind", Path::new("/bin")).is_some(),
                    "{args:?}"
                );
            }
        }

        assert_eq!(args[args.len() - 4..], ["--", "bash", "-c", "echo hi"]);
    }

    /// The Write profile names every writable root and nothing else, and the
    /// rest of it is the spec's text byte for byte. Pure string work, so it
    /// runs on every platform — the macOS arm cannot be exercised off macOS,
    /// but the profile it would hand to `sandbox-exec` can.
    #[test]
    fn seatbelt_profile_lists_every_writable_root() {
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![PathBuf::from("/work/wt"), PathBuf::from("/tmp/scratch")],
            readable_roots: Vec::new(),
            readonly_subpaths: Vec::new(),
        };
        assert_eq!(
            seatbelt_profile(SandboxMode::Write, &policy),
            concat!(
                "(version 1)\n",
                "(deny default)\n",
                "(allow process-fork)\n",
                "(allow process-exec*)\n",
                "(allow signal)\n",
                "(allow sysctl-read)\n",
                "(allow mach-lookup)\n",
                "(allow ipc-posix*)\n",
                "(allow file-read*)\n",
                "(allow file-write* (subpath \"/work/wt\") (subpath \"/tmp/scratch\"))\n",
                "(allow network*)\n",
            )
        );

        // A quote in a path is escaped, not left to break the profile.
        let odd = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![PathBuf::from("/work/we\"ird")],
            readable_roots: Vec::new(),
            readonly_subpaths: Vec::new(),
        };
        assert!(
            seatbelt_profile(SandboxMode::Write, &odd)
                .contains("(allow file-write* (subpath \"/work/we\\\"ird\"))"),
            "{}",
            seatbelt_profile(SandboxMode::Write, &odd)
        );

        // With no writable roots the write line is absent entirely: an
        // unfiltered `(allow file-write*)` would allow every write there is.
        let empty = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: Vec::new(),
            readable_roots: Vec::new(),
            readonly_subpaths: Vec::new(),
        };
        assert!(
            !seatbelt_profile(SandboxMode::Write, &empty).contains("file-write*"),
            "an empty root set must stay closed, not open"
        );
    }

    /// Read mode grants no writes at all and narrows reads to the system
    /// directories plus the readable roots.
    #[test]
    fn seatbelt_strict_profile_allows_no_writes_and_only_the_read_roots() {
        let policy = SandboxPolicy {
            mode: SandboxMode::Strict,
            writable_roots: Vec::new(),
            readable_roots: vec![PathBuf::from("/work/wt")],
            readonly_subpaths: Vec::new(),
        };
        assert_eq!(
            seatbelt_profile(SandboxMode::Strict, &policy),
            concat!(
                "(version 1)\n",
                "(deny default)\n",
                "(allow process-fork)\n",
                "(allow process-exec*)\n",
                "(allow signal)\n",
                "(allow sysctl-read)\n",
                "(allow mach-lookup)\n",
                "(allow ipc-posix*)\n",
                "(allow file-read* (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\")\n",
                "  (subpath \"/System\") (subpath \"/Library\") (subpath \"/private/etc\")\n",
                "  (subpath \"/dev\") (subpath \"/work/wt\"))\n",
                "(allow network*)\n",
            )
        );
    }

    /// The Seatbelt argv: `-p <profile> -- <shell> -c <cmd>`, and the wrapper
    /// is the pinned absolute path, never a `PATH` lookup.
    #[test]
    fn seatbelt_args_pass_the_profile_then_the_shell_invocation() {
        assert_eq!(SEATBELT_PROGRAM, "/usr/bin/sandbox-exec");
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![PathBuf::from("/work/wt")],
            readable_roots: Vec::new(),
            readonly_subpaths: Vec::new(),
        };
        let args = argv(&seatbelt_args(&policy, crate::Shell::Bash, "echo hi"));
        assert_eq!(args[0], "-p");
        assert_eq!(args[1], seatbelt_profile(SandboxMode::Write, &policy));
        assert_eq!(args[2..], ["--", "bash", "-c", "echo hi"]);
    }

    /// The real thing, on the only platform that has it: a write outside the
    /// roots is refused by the kernel, a write inside lands.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn shell_write_outside_roots_is_denied_under_seatbelt() {
        if !Path::new(SEATBELT_PROGRAM).exists() {
            return; // best-effort: exercise the real backend when available
        }
        let Some(shell) = crate::Shell::detect() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // Struct literal, not `for_agent`: a writable `env::temp_dir()` root
        // would cover the "outside" tempdir too (the slice-3/5/6 trap).
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![canonicalize_nearest(dir.path())],
            readable_roots: Vec::new(),
            readonly_subpaths: Vec::new(),
        };

        let target = canonicalize_nearest(outside.path()).join("escaped");
        let mut cmd = shell_command_with_backend(
            OsSandboxBackend::Seatbelt,
            shell,
            &format!("echo x > {}", target.display()),
            &policy,
            dir.path(),
            &notices(),
        );
        cmd.current_dir(dir.path());
        let out = cmd.output().await.unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(!out.status.success(), "the write was allowed: {stderr}");
        assert!(stderr.contains("Operation not permitted"), "{stderr}");
        assert!(!target.exists(), "the write landed anyway");

        let inside = canonicalize_nearest(dir.path()).join("mine");
        let mut cmd = shell_command_with_backend(
            OsSandboxBackend::Seatbelt,
            shell,
            &format!("echo x > {}", inside.display()),
            &policy,
            dir.path(),
            &notices(),
        );
        cmd.current_dir(dir.path());
        let out = cmd.output().await.unwrap();
        assert!(
            out.status.success(),
            "the cwd write was blocked: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(std::fs::read_to_string(&inside).unwrap().trim(), "x");
    }

    /// The canonical skip guard for the end-to-end tests: the real backend
    /// when this machine has one, plus a shell to run through it.
    #[cfg(target_os = "linux")]
    fn bwrap_shell() -> Option<crate::Shell> {
        if detect_backend() != OsSandboxBackend::Bwrap {
            return None; // best-effort: exercise the real backend when available
        }
        crate::Shell::detect()
    }

    /// Run `command` through the real `shell` tool with `ctx`'s policy.
    #[cfg(target_os = "linux")]
    async fn run_shell(shell: crate::Shell, ctx: &crate::ToolContext, command: &str) -> String {
        use crate::Tool as _;
        crate::ShellTool::new(shell)
            .execute(
                serde_json::json!({"command": command, "timeout_secs": 60}),
                ctx,
            )
            .await
            .unwrap()
    }

    /// A write outside the roots dies in the kernel, not in the guard: the
    /// mount is read-only, so the redirect fails and nothing is created. This
    /// is the escape that motivated the whole feature, at the shell layer.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shell_write_outside_roots_hits_a_readonly_fs() {
        let Some(shell) = bwrap_shell() else { return };
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // A `for_agent` policy would bind `env::temp_dir()` writable and the
        // second tempdir with it; confine to the cwd alone.
        let ctx = confined_ctx(dir.path(), SandboxMode::Write);

        let target = outside.path().join("escaped");
        let out = run_shell(shell, &ctx, &format!("echo x > {}", target.display())).await;
        assert!(out.contains("Read-only file system"), "{out}");
        assert!(!target.exists(), "the write landed anyway");

        // …including the shape actually observed in the wild: `cd` out first.
        let out = run_shell(
            shell,
            &ctx,
            &format!("cd {} && echo x > escaped2", outside.path().display()),
        )
        .await;
        assert!(out.contains("Read-only file system"), "{out}");
        assert!(!outside.path().join("escaped2").exists());
    }

    /// The flip side: everything the default root set covers really is
    /// writable inside the sandbox, or no build would survive it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shell_write_in_cwd_and_tmp_succeeds_under_bwrap() {
        let Some(shell) = bwrap_shell() else { return };
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = crate::ToolContext::new(dir.path().to_path_buf());
        ctx.sandbox = std::sync::Arc::new(SandboxPolicy::for_agent(
            SandboxMode::Write,
            dir.path(),
            &[],
        ));

        let in_cwd = dir.path().join("in-cwd");
        let in_scratch = session_scratch_dir().join("bwrap-write-probe");
        let out = run_shell(
            shell,
            &ctx,
            &format!(
                "echo a > {} && echo b > {}",
                in_cwd.display(),
                in_scratch.display()
            ),
        )
        .await;
        assert!(!out.contains("[exit status"), "{out}");
        assert_eq!(std::fs::read_to_string(&in_cwd).unwrap().trim(), "a");
        assert_eq!(std::fs::read_to_string(&in_scratch).unwrap().trim(), "b");
        let _ = std::fs::remove_file(&in_scratch);
    }

    /// hrdr launched inside a linked worktree commits THERE and nowhere else:
    /// [`git_metadata_roots`] grants the shared object store and the `hrdr/` ref
    /// namespace, never the parent `.git` itself, so a commit against the parent
    /// checkout is still refused at the OS layer.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_linked_worktree_commits_but_the_parent_repo_stays_blocked() {
        let Some(shell) = bwrap_shell() else { return };
        if which::which("git").is_err() {
            return; // best-effort: exercise the real backend when available
        }
        let dir = tempfile::tempdir().unwrap();
        let (repo, wt) = repo_with_linked_worktree(dir.path());

        // Struct literal, not `for_agent`: the repo lives in a tempdir, and a
        // writable `env::temp_dir()` root would make the parent writable too
        // and void the whole test.
        let mut roots = vec![canonicalize_nearest(&wt)];
        roots.extend(
            git_metadata_roots(&wt)
                .iter()
                .map(|r| canonicalize_nearest(r)),
        );
        let mut ctx = crate::ToolContext::new(wt.clone());
        ctx.sandbox = std::sync::Arc::new(SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: roots,
            readable_roots: Vec::new(),
            readonly_subpaths: Vec::new(),
        });

        std::fs::write(wt.join("f.txt"), "hi").unwrap();
        let ident = "-c user.email=t@example.com -c user.name=t";
        let out = run_shell(
            shell,
            &ctx,
            &format!("git add f.txt && git {ident} commit -q -m mine"),
        )
        .await;
        assert!(
            !out.contains("[exit status"),
            "the worktree commit failed: {out}"
        );
        let log = std::process::Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(&wt)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&log.stdout).contains("mine"),
            "the commit did not land: {log:?}"
        );

        // The parent repo's index is outside the roots, so committing there
        // dies before it can touch a ref.
        let out = run_shell(
            shell,
            &ctx,
            &format!(
                "git -C {} {ident} commit --allow-empty -m escaped",
                repo.display()
            ),
        )
        .await;
        assert!(
            out.contains("[exit status"),
            "the parent commit succeeded: {out}"
        );
        let log = std::process::Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&log.stdout).contains("escaped"),
            "a commit landed on the parent repo: {log:?}"
        );
    }

    /// Read mode does not merely refuse writes — the rest of the filesystem
    /// is not mounted at all, so an outside path is ENOENT rather than EROFS.
    /// (`/usr` and `/etc` stay readable by design; probe `/home`.)
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn strict_mode_cannot_even_see_outside_paths() {
        let Some(shell) = bwrap_shell() else { return };
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("visible.txt"), "x").unwrap();
        let ctx = confined_ctx(dir.path(), SandboxMode::Strict);

        let out = run_shell(shell, &ctx, "ls /home").await;
        assert!(out.contains("No such file or directory"), "{out}");

        let out = run_shell(shell, &ctx, "ls").await;
        assert!(out.contains("visible.txt"), "{out}");
    }

    /// The timeout still reaps the whole tree through bwrap:
    /// `--die-with-parent` plus the pid-namespace init mean killing the spawn
    /// group takes every descendant with it.
    ///
    /// The sibling test in `shell.rs` probes the backgrounded grandchild by
    /// pid; that cannot work here, because `--unshare-pid` makes the pid the
    /// child records a *namespace* pid that means something else on the host.
    /// The marker file is the portable proof: it only appears if the
    /// grandchild outlived the kill.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timeout_kill_reaches_through_bwrap() {
        let Some(shell) = bwrap_shell() else { return };
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = confined_ctx(dir.path(), SandboxMode::Write);
        ctx.enforce_timeout_floor = false;
        let marker = dir.path().join("grandchild-finished");

        let command = format!("(sleep 5 && touch {m}) & sleep 5", m = marker.display());
        use crate::Tool as _;
        let out = crate::ShellTool::new(shell)
            .execute(
                serde_json::json!({"command": command, "timeout_secs": 1}),
                &ctx,
            )
            .await
            .expect_err("a killed command is not a successful one")
            .to_string();
        assert!(out.contains("timed out"), "{out}");

        // Well past the grandchild's own sleep: if it were alive it would
        // have touched the marker by now.
        tokio::time::sleep(std::time::Duration::from_millis(5500)).await;
        assert!(
            !marker.exists(),
            "the backgrounded grandchild survived the group kill"
        );
    }

    /// The claim the whole delegation model rests on, proved against the real
    /// OS backend rather than against the argv: a write sub-agent can change
    /// tracked files and read history, and CANNOT commit.
    ///
    /// Not an argv assertion on purpose. `--ro-bind` after `--bind` is only the
    /// mechanism on one backend; what must hold is that `git commit` fails, and
    /// that is a property of whatever backend this machine actually runs.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_subagent_can_edit_files_but_not_commit() {
        let Some(shell) = bwrap_shell() else { return };
        let dir = tempfile::tempdir().unwrap();
        let repo = canonicalize_nearest(dir.path());
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .expect("git")
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "before\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-qm", "init"]);
        if !repo.join(".git").is_dir() {
            return; // git unavailable — nothing to prove
        }

        let mut policy = SandboxPolicy::for_agent(SandboxMode::Write, &repo, &[]);
        policy.deny_git_writes(&repo);
        assert_eq!(
            policy.readonly_subpaths,
            vec![repo.join(".git")],
            "the repo's own .git is what gets subtracted"
        );
        let mut ctx = crate::ToolContext::new(repo.clone());
        ctx.sandbox = std::sync::Arc::new(policy);
        let run = |command: String| {
            let ctx = ctx.clone();
            async move {
                use crate::Tool as _;
                crate::ShellTool::new(shell)
                    .execute(serde_json::json!({"command": command}), &ctx)
                    .await
                    .map_err(|e| e.to_string())
                    .unwrap_or_else(|e| e)
            }
        };

        // Reading history is untouched — a sub-agent reviewing its own work
        // needs `log`/`diff`/`status`, and none of them writes.
        let log = run("git log --oneline".to_string()).await;
        assert!(log.contains("init"), "history stays readable: {log}");

        // Editing a tracked file is the sub-agent's whole job.
        let edit = run("printf after > f.txt".to_string()).await;
        assert!(!edit.to_lowercase().contains("read-only"), "{edit}");
        assert_eq!(
            std::fs::read_to_string(repo.join("f.txt")).unwrap(),
            "after"
        );

        // …and `git status` sees it, through the same read-only .git.
        let status = run("git status --short".to_string()).await;
        assert!(status.contains("f.txt"), "{status}");

        // The line that matters. Staging writes the index; committing writes an
        // object and moves a ref. Both live in `.git`.
        let commit = run("git add f.txt && git commit -m nope".to_string()).await;
        assert!(
            commit.to_lowercase().contains("read-only"),
            "a sub-agent must not be able to commit: {commit}"
        );
        let head = git(&["log", "--oneline"]);
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).lines().count(),
            1,
            "history is exactly where the parent left it"
        );

        // Moving a ref directly is the same wall, reached another way.
        let ref_write = run("git update-ref refs/heads/main HEAD".to_string()).await;
        assert!(
            ref_write.to_lowercase().contains("read-only"),
            "ref writes are refused too: {ref_write}"
        );
    }

    /// The main agent keeps full authority: nothing is subtracted unless
    /// `deny_git_writes` is called, and `Agent::new` calls it only for a
    /// delegated writer.
    #[test]
    fn a_plain_write_policy_subtracts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        assert!(
            policy.readonly_subpaths.is_empty(),
            "the parent commits; it must not be locked out of its own repository"
        );
    }

    /// A repo-less directory has nothing to subtract, and asking for the denial
    /// must not invent a path that does not exist (bwrap would fail the spawn on
    /// a missing bind source).
    #[test]
    fn denying_git_writes_without_a_repo_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        policy.deny_git_writes(dir.path());
        assert!(policy.readonly_subpaths.is_empty());
    }

    /// Every notice assertion below owns its own channel, so none of them can
    /// interleave with another — which is what the process-global cell used to
    /// need a test-only mutex for.
    fn notices() -> SandboxNotices {
        SandboxNotices::default()
    }

    /// Everything queued, in order, for the assertions that want the whole set.
    ///
    /// Only the Landlock degradation test queues more than one notice, and that
    /// test is Linux-only — so off Linux this is dead code, and `-D warnings`
    /// says so (it reached a tag that way once already).
    #[cfg(target_os = "linux")]
    fn drain(notices: &SandboxNotices) -> Vec<String> {
        std::iter::from_fn(|| notices.take()).collect()
    }

    /// Landlock really does block a write outside the roots.
    ///
    /// The backend is forced: this machine picks bwrap whenever it can, so the
    /// fallback would never run under `detect_backend`. The policy is a struct
    /// literal for the same reason as slice 3/5 — a `for_agent` policy makes
    /// `env::temp_dir()` writable and the "outside" tempdir with it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn landlock_blocks_writes_outside_roots() {
        if !std::fs::read_to_string("/sys/kernel/security/lsm")
            .unwrap_or_default()
            .contains("landlock")
        {
            return; // best-effort: exercise the real backend when available
        }
        let Some(shell) = crate::Shell::detect() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![canonicalize_nearest(dir.path())],
            readable_roots: Vec::new(),
            readonly_subpaths: Vec::new(),
        };

        let target = outside.path().join("escaped");
        let mut cmd = shell_command_with_backend(
            OsSandboxBackend::Landlock,
            shell,
            &format!("echo x > {}", target.display()),
            &policy,
            dir.path(),
            &notices(),
        );
        cmd.current_dir(dir.path());
        let out = cmd.output().await.unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(!out.status.success(), "the write was allowed: {stderr}");
        assert!(stderr.contains("Permission denied"), "{stderr}");
        assert!(!target.exists(), "the write landed anyway");

        // …and the cwd stays writable, or no agent could work at all.
        let inside = dir.path().join("mine");
        let mut cmd = shell_command_with_backend(
            OsSandboxBackend::Landlock,
            shell,
            &format!("echo x > {}", inside.display()),
            &policy,
            dir.path(),
            &notices(),
        );
        cmd.current_dir(dir.path());
        let out = cmd.output().await.unwrap();
        assert!(
            out.status.success(),
            "the cwd write was blocked: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(std::fs::read_to_string(&inside).unwrap().trim(), "x");
    }

    /// Landlock has no read axis, so a read-mode agent's shell commands are
    /// only write-confined — which must never be silent.
    #[cfg(target_os = "linux")]
    #[test]
    fn strict_mode_under_landlock_degrades_with_a_notice() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy {
            mode: SandboxMode::Strict,
            writable_roots: Vec::new(),
            readable_roots: vec![canonicalize_nearest(dir.path())],
            readonly_subpaths: Vec::new(),
        };
        let mine = notices();

        let _cmd = shell_command_with_backend(
            OsSandboxBackend::Landlock,
            crate::Shell::Bash,
            "true",
            &policy,
            dir.path(),
            &mine,
        );
        // Whatever else this host queued first (the reason bwrap was skipped,
        // on a machine that actually fell back), the read admission is there.
        let queued = drain(&mine);
        assert!(
            queued
                .iter()
                .any(|n| n == STRICT_DEGRADES_UNDER_LANDLOCK_NOTICE),
            "{queued:?}"
        );

        // A second agent hears the same thing on its own channel: "at most once"
        // is per agent, not per process.
        let sibling = notices();
        let _cmd = shell_command_with_backend(
            OsSandboxBackend::Landlock,
            crate::Shell::Bash,
            "true",
            &policy,
            dir.path(),
            &sibling,
        );
        let theirs = drain(&sibling);
        assert!(
            theirs
                .iter()
                .any(|n| n == STRICT_DEGRADES_UNDER_LANDLOCK_NOTICE),
            "a sibling agent must hear its own degradation: {theirs:?}"
        );
    }

    /// With no backend the command runs unconfined — allowed, but only ever
    /// once the user has been told, and only told once *to that agent*.
    #[test]
    fn no_backend_emits_the_not_confined_notice_once() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        let mine = notices();

        let _cmd = shell_command_with_backend(
            OsSandboxBackend::None,
            crate::Shell::Bash,
            "true",
            &policy,
            dir.path(),
            &mine,
        );
        assert_eq!(
            mine.take().as_deref(),
            Some(NO_OS_SANDBOX_NOTICE),
            "the first unconfined command must say so"
        );

        let _cmd = shell_command_with_backend(
            OsSandboxBackend::None,
            crate::Shell::Bash,
            "true",
            &policy,
            dir.path(),
            &mine,
        );
        assert_eq!(
            mine.take(),
            None,
            "the same notice must not repeat every command"
        );

        // The recurrence is what gets silenced, never the sibling: a second
        // agent running unconfined is told, whatever the first was told.
        let sibling = notices();
        let _cmd = shell_command_with_backend(
            OsSandboxBackend::None,
            crate::Shell::Bash,
            "true",
            &policy,
            dir.path(),
            &sibling,
        );
        assert_eq!(sibling.take().as_deref(), Some(NO_OS_SANDBOX_NOTICE));
    }

    #[test]
    fn sandbox_notice_is_take_once_per_agent() {
        let msg = "sandbox: test notice — take once".to_string();
        let mine = notices();
        mine.set(msg.clone());
        assert_eq!(mine.take().as_deref(), Some(msg.as_str()));
        assert_eq!(mine.take(), None);
        // The same message never notices twice to the same agent…
        mine.set(msg.clone());
        assert_eq!(mine.take(), None);
        // …and a sibling's queue knows nothing about any of that.
        let sibling = notices();
        sibling.set(msg.clone());
        assert_eq!(sibling.take().as_deref(), Some(msg.as_str()));
    }

    /// The notice texts are pinned bytes: they are what the user reads when the
    /// boundary quietly got weaker, and they name the fix.
    #[test]
    fn degradation_notices_say_what_was_lost() {
        assert_eq!(
            NO_OS_SANDBOX_NOTICE,
            "sandbox: no OS-level sandbox is available on this system — shell commands are NOT \
             OS-confined; the file tools remain guarded. Use --sandbox none to silence this."
        );
        #[cfg(target_os = "linux")]
        {
            assert_eq!(
                BWRAP_MISSING_NOTICE,
                "sandbox: bwrap not found — falling back to Landlock: writes are still confined, \
                 but reads are not, and strict-mode agents degrade to write-only confinement for \
                 shell commands. Install bubblewrap for full confinement."
            );
            assert_eq!(
                USERNS_DISABLED_NOTICE,
                "sandbox: unprivileged user namespaces are disabled on this system — falling back \
                 to Landlock: writes are still confined, but reads are not, and strict-mode \
                 agents degrade to write-only confinement for shell commands."
            );
            assert_eq!(
                STRICT_DEGRADES_UNDER_LANDLOCK_NOTICE,
                "sandbox: Landlock cannot confine reads — this strict-mode agent's shell \
                 commands are write-confined only, so paths outside its readable roots remain \
                 readable."
            );
        }
    }
}
