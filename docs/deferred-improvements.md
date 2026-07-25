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
  and in `crates/hrdr-agent/src/templates/system.j2` (prompt guidance that tells
  the model not to attempt them). Adding a rule means editing both, or they
  drift. Not worth auto-deriving (the prompt phrasing is deliberately more
  nuanced than the terse guardrail messages) — but a checklist/test that the two
  sets agree would catch drift.
- **The pipe-to-shell guardrail assumes POSIX.** Its recovery text
  (`curl -fsSL … -o /tmp/script.sh`) and its nested-shell regex live in
  `guardrails.rs`, outside the `Shell` seam, because `default_guardrails()` has
  no shell in scope. Correct today — the shell is always bash/sh — but it is the
  one place a new shell dialect would have to be threaded through by hand.

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
