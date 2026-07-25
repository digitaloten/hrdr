# OS sandbox — filesystem confinement for agents

Status: **design** (not yet implemented). Motivated by a delegated non-Claude
sub-agent that `cd`'d out of its worktree into the parent repo and committed to
`main`. Guidance alone only reaches models inclined to obey; hrdr runs arbitrary
models, so it needs an **enforced** boundary — what Codex has by default and
Claude Code has opt-in.

## Resume plan (start here — refinements from review)

The full cross-platform sandbox below is the ambition; the **actual problem** is
narrow (one non-Claude write sub-agent, on Linux, `cd`'d out via `shell` and
committed to `main`). Don't boil the ocean. In priority order:

- **Guidance clause — DONE** (`system.j2`, write sub-agents). Handles steerable
  models.
- **Software path-guard (slice 2)** — closes the `write`/`edit`/`delete` **file
  tool** vector. Portable, days of work. **Does NOT catch the `shell` escape.**
- **bwrap wrap on Linux (the real fix for the observed failure)** — the `shell`
  → `cd /parent && git commit` escape is a subprocess, so only an OS sandbox on
  the shell child catches it. **Shell out to the system `bwrap`** (bundle
  later); do **not** reimplement it (weeks of security-critical code). Days.
  Landlock via the `landlock` crate is the fallback where unprivileged user
  namespaces are disabled.
- **macOS = seatbelt profile + `sandbox-exec`** (days, deprecated-but-works).
  **Windows = software-layer only** — real Windows sandboxing (AppContainer /
  restricted tokens) is weeks and the guarantee is weak; skip it for v1, like
  Claude Code and hrdr today.

Which layer catches which vector (the crux):

| Vector                                              | Caught by                     |
| --------------------------------------------------- | ----------------------------- |
| `write`/`edit`/`delete` tool with a parent path     | software path-guard (slice 2) |
| `shell` → `cd /parent && git commit` (**observed**) | bwrap/landlock (Linux)        |
| TOCTOU-proof, all shells/interpreters/redirects     | bwrap/landlock (kernel)       |

A shell **command parser** (split on `&&`/`;`/`\|`) can catch the _honest_ shell
escapes with a nice message, but it is a heuristic, not a boundary: command
substitution, variables, nested interpreters (`python -c os.system(...)`), and
non-git write vectors evade it. Use it as a friendly pre-flight in front of
bwrap, never instead of it.

**Off-ramp:** hrdr runs Claude models almost always and has seen this exactly
once (a deepseek delegate). Shipping only the guidance clause and building the
bwrap wrap **the day writes to non-Claude models become routine** is a
defensible call. Don't build a boundary for a threat you rarely run.

Reference implementation: Codex (`~/Projects/harness/codex/codex-rs`) —
`linux-sandbox/` (bwrap primary + seccomp, Landlock backup, a
`codex-linux-sandbox` helper binary, bundled bwrap),
`sandboxing/src/seatbelt.rs` (macOS), `windows-sandbox-rs/` (Windows). Its
Landlock ruleset is exactly the "read-all, write-roots" model below.

## Goal

Confine an agent's filesystem access to its **working directory** plus a
**dedicated per-session scratch dir**, enforced by the OS (not just the prompt),
**on by default**. A write sub-agent's cwd is its worktree, so the same
mechanism makes the parent repo unwritable — the escape becomes impossible, not
merely discouraged.

## `SandboxMode`

```rust
/// How much of the filesystem an agent may touch. Enforced by the OS for shell
/// children and by a software path-guard for the in-process file tools.
pub enum SandboxMode {
    /// No confinement — full read/write everywhere. The pre-sandbox behavior.
    None,
    /// Read broadly (builds need /usr, toolchains, ~/.cargo, …); write ONLY
    /// within the writable roots (cwd + session scratch + tool-output dir).
    Write,
    /// Read ONLY within the readable roots (cwd + session scratch); no writes
    /// anywhere. For read-only / research agents.
    Read,
}
```

Default: **`Write`** (a coding agent must write in its cwd; it must read the
system to build). `None` is the explicit opt-out.

### Root sets per mode

| Mode  | Readable                 | Writable                                  |
| ----- | ------------------------ | ----------------------------------------- |
| None  | everything               | everything                                |
| Write | everything¹              | `{cwd, session_scratch, tool_output_dir}` |
| Read  | `{cwd, session_scratch}` | — (none)                                  |

¹ **Broad reads in `Write` are a deliberate tradeoff** (matches Codex
`workspace-write`): builds/toolchains read all over the FS, and enumerating
every ecosystem's read roots is fragile. The cost is that a shell command can
_read_ `~/.ssh`, `~/.aws/credentials`, etc. Mitigations: the file tools keep
`guard_secret_read` (already blocks reading known secret files in-process), and
a later refinement can Landlock-allow a curated read set (system dirs +
toolchain caches) instead of `/` to also close shell secret-reads. Flagged, not
solved, in v1.

