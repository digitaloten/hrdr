# task_revive: re-engage a finished, pruned, or crashed sub-agent

Status: **implemented.** `task_revive { id, prompt }` ships alongside the
disk-aware `task_list` / `task_output` (all three in
`crates/hrdr-agent/src/delegation.rs`). Unblocked by Phase 1 of the agent-logic
migration (`docs/agent-logic-migration.md`), which made sub-agents persist their
own `SessionState`.

## Motivation

Two use cases the current delegation model can't serve:

1. **Review loop.** The main agent delegates work, reviews the result, finds
   issues — and wants to hand the fixes back to the **same** sub-agent, which
   already holds the full context, instead of re-delegating from scratch. Today
   it can't: `task_steer` only injects into a **running** turn ("finished or
   unknown tasks cannot be steered"), so a completed sub-agent is unreachable.
2. **Recovery.** A sub-agent still working when the session was closed (or hrdr
   crashed) is dead after a resume — its tokio task died with the process. Its
   committed work survives in its git worktree, but it can't be continued.

## Why it's possible now

Phase 1 of the migration moved `SessionState` persistence into `hrdr-agent` and
made each sub-agent **persist its own `SessionState`** (model-facing
`messages` + metadata) to `sessions/<cwd>/subagents/<main-id>/<stem>.json`, with
its transcript in the sibling `<stem>.jsonl` (rebuilt via `read_transcript` on
load). So a sub-agent's context survives on disk, **losslessly** —
`persisted_messages` keeps the Anthropic signed thinking blocks needed to
continue a pending `tool_use`. This is the key enabler: revive loads real
messages, not a lossy reconstruction from the display transcript.

## Design

`task_revive { id, prompt }` — re-engage sub-agent `id` with a follow-up.

**Resolution — live-first, disk-fallback (one tool, transparent to the model):**

1. **Live.** If the sub-agent is still retained in the in-memory `LiveSubagents`
   registry (finished but not yet pruned), reuse it directly.
2. **Disk.** Otherwise load its persisted `SessionState` from
   `subagents/<main-id>/<id>.json`, hydrate a fresh `Agent` from it (the same
   `adopt_state` path the main agent's `/resume` uses), and reuse its existing
   git worktree/branch, which is still on disk.

Then append `prompt` as the next user turn through the unified queue path (every
user message is a queued `Steer`; see the input-path unification in
`docs/agent-logic-migration.md`), run it, and deliver the result the way a
`task` result is delivered.

- **Reuse the existing worktree** — do not create a new one — so follow-up
  changes stack on the same branch.
- If the worktree was deleted since, revive read-only or warn.
- **Pruning becomes safe.** Aggressive pruning of retained sub-agent panes is no
  longer a data-loss risk, because revive-from-disk is the fallback.

**Blocking vs background.** Default to background (like `task`), delivering the
follow-up result the same way. A blocking variant for a quick fix the main agent
wants to wait on is optional.

## Prerequisite: disk-aware `task_list` / `task_output`

Post-resume the in-memory `LiveSubagents` / `background_tasks` registries are
empty, so the model can't even **see** a dead sub-agent to revive it. Both tools
need a disk fallback:

- **`task_list`** — scan `subagents/<main-id>/`, list each run with its label,
  worktree/branch, and completion state (`is_complete` = has an `End` record).
- **`task_output`** — for a finished/orphaned run, read its persisted transcript
  (`read_transcript` on the jsonl) instead of only the in-memory event log.

This enumeration layer is what `task_revive` selects an `id` from.

## Note on reconstruction vs persisted state

An earlier design explored reconstructing the agent's message history from the
transcript stream. Don't: the transcript (event fold) lacks the Anthropic signed
thinking blocks, so a Claude sub-agent that died mid-`tool_use` couldn't resume
byte-exact. Because sub-agents now persist their real `messages` (with those
blocks), revive uses the persisted `SessionState` and is lossless. The
transcript jsonl is for display; the `.json` snapshot is for revive.

## Known limitations (as shipped)

- **A revive always builds a FRESH agent** from the resolved `messages` (live:
  the retained agent's `messages_owned()`; disk: the snapshot's persisted
  `messages`) rather than re-driving the exact retained `Agent` object. This
  keeps the live and disk paths on one codepath and is lossless — the persisted
  messages _are_ the conversation, signed thinking blocks included — but the
  revive run is a NEW background id with its own `<stem>.jsonl`/`.json` (the
  original transcript is preserved; a run owns its file).
- **A revived run is treated as write-capable** (takes a write slot, reuses the
  worktree). Read-only-ness was never persisted in `SessionState`, so a revived
  former read-only explorer runs write-capable in the recorded/main dir. Not
  central to the use cases (both target write sub-agents + worktree reuse).
- **The follow-up `prompt` is passed through as-is** — unlike a fresh `task`
  brief, no parent-absolute-path rewrite is applied, since the sub-agent is
  already isolated in its own worktree.
- A model-supplied stem id is validated (`valid_run_stem`) before it is joined
  onto the snapshot dir, so a `task_output` / `task_revive` lookup can't escape
  `subagents/<main-id>/`.
