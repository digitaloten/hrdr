# Seam Audit

**Date:** 2026-07-25 · **Scope:** full workspace — `hrdr-tools`, `hrdr-llm`,
`hrdr-agent`, `hrdr-app`, `hrdr-tui`, `hrdr-editor`, the `hrdr` binary.

Companion to `dry-audit.md`, which asks "is this code duplicated?". This one
asks a narrower question: **when a cross-cutting concern has variants, does one
type own every difference between them?** Line references are point-in-time and
may drift.

## What a seam is

One type owns every difference between variants of a concern, so adding a
variant means filling in methods on that type — and nothing else. The exemplar
is `Shell` (`crates/hrdr-tools/src/tools/shell.rs`), refactored in `d301529`:
detection, program name, invocation arguments, argument quoting, the tool
description, the prompt label, and the POSIX caveat all hang off one enum.
Adding PowerShell means adding a variant, and the `match` arms stop compiling
until it answers all of them.

That refactor found five concrete defects, which are the patterns this audit
hunts for:

1. **Stringly-typed dispatch** — behaviour chosen by comparing a `&str` instead
   of matching an enum.
2. **Silent catch-all defaults** — an unrecognised variant quietly reported as a
   _specific_ wrong one, instead of failing to compile.
3. **Variant knowledge outside the owning type** — POSIX quoting lived in
   `hooks.rs`, not on the shell type that consumes it.
4. **The same invariant hardcoded at N call sites** — `-c` appeared at three
   spawn sites.
5. **Parallel implementations that must stay in lockstep** — N functions each
   re-deriving per-variant behaviour, where adding a variant means finding all
   N.

Findings are ranked by **whether something is currently wrong**, then by drift
risk — not by line count. A three-line duplication that never changes is not
worth a trait, and is recorded as DAMP rather than as a finding.

---

## 1. `HRDR_LOG_REQUESTS` only instruments the OpenAI backend — FIXED ✅ (`579e286`)

**Concern:** wire-level debug logging — request body, non-2xx error body, raw
SSE lines. **Should be owned by:** `log_wire` (`hrdr-llm/src/client.rs:187`),
which is a private `fn`, so the other two backends _cannot call it at all_.

The doc comment at `client.rs:20-24` promises this unconditionally:

> every chat request body, every raw SSE data line, and every non-2xx response
> body is appended to the file

That holds only for the generic OpenAI-compat path, which logs at three points:

- `client.rs:760` — request body, **before** `.send()`
- `client.rs:777` — non-2xx response body (`"error_response"`)
- `client.rs:855` — every decoded SSE `data:` line (`"sse"`)

The native Anthropic and Codex paths build their own request and read their own
response inside `anthropic::chat_stream` / `codex::chat_stream`, and neither
module contains a single `log_wire` call (verified: zero occurrences in
`anthropic.rs` and `codex.rs`). Two consequences:

- **A failing request logs nothing at all.** `client.rs:704-731` (Anthropic) and
  `client.rs:733-757` (Codex) call `chat_stream(...).await?` and log the request
  only _after_ it returns `Ok`. But the status check lives inside that call —
  `anthropic.rs:316-333` reads a non-2xx body and returns `Err` — so the `?`
  propagates and the `log_wire("request", …)` below it never runs. The body that
  was sent is not recorded either.
- **No `error_response` and no `sse` records exist for these backends**, since
  those call sites only exist in the OpenAI branch.

**Patterns:** #5 (three parallel implementations, one of which implements the
full contract) and #2 in effect — nothing fails to compile, the feature just
produces a partial log.

**Repro:** `HRDR_LOG_REQUESTS=/tmp/wire.log`, point hrdr at `api.anthropic.com`
with a bad API key, send a message. Expected per the doc comment: a `request`
record plus an `error_response` carrying the 401 body. Actual: nothing for that
turn — the exact failure the feature exists to diagnose is invisible on 2 of 3
backends, while working fine against OpenAI-compatible endpoints.

**Fix:** make `log_wire` `pub(crate)` and add the same three call sites to
`anthropic.rs` / `codex.rs`, logging the request _before_ the HTTP call so
failures are captured. Mechanical parity, not a new abstraction — the three wire
protocols genuinely differ and should keep their own body-building and
event-mapping code. ~4 call sites, no design change.

This is the same shape as security finding **O4** (`docs/security-audit.md`),
where `extra_headers` was applied before the credential in `client.rs` but after
it in the other two — one concern implemented three times, and two of them
wrong. Worth assuming there are more: **any invariant that lives in
`client.rs`'s streaming path should be checked against the other two.**

