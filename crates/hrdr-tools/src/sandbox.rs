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
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    /// Canonicalized (via [`canonicalize_nearest`]) writable roots. Empty when
    /// mode is `None` (meaning "everything") or `Read` (meaning "nothing").
    pub writable_roots: Vec<PathBuf>,
    /// Canonicalized readable roots; only consulted in `Read` mode.
    pub readable_roots: Vec<PathBuf>,
    /// Whether this agent's shell children may reach the network. True for
    /// every agent unless [`deny_network`](Self::deny_network) says otherwise —
    /// which `Agent::new` says only for a delegated one.
    ///
    /// Purely about *shell children*: hrdr's own `web_fetch`/`web_search` run
    /// in the parent process and are untouched by it, which is what makes the
    /// denial affordable (see [`deny_network`](Self::deny_network)).
    pub allow_network: bool,
    /// Which of [`writable_roots`](Self::writable_roots) are package-manager
    /// caches ([`package_cache_roots`]).
    ///
    /// A **rendering label, never a boundary**: every path here is also in
    /// `writable_roots`, and enforcement reads only that. The duplication is
    /// deliberate — a separate set the OS layer had to remember to consult is one
    /// forgotten call away from a hole, whereas a label nobody consults for
    /// permission cannot open one.
    ///
    /// It exists because these roots are machinery, not choices. Two dozen cache
    /// paths in the system prompt's "you may write only under" list is noise the
    /// model has to read on every turn, and the model never decides to write
    /// there — `cargo` and `npm` do. So prompts and refusals name
    /// [`project_writable_roots`](Self::project_writable_roots) and summarize the
    /// rest in one clause.
    pub cache_roots: Vec<PathBuf>,
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
            allow_network: true,
            cache_roots: Vec::new(),
        }
    }

    /// Build the policy for an agent working in `cwd`.
    ///
    /// Writable (mode `Write` only): `cwd`, [`std::env::temp_dir`],
    /// [`session_scratch_dir`], [`tool_output_dir`], the git metadata roots a
    /// linked worktree needs to commit (see [`git_metadata_roots`]), the
    /// package-manager caches ([`package_cache_roots`]), then the caller's
    /// configured `extras`. Every root is run through [`canonicalize_nearest`]
    /// and deduped (a root already under an earlier root is dropped).
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
        let (writable_roots, cache_roots) =
            if matches!(mode, SandboxMode::Read | SandboxMode::Strict) {
                (Vec::new(), Vec::new())
            } else {
                let caches = package_cache_roots();
                let mut roots = vec![cwd.to_path_buf(), std::env::temp_dir(), scratch, output];
                roots.extend(git_metadata_roots(cwd));
                roots.extend(caches.iter().cloned());
                roots.extend(extras.iter().filter(|p| p.exists()).cloned());
                let roots = canonical_roots(roots);
                // Labelled *after* canonicalization and intersected with what
                // survived it, so a cache root that a broader root swallowed (a
                // session whose cwd is `$HOME`) is not claimed as a separate root
                // the prompt then omits.
                let caches = canonical_roots(caches)
                    .into_iter()
                    .filter(|c| roots.contains(c))
                    .collect();
                (roots, caches)
            };
        Self {
            mode,
            writable_roots,
            readable_roots,
            allow_network: true,
            cache_roots,
        }
    }

    /// The writable roots worth naming to a human or a model: everything except
    /// the package-manager caches (see [`cache_roots`](Self::cache_roots)).
    ///
    /// Pair it with [`cache_roots_clause`](Self::cache_roots_clause), which says
    /// in one clause what this omits.
    pub fn project_writable_roots(&self) -> Vec<&Path> {
        self.writable_roots
            .iter()
            .filter(|root| !self.cache_roots.iter().any(|c| c == *root))
            .map(PathBuf::as_path)
            .collect()
    }

    /// One clause naming the caches [`project_writable_roots`] leaves out, or
    /// empty when none were granted. Written to be appended to a sentence.
    pub fn cache_roots_clause(&self) -> &'static str {
        if self.cache_roots.is_empty() {
            ""
        } else {
            ", plus this machine's package-manager caches (cargo, npm, pip, go, … \
             — so dependency fetches and builds work without asking)"
        }
    }

    /// Cut this agent's shell children off the network: no socket a command it
    /// spawns opens can leave the machine.
    ///
    /// Installed for **sub-agents**, and the reason it costs them nothing is
    /// that a delegated agent's legitimate network needs are already served by
    /// tools that do not go through here. `web_fetch` and `web_search` run
    /// in-process in the hrdr parent, whose sockets this never touches — so a
    /// sub-agent that has to read a page or search still can. What is left is
    /// raw network from a shell command: exfiltration surface with no matching
    /// use, on an agent whose whole job is to change files in a directory it was
    /// handed.
    ///
    /// The main agent keeps the network, deliberately: it is the one that runs
    /// `git push`/`pull`/`fetch`, and those are exactly the network operations
    /// the delegation model reserves to the parent — the same split
    /// [`deny_git_writes`](Self::deny_git_writes) makes for history.
    ///
    /// Mode `None` is left alone for the same reason it is there: no OS wrapper
    /// runs at all, so a policy claiming no network would be describing a
    /// boundary nothing enforces.
    pub fn deny_network(&mut self) {
        if self.mode == SandboxMode::None {
            return;
        }
        self.allow_network = false;
    }

    /// Err unless `canon` (already run through [`canonicalize_nearest`]) is under
    /// a writable root. `shown` is the path as the model named it, so the refusal
    /// talks about what it asked for.
    ///
    /// The question is answered on the *canonical* path, which resolves symlinks
    /// and lexical `..` — that is what makes the check escape-proof rather than
    /// textual.
    ///
    /// Mode `None` answers nothing: the unconfined path stays byte-identical to
    /// the pre-sandbox behavior (see the
    /// [`ToolContext::new`](crate::ToolContext::new) rule).
    ///
    /// This guards the **model's file tools** and nothing else — `shell` does not
    /// come through here. That asymmetry is why there is no longer a `.git`
    /// carve-out on top of the root check: it refused the file tools a write that
    /// `shell` performed one `git config` away, so it stopped the honest path and
    /// nothing else, while refusing legitimate `.git/info/exclude` edits and hooks
    /// the user had asked for. Oversight of git belongs at the shell layer, where
    /// guardrails run.
    pub fn check_write(&self, canon: &Path, shown: &Path) -> anyhow::Result<()> {
        if self.mode == SandboxMode::None {
            return Ok(());
        }
        if !is_under_any(canon, &self.writable_roots) {
            anyhow::bail!(
                "sandbox: refusing to write {} — it is outside this agent's writable roots. \
                 You may write only under: {}{}. Keep work inside your working directory; \
                 use the scratch dir for throwaway files.",
                shown.display(),
                join_paths(&self.project_writable_roots()),
                self.cache_roots_clause()
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

/// The roots as the refusal messages list them.
fn join_roots(roots: &[PathBuf]) -> String {
    roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// [`join_roots`] for a borrowed set — what
/// [`SandboxPolicy::project_writable_roots`] returns.
fn join_paths(paths: &[&Path]) -> String {
    paths
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

/// Home directory, cross-platform (`$HOME`, else `%USERPROFILE%`). `None` in an
/// environment with neither, where every home-relative default below drops out.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// `$var` as a directory, if it is set to a non-empty absolute path.
///
/// Absolute on purpose: a relative `CARGO_HOME` would resolve against whatever
/// cwd this process happens to have, which is not what the tool that reads it
/// will resolve it against.
fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// `$var` if set (see [`env_dir`]), else `<home>/<fallback>`.
///
/// **Resolving the override is not a nicety.** A hardcoded `~/.cargo/registry`
/// on a machine with `CARGO_HOME=/opt/cargo` grants nothing at all, and the
/// build then fails with exactly the confusing EROFS the grant exists to
/// prevent — silently, because the default *looks* present.
fn tool_home(var: &str, fallback: &str, home: Option<&Path>) -> Option<PathBuf> {
    env_dir(var).or_else(|| home.map(|h| h.join(fallback)))
}

/// Directories a package manager must be able to write for ordinary project
/// work — `cargo build`, `npm i`, `go build`, `mvn`, `pip install` — to succeed
/// under [`SandboxMode::Write`].
///
/// **The common case must work out of the box.** `sandbox_writable_roots` and
/// `--sandbox-writable-root` are the escape hatch for a bespoke layout, not the
/// mechanism by which mainstream tooling becomes usable. Two failures verified
/// under a sandbox with only cwd/temp/scratch/output writable:
///
/// ```text
/// error: failed to open `~/.cargo/registry/cache/…/anyhow-1.0.75.crate`
/// Caused by: Read-only file system (os error 30)
/// ```
/// ```text
/// npm error code EROFS
/// npm error path /home/…/.npm/_cacache/tmp/0b23206c
/// ```
///
/// Note *where* cargo fails: the download succeeded, and it died writing the
/// crate into the cache. A build whose dependencies happen to be cached passes,
/// so this works on a warm machine and fails on a cold one — or the first time a
/// dependency is added. The npm case is [`sandbox_denial`]'s founding incident
/// reproduced exactly.
///
/// One cross-cutting entry does most of the work — `$XDG_CACHE_HOME`, plus
/// `~/Library/Caches` on macOS — covering pip, uv, deno, `go-build`, yarn v1,
/// composer, node-gyp and cabal. The rest of the list is the non-XDG holdouts.
///
/// **Never grant a tool's home directory, only its cache.** Verified, not
/// assumed: `~/.local/share/uv/` holds `credentials/` beside its data,
/// `~/.nuget/` holds config beside `packages/`, `~/.cargo/credentials.toml` is
/// commonly a symlink to a secret store, and `~/.m2/settings.xml`,
/// `~/.gradle/gradle.properties`, `~/.gem/credentials`, `~/.bundle/config` and
/// `~/.composer/auth.json` are all credential-bearing. `~/.npm` is the one safe
/// whole grant (`_cacache`, `_logs`, `_npx`, `_prebuilds`; config lives in
/// `~/.npmrc`, outside it) — worth saying so, so nobody tidies the list into
/// symmetry.
///
/// **Deliberately excluded: anything that puts a binary on `PATH`** —
/// `$CARGO_HOME/bin`, `$GOPATH/bin`, `~/.local/bin`, `~/.bun/bin`, and
/// `$PNPM_HOME` itself (which is pnpm's global *bin* dir; only its `store`
/// subdirectory is granted). A binary on `PATH` is a persistence vector: the next
/// command the *user* runs could be the agent's. So `cargo install` and
/// `go install` fail by default, with [`sandbox_denial`] naming the flag —
/// installing a tool is machine setup, not project work. Language toolchain
/// managers (`~/.nvm`, `~/.pyenv`, `~/.rbenv`, `~/.asdf`, uv's managed pythons)
/// are out for the same reason.
///
/// `$RUSTUP_HOME/toolchains` is the deliberate exception: a `rust-toolchain.toml`
/// pinning an uninstalled version makes `cargo build` itself fail on a fresh
/// checkout, which is project work. The download is checksum-verified and those
/// binaries are not on `PATH` (the rustup shims in `$CARGO_HOME/bin` are, and
/// stay excluded). `settings.toml` stays out, so the default toolchain cannot be
/// switched.
///
/// **The risk being accepted, stated.** Permanently writable caches escape the
/// project boundary durably: poison `~/.cargo/registry` and builds in *other*
/// projects are affected, including ones the user later runs by hand. What blunts
/// it enough to accept is that both caches are content-addressed and
/// integrity-checked — cargo verifies `.crate` files against the index checksum
/// before extraction, npm's `_cacache` is keyed by integrity hash — so writing
/// garbage there fails verification rather than executing. And an agent with
/// `shell`, a network and a writable cwd can already add a dependency whose
/// `build.rs` does anything, so this is a second route to something already
/// reachable, not a new capability.
pub fn package_cache_roots() -> Vec<PathBuf> {
    let home = home_dir();
    let home = home.as_deref();
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut add = |p: Option<PathBuf>| {
        if let Some(p) = p {
            roots.push(p);
        }
    };
    let under = |dir: &str| home.map(|h| h.join(dir));
    // A cache that IS the tool's home directory (`~/.npm`, `~/.stack`) has no
    // parent marker to test, so its ecosystem is detected on `PATH` instead —
    // see [`ensure_cache_root`] for why the two rules differ.
    let installed = |cmd: &str, p: Option<PathBuf>| p.filter(|_| command_on_path(cmd));

    // Cross-cutting.
    add(tool_home("XDG_CACHE_HOME", ".cache", home));
    if cfg!(target_os = "macos") {
        add(under("Library/Caches"));
    }

    // Rust.
    let cargo = tool_home("CARGO_HOME", ".cargo", home);
    add(cargo.as_ref().map(|c| c.join("registry")));
    add(cargo.as_ref().map(|c| c.join("git")));
    let rustup = tool_home("RUSTUP_HOME", ".rustup", home);
    for sub in ["toolchains", "downloads", "tmp", "update-hashes"] {
        add(rustup.as_ref().map(|r| r.join(sub)));
    }

    // Node.
    add(installed("npm", under(".npm")));
    add(installed("node", under(".node-gyp")));
    add(env_dir("PNPM_HOME").map(|p| p.join("store")));
    add(under(".local/share/pnpm/store"));
    add(under("Library/pnpm/store"));
    add(installed("pnpm", under(".pnpm-store")));
    add(under(".yarn/berry/cache"));
    add(under(".bun/install/cache"));
    add(env_dir("DENO_DIR"));

    // Python.
    add(env_dir("UV_CACHE_DIR"));
    add(env_dir("PIP_CACHE_DIR"));
    add(under(".local/share/pypoetry/venvs"));
    add(under(".local/share/pipx"));

    // Go.
    add(env_dir("GOCACHE"));
    add(env_dir("GOMODCACHE").or_else(|| {
        env_dir("GOPATH")
            .or_else(|| under("go"))
            .map(|p| p.join("pkg").join("mod"))
    }));

    // JVM.
    add(under(".m2/repository"));
    let gradle = tool_home("GRADLE_USER_HOME", ".gradle", home);
    add(gradle.as_ref().map(|g| g.join("caches")));
    add(gradle.as_ref().map(|g| g.join("wrapper")));

    // .NET.
    add(env_dir("NUGET_PACKAGES").or_else(|| under(".nuget/packages")));

    // Ruby.
    add(under(".local/share/gem"));
    add(under(".gem/ruby"));
    add(under(".bundle/cache"));

    // PHP — the default is XDG; only an override needs naming.
    add(env_dir("COMPOSER_HOME").map(|c| c.join("cache")));

    // Dart.
    add(env_dir("PUB_CACHE").or_else(|| installed("dart", under(".pub-cache"))));

    // Elixir.
    add(under(".hex/packages"));
    add(installed("mix", under(".mix")));

    // Haskell.
    add(env_dir("STACK_ROOT").or_else(|| installed("stack", under(".stack"))));
    add(under(".cabal/packages"));

    roots.retain(|root| ensure_cache_root(root));
    roots
}

/// Whether `root` is a usable grant: it exists, or this created it.
///
/// **Creating it is what makes the grant real.** The OS layer can only confine a
/// path that exists — Landlock resolves a rule by opening it — so an absent root
/// is silently dropped, and the package manager cannot create it either, because
/// its parent is not writable. On a fresh machine `~/.npm` does not exist yet, so
/// the first `npm i` would fail with the same EROFS as if nothing had been
/// granted, *despite* the default being present.
///
/// Only when the immediate parent already exists, which is the line between
/// completing a layout and inventing one: `~/.cargo` exists exactly when cargo is
/// installed, so `~/.cargo/registry` is created on a machine that builds Rust and
/// skipped on one that never will. Without that, hrdr would scatter two dozen
/// empty package-manager directories through the home of anyone who runs it once.
/// The cost is bounded and named: a tool installed but never yet run — `mvn` with
/// no `~/.m2` — fails its first fetch with [`sandbox_denial`] pointing at
/// `--sandbox-writable-root`.
///
/// The rule needs a companion for the caches that ARE a tool's home directory
/// (`~/.npm`, `~/.stack`): their parent is `$HOME`, which always exists, so
/// parent-existence proves nothing and [`package_cache_roots`] gates those on the
/// tool being on `PATH` instead.
///
/// Failures are ignored, so a read-only `$HOME` degrades rather than aborting.
fn ensure_cache_root(root: &Path) -> bool {
    if root.is_dir() {
        return true;
    }
    if root.parent().is_some_and(Path::is_dir) {
        let _ = std::fs::create_dir_all(root);
    }
    root.is_dir()
}

/// Whether `cmd` is an executable file on `PATH` — a `which(1)` with no
/// subprocess, used to decide whether an ecosystem exists on this machine.
///
/// Deliberately does not consult a shell: aliases and functions are not what a
/// package manager's own child process will find, and spawning one per lookup at
/// session start would cost more than every stat here combined.
fn command_on_path(cmd: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(cmd);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::metadata(&candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        {
            // No mode bits to test; PATHEXT-suffixed names are what exists.
            candidate.is_file()
                || ["exe", "cmd", "bat"]
                    .iter()
                    .any(|ext| candidate.with_extension(ext).is_file())
        }
    })
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

/// Emitted when an agent whose policy denies the network runs a shell command on
/// Landlock: the ruleset denies TCP `bind`/`connect` and nothing else, so UDP —
/// DNS and QUIC/HTTP3 with it — raw sockets and ICMP still leave the machine.
///
/// bwrap's `--unshare-net` has no such hole, which is why this is a *degradation*
/// notice and not a description of the feature. Said out loud on the same
/// principle as the notice above: a boundary that is quietly narrower than it
/// claims is worse than one whose limits are stated.
#[cfg(target_os = "linux")]
const NETWORK_PARTIAL_UNDER_LANDLOCK_NOTICE: &str = "sandbox: Landlock can deny only TCP \
     bind/connect — this agent's shell commands are cut off from HTTP(S), git and ssh, but UDP \
     (DNS, QUIC/HTTP3) and raw sockets are not blocked on this backend. Install bubblewrap for a \
     complete network denial.";

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
    if landlock_available() {
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

/// Whether this kernel has the Landlock LSM enabled — the authoritative answer
/// being the list of active LSMs, not a probe.
#[cfg(target_os = "linux")]
fn landlock_available() -> bool {
    std::fs::read_to_string("/sys/kernel/security/lsm")
        .unwrap_or_default()
        .contains("landlock")
}

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

/// Which confinement produced a failure — the machine-readable half of
/// [`sandbox_denial`], so a caller can decide what to *do* about it rather than
/// only what to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenialKind {
    /// A GPU node missing under `strict`.
    GpuStrict,
    /// ssh refusing a config file the user namespace made look root-less.
    SshUserNamespace,
    /// A shell cut off the network (`deny_network`).
    NetworkDenied,
    /// An ordinary EROFS: a write outside this agent's roots.
    WriteOutsideRoots,
}

/// A recognized sandbox denial: what it was, and the note explaining it.
#[derive(Debug, Clone)]
pub struct SandboxDenial {
    pub kind: DenialKind,
    /// The text appended to the command's output, leading newlines included.
    pub note: String,
}

/// The note alone, for callers that only report.
pub fn sandbox_denial_note(policy: &SandboxPolicy, output: &str) -> Option<String> {
    sandbox_denial(policy, output).map(|denial| denial.note)
}

/// Recognize a failure the sandbox caused, and say both what it was and why.
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
/// Every arm is deliberately narrow: only `EROFS`/"read-only file system"
/// triggers the write case, because those are all but unheard of on a
/// developer's box outside a sandbox, whereas a bare "Permission denied" is a
/// normal error this must not editorialize over. `None` when unconfined, or when
/// nothing in the output matches.
pub fn sandbox_denial(policy: &SandboxPolicy, output: &str) -> Option<SandboxDenial> {
    fn denial(kind: DenialKind, note: String) -> Option<SandboxDenial> {
        Some(SandboxDenial { kind, note })
    }
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
        return denial(
            DenialKind::GpuStrict,
            "\n\n[sandbox] `strict` mode does not bind GPU devices (`/dev/kfd`, `/dev/dri`, \
             `/dev/nvidia*`), so a card that exists on this host is invisible in here. This is \
             not a machine without a GPU and not a broken driver — it is the confinement this \
             mode asks for. `write` and `read` mode both pass the devices through; if this work \
             needs the GPU, say so rather than reporting the hardware as absent."
                .to_string(),
        );
    }
    // OpenSSH refusing a config file it cannot vouch for. Not a permissions
    // problem on disk, and the obvious "fix" — chmod'ing a system file — is a
    // real change made for a false reason, so say what is actually happening.
    if lower.contains("bad owner or permissions") {
        return denial(
            DenialKind::SshUserNamespace,
            "\n\n[sandbox] that is the OS sandbox's user namespace, not a broken file. It maps \
             only your uid, so root-owned files (like `/etc/ssh/ssh_config`) read as `nobody` \
             inside here, and ssh refuses any config file it cannot vouch for. The file on disk \
             is fine — do NOT chmod or chown it. `git` over ssh already works (hrdr points it at \
             `ssh -F`, which skips the system config); for a bare `ssh`, pass `-F ~/.ssh/config` \
             (or `-F /dev/null`) yourself."
                .to_string(),
        );
    }
    // A shell cut off the network (a sub-agent — see `deny_network`). Nothing in
    // the failure says "sandbox": the child sits in an empty network namespace,
    // so the resolver times out or the connect finds no route, and curl, git,
    // cargo, npm and pip all report that as a name it could not resolve or an
    // unreachable network. That is what a machine with a dead link looks like,
    // and a model that believes it starts debugging DNS or declaring the host
    // offline in its report.
    //
    // Narrow on purpose, like the EROFS case below: only failures that name
    // resolution or routing produce, never a bare "connection refused" or
    // "permission denied" — those are ordinary errors on a working machine
    // (a service that is not up yet, a file that is not yours) and a note
    // asserting the sandbox over them would be wrong far more often than right.
    if !policy.allow_network
        && [
            "could not resolve host",
            "could not resolve proxy",
            "temporary failure in name resolution",
            "name or service not known",
            "nodename nor servname provided",
            "network is unreachable",
            "no route to host",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return denial(
            DenialKind::NetworkDenied,
            "\n\n[sandbox] that is this agent's network being denied, not a broken resolver or an \
             offline machine — a delegated agent's shell commands have no network at all, \
             deliberately. Do NOT retry it, edit `/etc/resolv.conf`, or report the host as \
             offline. What still works: `web_fetch` to read a URL and `web_search` to search, \
             both of which run outside this sandbox and are the right way to reach the network \
             from here. Anything genuinely needing a network shell command — cloning a repo, \
             installing a dependency, pushing — belongs to the agent that delegated to you: say \
             so in your report and let it run."
                .to_string(),
        );
    }
    if !lower.contains("read-only file system") && !lower.contains("erofs") {
        return None;
    }
    let where_writable = if policy.writable_roots.is_empty() {
        "nothing is writable for this agent (read-only mode)".to_string()
    } else {
        format!(
            "writable here: {}{}",
            join_paths(&policy.project_writable_roots()),
            policy.cache_roots_clause(),
        )
    };
    denial(
        DenialKind::WriteOutsideRoots,
        format!(
            "\n\n[sandbox] the \"read-only file system\" above is hrdr's sandbox refusing a write \
         outside this agent's roots — {where_writable}. The program is installed and working; \
         it tried to write somewhere it may not. If it was a package runner fetching a tool \
         (`npx`, `uvx`, `pipx`), run the copy already on PATH instead of downloading one. If the \
         write is genuinely needed, say so and name the directory — the user can allow it with \
         `sandbox_writable_roots` in the config or `--sandbox-writable-root <PATH>` on the \
         command line, and they can run the command themselves with `!<command>` — but do not \
         report the tool as missing or broken."
        ),
    )
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
            if let Some(ssh) = git_ssh_command_for_userns() {
                cmd.env("GIT_SSH_COMMAND", ssh);
            }
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
            // Same admission for the other axis this backend cannot fully carry:
            // the ruleset reaches TCP and stops there (see
            // `install_landlock_rules`), so a denial that is absolute under bwrap
            // is partial here.
            if !policy.allow_network {
                notices.set(NETWORK_PARTIAL_UNDER_LANDLOCK_NOTICE.to_string());
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
    let allow_network = policy.allow_network;
    // SAFETY: the closure runs in the forked child before `exec`. It issues
    // landlock/prctl syscalls and builds the ruleset from data moved in
    // beforehand; it shares no lock, handle, or global with the parent, and it
    // never spawns a thread.
    unsafe {
        cmd.pre_exec(move || install_landlock_rules(&writable, allow_network));
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
///
/// `allow_network` false handles [`landlock::AccessNet`] and grants no
/// [`landlock::NetPort`] rule, which is how a Landlock ruleset says "none": ABI
/// v4 added exactly two network rights, TCP `bind` and TCP `connect`, and v5
/// adds none. That covers the traffic anyone actually leaves with — HTTP(S),
/// git, ssh, every package registry — but it is **not** the whole network, and
/// the gap is admitted rather than papered over: UDP (so DNS, and QUIC/HTTP3),
/// raw and ICMP sockets, and anything already-connected are outside what
/// Landlock can express, so this backend confines the network less than bwrap's
/// `--unshare-net` does. [`NETWORK_PARTIAL_UNDER_LANDLOCK_NOTICE`] tells the
/// agent so.
#[cfg(target_os = "linux")]
fn install_landlock_rules(writable_roots: &[PathBuf], allow_network: bool) -> std::io::Result<()> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus, path_beneath_rules,
    };

    let abi = ABI::V5;
    let access_rw = AccessFs::from_all(abi);
    let access_ro = AccessFs::from_read(abi);

    let mut base = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(access_rw)
        .map_err(std::io::Error::other)?;
    // Handled but never granted to any port: a right the ruleset handles and no
    // rule allows is denied outright. Only when the network is denied — handling
    // the right and then allowing every port would be the same permission at
    // twice the cost, and on a pre-6.7 kernel `BestEffort` would drop it anyway.
    if !allow_network {
        base = base
            .handle_access(AccessNet::from_all(abi))
            .map_err(std::io::Error::other)?;
    }

    let mut ruleset = base
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
    let status = ruleset.restrict_self().map_err(std::io::Error::other)?;
    if status.ruleset == RulesetStatus::NotEnforced {
        // Half-confined is not confined: refuse the spawn instead.
        return Err(std::io::Error::other("landlock not enforced"));
    }
    Ok(())
}

/// `GIT_SSH_COMMAND` that survives bwrap's user namespace, or `None` when the
/// caller already set one.
///
/// **The problem.** Unprivileged bwrap has to create a user namespace, and one
/// maps only the invoking uid — so every root-owned file inside it reads as uid
/// 65534 (`nobody`). OpenSSH validates its config files' ownership:
///
/// ```c
/// if (((sb.st_uid != 0 && sb.st_uid != getuid()) || (sb.st_mode & 022) != 0))
///     fatal("Bad owner or permissions on %s", filename);
/// ```
///
/// 65534 is neither, so `/etc/ssh/ssh_config` (and anything it `Include`s) is
/// refused and ssh dies before connecting. Nothing is wrong on disk: the file is
/// `root:root 0644` and reads correctly outside the sandbox. The effect is that
/// **every `git push`/`fetch`/`clone` over ssh fails inside the sandbox**, with
/// an error that points at a system file and invites the user to `chmod` it —
/// which would not help and is a real permissions change made for a false
/// reason. This is not fixable by dropping `--unshare-user` (bwrap creates the
/// namespace regardless when unprivileged) or by mapping root (that needs a
/// privileged helper).
///
/// **The fix.** `ssh -F <file>` makes ssh ignore the system-wide config
/// entirely, per ssh(1) — so the unreadable-looking files are never opened. The
/// user's own `~/.ssh/config` is owned by the invoking uid, which maps to itself,
/// so it still passes the check and their Host aliases and identities survive.
/// Without one, `/dev/null` gives ssh its compiled-in defaults.
///
/// Not set when the caller already exported `GIT_SSH_COMMAND`: an explicit
/// setting is a decision, and silently rewriting it would be worse than the bug.
/// Only `git` is covered — a bare `ssh` in a shell command still hits this, which
/// is what [`sandbox_denial_note`] explains when it does.
fn git_ssh_command_for_userns() -> Option<std::ffi::OsString> {
    if std::env::var_os("GIT_SSH_COMMAND").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let user_config = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".ssh").join("config"))
        .filter(|p| p.is_file());
    let path = match &user_config {
        Some(p) => p.to_string_lossy().into_owned(),
        None => "/dev/null".to_string(),
    };
    // Quoted: git splits this value with shell rules, so a `$HOME` containing a
    // space would otherwise arrive as two arguments.
    Some(format!("ssh -F {}", shell_words::quote(&path)).into())
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
    // The namespace flags are the one place argv order carries no meaning —
    // bwrap unshares everything it was asked for in one go, before it lays down
    // a single mount — so this belongs with its siblings rather than woven into
    // the mount sequence above. `--unshare-net` leaves the child a fresh network
    // namespace with nothing but its own loopback: no route off the machine, and
    // no reach into a service listening on the host's loopback either.
    if !policy.allow_network {
        push(&mut args, &["--unshare-net"]);
    }
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
/// needs plus the readable roots, and grants no writes at all. Network is
/// allowed unless the policy denies it ([`SandboxPolicy::deny_network`]), in
/// which case the `(allow network*)` line is simply absent and `(deny default)`
/// answers instead.
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
    // Omission IS the denial here: the profile opens with `(deny default)`, so
    // an operation nothing allows is already refused, and an explicit
    // `(deny network*)` would add a line that changes no decision. Unlike the
    // `.git` case above there is no earlier `allow` to subtract from — that one
    // needs its trailing `deny` precisely because SBPL is last-match-wins and
    // `(allow file-write* …)` came first.
    if policy.allow_network {
        profile.push_str("(allow network*)\n");
    }
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
        allow_network: true,
        cache_roots: Vec::new(),
    });
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defaults have to make `cargo build` and `npm i` work with no
    /// configuration, which is the whole point of granting them: config and
    /// `--sandbox-writable-root` are the escape hatch for a bespoke layout, not
    /// the mechanism by which mainstream tooling becomes usable.
    ///
    /// Cargo's own caches are the subject because they are the verified failure:
    /// a build under cwd-only confinement downloads the crate successfully and
    /// then dies writing it into `$CARGO_HOME/registry/cache`.
    #[test]
    fn write_mode_grants_the_package_caches() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        let cargo_home = tool_home("CARGO_HOME", ".cargo", home_dir().as_deref())
            .expect("a home or an override");
        if !cargo_home.is_dir() {
            return; // no cargo on this machine — nothing to grant
        }

        for cache in [cargo_home.join("registry"), cargo_home.join("git")] {
            assert!(cache.is_dir(), "{} was not created", cache.display());
            let probe = cache.join("probe");
            policy
                .check_write(&canonicalize_nearest(&probe), &probe)
                .unwrap_or_else(|e| panic!("{} must be writable: {e}", cache.display()));
            assert!(
                policy
                    .cache_roots
                    .iter()
                    .any(|c| c == &canonicalize_nearest(&cache)),
                "a cache root must be LABELLED as one, or the prompt lists it"
            );
        }

        // A binary directory is NOT granted: a binary on PATH is a persistence
        // vector — the next command the *user* runs could be the agent's — so
        // `cargo install` fails by default.
        let bin = cargo_home.join("bin").join("malware");
        assert!(
            policy
                .check_write(&canonicalize_nearest(&bin), &bin)
                .is_err(),
            "a directory on PATH must not be writable"
        );
        // Nor is the tool home itself, which is where credentials live.
        let creds = cargo_home.join("credentials.toml");
        assert!(
            policy
                .check_write(&canonicalize_nearest(&creds), &creds)
                .is_err(),
            "granting a cache must not grant its parent"
        );
    }

    /// The caches are enforcement, not narration: they are in `writable_roots`
    /// (which is all the OS layer reads) and out of what a prompt or a refusal
    /// names, because the model never chooses to write there — `cargo` does.
    #[test]
    fn the_caches_are_enforced_but_not_narrated() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        assert!(!policy.cache_roots.is_empty(), "some cache was granted");
        for cache in &policy.cache_roots {
            assert!(
                policy.writable_roots.contains(cache),
                "{} must be enforced, not only labelled",
                cache.display()
            );
        }
        let named = policy.project_writable_roots();
        for cache in &policy.cache_roots {
            assert!(
                !named.iter().any(|n| *n == cache.as_path()),
                "{} must not be listed to the model",
                cache.display()
            );
        }
        assert!(
            named.iter().any(|n| *n == canonicalize_nearest(dir.path())),
            "the cwd is still named: {named:?}"
        );
        assert!(
            policy.cache_roots_clause().contains("package-manager"),
            "the omission is summarized in one clause"
        );

        // Read mode grants nothing at all, caches included.
        let read = SandboxPolicy::for_agent(SandboxMode::Read, dir.path(), &[]);
        assert!(read.cache_roots.is_empty() && read.writable_roots.is_empty());
    }

    /// A cache whose location is overridden by an env var must be resolved from
    /// that var. A hardcoded `~/.cargo/registry` on a machine with
    /// `CARGO_HOME=/opt/cargo` grants nothing, and the build then fails with
    /// exactly the confusing EROFS the grant exists to prevent.
    ///
    /// Reads the resolver directly rather than mutating the process environment:
    /// `set_var` is unsound once the test harness has threads.
    #[test]
    fn an_env_override_decides_where_a_cache_lives() {
        let home = home_dir().expect("the test sandbox set $HOME");
        assert_eq!(
            tool_home("HRDR_NO_SUCH_VAR", ".cargo", Some(&home)),
            Some(home.join(".cargo")),
            "an unset var falls back to the home-relative default"
        );
        // `$HOME` itself is a set, absolute var — enough to prove the override
        // wins over the fallback without touching the environment.
        assert_eq!(
            tool_home("HOME", ".cargo", Some(&home)),
            Some(home.clone()),
            "a set override wins outright"
        );
        // Relative values are ignored: they would resolve against whatever cwd
        // this process happens to have, not the one the tool will use.
        assert_eq!(env_dir("PATH").map(|p| p.is_absolute()), Some(true));
    }

    /// Creation completes an existing layout; it does not invent one. `~/.cargo`
    /// exists exactly when cargo is installed, so `~/.cargo/registry` is created
    /// on a machine that builds Rust and skipped on one that never will —
    /// otherwise hrdr would scatter two dozen empty directories through the home
    /// of anyone who runs it once.
    #[test]
    fn a_cache_root_is_only_created_inside_a_layout_that_exists() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("tool-home").join("cache");
        assert!(
            !ensure_cache_root(&nested),
            "no tool home, no grant: {}",
            nested.display()
        );
        assert!(!nested.exists(), "and nothing was created");

        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        assert!(ensure_cache_root(&nested), "the layout now exists");
        assert!(nested.is_dir(), "so the cache was created");

        // Idempotent, and a directory that is already there is simply granted.
        assert!(ensure_cache_root(&nested));
    }

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

    /// Which confinement each recognised failure is attributed to. The kinds are
    /// what a caller switches on, so a note that named the right cause under the
    /// wrong kind would still mislead anything acting on it.
    #[test]
    fn each_denial_is_attributed_to_the_confinement_that_caused_it() {
        let dir = tempfile::tempdir().unwrap();

        let write = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        let erofs = sandbox_denial(&write, "EROFS: read-only file system").expect("recognized");
        assert_eq!(erofs.kind, DenialKind::WriteOutsideRoots);
        // The remedy is named, not just the cause: an error that explains itself
        // and withholds the fix is half an error.
        assert!(
            erofs.note.contains("--sandbox-writable-root"),
            "{}",
            erofs.note
        );

        let ssh = sandbox_denial(&write, "Bad owner or permissions on /etc/ssh/ssh_config")
            .expect("recognized");
        assert_eq!(ssh.kind, DenialKind::SshUserNamespace);

        let mut offline = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        offline.deny_network();
        let net = sandbox_denial(&offline, "curl: (6) Could not resolve host: example.com")
            .expect("recognized");
        assert_eq!(net.kind, DenialKind::NetworkDenied);

        let strict = SandboxPolicy::for_agent(SandboxMode::Strict, dir.path(), &[]);
        let gpu = sandbox_denial(&strict, "failed to open /dev/kfd").expect("recognized");
        assert_eq!(gpu.kind, DenialKind::GpuStrict);

        // The note half is unchanged by the split — same text, same callers.
        assert_eq!(
            sandbox_denial_note(&write, "EROFS: read-only file system").as_deref(),
            Some(erofs.note.as_str())
        );
    }

    /// A git-metadata write is an ORDINARY write now: the `.git` lock is gone, so
    /// a repo under a writable root takes commits from any agent, main or
    /// delegated. Pinned because the failure mode of a re-introduced lock is
    /// silent — a sub-agent told to commit its own work simply cannot.
    #[test]
    fn git_metadata_is_writable_like_any_other_path() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join(".git");
        std::fs::create_dir_all(repo.join("refs").join("heads")).unwrap();
        let policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        for path in [
            repo.join("index"),
            repo.join("refs").join("heads").join("main"),
            repo.join("hooks").join("pre-commit"),
            repo.join("config"),
        ] {
            let canon = canonicalize_nearest(&path);
            policy
                .check_write(&canon, &path)
                .unwrap_or_else(|e| panic!("{} must be writable: {e}", path.display()));
        }
    }

    /// A denied network reads as a dead machine, so the note has to say
    /// otherwise — and has to point at the tools that still work, or the model
    /// concludes it cannot reach the web at all and reports the task as
    /// impossible.
    #[test]
    fn a_denied_network_is_named_as_the_sandbox_and_points_at_the_web_tools() {
        let dir = tempfile::tempdir().unwrap();
        let mut sub = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        sub.deny_network();

        for failure in [
            "curl: (6) Could not resolve host: api.example.com",
            "fatal: unable to access 'https://github.com/o/r/': Could not resolve host: github.com",
            "pip install foo\nTemporary failure in name resolution",
            "ping: connect: Network is unreachable",
        ] {
            let note = sandbox_denial_note(&sub, failure).unwrap_or_else(|| panic!("{failure}"));
            assert!(note.contains("[sandbox]"), "{note}");
            assert!(
                note.contains("web_fetch") && note.contains("web_search"),
                "{note}"
            );
            assert!(
                note.contains("do not debug") || note.contains("Do NOT"),
                "{note}"
            );
            // The main agent has the network, and that is where the work goes.
            assert!(note.contains("delegated to you"), "{note}");
        }

        // The parent keeps its network, so the same output from it is an ordinary
        // failure and must not be blamed on a boundary it does not have.
        let parent = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        assert_eq!(
            sandbox_denial_note(&parent, "curl: (6) Could not resolve host: api.example.com"),
            None
        );

        // Narrow, like the EROFS case: these are what a machine says when a
        // service is down or a file is not yours, not what the sandbox says.
        for ordinary in [
            "curl: (7) Failed to connect to localhost port 8080: Connection refused",
            "bind: Permission denied",
        ] {
            assert_eq!(sandbox_denial_note(&sub, ordinary), None, "{ordinary}");
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
        for root in policy.project_writable_roots() {
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
        // Narrow, and that is the point: the parent repo's own `index`, `config`
        // and other branches' refs are NOT granted. The grant exists so a
        // worktree can commit, not so it can rewrite the repository it hangs off.
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: canonical_roots(
                std::iter::once(wt.clone()).chain(roots).collect::<Vec<_>>(),
            ),
            readable_roots: Vec::new(),
            allow_network: true,
            cache_roots: Vec::new(),
        };
        check_write(&policy, &common.join("index")).unwrap_err();
        check_write(&policy, &common.join("refs").join("heads").join("main")).unwrap_err();
        // What IS granted is the append-only object store — bind it read-only and
        // no commit from the worktree can complete.
        check_write(&policy, &common.join("objects").join("aa").join("bb")).unwrap();
    }

    /// "Is it under a writable root" is the ONLY question a write has to answer.
    ///
    /// There used to be a second one: a `.git` component anywhere in the
    /// canonical path was refused to the file tools, on the theory that
    /// `.git/hooks/pre-commit` is a file the user's next commit executes. It is
    /// deleted, and this pins the deletion — `shell` reached every one of those
    /// paths regardless (`git config`, a heredoc, `printf >`), so the guard
    /// stopped the honest path and nothing else, while refusing legitimate
    /// `.git/info/exclude` edits and the hooks a user had asked for. Oversight of
    /// git belongs at the shell layer, where guardrails run.
    #[test]
    fn a_writable_root_is_writable_all_the_way_down() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonicalize_nearest(dir.path());
        // Struct literal, not `for_agent`: the subject is paths *inside* the
        // root, and a writable `env::temp_dir()` root only adds noise.
        let policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![root.clone()],
            readable_roots: vec![root],
            allow_network: true,
            cache_roots: Vec::new(),
        };

        for allowed in [
            ".git/hooks/pre-commit",
            ".git/config",
            ".git/info/exclude",
            "vendor/dep/.git/hooks/post-checkout",
            ".hrdr/skills/helpful.md",
            ".claude/agents/reviewer.md",
            "src/main.rs",
            ".gitignore",
            ".github/ci.yml",
        ] {
            check_write(&policy, &dir.path().join(allowed))
                .unwrap_or_else(|e| panic!("{allowed} must be writable: {e}"));
        }

        // Outside the root is still refused — removing the metadata rule did not
        // remove the boundary.
        let outside = dir.path().parent().unwrap().join("hrdr-outside-probe");
        check_write(&policy, &outside).unwrap_err();

        // …including through a symlink, which is why the check is on canonical
        // paths rather than on the string the model typed.
        #[cfg(unix)]
        {
            let link = dir.path().join("escape");
            std::os::unix::fs::symlink(dir.path().parent().unwrap(), &link).unwrap();
            check_write(&policy, &link.join("hrdr-outside-probe")).unwrap_err();
        }
    }

    /// hrdr can itself be launched inside a linked worktree (the user made one,
    /// or another harness did), where `<cwd>/.git` is a *file* pointing at the
    /// parent repo and a commit writes objects and refs that live outside the
    /// worktree entirely. [`git_metadata_roots`] is what keeps that working.
    #[test]
    fn hrdr_inside_a_linked_worktree_still_commits() {
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
            allow_network: true,
            cache_roots: Vec::new(),
        };

        check_write(&policy, &wt.join("f.txt")).unwrap();

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
            "the commit did not land"
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
            allow_network: true,
            cache_roots: Vec::new(),
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

    /// The network axis in the argv: absent for an agent that keeps the network
    /// (the main one), `--unshare-net` for one that does not (any sub-agent).
    ///
    /// The flag is asserted to sit inside the argv proper — before the `--` that
    /// ends bwrap's own options — because after it, it would be an argument to
    /// the shell instead of an option to bwrap, and the command would run with
    /// its network intact and no error at all.
    #[test]
    fn bwrap_unshares_the_network_only_when_the_policy_denies_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        let allowed = argv(&bwrap_args(
            SandboxMode::Write,
            &policy,
            dir.path(),
            crate::Shell::Bash,
            "echo hi",
        ));
        assert!(
            !allowed.iter().any(|a| a == "--unshare-net"),
            "the main agent runs git push/fetch: {allowed:?}"
        );

        policy.deny_network();
        for mode in [SandboxMode::Write, SandboxMode::Read, SandboxMode::Strict] {
            let args = argv(&bwrap_args(
                mode,
                &policy,
                dir.path(),
                crate::Shell::Bash,
                "echo hi",
            ));
            let net = args
                .iter()
                .position(|a| a == "--unshare-net")
                .unwrap_or_else(|| panic!("{mode}: no network denial in {args:?}"));
            let sep = args.iter().position(|a| a == "--").expect("the separator");
            assert!(
                net < sep,
                "{mode}: the flag reaches bash, not bwrap: {args:?}"
            );
            // It travels with the other namespace flags rather than in the middle
            // of the mount sequence, where a reader would have to work out
            // whether its position mattered.
            assert_eq!(args[net - 1], "--unshare-pid", "{args:?}");
        }
    }

    /// Mode `None` has no OS wrapper to carry a denial, so the policy must not
    /// claim one — the prompt line and the denial note both read this flag, and
    /// an unconfined agent told it has no network would be told a falsehood.
    #[test]
    fn denying_the_network_is_a_no_op_when_unconfined() {
        let mut policy = SandboxPolicy::unconfined();
        policy.deny_network();
        assert!(policy.allow_network, "nothing enforces it in mode None");
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
            allow_network: true,
            cache_roots: Vec::new(),
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
            allow_network: true,
            cache_roots: Vec::new(),
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
            allow_network: true,
            cache_roots: Vec::new(),
        };
        assert!(
            !seatbelt_profile(SandboxMode::Write, &empty).contains("file-write*"),
            "an empty root set must stay closed, not open"
        );
    }

    /// A sub-agent's profile simply stops saying `(allow network*)`, and the
    /// `(deny default)` it opens with is what refuses the socket.
    ///
    /// Asserted as the WHOLE profile rather than as a missing substring, because
    /// what has to be true is that nothing else moved: an SBPL profile is
    /// last-match-wins, so a stray later `allow` would undo this silently and a
    /// `contains` check would never see it. The trailing `deny` the `.git`
    /// denial needs is the case that proves the rule — it exists only because an
    /// `(allow file-write* …)` came before it.
    #[test]
    fn seatbelt_omits_the_network_allowance_when_the_policy_denies_it() {
        let mut policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![PathBuf::from("/work/wt")],
            readable_roots: Vec::new(),
            allow_network: true,
            cache_roots: Vec::new(),
        };
        policy.deny_network();
        let profile = seatbelt_profile(SandboxMode::Write, &policy);
        assert_eq!(
            profile,
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
                "(allow file-write* (subpath \"/work/wt\"))\n",
            )
        );
        assert!(!profile.contains("network"), "{profile}");

        // Every mode, not just the one a write sub-agent gets: a read-only
        // `explore` agent is delegated too, and loses the network with it.
        for mode in [SandboxMode::Read, SandboxMode::Strict] {
            let profile = seatbelt_profile(mode, &policy);
            assert!(!profile.contains("network"), "{mode}: {profile}");
        }
    }

    /// Read mode grants no writes at all and narrows reads to the system
    /// directories plus the readable roots.
    #[test]
    fn seatbelt_strict_profile_allows_no_writes_and_only_the_read_roots() {
        let policy = SandboxPolicy {
            mode: SandboxMode::Strict,
            writable_roots: Vec::new(),
            readable_roots: vec![PathBuf::from("/work/wt")],
            allow_network: true,
            cache_roots: Vec::new(),
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
            allow_network: true,
            cache_roots: Vec::new(),
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
            allow_network: true,
            cache_roots: Vec::new(),
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
            allow_network: true,
            cache_roots: Vec::new(),
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

    /// bwrap's user namespace makes every root-owned file read as `nobody`, and
    /// OpenSSH refuses a config file it cannot vouch for — so `git push` over ssh
    /// dies inside the sandbox with an error that points at a system file.
    ///
    /// Proved end to end against the real backend, because the whole failure is a
    /// property of the namespace and an argv assertion would not see it: plain
    /// `ssh -G` fails in here, and it succeeds with the override hrdr installs.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn ssh_works_in_the_sandbox_despite_the_user_namespace() {
        let Some(shell) = bwrap_shell() else { return };
        if which::which("ssh").is_err() {
            return; // no ssh on this host — nothing to prove
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = confined_ctx(dir.path(), SandboxMode::Write);
        let run = |command: &str| {
            let ctx = ctx.clone();
            let command = command.to_string();
            async move {
                use crate::Tool as _;
                crate::ShellTool::new(shell)
                    .execute(serde_json::json!({"command": command}), &ctx)
                    .await
                    .map_err(|e| e.to_string())
                    .unwrap_or_else(|e| e)
            }
        };

        // The bug, reproduced: ssh reading the system config sees root-owned
        // files as `nobody` and bails. Skipped if this host's ssh config happens
        // not to trip the check (no system config, or an unusual layout).
        let bare = run("ssh -G example.invalid 2>&1").await;
        if !bare.to_lowercase().contains("bad owner or permissions") {
            return;
        }

        // …and the fix: `-F` makes ssh skip the system config entirely, which is
        // exactly what `git_ssh_command_for_userns` hands git.
        let fixed = run("ssh -F /dev/null -G example.invalid 2>&1").await;
        assert!(
            !fixed.to_lowercase().contains("bad owner or permissions"),
            "-F must bypass the unreadable system config: {fixed}"
        );
        assert!(fixed.contains("host example.invalid"), "{fixed}");

        // The note explains it rather than inviting a chmod of a system file.
        let note = sandbox_denial_note(&ctx.sandbox, &bare).expect("a note is owed");
        assert!(note.contains("do NOT chmod"), "{note}");
        assert!(note.contains("user namespace"), "{note}");
    }

    /// An explicit `GIT_SSH_COMMAND` is a decision; the sandbox must not rewrite
    /// it. (Serialised with the unset case below by running both here — they
    /// share one process-global env.)
    #[test]
    fn the_git_ssh_override_defers_to_an_explicit_one() {
        // SAFETY: set and removed within this body. The var IS process-global and
        // the bwrap tests call `git_ssh_command_for_userns` indirectly, so one of
        // them may observe it set during this window — harmless, because they run
        // `echo`/`touch` and never reach git or ssh.
        unsafe { std::env::set_var("GIT_SSH_COMMAND", "ssh -i /custom/key") };
        assert_eq!(git_ssh_command_for_userns(), None, "an explicit value wins");
        unsafe { std::env::remove_var("GIT_SSH_COMMAND") };

        let installed = git_ssh_command_for_userns().expect("set when unset");
        let installed = installed.to_string_lossy().into_owned();
        assert!(installed.starts_with("ssh -F "), "{installed}");
    }

    /// A confined agent can commit its own work, proved against the real OS
    /// backend rather than against the argv.
    ///
    /// This is the reversal the redesign turns on: `.git` used to be subtracted
    /// from a write sub-agent's mounts, so `git add`/`commit`/`update-ref` all
    /// died on EROFS. An agent working in the user's project is now assumed to
    /// have authority over that project, and a sub-agent told to commit its own
    /// changes can. Asserted as a *property* of whatever backend this machine
    /// runs, so re-introducing the lock on any of them fails here.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_confined_agent_can_commit_its_own_work() {
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

        let mut ctx = crate::ToolContext::new(repo.clone());
        ctx.sandbox = std::sync::Arc::new(SandboxPolicy::for_agent(SandboxMode::Write, &repo, &[]));
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

        let log = run("git log --oneline".to_string()).await;
        assert!(log.contains("init"), "history stays readable: {log}");

        let edit = run("printf after > f.txt".to_string()).await;
        assert!(!edit.to_lowercase().contains("read-only"), "{edit}");

        // The line that matters. Staging writes the index; committing writes an
        // object and moves a ref. Both live in `.git`, and both must work.
        let commit = run("git add f.txt && git commit -qm mine".to_string()).await;
        assert!(
            !commit.to_lowercase().contains("read-only"),
            "a confined agent must be able to commit: {commit}"
        );
        let head = git(&["log", "--oneline"]);
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).lines().count(),
            2,
            "the commit landed"
        );

        // A ref write directly, the other way in.
        let ref_write = run("git update-ref refs/heads/scratch HEAD".to_string()).await;
        assert!(
            !ref_write.to_lowercase().contains("read-only"),
            "ref writes work too: {ref_write}"
        );
    }

    /// The other half of the sub-agent's boundary, proved the same way: a
    /// delegated shell cannot open a socket, and the identical command with the
    /// network allowed can.
    ///
    /// The target is a listener **this test bound itself** on loopback, chosen
    /// because it needs no external service — a CI runner with no egress would
    /// fail an internet probe for the wrong reason — and because it cannot pass
    /// by accident: `--unshare-net` hands the child a private network namespace
    /// whose loopback is its own, so the connect that succeeds outside finds
    /// nothing listening inside. Nothing else on the machine produces that
    /// difference between two otherwise identical runs.
    ///
    /// Never accepted, deliberately: the kernel completes the handshake out of
    /// the listen backlog, so there is no server thread to start or to join.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_subagent_shell_has_no_network() {
        let Some(shell) = bwrap_shell() else { return };
        if shell != crate::Shell::Bash {
            return; // the probe is bash's `/dev/tcp`; POSIX sh has no equivalent
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let port = listener.local_addr().unwrap().port();
        let probe = format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo CONNECTED");
        // `/dev/tcp` is a bash *compile-time* feature (`--enable-net-redirections`).
        // A bash built without it fails both arms and would turn this into a test
        // that passes while proving nothing, so ask the host's bash first.
        let host = std::process::Command::new("bash")
            .arg("-c")
            .arg(&probe)
            .output()
            .expect("bash");
        if !host.status.success() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let mut ctx = confined_ctx(dir.path(), SandboxMode::Write);
        let allowed = run_shell(shell, &ctx, &probe).await;
        assert!(
            allowed.contains("CONNECTED"),
            "the main agent keeps its network — it is the one that pushes: {allowed}"
        );

        let mut policy = SandboxPolicy::for_agent(SandboxMode::Write, dir.path(), &[]);
        policy.deny_network();
        ctx.sandbox = std::sync::Arc::new(policy);
        let denied = run_shell(shell, &ctx, &probe).await;
        assert!(
            !denied.contains("CONNECTED"),
            "a delegated shell must not reach a socket: {denied}"
        );
        assert!(
            denied.contains("[exit status"),
            "…and it fails rather than quietly doing nothing: {denied}"
        );
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
            allow_network: true,
            cache_roots: Vec::new(),
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

    /// The fallback backend's half of the network denial, against the real
    /// kernel: ABI v4 gave Landlock exactly two network rights, and handling
    /// `AccessNet` while granting no port is how a ruleset spells "no TCP".
    ///
    /// Run here rather than trusted from the API docs because the failure mode
    /// this guards against is a ruleset that *builds* and enforces nothing —
    /// `BestEffort` downgrades silently, so only a real connect proves it.
    ///
    /// And the notice is asserted alongside, because what this backend cannot do
    /// matters as much as what it can: UDP and raw sockets are outside
    /// Landlock's vocabulary, so the denial here is narrower than bwrap's and
    /// the agent is told so.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn landlock_denies_tcp_and_admits_what_it_cannot_reach() {
        if !std::fs::read_to_string("/sys/kernel/security/lsm")
            .unwrap_or_default()
            .contains("landlock")
        {
            return; // best-effort: exercise the real backend when available
        }
        if crate::Shell::detect() != Some(crate::Shell::Bash) {
            return; // the probe is bash's `/dev/tcp`
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let port = listener.local_addr().unwrap().port();
        let probe = format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo CONNECTED");
        let host = std::process::Command::new("bash")
            .arg("-c")
            .arg(&probe)
            .output()
            .expect("bash");
        if !host.status.success() {
            return; // a bash without `--enable-net-redirections` proves nothing
        }

        let dir = tempfile::tempdir().unwrap();
        let mut policy = SandboxPolicy {
            mode: SandboxMode::Write,
            writable_roots: vec![canonicalize_nearest(dir.path())],
            readable_roots: Vec::new(),
            allow_network: true,
            cache_roots: Vec::new(),
        };
        let mine = notices();
        let run = |policy: &SandboxPolicy, notices: &SandboxNotices| {
            let mut cmd = shell_command_with_backend(
                OsSandboxBackend::Landlock,
                crate::Shell::Bash,
                &probe,
                policy,
                dir.path(),
                notices,
            );
            cmd.current_dir(dir.path());
            async move { cmd.output().await.unwrap() }
        };

        let out = run(&policy, &mine).await;
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("CONNECTED"),
            "an agent that keeps its network still connects: {out:?}"
        );

        policy.deny_network();
        let out = run(&policy, &mine).await;
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(!out.status.success(), "the connect was allowed: {out:?}");
        assert!(stderr.contains("Permission denied"), "{stderr}");

        let queued = drain(&mine);
        assert!(
            queued
                .iter()
                .any(|n| n == NETWORK_PARTIAL_UNDER_LANDLOCK_NOTICE),
            "the UDP/raw-socket gap is admitted, not hidden: {queued:?}"
        );
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
            allow_network: true,
            cache_roots: Vec::new(),
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
