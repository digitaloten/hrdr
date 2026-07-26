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

/// Sandbox degradation notices awaiting delivery through the agent's event
/// stream, plus the set of notices already delivered.
///
/// The seen-set is the difference from `hrdr_llm::take_client_warning`'s plain
/// cell: a degradation is detected on *every* confined shell command, so a
/// bare slot would re-fill after each drain and the user would see the same
/// warning once per command. Each distinct message is delivered exactly once
/// per process.
///
/// Pending is a *queue* rather than a single slot because one command can
/// degrade twice — a read-mode agent on the Landlock fallback both loses its
/// primary backend and loses read confinement — and a single slot would
/// silently drop the first of the two while still marking it seen.
static SANDBOX_NOTICE: OnceLock<Mutex<(HashSet<String>, VecDeque<String>)>> = OnceLock::new();

fn notice_cell() -> &'static Mutex<(HashSet<String>, VecDeque<String>)> {
    SANDBOX_NOTICE.get_or_init(|| Mutex::new((HashSet::new(), VecDeque::new())))
}

/// Take the next pending sandbox notice for delivery through the caller's
/// normal event channel (never stderr — a TUI owns the terminal).
pub fn take_sandbox_notice() -> Option<String> {
    notice_cell()
        .lock()
        .ok()
        .and_then(|mut cell| cell.1.pop_front())
}

/// Emitted when a confined agent's shell command runs without any OS-level
/// confinement — no bwrap and no Landlock, a pre-Seatbelt macOS, or Windows.
/// Never silently pretend to sandbox: the file tools stay guarded, the shell
/// does not.
const NO_OS_SANDBOX_NOTICE: &str = "sandbox: no OS-level sandbox is available on this system — \
     shell commands are NOT OS-confined; the file tools remain guarded. Use --sandbox none to \
     silence this.";

/// Emitted when `bwrap(1)` is not installed and Landlock catches the fall.
const BWRAP_MISSING_NOTICE: &str = "sandbox: bwrap not found — falling back to Landlock: writes \
     are still confined, but reads are not, and read-mode agents degrade to write-mode \
     confinement for shell commands. Install bubblewrap for full confinement.";

/// Emitted when bwrap is installed but the kernel/distro forbids unprivileged
/// user namespaces, so it cannot build a mount namespace.
const USERNS_DISABLED_NOTICE: &str = "sandbox: unprivileged user namespaces are disabled on this \
     system — falling back to Landlock: writes are still confined, but reads are not, and \
     read-mode agents degrade to write-mode confinement for shell commands.";

/// Emitted in addition to the fallback notice when a *read-mode* agent runs a
/// shell command on Landlock: the ruleset confines writes only, so this agent
/// is quietly weaker than its mode claims — say so, loudly.
const READ_DEGRADES_UNDER_LANDLOCK_NOTICE: &str = "sandbox: Landlock cannot confine reads — this \
     read-only agent's shell commands are write-confined only.";

/// Record a degradation notice. Only a message seen for the first time this
/// process is queued; repeats are dropped.
pub fn set_sandbox_notice(msg: String) {
    if let Ok(mut cell) = notice_cell().lock()
        && cell.0.insert(msg.clone())
    {
        cell.1.push_back(msg);
    }
}

/// The OS mechanism available to confine *shell children* on this machine.
///
/// The file tools are guarded in-process regardless; this is only about the
/// subprocesses `shell` and `watch` spawn, which the software guard cannot see
/// inside of. bwrap is primary because it is the only one of the three that
/// can confine reads as well as writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsSandboxBackend {
    /// `bwrap(1)` — a mount namespace built per command (§3.6.1).
    Bwrap,
    /// Landlock LSM rules the child applies to itself. Writes only.
    Landlock,
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

    const TIMEOUT: Duration = Duration::from_millis(500);
    const POLL: Duration = Duration::from_millis(50);

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
    let deadline = Instant::now() + TIMEOUT;
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
        std::thread::sleep(POLL);
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