## The session scratch dir

`/tmp/hrdr.<random>/`, created once at session start (mode `0700`), removed at
session end. It is a writable root in `Write`/`Read`-relevant modes so the agent
has a scratch area outside the project tree. Distinct from the existing
`tool_output_dir` (where `shell`/`grep`/`git` spill overflow) — **both** must be
writable roots in `Write` mode or overflow-spill breaks under the sandbox.

Sub-agents share the session scratch (they are one session); each write
sub-agent's _cwd_ root is its own worktree, so their writable sets are
`{own worktree, shared scratch, tool_output_dir}` — mutually isolated on the
project tree, shared only on throwaway scratch.

## Two enforcement layers (this is the crux)

hrdr is a single process doing both the agent's tool I/O **and** the app's own
I/O (sessions in `~/.local/share`, config, memory). We cannot Landlock the whole
process — it would break the app. So enforcement is split by where the I/O
happens:

### 1. OS sandbox — for `shell` children (the untrusted-command vector)

Applied to each spawned command, not to hrdr itself, so the app is unaffected.
**Mirror Codex** (`~/Projects/harness/codex/codex-rs`): one policy (writable
roots + read/network access) behind a **per-OS backend**, each delegating to
that OS's kernel mechanism. There is no userspace-only boundary — a child makes
its syscalls straight to the kernel, so enforcement must sit **below** it
(kernel or VM). Do NOT reimplement the mechanisms; drive the OS's own.

**Linux — bubblewrap primary, Landlock fallback, seccomp for network.**

- **Primary: bubblewrap (`bwrap`).** Reconstruct the child's filesystem view
  with Linux mount + user + pid namespaces (unprivileged — no root): bind-mount
  the writable roots read-write, the rest read-only, nothing else visible.
  Invoke it as a wrapper, like Codex: build the arg list and run
  `bwrap <opts> -- <shell> -c <command>`. For `Write` mode: `--ro-bind / /`
  (read-all) `--bind <worktree> <worktree>` `--bind <scratch> <scratch>`
  `--bind <tool_output_dir> …` `--proc /proc` `--dev /dev` `--unshare-pid`
  (`--unshare-net` when network denied) `--chdir <worktree>`. A `git commit` in
  the parent then hits a **read-only mount → `EROFS`**, however it's reached
  (`cd`, `eval`, `python -c`, a redirect) — the kernel enforces the _effect_,
  not the command syntax. For `Read` mode, DON'T `--ro-bind / /`; bind only
  `<worktree>` + the specific tool dirs (`/usr`, `/lib`, `/bin` ro), so the rest
  of the FS isn't even readable (Landlock cannot do this — see below). **Don't
  reimplement bwrap** (weeks of security-critical code); shell out to the system
  binary, and **bundle** one later (Codex ships `bundled_bwrap`) for hosts
  without it. Caveat: needs unprivileged user namespaces enabled (some hardened
  distros disable them → fall back).
- **Fallback: Landlock** (`landlock` crate), applied via a `pre_exec` closure in
  the child, for hosts where unprivileged user namespaces are off or `bwrap` is
  missing. Ruleset = Codex's (`linux-sandbox/src/landlock.rs`): `handle_access`
  all rights; read-only on `/`; read-write on `/dev/null` + the writable roots;
  `restrict_self()`. ~15 lines, kernel ≥ 5.13. **Limit:** Landlock can't cleanly
  restrict _reads_, so `Read` mode degrades to `Write`-with-notice under the
  Landlock fallback — exactly why Codex made bwrap primary.
- **Network + hardening: seccomp** (`seccompiler`, as Codex) — deny network
  syscalls for `Read`/no-network, plus `no_new_privs`. The hook lives here;
  wiring the network axis is the deferred follow-up.

**macOS — Seatbelt (bespoke; mirror Codex `sandboxing/src/seatbelt.rs`).**
Generate an SBPL profile — `(deny default)`, `(allow file-read* (subpath "/"))`,
`(allow file-write* (subpath "<worktree>") (subpath "<scratch>"))`, network
clauses — and run under `/usr/bin/sandbox-exec -p <profile> -- <command>` (pin
the `/usr/bin` path for tamper-resistance, like Codex). No namespace equivalent
exists, so Seatbelt is the ceiling on macOS; it's deprecated-but-present. `Read`
mode drops the broad `file-read*` and allow-lists only the needed subpaths.

**Windows — restricted token / AppContainer (bespoke; mirror Codex
`windows-sandbox-rs/`).** The hard one, weakest guarantee: no namespaces, no
clean "confine writes to a dir" primitive. Either a **restricted token**
(`CreateRestrictedToken`, drop write SIDs) or an **AppContainer** with explicit
filesystem-capability grants for the writable roots, plus a Job Object. Codex
needed a whole crate for it; multi-week. **Out of v1** — Windows runs
software-layer + command-heuristic only, with a notice that shell isn't
OS-confined. Not a regression (no sandbox today).