**Fixed** (`579e286`): `log_wire` is `pub(crate)`, and both native backends emit
the same three records the OpenAI path does, with the request logged _before_
the send. The post-hoc logging in `client.rs` is gone — it would have
double-logged every success — and since the returned body existed only to feed
it, `chat_stream` now returns `ChatStream` rather than `(Value, ChatStream)`.
Bodies never see the credential (it is a header, applied after the body is
built); `crates/hrdr-llm/tests/wire_log_native_backends.rs` pins both the
request records and the absence of the key, and fails with "the wire log was
created: NotFound" against the pre-fix code. `error_response` and `sse` for the
native backends stay uncovered by tests — backend selection keys on the host, so
a mock server on `127.0.0.1` cannot reach those paths.

## 2. Process-group kill invariant repeated at five spawn sites — FIXED ✅ (`3fb99b5`)

**Concern:** "spawn a child, and kill its whole process tree rather than just
the direct pid." **Should be owned by:** a helper in `hrdr-tools/src/proc.rs`,
next to `configure` and `ProcessGroup`. No such helper exists; each site
hand-rolls the sequence `proc::configure` → `spawn` → `pid = child.id()` →
`ProcessGroup::attach` → … → `group.kill(pid)`.

| Site                                     | `configure` | `attach` | `group.kill` | Kill trigger    |
| ---------------------------------------- | ----------- | -------- | ------------ | --------------- |
| `hooks.rs:108` (`run_file_hooks`)        | 108         | 117      | 125          | timeout         |
| `hooks.rs:279` (`run_event_hooks`)       | 279         | 287      | 305          | timeout         |
| `tools/watch.rs:181` (`run_check`)       | 181         | 186      | 202          | timeout         |
| `tools/shell.rs:264`                     | 264         | 267      | 421          | timeout         |
| `tools/mod.rs:100` (`run_capped_output`) | 100         | 103      | 154          | output overflow |

All five share the setup and the tree-kill; only the **trigger** differs (four
race a `tokio::time::timeout`, `run_capped_output` fires on hitting its output
cap). Four of the five also pair `group.kill(pid)` with a direct child kill
(`child.kill()`, `start_kill()`), and each explains in its own comment why
`kill_on_drop` alone is insufficient — the same reasoning written five times.

**Pattern:** #4 and #5. If the kill path ever needs another step — a SIGTERM
grace period before SIGKILL, telemetry on the kill, a different ordering against
`child.kill()` — five sites need the same edit, and a sixth added later is
likely to copy whichever one the author happened to find.

**Fix:** a helper in `proc.rs` owning configure/spawn/attach/kill and handing
the caller the `pid` and a guard, with the trigger left to the caller (the
bodies genuinely differ: streaming line ingestion, capped reads, a stdin-write
inside the timed future). Roughly:

```rust
pub(crate) fn spawn_group(cmd: &mut Command) -> io::Result<(Child, GroupKill)>;
```

where `GroupKill::kill()` is the one place the tree-kill sequence is written.
This is a real but modest win — five call sites shed ~8 lines each and the
sequence stops being folklore. Note it is _not_ a full unification: the timeout
race stays with the caller, because collapsing that too would mean a generic
over the body future for little gain.

**Fixed** (`3fb99b5`), with two departures from the sketch above. `GroupKill`
carries the pid, so `kill()` takes no argument and no caller holds a pid across
the point where the `Child` is consumed — which was the awkward part the sketch
left in place. And there are two entry points, because one `Result` cannot
express both attach policies: hooks run their child even when attach failed, so
`spawn_group_best_effort` keeps that (its `kill()` no-ops) while `spawn_group`
treats a failed attach as fatal, as `watch`/`shell`/`mod` already did. Attach
only fails on Windows, where it creates a Job Object.

`lsp.rs` and `mcp/client.rs` were left alone deliberately: they hold
`Option<ProcessGroup>` in long-lived struct fields, rely purely on the guard's
`Drop` with documented field ordering (`lsp.rs:400`), and never kill explicitly
— a `GroupKill` they would never call. One error-message nuance: `shell.rs` had
separate contexts for spawn and attach failure, and one call means one context,
so a Windows attach failure now surfaces under `"spawning command"`.

## 3. `find.rs` and `grep.rs` build a byte-identical `WalkBuilder` — WET ⚠️

**Concern:** the ignore-aware directory walk. **Should be owned by:**
`ignore_walker`, which already exists in `grep.rs:309` but is private to that
module and takes `&GrepArgs`.

