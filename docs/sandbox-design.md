# OS sandbox — implementation specification

Status: **implementation-ready spec** (2026-07-26). Supersedes the earlier
design/discussion version of this document; every seam below has been verified
against the code as of commit `6649de4`, and re-verified in an adversarial
second pass (2026-07-26) that also **runtime-validated the bwrap invocations on
Arch Linux (bubblewrap 0.11.2)** — see the validation notes in §3.6.1.

Target implementer: **a weaker model driven through hrdr's own task
delegation**, one slice per delegated task. This document makes every design
decision so the implementer makes none.

> **Rule box (binding):**
>
> - Implement the slices **in order**. Each slice must compile, pass
>   `cargo fmt --all` + `cargo clippy --all-targets -- -D warnings` +
>   `cargo test`, and be **committed** (Conventional Commits, no attribution
>   footers) before the next slice starts.
> - Pre-1.0: **no migration shims, no back-compat fallbacks** (see
>   `docs/deferred-improvements.md` → Standing constraints).
> - **hrdr-agent owns ALL agent logic; main and sub-agents share one codepath.**
>   Mode derivation, prompt assembly, and policy wiring must never branch "if
>   sub-agent do X specially" outside the single shared path.
> - Do not redesign. Where this spec and your instinct disagree, the spec wins.
> - Never silently pretend to sandbox: every degradation emits the exact notice
>   string given below.

Motivation (unchanged): a delegated non-Claude write sub-agent `cd`'d out of its
worktree into the parent repo and committed to `main`. Guidance only reaches
steerable models; hrdr runs arbitrary models, so the boundary must be
**enforced**. Priority order (decided, do not reorder): software path-guard for
the in-process file tools → Linux `bwrap` wrap on shell children (the real fix
for the observed escape) with Landlock fallback → prompt declaration → macOS
Seatbelt. Windows stays software-layer-only in v1. Shell out to the system
`bwrap`; **never reimplement it**. Default mode: `write`.

Reference implementation: Codex, local clone at
`~/Projects/harness/codex/codex-rs` (**machine-specific path** — a local
checkout of openai/codex, not part of this repo). Verified present:
`linux-sandbox/src/bwrap.rs` (arg assembly), `linux-sandbox/src/landlock.rs`
(the fallback ruleset, lines ~137–163), `linux-sandbox/src/bundled_bwrap.rs`
(the later "bundle bwrap" follow-up), `sandboxing/src/seatbelt.rs` +
`seatbelt_base_policy.sbpl` (macOS), `windows-sandbox-rs/` (Windows, out of
scope v1). Codex pins `landlock = "0.4.4"`.

---

## 1. Verified seam inventory

Everything in this table was read from the code on 2026-07-26. Line numbers
drift; symbol names are the anchor.

| Seam                        | Crate / file                                               | Verified symbol / signature                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               | What this spec adds there                                                                                               |
| --------------------------- | ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| Tool execution context      | `crates/hrdr-tools/src/lib.rs`                             | `pub struct ToolContext` (~line 165), fields `cwd`, `guardrails`, `lsp`, `hooks`, …; `ToolContext::new(cwd)`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              | new field `pub sandbox: std::sync::Arc<SandboxPolicy>` (unconfined in `new()`)                                          |
| Path resolution chokepoint  | `crates/hrdr-tools/src/lib.rs`                             | `ToolContext::resolve(&self, path: &str) -> PathBuf` (~255) → `pub fn resolve_under(base, path)` (~453); `pub fn canonicalize_nearest(path) -> PathBuf` (~472) — already symlink/`..`-escape-safe                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         | new `ToolContext::resolve_read` / `resolve_write` (resolve + canonicalize + root check)                                 |
| Secret guard (pattern)      | `crates/hrdr-tools/src/lib.rs`                             | `pub(crate) fn guard_secret_read(path) -> Result<()>` (~892), `secret_file_reason` (~767)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 | nothing changed; the sandbox guard sits beside it and follows its error style                                           |
| Removed cwd-confinement     | git history, commit `f0d903a`                              | `restrict_to_cwd`, `ensure_within_cwd`, `ensure_inside_cwd`, `ensure_read_inside_cwd`, `ensure_no_symlink_components`, `allow_outside_cwd`, `$HRDR_ALLOW_OUTSIDE_CWD` — **all deleted**. Only `canonicalize_nearest` and `secret_file_reason` survive.                                                                                                                                                                                                                                                                                                                                                                                                                                                                    | **write the guard fresh** in `sandbox.rs`; do NOT try to restore the deleted functions (pre-1.0: no resurrection)       |
| Shell tool spawn site       | `crates/hrdr-tools/src/tools/shell.rs`                     | `enum Shell { Bash, Posix }`; `Shell::command(self, command) -> tokio::process::Command` (builds `bash -c`/`sh -c`); `ShellTool::execute` (~185): `self.shell.command(&a.command)` → `cmd.current_dir(&ctx.cwd)` → `run_streamed_command` → `proc::spawn_group`                                                                                                                                                                                                                                                                                                                                                                                                                                                           | replace the `Shell::command` call with `sandbox::sandboxed_shell_command(shell, cmd_str, &ctx.sandbox, &ctx.cwd)`       |
| Second shell spawn site     | `crates/hrdr-tools/src/tools/watch.rs`                     | `run_check` (~164): `Shell::detect()` → `shell.command(command)` → `current_dir(&ctx.cwd)`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                | same wrapper as `shell` — model-supplied commands both times                                                            |
| Git tool                    | `crates/hrdr-tools/src/tools/git.rs`                       | `GitTool` spawns `git` directly but `read_only() == true` — its subcommands are an ALLOW-list of non-mutating ones                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | **nothing** in v1 (it cannot write); noted as a Read-mode leak in §5                                                    |
| Group kill                  | `crates/hrdr-tools/src/proc.rs`                            | `pub(crate) fn spawn_group(&mut tokio::process::Command) -> io::Result<(Child, GroupKill)>`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               | unchanged — the bwrap wrapper is just a different `Command`, group-kill still applies                                   |
| Overflow spill dir          | `crates/hrdr-tools/src/lib.rs`                             | `pub fn tool_output_dir() -> PathBuf` (~1320): `$XDG_RUNTIME_DIR/hrdr-tool-output`, else `temp_dir()/hrdr-tool-output-<user>`; `ensure_private_dir` chmods 0700                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           | listed as a writable/readable root in every mode that confines                                                          |
| Session scratch             | — (does not exist yet)                                     | no scratch dir exists in the code today                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | new `pub fn session_scratch_dir() -> &'static Path` in `sandbox.rs`, mirroring `tool_output_dir`'s style                |
| Worktrees                   | `crates/hrdr-agent/src/delegation.rs`                      | `Worktree::create` (~3060): path `<git toplevel>/.hrdr/worktrees/wt-<stamp>-<pid>-<seq>`, branch `hrdr/task-<uniq>`; `cfg.cwd = wt.path` (~1423); revive repoints too (~2031); `is_hrdr_worktree` (~1490)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 | nothing — the sandbox reads `cwd` and detects a linked worktree generically via the `.git` **file** (§3.3)              |
| Sub-agent config derivation | `crates/hrdr-agent/src/delegation.rs`                      | `config_for_agent_profile` (~1021) sets `cfg.read_only`/`cfg.allowed_tools`; `SubagentTool::execute` clones the base config, `let write_capable = !cfg.read_only;` (~1354)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                | nothing here — mode derivation happens once, in `Agent::new` (shared codepath, §2.3)                                    |
| Agent construction          | `crates/hrdr-agent/src/lib.rs`                             | `Agent::new`: `let mut ctx = ToolContext::new(config.cwd.clone()); ctx.lsp = …; ctx.max_output = …` (~1287–1294) — the pattern for populating `ToolContext` from config                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | `ctx.sandbox = Arc::new(SandboxPolicy::for_agent(mode, &config.cwd, &config.sandbox_writable_roots));`                  |
| Config struct               | `crates/hrdr-agent/src/config.rs`                          | `pub struct AgentConfig` (~196), `impl Default for AgentConfig` (~819), fields incl. `read_only` (bool), `allowed_tools`, `is_subagent`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | `pub sandbox: SandboxMode`, `pub sandbox_writable_roots: Vec<PathBuf>`                                                  |
| Config-file parsing         | `crates/hrdr-agent/src/config.rs`                          | `pub(crate) struct FileConfig` (~673, serde), `FileConfig::validate` (~728, hard errors), `AgentConfig::apply_file(&mut self, fc: FileConfig)` (~1435, `if let Some(v) = fc.x { self.x = v }` per field)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  | `sandbox: Option<SandboxMode>` (serde enum → invalid spelling is a hard TOML error), `sandbox_writable_roots`           |
| Env plumbing                | `crates/hrdr-agent/src/config.rs`                          | `ENV_SETTERS: &[(&str, EnvSetter)]` (~1666) — one row per knob, `Err(reason)` → warning, value kept; helpers `parse_env_bool`, `env_parse`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                | one `("HRDR_SANDBOX", …)` row using `SandboxMode::from_str`                                                             |
| CLI plumbing                | `apps/hrdr/src/main.rs`                                    | `struct Cli` (~44) with `#[arg(long, global = true)]` fields; override block ~495–515 (`if let Some(n) = cli.max_write_subagents { config.max_write_subagents = n; }`)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    | `--sandbox <write\|read\|none>` and `--no-sandbox`, applied in the same block                                           |
| Prompt sections             | `crates/hrdr-agent/src/prompt.rs`                          | `SystemPrompt { sections: Vec<(&'static str, String)> }` with `push` (drops empty bodies), `names()`, `prefix_len_before(name)`, `render()`; section constants `SECTION_BASE … SECTION_PERSONA, SECTION_ENVIRONMENT`; runtime-built sections follow `environment_section(cwd, tools) -> String`                                                                                                                                                                                                                                                                                                                                                                                                                           | `pub const SECTION_SANDBOX: &str = "sandbox";` + `pub fn sandbox_section(policy) -> String`                             |
| Prompt assembly + cache     | `crates/hrdr-agent/src/lib.rs`                             | `build_system_prompt_sections(tools, cwd, docs, memory, persona, is_subagent)` (~1019) pushes sections in the documented least-volatile-first order; `build_system_prompt` (~1072) computes the cache split as `p.prefix_len_before(SECTION_ENVIRONMENT)`; **three** call sites: `Agent::new` (~1350), `refresh_system_prompt_in_place` (~1575), `refresh_system` (~1619) — slice 7 must thread the policy through all three. **The old doc's `system.j2` is GONE** — the prompt is ten `include_str!` markdown fragments in `crates/hrdr-agent/src/templates/*.md` plus runtime-built sections; interpolation into the static fragments is impossible. The worktree guidance now lives in `templates/subagent_write.md`. | push `SECTION_SANDBOX` **after** `SECTION_ENVIRONMENT` (dead last — see §4 slice 7 for why that is the cache-safe spot) |
| Prompt-order test           | `crates/hrdr-agent/src/lib.rs`                             | `system_prompt_is_ordered_least_volatile_first` (~4242) asserts `p.names().last() == Some(&SECTION_ENVIRONMENT)` (~4308); it builds sections at ~4248, and two more tests do at ~4292/~4319                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               | update to expect `SECTION_SANDBOX` last, `SECTION_ENVIRONMENT` second-to-last                                           |
| Degradation notice channel  | `crates/hrdr-llm/src/client.rs`, `hrdr-agent/turn_loop.rs` | `pub fn take_client_warning() -> Option<String>` (OnceLock<Mutex<Option<String>>> drain, client.rs ~41) → drained in `turn_loop.rs` (~507): `if let Some(warning) = hrdr_llm::take_client_warning() { on_event(AgentEvent::Notice(warning)); }`; `AgentEvent::Notice(String)` (lib.rs ~622)                                                                                                                                                                                                                                                                                                                                                                                                                               | mirror it: `hrdr_tools::take_sandbox_notice()` + one drain line beside the existing one                                 |
| Test skip pattern           | `crates/hrdr-tools/src/tools/grep.rs`                      | `if which::which("rg").is_err() { return; } // best-effort: exercise the real backend when available` (~653)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              | the exact pattern for skipping bwrap/seatbelt tests (§6)                                                                |