/// Every other platform: nothing yet. macOS gets Seatbelt in a later slice;
/// Windows stays software-layer-only.
#[cfg(not(target_os = "linux"))]
fn detect_backend_uncached() -> Detection {
    Detection {
        backend: OsSandboxBackend::None,
        degraded: Some(NO_OS_SANDBOX_NOTICE),
    }
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
/// directories that are not where the command should start.
pub fn sandboxed_shell_command(
    shell: crate::Shell,
    cmd_str: &str,
    policy: &SandboxPolicy,
    cwd: &Path,
) -> tokio::process::Command {
    if policy.mode == SandboxMode::None {
        return shell.command(cmd_str);
    }
    shell_command_with_backend(detect_backend(), shell, cmd_str, policy, cwd)
}

/// [`sandboxed_shell_command`] with the backend chosen for it.
///
/// Split out so the fallback arms are reachable on a machine whose detection
/// would never pick them: on a host with a working bwrap, Landlock is dead
/// code that no test could otherwise execute.
///
/// Every arm that ends up running a command with less confinement than the
/// mode asks for sets its §5 notice *first* — the one rule this layer may
/// never break is pretending to sandbox.
fn shell_command_with_backend(
    backend: OsSandboxBackend,
    shell: crate::Shell,
    cmd_str: &str,
    policy: &SandboxPolicy,
    cwd: &Path,
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
                set_sandbox_notice(why.to_string());
            }
            // Landlock has no read axis, so a read-mode agent gets write
            // confinement and an explicit admission of the gap.
            if policy.mode == SandboxMode::Read {
                set_sandbox_notice(READ_DEGRADES_UNDER_LANDLOCK_NOTICE.to_string());
            }
            landlock_command(shell, cmd_str, policy)
        }
        // `Landlock` is unreachable off Linux (detection never returns it),
        // but the variant exists on every platform, so the arm must too.
        #[cfg(not(target_os = "linux"))]
        OsSandboxBackend::Landlock => {
            set_sandbox_notice(NO_OS_SANDBOX_NOTICE.to_string());
            shell.command(cmd_str)
        }
        OsSandboxBackend::None => {
            set_sandbox_notice(NO_OS_SANDBOX_NOTICE.to_string());
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
    // SAFETY: the closure runs in the forked child before `exec`. It issues
    // landlock/prctl syscalls and builds the ruleset from data moved in
    // beforehand; it shares no lock, handle, or global with the parent, and it
    // never spawns a thread.
    unsafe {
        cmd.pre_exec(move || install_landlock_rules(&writable));
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
fn install_landlock_rules(writable_roots: &[PathBuf]) -> std::io::Result<()> {
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
        // Codex calls this `set_no_new_privs`, deprecated since its pin.
        .no_new_privs(true);

    if !writable_roots.is_empty() {
        ruleset = ruleset
            .add_rules(path_beneath_rules(writable_roots, access_rw))
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
        SandboxMode::Write => {
            // Everything readable, nothing writable…
            push(
                &mut args,
                &["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"],
            );
            // …then punch the writable roots through, in policy order.
            for root in policy.writable_roots.iter().filter(|root| root.exists()) {
                bind(&mut args, "--bind", root);
            }
        }
        SandboxMode::Read => {
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
    });
    ctx
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
        };
        check_write(&policy, &common.join("index")).unwrap_err();
        check_write(&policy, &common.join("refs").join("heads").join("main")).unwrap_err();
        check_write(&policy, &common.join("objects").join("aa").join("bb")).unwrap();
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

    #[test]
    fn bwrap_read_args_omit_rw_binds_and_private_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Read, dir.path(), &[]);
        let args = argv(&bwrap_args(
            SandboxMode::Read,
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

    /// The point of the whole feature: a sub-agent can commit its own work in
    /// its worktree, and cannot commit to the parent repo — the escape that
    /// started this. Only the §3.3 metadata roots are writable, never the
    /// parent `.git` itself.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn worktree_commit_succeeds_but_parent_commit_is_blocked() {
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
    async fn read_mode_cannot_even_see_outside_paths() {
        let Some(shell) = bwrap_shell() else { return };
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("visible.txt"), "x").unwrap();
        let ctx = confined_ctx(dir.path(), SandboxMode::Read);

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
        let ctx = confined_ctx(dir.path(), SandboxMode::Write);
        let marker = dir.path().join("grandchild-finished");

        let command = format!("(sleep 5 && touch {m}) & sleep 5", m = marker.display());
        use crate::Tool as _;
        let out = crate::ShellTool::new(shell)
            .execute(
                serde_json::json!({"command": command, "timeout_secs": 1}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.contains("timed out"), "{out}");

        // Well past the grandchild's own sleep: if it were alive it would
        // have touched the marker by now.
        tokio::time::sleep(std::time::Duration::from_millis(5500)).await;
        assert!(
            !marker.exists(),
            "the backgrounded grandchild survived the group kill"
        );
    }

    /// The notice cell is process-global, so the tests that assert on its
    /// exact contents must not interleave with each other.
    fn notice_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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
        };

        let target = outside.path().join("escaped");
        let mut cmd = shell_command_with_backend(
            OsSandboxBackend::Landlock,
            shell,
            &format!("echo x > {}", target.display()),
            &policy,
            dir.path(),
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
    fn read_mode_under_landlock_degrades_with_a_notice() {
        let _guard = notice_lock();
        while take_sandbox_notice().is_some() {}
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy {
            mode: SandboxMode::Read,
            writable_roots: Vec::new(),
            readable_roots: vec![canonicalize_nearest(dir.path())],
        };

        let _cmd = shell_command_with_backend(
            OsSandboxBackend::Landlock,
            crate::Shell::Bash,
            "true",
            &policy,
            dir.path(),
        );
        // Whatever else this host queued first (the reason bwrap was skipped,
        // on a machine that actually fell back), the read admission is there.
        let mut notices = Vec::new();
        while let Some(n) = take_sandbox_notice() {
            notices.push(n);
        }
        assert!(
            notices
                .iter()
                .any(|n| n == READ_DEGRADES_UNDER_LANDLOCK_NOTICE),
            "{notices:?}"
        );
    }

    /// With no backend the command runs unconfined — allowed, but only ever
    /// once the user has been told, and only told once.
    #[test]
    fn no_backend_emits_the_not_confined_notice_once() {
        let _guard = notice_lock();
        while take_sandbox_notice().is_some() {}
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);

        let _cmd = shell_command_with_backend(
            OsSandboxBackend::None,
            crate::Shell::Bash,
            "true",
            &policy,
            dir.path(),
        );
        assert_eq!(
            take_sandbox_notice().as_deref(),
            Some(NO_OS_SANDBOX_NOTICE),
            "the first unconfined command must say so"
        );

        let _cmd = shell_command_with_backend(
            OsSandboxBackend::None,
            crate::Shell::Bash,
            "true",
            &policy,
            dir.path(),
        );
        assert_eq!(
            take_sandbox_notice(),
            None,
            "the same notice must not repeat every command"
        );
    }

    #[test]
    fn sandbox_notice_is_take_once() {
        let _guard = notice_lock();
        while take_sandbox_notice().is_some() {}
        let msg = "sandbox: test notice — take once".to_string();
        set_sandbox_notice(msg.clone());
        assert_eq!(take_sandbox_notice().as_deref(), Some(msg.as_str()));
        assert_eq!(take_sandbox_notice(), None);
        // The same message never notices twice per process.
        set_sandbox_notice(msg);
        assert_eq!(take_sandbox_notice(), None);
    }
}
