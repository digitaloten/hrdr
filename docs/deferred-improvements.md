# Deferred improvements / backlog

Smaller items that were identified but not yet done or tracked elsewhere. Larger
efforts have their own docs: `sandbox-design.md` (also issue #13),
`web-ui-plan.md`, `security-audit.md` (one LOW residual left). The Codex catalog
pin is issue #2.

Docs for finished work are deleted rather than kept as history — read the code
and `git log`. What survives from a completed effort is only what still binds
future work; those are collected under **Standing constraints** at the bottom.

## Tooling / agent capability

- **Memory drift detection.** A periodic prune/verify pass over the `memory`
  store — check each `<slug>.md` still has a `MEMORY.md` pointer (and vice
  versa) and flag/prune stale or contradicted memories. Cheap now that the tool
  regenerates the pointer index on every change, so the files and index can't
  drift structurally; this is the leftover §G7 thread from the (now-shipped)
  memory-tool design.
- **Model pre-flight validation.** Verify a configured model actually exists on
  its provider before starting a turn, so a typo'd/unavailable model fails fast
  with a clear message instead of mid-turn.
- **Batched `edits[]` on the `edit` tool.** Let one `edit` call carry an array
  of `{old_string, new_string}` edits against a file, applied in order — fewer
  round trips than one call per hunk, and atomic per file.
- **LSP diagnostics dedup.** The same diagnostic can surface more than once
  (overlapping ranges / re-published sets); dedupe before showing the model.
- **Sub-agent isolation guard.** A defensive check that a write sub-agent's tool
  operations stay within its worktree — belt-and-suspenders on top of the cwd
  being set to the worktree (escaping is by design for full-FS access, but an
  accidental parent-tree write is worth catching/telemetering).
- **A revived sub-agent always runs write-capable.** `task_revive` reuses the
  worktree and takes a write slot, because read-only-ness was never persisted in
  `SessionState` — so a revived former read-only explorer runs write-capable in
  the recorded/main dir. Not central to the shipped use cases (both target write
  sub-agents), but it is a real gap: persist the flag and honour it on revive.

## Consistency / robustness

- **Guardrail rules live in two places.** The shell guardrail rule set is
  encoded both in `crates/hrdr-tools/src/guardrails.rs` (mechanical enforcement)
  and in the prompt fragments under `crates/hrdr-agent/src/templates/*.md`
  (guidance that tells the model not to attempt them). Adding a rule means
  editing both, or they drift. Not worth auto-deriving (the prompt phrasing is
  deliberately more nuanced than the terse guardrail messages) — but a
  checklist/test that the two sets agree would catch drift.
- **The pipe-to-shell guardrail assumes POSIX.** Its recovery text
  (`curl -fsSL … -o /tmp/script.sh`) and its nested-shell regex live in
  `guardrails.rs`, outside the `Shell` seam, because `default_guardrails()` has
  no shell in scope. Correct today — the shell is always bash/sh — but it is the
  one place a new shell dialect would have to be threaded through by hand.
- **Windows-drift audit pass — done, three fixes landed** (`8e5bc9d`). All ~130
  `cfg` gates were classified. Roughly 25 are `#[cfg(unix)]` on _tests_ (needing
  bash, python3 or symlinks) and are not findings; `proc.rs`, the pid-liveness
  probes in `session.rs`/`delegation.rs`, and `prompt.rs`'s package-manager
  names are deliberate and documented. Three were real and are fixed: the
  credential `sync_all` gated on unix though it is portable, `atomic_write`'s
  symlink guard running only on unix though `is_symlink` is portable, and
  owner-only file creation being re-decided at four sites. What is left is the
  honest residual: **hrdr sets no Windows ACL on any file it writes** — the
  guarantee is the containing per-user directory's inherited default, stated
  once on `hrdr_llm::owner_only_options`. Setting per-user ACLs needs a new
  dependency in `hrdr-llm` and is a deliberate non-goal until someone runs hrdr
  on Windows in anger.
- **`O_NOFOLLOW` covers only the final path component.** A symlinked _parent_
  directory is still traversed on the wire-log open, and there is no Windows
  equivalent applied at all, so callers relying on it keep their own preflight
  check. Recorded on `owner_only_options_no_follow`; closing it properly means
  resolving the whole path under a directory handle.

## Test coverage gaps

- **TUI history up/down fix** (`6ff0172`, `suppress_completions`) shipped
  without a regression test — a test that Up/Down after a slash-command history
  entry navigates history rather than the completion popup.
- **Wire log on the native backends.** `error_response` and `sse` records are
  emitted by `anthropic.rs` / `codex.rs` but untested: backend selection keys on
  the host, so a mock server on `127.0.0.1` cannot reach those paths. Only the
  `request` record is covered (`hrdr-llm/tests/wire_log_native_backends.rs`).

## Known behaviour to revisit

- **Input-path unification UX.** After the "every user message is a queued
  `Steer`" refactor, a submitted message renders when its `Steered` event is
  pumped (a beat after submit) rather than synchronously, matching sub-agent
  behaviour. Intended and imperceptible with a fast pump; if it ever reads as
  laggy, the opener could be pumped synchronously.
- **tok/s excludes tool time.** The generating marker's throughput figure
  divides streamed tokens by _model working time_: `infer_elapsed()` pauses
  while `tools_running > 0`, and the loader is hidden entirely during a tool
  call. By design (it reports model speed, not wall-clock throughput). Showing
  "running tool…" instead of hiding the loader, or tracking wall-clock
  throughput separately, would be a new feature rather than a bug fix.

## Considered and declined

Duplication and near-duplication that two audits (a DRY pass and a seam pass,
both since closed and deleted) examined and deliberately left alone. Recorded so
the next audit does not re-litigate them — if you disagree, argue with the
reason, don't just re-file the finding.

- **Slash-command dispatch is mirrored** between `hrdr-tui/src/app/commands.rs`
  and `hrdr-app/src/commands/dispatch.rs`. Intentional: the TUI handler
  intercepts TUI-only commands (`edit`, `reload`, `goto`, `find`, `next`/`prev`)
  then falls through to the shared dispatcher. The `CommandHost` trait is the
  DRY mechanism; the split is explained in a comment at the call site.
- **Two project-dir walks** — `skills.rs::skill_dirs` and
  `prompt.rs::gather_agent_docs` both walk cwd → `/` plus XDG dirs. Same
  pattern, different payloads (skills vs AGENTS.md). A shared iterator would DRY
  the traversal, but each is ~15 lines and what they collect diverges; judged
  borderline over-engineering at this scale.
- **Three `CommandHost` impls** — the real TUI host plus `TestHost` and
  `TestLoginHost`. The trait is the shared mechanism; the two test hosts share
  some trivial no-op bodies, but the login host carries login-specific state. A
  shared test base would remove a few no-ops for very little gain.
- **Secret-file write/edit guards are tailored, not shared.** `write.rs`,
  `edit.rs` and `fileops.rs` each `bail!` with their own message ("refusing to
  write…", "refusing to edit…", "copying it would place its contents…"). The
  structure repeats; the wording is deliberately specific and meaningful to the
  model. The read side (`guard_secret_read`) is already shared.
- **`tree.rs` and `replace.rs` build their own walkers.** Genuinely different
  configuration — variable `max_depth` and no ignore toggles in `tree.rs`;
  `hidden(false)` with no `.gitignore` handling at all in `replace.rs` — so they
  stayed out of the shared `ignore_walker` that `find` and `grep` now use.
- **The three grep backends keep separate bodies.** `grep_ripgrep` /
  `grep_posix` / `grep_builtin` have divergent flag sets (ripgrep's
  `--hidden`/`--glob`, POSIX grep's documented `--exclude-dir` trap, the
  built-in `ignore::Walk`). `GrepBackend` already dispatches them by exhaustive
  match; shared methods would be a thin wrapper over nothing.
- **Two "is this the ChatGPT/Codex endpoint" checks, on purpose.**
  `hrdr-llm::detect_backend` uses a permissive host+substring test to pick a
  wire protocol (a mirror or gateway still needs the Responses-API body shape);
  `is_codex_oauth` uses strict equality against one constant to gate OAuth
  credential injection. Unifying them would weaken a security boundary that is
  documented at its call site.
- **`AgentEvent` is matched in two places** — `transcript.rs` and
  `subagent_transcript.rs` — but they build different artifacts (live TUI
  transcript vs serializable `Record`) from the shared `apply_event` fold. Not a
  fork.
- **`lsp.rs` and `mcp/client.rs` spawn without `proc::spawn_group`.** They hold
  `Option<ProcessGroup>` in long-lived struct fields, rely purely on the guard's
  `Drop` with documented field ordering, and never kill explicitly — the
  `GroupKill` handle would be dead weight.

Seams already done right, worth copying rather than reinventing: `Shell`
(`tools/shell.rs`), `EditorEngine` (`hrdr-editor`, trait + 2 impls with zero
call-site branching), `Transport` (`mcp/types.rs`), `GrepBackend`, `ModelRef`,
`ChatErrorKind`, `proc::ProcessGroup`.

## Standing constraints

Decisions from completed work that still govern new work. These are not backlog
items — they are rules.

- **hrdr-agent owns ALL agent logic; hrdr-app is only agent↔TUI glue.** Every
  agent, main or sub, runs the same codepath — no special-casing, no parity
  forks. Do not ask "how should sub-agents behave"; they behave exactly like the
  main agent because it is the same code.
- **Only the `AgentEvent` fold persists in a transcript.** User=`Steered`,
  Assistant=`Text`, `Reasoning` text, tool args+results, agent `Notice`→System.
  Frontend-pushed _chrome_ is not persisted — slash-command System output,
  `/diff` output, per-turn Stats, Header, `Reasoning.took_ms` — it is
  display-only, not context, and not needed to resume.
- **No migration or back-compat fallback before 1.0.** Clean breaks; delete
  old-format fallback code when you find it.
- **`hrdr-llm` has three streaming paths** (`client.rs`, `anthropic.rs`,
  `codex.rs`), and an invariant added to one does not reach the other two. This
  produced security finding O4 (duplicate auth header) and a wire log that
  silently covered only one backend. Anything cross-cutting added to
  `client.rs`'s request path must be checked against the other two.