`grep.rs:309-318` and `find.rs:61-68` build the same chain, driven by the same
two flags:

```rust
.max_depth(Some(20))
.hidden(!a.hidden)
.ignore(!a.no_ignore)
.git_ignore(!a.no_ignore)
.git_global(!a.no_ignore)
.git_exclude(!a.no_ignore)
.parents(!a.no_ignore)
```

`find.rs:59` even says so out loud: _"the same flags `grep` exposes on its
identical walker."_ **This makes `dry-audit.md` #15 stale** — that entry records
find's copy as "differing only in `max_depth` and `parents`", which is no longer
true; they have since converged to identical. Pattern #4.

**Fix:** change `ignore_walker`'s signature to
`(root: &Path, hidden: bool, no_ignore: bool)`, move it to `tools/mod.rs`
alongside `NO_MATCHES` / `ensure_parent_dir`, and call it from both. `tree.rs`
and `replace.rs` stay as they are — their configurations are genuinely different
(variable `max_depth` and no ignore toggles in `tree.rs`; `hidden(false)` with
no `.gitignore` handling at all in `replace.rs`), so `dry-audit.md` #15's DAMP
verdict still holds for those two.

## 4. `is_anthropic_native` re-derives a `Backend` decision from a display string — WET ⚠️

**Concern:** "is this endpoint the native Anthropic Messages API." **Should be
owned by:** `Backend` / `detect_backend` in `hrdr-llm`, which already knows —
but `Backend` is a private enum (`client.rs:327`), unreachable from
`hrdr-agent`.

`hrdr-agent/src/config.rs:1800-1802`:

```rust
pub(crate) fn is_anthropic_native(base_url: &str) -> bool {
    wire_protocol(base_url) == "Anthropic"
}
```

`wire_protocol` (`client.rs:440-446`) exists to produce a **display** string;
its own doc comment describes it as something "a caller that compares this
across two URLs can say out loud." Here it is doing double duty as a boolean
backdoor into an enum decision. The result drives `resolve_cache_mode`'s `auto`
branch — i.e. whether prompt-cache `cache_control` breakpoints are emitted at
all. Pattern #1, at exactly the seam boundary the shell refactor targeted.

**Not currently wrong**: only one string means Anthropic today, and both sides
live behind the same crate's API. But nothing would catch a rename on either
side, and there is no test pinning it — no `config.rs` test exercises an
`api.anthropic.com` URL through this path.

**Fix:** add `pub fn is_anthropic_backend(base_url: &str) -> bool` next to
`detect_backend`, backed by `detect_backend(base_url) == Backend::Anthropic`
with no string in the middle, and have `config.rs` call it. Trivial, and it
gives `wire_protocol` its single documented purpose back.

---

## Precedents — seams already done right

Worth pointing at rather than re-litigating; a new variant of any of these has
one obvious home.

| Seam                 | Where                             | Shape                                                                                                                     |
| -------------------- | --------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `Shell`              | `hrdr-tools/src/tools/shell.rs`   | Enum + inherent methods. The exemplar (`d301529`).                                                                        |
| `EditorEngine`       | `hrdr-editor/src/lib.rs:57`       | Trait, 2 impls (`lib.rs:332`, `plain.rs:138`), **zero** call-site branching in `hrdr-tui`. The cleanest seam in the repo. |
| `Transport`          | `hrdr-tools/src/mcp/types.rs:19`  | Enum + per-variant free functions; exactly 2 exhaustive dispatch sites (`mcp/client.rs:426`, `:448`).                     |
| `GrepBackend`        | `hrdr-tools/src/tools/grep.rs:14` | Enum, exhaustive match, no catch-all — what the shell case was corrected _to_.                                            |
| `ModelRef`           | `hrdr-agent/src/model_ref.rs`     | `provider()` / `catalog_key()` / `auth_key()` / `is_builtin()` all on the type.                                           |
| `ChatErrorKind`      | `hrdr-llm/src/client.rs`          | One definition; all three backends construct it through shared `pub(crate)` helpers rather than re-deriving retry logic.  |
| `proc::ProcessGroup` | `hrdr-tools/src/proc.rs`          | One type, `cfg(unix)`/`cfg(windows)` impls, identical _contract_ via platform-native mechanisms. Two spellings, no drift. |

## Checked and found clean

Negative results, recorded so they aren't re-audited.

- **`extra_headers` ordering across the three backends** — `client.rs::auth()`
  applies extras before the credential; `anthropic.rs` and `codex.rs` after.
  Inert: `apply_extra_headers` (`client.rs:474`) filters auth-type header names
  categorically, so the final header set is identical either way. Fixed properly
  by `483fa42` (O4), not a residual.