**Stale claims in the old doc, corrected here:** (1) "guidance clause in
`system.j2`" — that file is deleted; the guidance is `subagent_write.md`, and
the prompt has no template engine at all. (2) "the removed cwd-confinement code
is the starting point" — it is fully deleted in `f0d903a`; write fresh. (3)
"`ToolContext::resolve` is the natural home" — correct in spirit; the real
chokepoint is `ToolContext::resolve` + `canonicalize_nearest`, and every file
tool goes through it (call sites enumerated in slice 3). (4) The doc's root set
missed two things the code makes mandatory: `std::env::temp_dir()` must be
writable (compilers/linkers write there; the scratch dir lives there) and a
**linked worktree's commits write to the parent repo's `.git`** — without the
git-metadata roots in §3.3, every write sub-agent's `git commit` breaks under
the sandbox.

---

## 2. `SandboxMode`

### 2.1 The type

Lives in a **new file** `crates/hrdr-tools/src/sandbox.rs` (declared
`pub mod sandbox;` in `crates/hrdr-tools/src/lib.rs`, with
`pub use sandbox::{SandboxMode, SandboxPolicy};`). It lives in hrdr-tools, not
hrdr-agent, because `ToolContext` must hold it and hrdr-agent already depends on
hrdr-tools (the reverse would be a cycle).

```rust
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

impl std::fmt::Display for SandboxMode { /* "write" | "read" | "none" */ }
```

No `Default` derive: `AgentConfig::default()` names the variant explicitly
(`SandboxMode::None` until slice 4 flips it to `SandboxMode::Write`), and
`ToolContext::new` always starts unconfined (§4 slice 3).

The serde derive makes `sandbox = "wrote"` in config.toml a **hard TOML parse
error at startup** — consistent with the config module's rule that file values
are errors while env values are warnings (see the module docs at the top of
`config.rs`).

### 2.2 Root sets per mode

| Mode  | Readable                                                                                       | Writable                                                                                                                                            |
| ----- | ---------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| None  | everything                                                                                     | everything                                                                                                                                          |
| Write | everything¹                                                                                    | `cwd` + `std::env::temp_dir()` + `session_scratch_dir()` + `tool_output_dir()` + git metadata roots (§3.3) + `sandbox_writable_roots` config extras |
| Read  | `cwd` + `session_scratch_dir()` + `tool_output_dir()` (+ system dirs for the OS layer, §3.6.1) | — (none)                                                                                                                                            |

¹ Broad reads in `Write` are a **deliberate, decided tradeoff** (matches Codex
`workspace-write`): builds read all over the FS. Cost: a shell command can read
`~/.ssh` etc. The in-process file tools keep `guard_secret_read`; a curated read
allow-list is a listed follow-up, **not** v1. Do not "fix" this.

**Known limitation to document, not solve:** a cold `cargo build`/`npm install`
in a fresh worktree writes to `~/.cargo/registry` / `~/.npm` and will hit EROFS
under `Write` mode. The escape hatch is the user-configured
`sandbox_writable_roots = ["/home/me/.cargo", …]` (which mirrors Codex's
`writable_roots` config knob). Do not add cache dirs to the default set.

### 2.3 Per-agent mode derivation (decision table)

One function, one call site, shared by main and sub-agents (standing constraint
— no parity forks). In `crates/hrdr-agent/src/config.rs`:

```rust
/// The mode this agent actually runs: the session default, floored/capped by
/// what the agent is. Called ONCE, in `Agent::new`, for every agent — main or
/// sub, since both come through the same constructor.
pub fn effective_sandbox(session: SandboxMode, read_only: bool) -> SandboxMode {
    match (session, read_only) {
        (SandboxMode::None, _) => SandboxMode::None, // explicit global opt-out
        (_, true) => SandboxMode::Read,              // no write tools → read confinement
        (_, false) => SandboxMode::Write,            // a writer must write; its cwd confines it
    }
}
```

| Session default (`config.sandbox`) | Main agent (write-capable) | Main agent run as read-only profile (`--agent explore`) | Write sub-agent | Read-only sub-agent |
| ---------------------------------- | -------------------------- | ------------------------------------------------------- | --------------- | ------------------- |
| `none`                             | `none`                     | `none`                                                  | `none`          | `none`              |
| `write` (default after slice 4)    | `write`                    | `read`                                                  | `write`         | `read`              |
| `read`                             | `write`¹                   | `read`                                                  | `write`         | `read`              |

¹ Session `read` with a write-capable agent resolves to `write`: an agent that
has write tools cannot function under `read`, and its writable roots already
confine it. (This is the old doc's "min of session mode and Write" made
concrete.) `read` as a session default is meaningful for read-only profiles and
read-only sub-agents; for a write-capable agent it is not an error, it just
floors at `write`.

Sub-agents inherit `config.sandbox` automatically because
`SubagentTool::execute` clones the base `AgentConfig` (delegation.rs ~1237
`let mut cfg = self.base.clone();`) — `cfg.read_only` is already final before
`Agent::new` runs, so `effective_sandbox` in `Agent::new` needs no
delegation-side code at all. **Revived sub-agents** (`task_revive`,
delegation.rs ~2004) are covered by the same rows: revive also clones the base
config (`read_only` stays `false` — a revived run takes a write slot) and
repoints `cfg.cwd` at the reused worktree (~2031), so it derives exactly like a
write sub-agent, through the same `Agent::new` call — no extra code.

### 2.4 Configuration surface

- `AgentConfig.sandbox: SandboxMode` — default `SandboxMode::None` in slices
  1–3, flipped to `SandboxMode::Write` in slice 4.
- `AgentConfig.sandbox_writable_roots: Vec<PathBuf>` — default empty.
- config.toml: `sandbox = "write" | "read" | "none"`,
  `sandbox_writable_roots = ["/abs/path", …]` (absolute paths; relative entries
  are a `FileConfig::validate` hard error:
  `sandbox_writable_roots entries must be absolute paths`).