**Fallback (any OS, unsupported / old kernel):** skip the OS layer, keep the
software layer, and surface once that shell commands are **not** OS-confined —
never silently pretend to sandbox.

### 2. Software path-guard — for the in-process file tools

`read`/`write`/`edit`/`move`/`copy`/`delete`/`ls`/`grep`/`find`/`tree` do their
I/O in the hrdr process, so the OS sandbox above does not touch them. They get a
mode-aware check at path resolution (the natural home is
`ToolContext::resolve` + the existing `guard_secret_*` seam):

- resolve + canonicalize the path (reuse the existing symlink-safe canonicalize,
  so a `..`/symlink escape is caught — the removed cwd-confinement code is the
  starting point),
- **write op** (`write`/`edit`/`move` dest/`delete`/`copy` dest): reject if the
  canonical path is not under a writable root,
- **read op** in `Read` mode: reject if not under a readable root,
- corrective error naming the roots, so the model self-corrects (Codex's
  positive-declaration lesson: say what IS allowed, not just what isn't).

This layer is also the only enforcement on Windows and in the Landlock-fallback
case, so it must be correct on its own, not merely a nicety.

## Composition with worktree isolation

This is the payoff. A write sub-agent's `cfg.cwd` is its worktree
(`<repo>/.hrdr/worktrees/wt-…`). Under `Write` mode:

- **OS layer:** the shell child can only write under the worktree + scratch, so
  `cd <repo> && git commit` cannot write the parent's index/objects — blocked.
- **Software layer:** `write`/`edit`/`touch`-via-tool against a parent path is
  rejected with a message.

The worktree-escape that started this whole thread becomes structurally
impossible for any model, Claude or not — which is the point.

## Telling the model (Codex's lesson)

Declare the boundary in the system prompt, interpolated like Codex's
`permissions_instructions`: the active `SandboxMode` and the concrete writable
roots ("you may write only within `<cwd>` and `<scratch>`; writing elsewhere is
refused"). A positive allow-list anchored to real paths beats the negative
"don't cd to the parent" clause — the model checks "is this under my root?"
rather than enumerating escapes. Keep the worktree clause too; belt and
suspenders.

## Configuration

- `AgentConfig.sandbox: SandboxMode` (default `Write`).
- Config file: `sandbox = "write" | "read" | "none"`.
- Flag: `--sandbox <mode>` and a `--no-sandbox` alias for `none`.
- Env: `HRDR_SANDBOX=write|read|none`.
- Per-agent: a read-only sub-agent is forced to (at most) `Read` regardless of
  the session default — it has no write tools anyway, so `Read` is the natural
  fit; a write sub-agent inherits the session mode (min of session mode and
  `Write`).

## Out of scope for v1 (follow-ups)

- **Network sandboxing.** Codex also confines network; hrdr's `web`/`fetch`
  tools are in-process (guarded by `web.rs` SSRF checks today). A network mode
  on `SandboxMode` (or a separate axis) is a later addition.
- **Curated read allow-list** for `Write` (close shell secret-reads) — see
  footnote 1.
- **`danger_full_access` parity** — `None` already covers it.

## Implementation slices

1. `SandboxMode` enum + `AgentConfig.sandbox` + config/flag/env plumbing +
   session scratch dir creation/teardown. No enforcement yet (default `None` so
   nothing changes) — just the wiring, tested.
2. Software path-guard in `ToolContext` (writable/readable roots + resolve-time
   check) for the file tools. Flip default to `Write`. Tests: write outside cwd
   refused; read outside cwd refused in `Read`; scratch + tool_output writable.
3. **Linux `bwrap` wrap** on the shell spawn — writable-root bind-mounts, shell
   out to the system binary. This is the real fix for the observed shell escape.
   **Landlock fallback** (`pre_exec`) where user namespaces are off; graceful
   degrade + the "not OS-confined" notice. Tests behind a Linux cfg.
4. Prompt declaration of mode + writable roots (interpolated, Codex-style).
5. macOS Seatbelt layer (`sandbox-exec` + generated profile). Windows stays
   software-layer only.
6. (later) seccomp network axis; bundle a `bwrap` binary for hosts without one.

Slice 1–2 give the software boundary (works everywhere, closes the file-tool
vector immediately); slice 3 adds the OS hard-floor for shell on Linux, which is
where the escape actually happened.

## No-migration note (pre-1.0)

New config key with a default; existing sessions/configs unaffected. Turning the
default to `Write` is a behavior change (writes outside cwd now refused) — call
it out in CHANGELOG under Changed/Breaking, and `--sandbox none` restores the
old full-access behavior.