- **Two "is this the ChatGPT/Codex endpoint" checks** — `detect_backend` uses a
  permissive host+substring test to pick a wire protocol; `is_codex_oauth` uses
  strict equality against one constant to gate OAuth credential injection.
  Deliberately different concerns: the protocol pick should accept a mirror or
  gateway, the credential gate must not. Rationale already documented at
  `turn_loop.rs:1037-1046`. **DAMP.**
- **Wire-log permission hardening** (`client.rs:80-136`) — `cfg(unix)` applies
  `0600` + `O_NOFOLLOW`; Windows gets no ACL equivalent. Real behavioural
  difference, but disclosed in the doc comment and gated behind an opt-in debug
  env var. **DAMP**, on record only.
- **Grep backend bodies** — `grep_ripgrep` / `grep_posix` / `grep_builtin` have
  genuinely divergent flag sets (ripgrep's `--hidden`/`--glob` vs POSIX grep's
  documented `--exclude-dir` trap vs the built-in walker). Shared methods would
  be a thin unhelpful wrapper. **DAMP.**
- **LSP server selection** — a data table (`lsp.rs:54-71`,
  `LspServerConfig { command, args, extensions }`) matched by extension, plus a
  static ext→language-id lookup. Already data-driven; no per-language behaviour
  branching.
- **Themes / palette, session storage, editor launching** — one mapping function
  or one format each, no variant dispatch. `run_editor`
  (`hrdr-tui/src/app/util.rs:79`) is a single function with two callers.
- **`AgentEvent` handling** — `transcript.rs` and `subagent_transcript.rs` match
  the same events but build different artifacts (live TUI transcript vs
  serializable `Record`), folded through the shared `apply_event`. Consistent
  with the shipped transcript unification, not a fork.
- **Sub-agent vs main-agent codepath** — no `is_subagent`-shaped behavioural
  fork found in `turn_loop.rs` / `lib.rs`, consistent with the completed
  agent-logic migration.
- **Slash-command dispatch, `CommandHost` impls, TUI selector modals** —
  re-checked against `dry-audit.md` #3/#7/#8; `Selector<T>` is itself a clean
  seam (one state machine, typed aliases, only filter functions differ). No new
  findings.
- **`git.rs`** — invokes `git` directly rather than through a shell, so the
  `Shell` dialect concern does not reach it.

## Not covered

The `cfg(unix)`-only blocks with no Windows counterpart were **not**
exhaustively audited — there are roughly 40 across the workspace, and most are
permission-bit or signal conveniences with no Windows analogue by design rather
than drifted dual implementations. The two most consequential were checked
(`proc.rs`, the wire log) and both are clean or honestly documented. If a deeper
Windows-drift pass is wanted, `hrdr-agent/src/store_lock.rs`, `auth.rs`, and
`auth_store.rs` are the next places to look.

One POSIX assumption is knowingly left outside the `Shell` seam: the
pipe-to-shell guardrail's recovery text and its nested-shell regex
(`guardrails.rs`). `default_guardrails()` has no shell in scope, and threading
one in was out of scope for `d301529`; a comment there names `Shell` as the seam
so a future dialect finds it.

---

## Summary

| #   | Concern                                     | Verdict            |
| --- | ------------------------------------------- | ------------------ |
| 1   | `HRDR_LOG_REQUESTS` OpenAI-only             | FIXED ✅ `579e286` |
| 2   | Process-group kill at 5 spawn sites         | FIXED ✅ `3fb99b5` |
| 3   | `find.rs`/`grep.rs` identical `WalkBuilder` | WET ⚠️             |
| 4   | `is_anthropic_native` string dispatch       | WET ⚠️             |

Verdict: **the seams are in better shape than the shell case suggested.** Four
of the seven concerns with real variants already own their differences properly,
and two more (`GrepBackend`, `Transport`) are enum-dispatched without
catch-alls. The one finding that mattered was #1 — a feature that silently did
nothing on two thirds of its surface, the same three-parallel-implementations
shape that produced security finding O4 — and it is fixed, along with #2. **#3
and #4 remain open**; both are maintenance hygiene, neither urgent.

The recurring lesson from both O4 and #1 is narrower than "unify the backends":
**`hrdr-llm` has three streaming paths, and an invariant added to one of them
does not reach the other two.** Anything cross-cutting added to `client.rs`'s
request path from here should come with a check of `anthropic.rs` and
`codex.rs`.