- Env: `HRDR_SANDBOX=write|read|none` (one `ENV_SETTERS` row; bad value →
  warning, value kept — the table's existing contract).
- CLI: `--sandbox <write|read|none>` and `--no-sandbox` (alias for `none`;
  declare with `conflicts_with = "sandbox"`).
- Precedence follows the existing layering in `AgentConfig::load_checked`:
  defaults → config file → env → CLI flags (the main.rs override block runs
  last). Copy the `max_write_subagents` plumbing verbatim at each layer.

---

## 3. The `SandboxPolicy` and its mechanics

All in `crates/hrdr-tools/src/sandbox.rs` unless said otherwise.

### 3.1 Policy struct

```rust
/// A resolved confinement policy: the mode plus the concrete, canonicalized
/// root sets. Built once per agent in `Agent::new`; `ToolContext` holds it
/// behind an Arc so tool calls share it cheaply.
#[derive(Debug)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    /// Canonicalized (via `canonicalize_nearest`) writable roots. Empty when
    /// mode is `None` (meaning "everything") or `Read` (meaning "nothing").
    pub writable_roots: Vec<PathBuf>,
    /// Canonicalized readable roots; only consulted in `Read` mode.
    pub readable_roots: Vec<PathBuf>,
}

impl SandboxPolicy {
    /// The no-op policy: mode None, no roots. What `ToolContext::new` installs.
    pub fn unconfined() -> Self { … }

    /// Build the policy for an agent working in `cwd`.
    /// writable = [cwd, env::temp_dir(), session_scratch_dir(),
    ///             tool_output_dir()] + git_metadata_roots(cwd) + extras,
    /// each canonicalized with canonicalize_nearest, deduped (drop any root
    /// that is already under an earlier root).
    /// readable = [cwd, session_scratch_dir(), tool_output_dir()], same
    /// treatment.
    pub fn for_agent(mode: SandboxMode, cwd: &Path, extras: &[PathBuf]) -> Self { … }

    /// Err unless `canon` (already canonicalized) is under a writable root or
    /// mode is None. The error message is EXACTLY the string in §5.
    pub fn check_write(&self, canon: &Path, shown: &Path) -> anyhow::Result<()> { … }

    /// Err iff mode is Read and `canon` is outside every readable root.
    pub fn check_read(&self, canon: &Path, shown: &Path) -> anyhow::Result<()> { … }
}
```

"Under a root" means `canon.starts_with(root)` **after both sides went through
`canonicalize_nearest`** — that function already resolves symlinks and lexical
`..` in the not-yet-existing suffix, which is what makes the check escape-proof
(its doc-comment describes exactly this bypass).

Non-existent `extras` entries are skipped silently when building the policy
(user config error, not fatal); everything else in the default writable set is
created by its own accessor before canonicalization.

### 3.2 Session scratch dir

Mirror `tool_output_dir` (same file placement style, same `ensure_private_dir`
0700 treatment), but per-process and cached:

```rust
/// The per-session scratch dir: `<temp_dir>/hrdr-scratch-<pid>-<8 hex rand>`,
/// created 0700 on first use, one per process (a session lives in one
/// process; sub-agents share the process, hence the scratch — by design).
/// First call also sweeps stale `hrdr-scratch-<pid>-*` siblings whose pid is
/// no longer alive (unix: `libc::kill(pid, 0)` == -1 with ESRCH; on
/// non-unix skip the sweep). Best-effort: sweep failures are ignored.
pub fn session_scratch_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    …
}
```

No teardown hook: the dead-pid sweep on the next start is the cleanup (the OS
tmp reaper also applies). Do not add an exit handler.

### 3.3 Git metadata roots (the linked-worktree trap)

A write sub-agent's cwd is a **linked git worktree**
(`<repo>/.hrdr/worktrees/wt-…`). In a linked worktree, `<cwd>/.git` is a
**file** (`gitdir: <abs path>`), the private worktree state lives at
`<repo>/.git/worktrees/<name>/` (index, HEAD, COMMIT_EDITMSG, locks), and
`git commit` writes objects into the **parent repo's** `<repo>/.git/objects` and
updates its branch ref under `<repo>/.git/refs/heads/hrdr/…`. If those are not
writable roots, **every sub-agent commit dies with EROFS/refusal** — while
making the whole parent `.git` writable would re-open the exact escape this
project exists to close (`git -C <parent> update-ref refs/heads/main …`). So:

```rust
/// Extra writable roots a linked git worktree needs to commit, and nothing
/// more. Empty when `<cwd>/.git` is a directory (a normal checkout: .git is
/// under cwd, already writable) or absent (not a repo).
fn git_metadata_roots(cwd: &Path) -> Vec<PathBuf> {
    // 1. read `<cwd>/.git`; if it is not a regular file, return vec![].
    // 2. parse the single line `gitdir: <path>` → gitdir (absolute; if
    //    relative, join onto cwd). Malformed → return vec![] (fail open to
    //    "no extras": worse ergonomics, no security hole).
    // 3. read `<gitdir>/commondir` (a relative path like `../..`), join onto
    //    gitdir and canonicalize → common (the parent `.git` dir).
    // 4. return vec![
    //        gitdir,                                  // index, HEAD, locks
    //        common.join("objects"),                  // content-addressed, append-only
    //        common.join("refs").join("heads").join("hrdr"),      // task branches only
    //        common.join("logs").join("refs").join("heads").join("hrdr"),
    //    ]  — create the two hrdr ref dirs with fs::create_dir_all first so
    //         they exist to be bind-mounted / canonicalized.
}
```

Deliberately NOT writable: `common` itself, `common/index`,
`common/refs/heads/<anything else>`, `common/packed-refs`, `common/config` —
that is what keeps `git -C <parent> commit` and `update-ref refs/heads/main`
blocked. Known cosmetic consequence: ref maintenance fired by a commit inside
the worktree may print
`error: Unable to create '<common>/packed-refs.lock': Read-only file system`
(observed verbatim in the runtime validation) while the commit itself exits 0
and has already landed. Accept the message; do not widen the roots for it.

### 3.4 Degradation notice cell

Mirror the drain shape of `hrdr_llm::take_client_warning`, but with one
addition: §5 requires each notice **at most once per process**, and a plain
`Option<String>` cell would re-fill (and re-notice) on every shell call after
each drain. So the cell keeps a seen-set beside the pending slot —
`OnceLock<Mutex<(HashSet<String>, Option<String>)>>`:

```rust
pub fn take_sandbox_notice() -> Option<String>;      // drain the pending slot (seen-set untouched)
pub(crate) fn set_sandbox_notice(msg: String);       // insert into the seen-set; only a message
                                                     // seen for the FIRST time becomes pending
```

Drained in `crates/hrdr-agent/src/turn_loop.rs` right beside the existing drain
(~line 507):

```rust
if let Some(warning) = hrdr_tools::take_sandbox_notice() {
    on_event(AgentEvent::Notice(warning));
}
```

### 3.5 OS backend detection (Linux)

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsSandboxBackend {
    Bwrap,
    Landlock,
    None,
}

/// Detected once per process (OnceLock). Linux only; other OSes return
/// their §3.7 answer.
pub fn detect_backend() -> OsSandboxBackend {
    // 1. `which::which("bwrap")` — missing → skip to 3.
    // 2. probe unprivileged user namespaces: run
    //      bwrap --unshare-user --unshare-pid --ro-bind / / --proc /proc --dev /dev -- /bin/true
    //    with stdin/stdout/stderr null and a 500 ms wait (std::process, spawn
    //    + poll try_wait in a 50 ms loop, kill on deadline — this mirrors
    //    Codex's SYSTEM_BWRAP_PROBE_TIMEOUT / _POLL_INTERVAL in
    //    sandboxing/src/bwrap.rs). Exit 0 → Backend::Bwrap.
    //    (Probe line runtime-validated on Arch — exits 0. §3.6.1.)
    //    Non-zero or timeout → user namespaces are disabled → step 3.
    // 3. `std::fs::read_to_string("/sys/kernel/security/lsm")` contains
    //    "landlock" → Backend::Landlock.
    // 4. → Backend::None.
}
```

Each degradation step sets the notice (§5) via `set_sandbox_notice` **at the
moment a confined shell command first needs the backend**, not at detection.

### 3.6 The shell wrapper

```rust
/// The command `shell`/`watch` actually spawn: `cmd_str` through `shell`,
/// wrapped in the OS sandbox the policy's mode demands. Mode None (or an
/// unconfinable platform) returns exactly what `Shell::command` returns
/// today. The caller still owns cwd, stdio, timeouts, groups.
pub fn sandboxed_shell_command(
    shell: crate::tools::Shell,   // re-exported as crate::Shell
    cmd_str: &str,
    policy: &SandboxPolicy,
    cwd: &std::path::Path,        // for `--chdir` — the policy holds roots, not the cwd
) -> tokio::process::Command
```

Behavior by `(os, detect_backend(), mode)`:

- mode `None` → `shell.command(cmd_str)` unchanged.
- Linux + `Bwrap` → build the §3.6.1 arg list, program `bwrap`.
- Linux + `Landlock` → `shell.command(cmd_str)` plus a `pre_exec` closure
  (§3.6.2); `Read` mode additionally sets the read-degradation notice and
  applies the same write-roots ruleset (Landlock cannot confine reads).
- Linux + `None`, or any non-Linux platform (until the Seatbelt slice) →
  `shell.command(cmd_str)` unchanged + the "not OS-confined" notice.

The caller keeps setting `cmd.current_dir(&ctx.cwd)` — bwrap inherits it, and
the explicit `--chdir` below makes the child's cwd deterministic even if the
inherited one is a symlink alias.

#### 3.6.1 bwrap argument lists — VERBATIM

> **Runtime-validated 2026-07-26 on Arch Linux, bubblewrap 0.11.2** (usr-merged:
> `/bin → usr/bin`, `/sbin → usr/bin`, `/lib`/`/lib64 → usr/lib`). Confirmed
> with the literal argv below: write inside an rw root succeeds; a write outside
> — via redirect, `cd <parent> && …`, or `python -c "open(…,'w')"` — fails with
> `Read-only file system` (EROFS) and creates nothing; exit status propagates
> (`bash -c 'exit 7'` → 7); stdout/stderr flow through untouched;
> `--die-with-parent` is accepted; killing the spawn group kills every
> descendant; `git commit` inside a linked worktree **succeeds** with only the
> §3.3 metadata roots bound rw while `git -C <parent> commit` and
> `git -C <parent> update-ref refs/heads/main …` both **fail** on
> `index.lock`/`packed-refs.lock` EROFS; in Read mode `/home` is ENOENT,
> `--tmpfs /tmp` is writable, and `python -c` and a `cc hello.c` compile both
> work from `/usr` + the compat symlinks. The §3.5 probe line was also run
> verbatim and exits 0.

Environment passthrough policy: **full inheritance, no `--clearenv`** —
PATH/HOME/CARGO_HOME must survive; env secrets are explicitly not hidden in v1
(same flagged tradeoff as broad reads). stdout/stderr/stdin: bwrap execs the
shell with inherited fds, so the existing `Stdio::piped()` capture, streaming,
overflow spill, timeout, and `[exit status: …]` reporting all work unchanged;
bwrap propagates the child's exit status. `--die-with-parent` guarantees the
sandbox dies with hrdr; the existing `spawn_group` group-kill kills bwrap, and
the pid-namespace init dying takes every descendant with it.

**Write mode** (`<rw>` iterates the policy's `writable_roots`, in order,
deduped, each existing):

```
bwrap
  --new-session
  --die-with-parent
  --ro-bind / /
  --dev /dev
  --proc /proc
  --bind <rw> <rw>          # once per writable root, AFTER --ro-bind / /
  --unshare-user
  --unshare-pid
  --chdir <cwd>
  --
  <shell.program()> -c <cmd_str>
```

Mount order matters: `--ro-bind / /` first, then the rw binds layer writable
mounts on top (bwrap applies mounts in argv order; later mounts shadow earlier
ones). No `--unshare-net`: the network axis is a declared follow-up, not v1.
This matches Codex's `create_bwrap_flags` skeleton
(`--new-session --die-with-parent` + filesystem args +
`--unshare-user --unshare-pid` + `--proc /proc`), minus its network/tmpfs-mask
machinery.

**Read mode** (order is load-bearing — see the `--tmpfs /tmp` note):

```
bwrap
  --new-session
  --die-with-parent
  --ro-bind /usr /usr
  --ro-bind /etc /etc
  <compat entries for /bin /sbin /lib /lib64>   # see below
  --tmpfs /tmp              # BEFORE the root binds — see note
  --ro-bind <cwd> <cwd>
  --ro-bind <session_scratch_dir()> <same>
  --ro-bind <tool_output_dir()> <same>
  --dev /dev
  --proc /proc
  --unshare-user
  --unshare-pid
  --chdir <cwd>
  --
  <shell.program()> -c <cmd_str>
```

`--tmpfs /tmp` gives the child a private tmp so mktemp/compilers work; its
writes vanish (fine: read mode). It MUST precede the cwd/scratch/tool-output
binds: bwrap applies mounts in argv order and later mounts shadow earlier ones,
and `session_scratch_dir()` (always) and `tool_output_dir()` (in its
no-`$XDG_RUNTIME_DIR` fallback) live **under `/tmp`** — with the tmpfs mounted
after them they vanish behind it. (Runtime-confirmed: with the tmpfs last, a cwd
under `/tmp` made even `--chdir` fail with "Can't chdir … No such file or
directory"; with the tmpfs first, all three binds punch through it correctly.)

Compat entries: for each `p` in `["/bin", "/sbin", "/lib", "/lib64"]`, if
`std::fs::symlink_metadata(p)` says symlink (usr-merged distro), read the real
target with `std::fs::read_link(p)` and emit `--symlink <target> <p>` — do NOT
guess `usr/<basename>`: on Arch, `/sbin → usr/bin` and `/lib64 → usr/lib`, so
the guessed `usr/sbin`/`usr/lib64` only resolve where `/usr` happens to carry
extra compat links. If `p` is a real directory, emit `--ro-bind <p> <p>`; if
absent, emit nothing. (Bind-mounting a symlink source creates a real dir at the
target and breaks usr-merge — this is the trap.) `/usr` and `/etc` remain
readable by design (interpreters, libc, DNS/ca-certs); everything else (`/home`,
`/root`, `/opt`, `/var`, …) simply does not exist in the child, so reads there
fail with **ENOENT**, stronger than EROFS. A tool living outside `/usr`
(`/opt/homebrew`-style installs) is "command not found" in Read mode —
acceptable, documented in §5.

#### 3.6.2 Landlock fallback — VERBATIM ruleset

Dependency (hrdr-tools `Cargo.toml`):

```toml
[target.'cfg(target_os = "linux")'.dependencies]
landlock = "0.4"
```

Applied **in the child only**, via `tokio::process::Command::pre_exec` (unsafe;
the closure runs post-fork pre-exec — landlock/prctl syscalls are fine there).
This is Codex's `install_filesystem_landlock_rules_on_current_thread`
(`linux-sandbox/src/landlock.rs` ~137–163) verbatim, minus its seccomp:

```rust
// inside: unsafe { cmd.pre_exec(move || { … }) }
use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, path_beneath_rules,
};
let abi = ABI::V5;
let access_rw = AccessFs::from_all(abi);
let access_ro = AccessFs::from_read(abi);
let mut ruleset = Ruleset::default()
    .set_compatibility(CompatLevel::BestEffort)
    .handle_access(access_rw)?
    .create()?
    .add_rules(path_beneath_rules(&["/"], access_ro))?
    .add_rules(path_beneath_rules(&["/dev/null"], access_rw))?
    .set_no_new_privs(true);
if !writable_roots.is_empty() {
    ruleset = ruleset.add_rules(path_beneath_rules(&writable_roots, access_rw))?;
}
let status = ruleset.restrict_self()?;
if status.ruleset == RulesetStatus::NotEnforced {
    return Err(std::io::Error::other("landlock not enforced"));
}
Ok(())
```

(`writable_roots` is moved into the closure as a `Vec<PathBuf>` clone; map
landlock errors to `std::io::Error::other`.) A `NotEnforced` result **fails the
spawn** — never run the command half-confined. `Read` mode under Landlock uses
these same write-roots rules and sets the degradation notice — Landlock cannot
cleanly restrict reads; this is exactly why bwrap is primary.

Never call any of this on the hrdr process/thread itself — it would confine the
app's own session/config/memory I/O. `pre_exec` is the only allowed site.

### 3.7 Non-Linux platforms in v1

- **macOS** — slice 8: generate an SBPL profile, run
  `/usr/bin/sandbox-exec -p <profile> -- <shell> -c <cmd>` (pin the absolute
  `/usr/bin/sandbox-exec` path, as Codex does). Until that slice ships, macOS
  behaves like `Backend::None` (software layer + notice).
- **Windows** — software layer + notice only, permanently for v1. Not a
  regression (there is no sandbox today).

---

## 4. Implementation slices

Nine slices. Each fits a weak-model day. "Tests" names the exact test functions
to write and what each asserts.

---

### Slice 1 — `SandboxMode`, `SandboxPolicy`, scratch dir (types only, no wiring)

**Goal:** the whole §2.1/§3.1–§3.3 vocabulary exists in hrdr-tools, fully
tested, used by nothing.

**Files:**

- create `crates/hrdr-tools/src/sandbox.rs` — `SandboxMode` (+ FromStr, Display,
  serde), `SandboxPolicy` (`unconfined`, `for_agent`, `check_write`,
  `check_read`), `session_scratch_dir` (+ stale sweep), `git_metadata_roots`,
  `take_sandbox_notice`/`set_sandbox_notice`.
- modify `crates/hrdr-tools/src/lib.rs` — `pub mod sandbox;`,
  `pub use sandbox::{SandboxMode, SandboxPolicy};`. (`canonicalize_nearest` and
  `tool_output_dir` are already `pub` in lib.rs — call them from the module, do
  not duplicate them.)

**Steps:** write the module top-down as specified in §2–§3; error strings copied
byte-for-byte from §5.

**Tests** (in `sandbox.rs` `#[cfg(test)]`):

- `sandbox_mode_parses_all_spellings_and_rejects_garbage` — FromStr accepts
  `write`/`READ`/`none`, rejects `"wrote"` with a message naming the three valid
  values.
- `session_scratch_dir_is_private_stable_and_under_temp` — same path on two
  calls; exists; on unix mode is 0700; path starts with `env::temp_dir()`.
- `policy_write_roots_cover_cwd_temp_scratch_and_tool_output` —
  `for_agent(Write, tmpdir, &[])` allows writes under all four defaults, refuses
  `/etc/passwd` and `/nonexistent-outside/f`, and the refusal message contains
  every writable root. **Trap:** a `<cwd>/../sibling` path is NOT a refusal case
  — the test cwd is a tempdir, so its sibling sits under `env::temp_dir()`,
  which is itself a writable root; assert that path is **allowed** (it pins the
  deliberate temp-dir tradeoff). Only paths outside the temp tree (like the two
  above) exercise refusal.
- `read_mode_refuses_reads_outside_roots_and_allows_cwd` — self-explanatory;
  also asserts `check_read` is a no-op in `Write` and `None` modes.
- `symlink_and_dotdot_escapes_are_caught` — create `<cwd>/link -> /etc` (the
  target must be OUTSIDE the temp tree — another tempdir would be under the
  writable `env::temp_dir()` root and not refused); `check_write` on
  `<cwd>/link/passwd` and on `<cwd>/a/` + `"../".repeat(40)` + `"etc/passwd"`
  (enough `..` to guarantee escaping to `/`) both refuse (this is
  `canonicalize_nearest` doing its job — assert through the policy, not the
  helper).
- `git_metadata_roots_for_a_linked_worktree` — build a real repo in a tempdir
  (`git init`, one commit — skip with
  `if which::which("git").is_err() { return; }`), `git worktree add`; assert the
  four roots of §3.3 and that a plain checkout yields `vec![]`. Run BOTH sides
  of each path assertion through `canonicalize_nearest` before comparing — macOS
  tempdirs live behind the `/var → /private/var` symlink and a raw comparison
  fails only on the mac CI runner.
- `sandbox_notice_is_take_once` — set, take → Some; take again → None; set twice
  with the same string → one notice total.

**Out of scope:** anything touching `ToolContext`, config, shell, prompt.

Commit: `feat(tools): SandboxMode, SandboxPolicy, session scratch (no wiring)`

---

### Slice 2 — config / env / flag plumbing + mode derivation (default stays `none`)

**Goal:** `config.sandbox` exists end-to-end; `effective_sandbox` exists;
nothing enforces anything yet.

**Files:**

- `crates/hrdr-agent/src/config.rs`:
  - `AgentConfig`: add `pub sandbox: SandboxMode` and
    `pub sandbox_writable_roots: Vec<PathBuf>`; `Default` impl sets
    `sandbox: SandboxMode::None`, empty roots.
  - `FileConfig`: add `sandbox: Option<SandboxMode>` and
    `#[serde(default)] sandbox_writable_roots: Vec<String>`;
    `FileConfig::validate`: push a hard error for any non-absolute
    `sandbox_writable_roots` entry.
  - `apply_file`: copy the `if let Some(v) = fc.x { self.x = v }` pattern; map
    the root strings to `PathBuf`.
  - `ENV_SETTERS`: add
    `("HRDR_SANDBOX", |c, v| { c.sandbox = v.parse()?; Ok(()) })` (the setter
    type is `fn(&mut AgentConfig, &str) -> Result<(), String>` and
    `SandboxMode::from_str`'s error is already `String`, so `?` just works).
  - add `pub fn effective_sandbox(session, read_only) -> SandboxMode` exactly as
    §2.3.
- `apps/hrdr/src/main.rs`: `Cli` fields
  `#[arg(long, global = true, value_name = "write|read|none")] sandbox: Option<String>`
  and
  `#[arg(long = "no-sandbox", global = true, conflicts_with = "sandbox")] no_sandbox: bool`;
  in the override block (beside `max_write_subagents`) — note the neighboring
  overrides silently drop invalid values, but a mistyped sandbox mode must not
  be silent, so:

  ```rust
  if let Some(s) = cli.sandbox.as_deref() {
      match s.parse::<hrdr_tools::SandboxMode>() {
          Ok(m) => config.sandbox = m,
          Err(e) => eprintln!("warning: --sandbox: {e} — keeping {}", config.sandbox),
      }
  }
  if cli.no_sandbox {
      config.sandbox = hrdr_tools::SandboxMode::None;
  }
  ```

  (`hrdr-tools` is already a dependency of `apps/hrdr`, and `Display` renders
  the kept mode.)

**Tests:**

- config.rs: `sandbox_config_key_parses_and_bad_value_is_a_hard_error` —
  `toml::from_str::<FileConfig>` with `sandbox = "read"` works; with
  `sandbox = "wrote"` errors.
- config.rs: `hrdr_sandbox_env_sets_mode_and_garbage_warns` — drive the
  `ENV_SETTERS` row directly; copy the harness from
  `subagent_caps_read_from_config_and_env` (`crates/hrdr-agent/src/lib.rs` ~4909
  — it looks the row up in `ENV_SETTERS` by name and calls the fn pointer on a
  config, no process env involved).
- config.rs: `relative_sandbox_writable_roots_are_rejected` — validate() error.
- config.rs: `effective_sandbox_matches_the_decision_table` — assert all six
  `(session, read_only)` combinations from §2.3.

**Out of scope:** enforcement, `ToolContext`, prompt.

Commit:
`feat(agent): sandbox config/env/flag plumbing + mode derivation (default none)`

---

### Slice 3 — software path-guard in the file tools (default still `none`)

**Goal:** every model-supplied path is checked against the policy. With the
default still `None`, nothing observable changes for users.

**Files & call sites (exhaustive — change these, only these):**

- `crates/hrdr-tools/src/lib.rs`:
  - `ToolContext`: add `pub sandbox: std::sync::Arc<SandboxPolicy>`;
    `ToolContext::new` sets `Arc::new(SandboxPolicy::unconfined())` — the bare
    constructor stays unconfined **on purpose** (hundreds of tests build it
    against tempdirs; only `Agent::new` installs a real policy).
  - add methods:
    ```rust
    /// Resolve a model-supplied path for a READ, refusing it in Read mode
    /// when it falls outside the readable roots.
    pub fn resolve_read(&self, path: &str) -> anyhow::Result<PathBuf> {
        let shown = self.resolve(path);
        self.sandbox.check_read(&canonicalize_nearest(&shown), &shown)?;
        Ok(shown)
    }
    /// Resolve a model-supplied path for a WRITE/mutation, refusing it when
    /// it falls outside the writable roots.
    pub fn resolve_write(&self, path: &str) -> anyhow::Result<PathBuf> { … same with check_write … }
    ```
- swap call sites (`let path = ctx.resolve(...)` → `ctx.resolve_write(...)?` or
  `ctx.resolve_read(...)?`):
  - `tools/write.rs:44` → `resolve_write`
  - `tools/edit.rs:104` → `resolve_write`
  - `tools/fileops.rs:122–123` (`move` from AND to) → both `resolve_write`
    (moving a file OUT of the roots is also a mutation of the source)
  - `tools/fileops.rs:325` (`delete`) → `resolve_write`
  - `tools/fileops.rs:410–411` (`copy`) → from `resolve_read`, to
    `resolve_write`
  - `tools/read.rs:94` → `resolve_read`
  - `tools/grep.rs:132` and the two `.map(|p| ctx.resolve(p))` sites (~315/~413)
    → `resolve_read`. The ~315/~413 sites map over an **`Option`** (the optional
    `path` arg), not a list — the mechanical transform is
    ```rust
    let root = match a.path.as_ref() {
        Some(p) => ctx.resolve_read(p)?,
        None => ctx.cwd.clone(),
    };
    ```
    (replacing the `.map(…).unwrap_or_else(…)` chain).
  - `tools/ls.rs:42` → `resolve_read`
  - `tools/tree.rs:90` → `resolve_read`
  - `tools/lsp_nav.rs:134` → `resolve_read`
- NOT changed: `find.rs` (walks `ctx.cwd` only — always in-roots), `memory.rs`
  (its storage dirs are app infrastructure, not model paths), post-edit hooks /
  LSP / MCP subprocess spawns (user-configured, trusted), `git.rs` (read-only
  allow-list; its Read-mode leak is documented, §5).

**Tests** (put policy-driven tool tests in the affected tool files, building
`ToolContext` then overwriting `ctx.sandbox` with a real policy). **Policy
construction for every "outside" case:** a `for_agent` policy makes
`env::temp_dir()` writable, so a second tempdir is INSIDE the roots and never
refused. Build the confining policy as a struct literal instead — the fields are
`pub`:

```rust
let policy = SandboxPolicy {
    mode: SandboxMode::Write, // or Read
    writable_roots: vec![canonicalize_nearest(dir.path())],
    readable_roots: vec![canonicalize_nearest(dir.path())],
};
ctx.sandbox = std::sync::Arc::new(policy);
```

Then a sibling tempdir really is outside and the assertions below hold.

- `write_outside_roots_is_refused_and_names_the_roots` (write.rs) — Write mode,
  target under a second tempdir → Err containing "You may write only under"
  (capital Y — the exact §5 casing) and the cwd path; target under cwd → Ok.
- `edit_outside_roots_is_refused` (edit.rs).
- `move_copy_delete_are_guarded_on_the_mutating_side` (fileops.rs) — move
  out-of-roots destination refused; copy with out-of-roots SOURCE is allowed in
  Write mode (reads are free) but its destination is guarded; delete outside
  refused.
- `read_and_search_refuse_outside_roots_in_read_mode` (read.rs) — Read mode:
  read of `/etc/hostname` refused with the §5 read string; read under cwd ok;
  grep/ls/tree with an outside `path` arg refused (one assertion each, can live
  in their files).
- `scratch_and_tool_output_stay_writable_under_write_mode` (write.rs) — writes
  under `session_scratch_dir()` and `tool_output_dir()` succeed. (This one has
  no outside case — build its policy with `for_agent(Write, cwd, &[])`, which
  includes both dirs.)
- `mode_none_changes_nothing` (write.rs) — unconfined policy, write to a sibling
  tempdir succeeds (pins the default behavior until slice 4).

**Out of scope:** shell/watch, default flip, prompt.

Commit: `feat(tools): software sandbox path-guard on the file tools`

---

### Slice 4 — wire the policy into `Agent::new` and flip the default to `write`

**Goal:** the guard actually runs for real agents; `write` becomes the default;
CHANGELOG notes the behavior change.

**Files:**

- `crates/hrdr-agent/src/lib.rs`, in `Agent::new` beside `ctx.lsp = lsp;`
  (~1288):
  ```rust
  let sandbox_mode = crate::config::effective_sandbox(config.sandbox, config.read_only);
  ctx.sandbox = std::sync::Arc::new(hrdr_tools::SandboxPolicy::for_agent(
      sandbox_mode,
      &config.cwd,
      &config.sandbox_writable_roots,
  ));
  ```
- `crates/hrdr-agent/src/config.rs`: `Default for AgentConfig` →
  `sandbox: SandboxMode::Write`.
- `CHANGELOG.md` under `## [Unreleased]` → `### Changed` (or `Breaking`): "File
  tools and (on Linux, once the OS layer lands) shell commands are now sandboxed
  by default (`sandbox = "write"`): writes outside the working directory,
  temp/scratch, and tool-output dirs are refused. `--sandbox none` restores the
  previous full-access behavior." Run `prettier --write CHANGELOG.md`.

**Steps:** flip, then run the FULL test suite — any existing test that performed
an out-of-cwd write through a **real `Agent`** (not a bare `ToolContext`) will
surface here; fix each by pointing the write inside the test's cwd or setting
`config.sandbox = SandboxMode::None` in that test's config, whichever matches
the test's intent. Do not weaken the guard to make a test pass.

**Tests:**

- `agent_config_defaults_to_write_sandbox` (config.rs) — pins the default.
- `a_read_only_agent_gets_read_confinement` (lib.rs, near the existing
  Agent::new tests) — build an `Agent` with `read_only: true`, assert
  `ctx.sandbox.mode == SandboxMode::Read` (expose what's needed for the
  assertion via the ctx the test can already reach, or assert behaviorally: a
  `read` tool call outside cwd errors).

**Out of scope:** OS layer.

Commit: `feat(agent): enforce sandbox policy per agent; default write`

---

### Slice 5 — Linux bwrap wrap on the shell spawn (the real fix)

**Goal:** `shell` and `watch` children run inside bwrap per §3.6.1 when
`detect_backend() == Bwrap` and mode ≠ `None`.

**Files:**

- `crates/hrdr-tools/src/sandbox.rs`: `OsSandboxBackend`, `detect_backend`
  (§3.5, bwrap arm only — Landlock detection may land here too but its wrap path
  is slice 6), `sandboxed_shell_command` (§3.6; Landlock/None arms just return
  `shell.command(cmd_str)` for now, plus the Backend::None notice), the bwrap
  arg builder as its own pure function for testability:
  ```rust
  /// The full bwrap argv (excluding argv[0] "bwrap") for `mode`.
  fn bwrap_args(mode: SandboxMode, policy: &SandboxPolicy, cwd: &Path,
                shell: Shell, cmd_str: &str) -> Vec<std::ffi::OsString>
  ```
- `crates/hrdr-tools/src/tools/shell.rs` (~190):
  `let mut cmd = self.shell.command(&a.command);` →
  `let mut cmd = crate::sandbox::sandboxed_shell_command(self.shell, &a.command, &ctx.sandbox, &ctx.cwd);`
  (`&ctx.sandbox` deref-coerces from `&Arc<SandboxPolicy>`; the
  `cmd.current_dir(&ctx.cwd)` line below it stays.)
- `crates/hrdr-tools/src/tools/watch.rs` (~168): same swap in `run_check`
  (`sandboxed_shell_command(shell, command, &ctx.sandbox, &ctx.cwd)`).

**Tests** (in `sandbox.rs`; arg-builder tests are pure and run everywhere,
end-to-end tests are gated):

- `bwrap_write_args_are_exactly_the_spec` — pure: assert the §3.6.1 Write argv
  verbatim for a policy with two writable roots (order included: `--ro-bind / /`
  precedes every `--bind`).
- `bwrap_read_args_omit_rw_binds_and_private_tmp` — pure: Read argv has no
  `--bind`, has `--tmpfs /tmp` **positioned before the cwd/scratch/tool-output
  `--ro-bind`s** (assert the index ordering — a tmpfs mounted after them shadows
  any of them living under `/tmp`, §3.6.1), and the /bin compat entry matches
  this machine's `read_link("/bin")`.
- End-to-end, all `#[cfg(target_os = "linux")]` `#[tokio::test]`, each starting
  with the repo's canonical skip guard (mirrors grep.rs:653):

  ```rust
  if crate::sandbox::detect_backend() != crate::sandbox::OsSandboxBackend::Bwrap {
      return; // best-effort: exercise the real backend when available
  }
  ```

  - `shell_write_outside_roots_hits_a_readonly_fs` — Write mode, run `ShellTool`
    with `echo x > <other_tempdir>/f`; output contains `Read-only file system`
    and the file does not exist. **Policy: struct-literal with
    `writable_roots: vec![cwd]` only** — a `for_agent` policy binds
    `env::temp_dir()` rw and the other tempdir would then be writable (same trap
    as slice 3; the argv builder iterates `policy.writable_roots`, so the
    hand-built policy emits no `/tmp` bind).
  - `shell_write_in_cwd_and_tmp_succeeds_under_bwrap` — both writes land
    (`for_agent` policy — this one wants the real default root set).
  - `worktree_commit_succeeds_but_parent_commit_is_blocked` — build repo +
    linked worktree (skip if no git); **struct-literal policy: `writable_roots`
    = `[worktree]` + `git_metadata_roots(worktree)`, each
    `canonicalize_nearest`-ed** (the repo lives in a tempdir, so a `for_agent`
    policy's `/tmp` root would make the parent writable and void the test);
    inside the worktree `git add -A && git commit -m x` **succeeds** (proves
    §3.3 — set `user.email`/`user.name` via `-c` flags);
    `git -C <parent> commit --allow-empty -m x` **fails** (EROFS on the parent
    `index.lock`). Both outcomes runtime-confirmed with exactly these roots.
  - `read_mode_cannot_even_see_outside_paths` — Read mode, `ls /home` fails with
    `No such file or directory` (ENOENT — `/home` is unmounted; do NOT probe
    under `/etc` or `/usr`, those are deliberately readable in Read mode),
    `ls <cwd>` works.
  - `timeout_kill_reaches_through_bwrap` — copy the existing
    `timeout_kills_the_whole_process_tree_not_just_the_leader` shape (shell.rs
    ~679) but with a confined ctx; the grandchild must die.

**Out of scope:** Landlock wrap, notices for Landlock (slice 6), seccomp,
bundling bwrap.

Commit: `feat(tools): bwrap-confine shell/watch children on Linux`

---

### Slice 6 — Landlock fallback + the full degradation-notice chain

**Goal:** hosts without bwrap/userns still get write confinement; every
degradation says so exactly once, in the agent event stream.

**Files:**

- `crates/hrdr-tools/Cargo.toml`: the `landlock = "0.4"` target dep (§3.6.2).
- `crates/hrdr-tools/src/sandbox.rs`: the Landlock `pre_exec` arm of
  `sandboxed_shell_command` (§3.6.2), notice emission per §5 at each degrade
  point. To make the Landlock path testable on machines where bwrap exists,
  factor the arm as
  `fn shell_command_with_backend(backend, shell, cmd_str, policy, cwd) -> Command`
  and have `sandboxed_shell_command` call it with `detect_backend()`.
- `crates/hrdr-agent/src/turn_loop.rs`: the drain line (§3.4) beside the
  existing `take_client_warning` drain.

**Tests:**

- `landlock_blocks_writes_outside_roots` — `#[cfg(target_os = "linux")]`, guard:
  `if !std::fs::read_to_string("/sys/kernel/security/lsm").unwrap_or_default().contains("landlock") { return; }`
  — call `shell_command_with_backend(Landlock, …)` directly with a
  struct-literal policy (`writable_roots: vec![cwd]` only — the slice-3/5 temp
  trap again: a `for_agent` policy makes a sibling tempdir writable), run
  `echo x > <outside>/f`: fails (EACCES); write under cwd succeeds.
- `read_mode_under_landlock_degrades_with_a_notice` — forced-Landlock command in
  Read mode → `take_sandbox_notice()` returns the §5 string.
- `no_backend_emits_the_not_confined_notice_once` — force Backend::None arm
  twice → exactly one notice.
- turn_loop side: extend whichever existing turn-loop test harness already
  asserts `AgentEvent::Notice` (see the `cost budget` assertions, lib.rs ~8703)
  with `sandbox_notice_reaches_the_event_stream` — seed `set_sandbox_notice`,
  run a turn, assert a `Notice` containing "sandbox:".

**Out of scope:** seccomp/network, macOS.

Commit: `feat(tools): landlock fallback + sandbox degradation notices`

---

### Slice 7 — prompt declaration of mode + writable roots

**Goal:** the model is TOLD its boundary, positively ("you may write under X,
Y"), per agent.

Codex interpolates `permissions_instructions` into its template; hrdr has **no
template engine** — the equivalent is a runtime-built section pushed into
`SystemPrompt`, exactly like `environment_section`.

**Position (cache-critical):** `SECTION_SANDBOX` goes **after
`SECTION_ENVIRONMENT`, dead last**. Rationale you must not "improve": the
section names the writable roots, which include the per-agent worktree cwd — it
is exactly as volatile as the Environment block's `Working directory` line. The
prompt cache split is computed as `prefix_len_before(SECTION_ENVIRONMENT)`
(lib.rs ~1081) — everything from Environment onward is the volatile tail, so
appending after Environment leaves the split and every cached prefix byte
untouched. Placing it any earlier would push per-agent bytes into the shared
prefix and cost the cache everything below them (see the ordering doc-comment on
`render_system` in `prompt.rs`).

**Files:**

- `crates/hrdr-agent/src/prompt.rs`:
  - `pub const SECTION_SANDBOX: &str = "sandbox";` (append after
    `SECTION_ENVIRONMENT` in the constants block).
  - ```rust
    /// The sandbox declaration — mode + concrete writable roots — as a prompt
    /// section. Empty (→ dropped by `SystemPrompt::push`) when mode is None.
    /// Volatile tail: the roots name the per-agent cwd, so this must stay
    /// BELOW the environment section (see the cache note on `render_system`).
    pub fn sandbox_section(policy: &hrdr_tools::SandboxPolicy) -> String
    ```
    Rendered text, verbatim (`{roots}` = one `- <path>` line per writable root;
    Read mode swaps in the readable roots and the read sentence):
    ```
    \n\nSandbox:\n- Mode: write — reads are unrestricted; writes are enforced by the OS and the tools.\n- You may write ONLY under:\n{roots}\n- Writing anywhere else is refused. If a task appears to require writing outside these roots, stop and say so instead of attempting it.
    ```
    ```
    \n\nSandbox:\n- Mode: read — this agent is read-only.\n- You may read ONLY under:\n{roots}\n- Reads elsewhere and all writes are refused.
    ```
- `crates/hrdr-agent/src/lib.rs`:
  - `build_system_prompt_sections` / `build_system_prompt`: add a
    `sandbox: &hrdr_tools::SandboxPolicy` parameter; push
    `p.push(SECTION_SANDBOX, prompt::sandbox_section(sandbox));` immediately
    after the `SECTION_ENVIRONMENT` push (~1061). `build_system_prompt` has
    **three** call sites and the policy must be threaded through every one:
    `Agent::new` (~1350 — the `ctx.sandbox` Arc built in slice 4 is in scope;
    pass `&ctx.sandbox`), `refresh_system_prompt_in_place` (~1575) and
    `refresh_system` (~1619) — both methods on `Agent`, so pass
    `&self.ctx.sandbox` there (the resume/`/clear`/`set_cwd` rebuild paths; miss
    one and the sandbox section silently drops out of the prompt after a
    resume).
  - also update the assembly-order doc-comment on `render_system` (`prompt.rs`,
    the numbered list ending "8. **Environment** … dead last"): add "9.
    **Sandbox**" and move the "dead last" wording — otherwise the doc-comment
    contradicts the shipped order.
  - update the three tests that call `build_system_prompt_sections`
    (~4248/~4292/~4319) to pass `&SandboxPolicy::unconfined()` (or a real one
    where the test asserts sandbox content).
  - update `system_prompt_is_ordered_least_volatile_first` (~4242): with a
    confined policy the last two names are
    `[…, SECTION_ENVIRONMENT, SECTION_SANDBOX]`; with an unconfined policy the
    section is absent and `SECTION_ENVIRONMENT` stays last (empty bodies are
    dropped by `push` — assert both).

**Tests:**

- `sandbox_section_names_mode_and_every_writable_root` (prompt.rs) — Write
  policy with 2 roots → both paths present, text starts with "Sandbox:",
  contains "write ONLY under".
- `sandbox_section_is_empty_for_mode_none` (prompt.rs).
- the updated ordering test above.

**Out of scope:** changing the cache split anchor — it stays
`SECTION_ENVIRONMENT`.

Commit:
`feat(agent): declare sandbox mode + writable roots in the system prompt`

---

### Slice 8 — macOS Seatbelt layer

**Goal:** the §3.6 wrapper's macOS arm: generated SBPL profile +
`/usr/bin/sandbox-exec`.

**Files:** `crates/hrdr-tools/src/sandbox.rs` — a `#[cfg(target_os = "macos")]`
arm in `sandboxed_shell_command`, plus a pure
`fn seatbelt_profile(mode, policy) -> String`.

Profile, verbatim (Write mode; `{allow_writes}` = one `(subpath "<root>")` per
writable root, paths with `"` escaped):

```
(version 1)
(deny default)
(allow process-fork)
(allow process-exec*)
(allow signal)
(allow sysctl-read)
(allow mach-lookup)
(allow ipc-posix*)
(allow file-read*)
(allow file-write* {allow_writes})
(allow network*)
```

Read mode replaces the last three lines with:

```
(allow file-read* (subpath "/usr") (subpath "/bin") (subpath "/sbin")
  (subpath "/System") (subpath "/Library") (subpath "/private/etc")
  (subpath "/dev") {allow_reads})
(allow network*)
```

(`{allow_reads}` = the readable roots. Network stays allowed — the network axis
is the same follow-up as on Linux.) Invocation:
`/usr/bin/sandbox-exec -p <profile> -- <shell.program()> -c <cmd_str>`, cwd and
stdio exactly as before. `sandbox-exec` is deprecated-but-present; if
`/usr/bin/sandbox-exec` is missing, fall to the software layer with the §5 "not
OS-confined" notice.

**Tests:** `seatbelt_profile_lists_every_writable_root` (pure, runs everywhere);
`#[cfg(target_os = "macos")]`
`shell_write_outside_roots_is_denied_under_seatbelt` with skip guard
`if !std::path::Path::new("/usr/bin/sandbox-exec").exists() { return; }` — write
outside fails ("Operation not permitted"), write in cwd succeeds.

Commit: `feat(tools): macOS seatbelt layer for shell confinement`

---

### Slice 9 — release hygiene (no new behavior)

Re-read this doc top to bottom against the shipped code; fix drift in the doc
(it is now the reference); ensure the CHANGELOG entry from slice 4 still
describes the final behavior (now including Linux/macOS OS-level confinement);
`prettier --write` both files. Delete the "Sub-agent isolation guard" backlog
bullet from `docs/deferred-improvements.md` (this work supersedes it) and note
the follow-ups below there if not already present.

Commit: `docs(sandbox): sync spec + changelog with shipped sandbox`

---

**Declared follow-ups (NOT scheduled, do not build):** seccomp network axis;
bundled bwrap binary (Codex `bundled_bwrap.rs`); curated read allow-list for
`Write` mode (closes shell secret-reads); Windows AppContainer/restricted-token;
friendly shell-command pre-flight parser (a heuristic in front of bwrap, never
instead of it); `git` tool Read-mode subprocess confinement.

---

## 5. Failure modes and exact strings

### What blocking looks like, per layer

| Scenario                                                           | Layer             | What the model sees                                                                                                                                                                       |
| ------------------------------------------------------------------ | ----------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `write`/`edit`/`move`/`copy`-dest/`delete` outside roots           | software guard    | tool error, the write-refusal string below                                                                                                                                                |
| `read`/`grep`/`ls`/`tree`/`lsp` outside roots in `Read` mode       | software guard    | tool error, the read-refusal string below                                                                                                                                                 |
| shell `echo x > /parent/f`, `git -C /parent commit` (Write, bwrap) | kernel (bwrap)    | the command's own stderr: `…: Read-only file system` (EROFS, os error 30); nonzero `[exit status: …]` appended as usual                                                                   |
| shell `cat /home/<u>/.ssh/id_rsa` (Read, bwrap)                    | kernel (bwrap)    | `No such file or directory` — `/home` is not mounted at all (ENOENT beats EROFS). NB `/usr` and `/etc` ARE readable in Read mode (§3.6.1)                                                 |
| shell write outside roots (Landlock fallback)                      | kernel (Landlock) | `…: Permission denied` (EACCES)                                                                                                                                                           |
| shell read outside cwd (Read mode, Landlock fallback)              | — (leak)          | succeeds; the degradation notice below was emitted                                                                                                                                        |
| `git` tool with a path outside cwd (Read mode)                     | — (leak, v1)      | succeeds (subprocess, read-only subcommands only) — documented, follow-up                                                                                                                 |
| cold `cargo build`/`npm install` in a worktree (Write)             | kernel            | EROFS on `~/.cargo`/`~/.npm` — user remedy: `sandbox_writable_roots = ["/home/<user>/.cargo", …]` (must be absolute — a literal `~` is rejected by validate) or `--sandbox none`          |
| tool binary outside `/usr` in Read mode                            | kernel (bwrap)    | `command not found`                                                                                                                                                                       |
| `git commit` inside the worktree (Write)                           | —                 | **succeeds** (via §3.3 metadata roots); a trailing `error: Unable to create '….git/packed-refs.lock': Read-only file system` from ref maintenance is possible and harmless (exit stays 0) |

### Software-guard error strings (byte-exact; positive allow-list)

Write refusal (`{roots}` = the writable roots joined with `", "`):

```
sandbox: refusing to write {path} — it is outside this agent's writable roots. You may write only under: {roots}. Keep work inside your working directory; use the scratch dir for throwaway files.
```

Read refusal (`Read` mode; `{roots}` = readable roots joined with `", "`):

```
sandbox: refusing to read {path} — this agent is read-only and may read only under: {roots}.
```

### Degradation notices (byte-exact; each emitted at most once per process)

bwrap binary missing (→ Landlock):

```
sandbox: bwrap not found — falling back to Landlock: writes are still confined, but reads are not, and read-mode agents degrade to write-mode confinement for shell commands. Install bubblewrap for full confinement.
```

user namespaces disabled (→ Landlock):

```
sandbox: unprivileged user namespaces are disabled on this system — falling back to Landlock: writes are still confined, but reads are not, and read-mode agents degrade to write-mode confinement for shell commands.
```

Read mode under Landlock (emitted in addition, when a Read-mode agent runs a
shell command on the Landlock backend):

```
sandbox: Landlock cannot confine reads — this read-only agent's shell commands are write-confined only.
```

no OS backend at all (Linux without either; macOS pre-slice-8 or missing
sandbox-exec; Windows):

```
sandbox: no OS-level sandbox is available on this system — shell commands are NOT OS-confined; the file tools remain guarded. Use --sandbox none to silence this.
```

---

## 6. Test matrix

Write tests from this table; every row is a named test in the slices above
(column 4). Platform "linux*" = `#[cfg(target_os = "linux")]` + the
`detect_backend()` / lsm-file skip guard from §4; "macos*" likewise with the
sandbox-exec existence guard; "all" = no cfg, no guard.

| #   | Behavior                                                                 | Platform           | Test (slice)                                                                                                                  | Expected                                          |
| --- | ------------------------------------------------------------------------ | ------------------ | ----------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------- |
| 1   | mode parsing (str/serde), garbage rejected                               | all                | `sandbox_mode_parses_all_spellings_and_rejects_garbage` (S1) + `sandbox_config_key_parses_and_bad_value_is_a_hard_error` (S2) | parse ok / listed-values error                    |
| 2   | scratch dir private, stable, under temp                                  | all                | `session_scratch_dir_is_private_stable_and_under_temp` (S1)                                                                   | 0700 (unix), same path twice                      |
| 3   | policy roots incl. cwd/temp/scratch/tool-output; refusals name roots     | all                | `policy_write_roots_cover_cwd_temp_scratch_and_tool_output` (S1)                                                              | allow/deny per §2.2, message per §5               |
| 4   | symlink + `..` escapes caught                                            | all                | `symlink_and_dotdot_escapes_are_caught` (S1)                                                                                  | refused                                           |
| 5   | linked worktree yields exactly the four §3.3 metadata roots              | all (skip: no git) | `git_metadata_roots_for_a_linked_worktree` (S1)                                                                               | 4 roots; plain checkout → none                    |
| 6   | env/flag/file plumbing + decision table                                  | all                | S2's four tests                                                                                                               | per §2.3/§2.4                                     |
| 7   | file tools guarded (write/edit/move/copy/delete/read/grep/ls/tree)       | all                | S3's six tests                                                                                                                | per §5 strings; None mode unchanged               |
| 8   | default flips to `write`; read-only agent → Read confinement             | all                | S4's two tests                                                                                                                | pins defaults                                     |
| 9   | bwrap argv verbatim (Write and Read)                                     | all                | `bwrap_write_args_are_exactly_the_spec`, `bwrap_read_args_omit_rw_binds_and_private_tmp` (S5)                                 | §3.6.1 exactly                                    |
| 10  | shell write outside roots blocked; inside + /tmp allowed                 | linux\*            | `shell_write_outside_roots_hits_a_readonly_fs`, `shell_write_in_cwd_and_tmp_succeeds_under_bwrap` (S5)                        | EROFS / success                                   |
| 11  | worktree commit works; parent-repo commit blocked                        | linux\*            | `worktree_commit_succeeds_but_parent_commit_is_blocked` (S5)                                                                  | the point of the whole feature                    |
| 12  | Read mode hides the FS outside roots                                     | linux\*            | `read_mode_cannot_even_see_outside_paths` (S5)                                                                                | ENOENT                                            |
| 13  | timeout group-kill still works through bwrap                             | linux\*            | `timeout_kill_reaches_through_bwrap` (S5)                                                                                     | grandchild dead                                   |
| 14  | Landlock blocks outside writes; allows cwd                               | linux (lsm guard)  | `landlock_blocks_writes_outside_roots` (S6)                                                                                   | EACCES / success                                  |
| 15  | degradation notices: read-degrade, none-backend, once-only, reach events | all                | S6's three notice tests + `sandbox_notice_reaches_the_event_stream`                                                           | §5 strings, exactly once, as `AgentEvent::Notice` |
| 16  | prompt section content, emptiness in None, ordering + cache split intact | all                | S7's tests + updated `system_prompt_is_ordered_least_volatile_first`                                                          | `SECTION_SANDBOX` last; split anchor unchanged    |
| 17  | seatbelt profile lists roots; denies outside writes                      | all / macos\*      | S8's two tests                                                                                                                | profile text / Operation not permitted            |

---

## 7. Pitfalls for the implementer (read before every slice)

1. **Never sandbox the hrdr process itself.** No Landlock `restrict_self`, no
   prctl, outside a child `pre_exec`. hrdr does its own I/O (sessions in
   `~/.local/share`, config, memory) in-process; confining it breaks the app.
2. **The writable set is cwd + temp + scratch + tool-output + git metadata, all
   of them.** Drop temp and every compiler/linker dies; drop tool-output/scratch
   and overflow-spill or scratch work breaks; drop the git metadata roots (§3.3)
   and every write sub-agent commit fails. Equally: do NOT widen to the whole
   parent `.git` — that re-opens the escape.
3. **`Read` mode degrades under Landlock** (write-confinement only) — that is
   expected, decided, and must be loudly noticed (§5), never silent, never
   "fixed" by blocking the spawn.
4. **Never silently pretend to sandbox.** Any arm that ends up running an
   unconfined command while mode ≠ None must have set a §5 notice first.
5. **Trust cargo, not rust-analyzer.** In this repo rust-analyzer diagnostics
   are frequently stale; the merge gate is `cargo fmt --all` +
   `cargo clippy --all-targets -- -D warnings` + `cargo test`, nothing else.
6. **Known flake:** `background_subagent_records_its_own_transcript`
   (`crates/hrdr-agent/src/lib.rs` ~10133) can miss its poll deadline on a
   loaded machine. If it is the only failure and your change is nowhere near
   delegation transcripts, rerun before touching anything.
7. **Prompt cache:** `SECTION_SANDBOX` sits AFTER `SECTION_ENVIRONMENT`, never
   above it or above persona/memory — the roots contain the per-agent worktree
   path; putting them earlier bills every agent for a cold cache. The cache
   split anchor stays `SECTION_ENVIRONMENT`.
8. **bwrap argv order is semantics** (later mounts shadow earlier ones): in
   Write mode `--ro-bind / /` must precede the rw `--bind`s; in Read mode
   `--tmpfs /tmp` must precede the cwd/scratch/tool-output `--ro-bind`s (the
   scratch dir lives under `/tmp` — a tmpfs mounted after it hides it,
   runtime-confirmed); and the terminal `--` must precede the shell argv.
9. **/bin is a symlink on usr-merged distros** — Read mode must emit
   `--symlink <read_link(p)> <p>`, not `--ro-bind /bin /bin` (which manufactures
   a real directory and breaks the merge) and not a guessed `usr/<basename>` (on
   Arch `/sbin → usr/bin`, not `usr/sbin`). Detect per-path with
   `symlink_metadata`, read the target with `fs::read_link`.
10. **`ToolContext::new` stays unconfined.** Only `Agent::new` installs a real
    policy. Do not "helpfully" default the bare constructor to `Write` — it will
    break hundreds of unrelated tool tests and violate the wiring plan.
11. **Guard model-supplied paths only.** Memory-tool storage, overflow-spill
    writes, hook/LSP/MCP subprocesses are app infrastructure — they bypass
    `resolve_read`/`resolve_write` by design.
12. **Keep full env passthrough in bwrap** (no `--clearenv`). PATH, HOME,
    CARGO_HOME must survive; hiding env secrets is a declared non-goal of v1.
13. **`pre_exec` is `unsafe` and post-fork:** only syscalls (landlock, prctl)
    inside; no allocation-heavy std machinery, no locks shared with the parent;
    move everything the closure needs into it beforehand.
14. **rtk prefix does not apply to code** — test commands inside Rust tests
    spawn real binaries (`git`, `bwrap`) directly.
15. **CI platforms bite:** Windows/macOS runners compile everything with
    `-D warnings` — `cfg(target_os = "linux")` items must not leave unused
    imports/vars on other platforms (put OS-specific `use` inside the
    `cfg`-gated module/fn). CI runners may lack `bwrap`, `rg`, even `git` in odd
    images — every gated test carries its skip guard.
