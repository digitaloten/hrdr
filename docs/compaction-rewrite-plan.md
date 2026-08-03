# Compaction rewrite — plan

**Status: designed, not built.** Drafted 2026-08-04 from a read of hrdr's
`crates/hrdr-agent/src/compaction.rs` against codex at `78306a3`. Living
document — refine it as decisions land, and **delete it once the work ships**
(what survives goes to `docs/backlog.md` under Standing constraints).

Context for why this matters now: tool-output pruning was removed the same day
(`10c88e8`), so compaction carries the whole job of relieving a filling context.

---

## The defect this is about

**hrdr's compaction request breaks the prompt cache on purpose.**
`Agent::compact` builds a one-off request that shares nothing with the session's
own:

- a dedicated `COMPACT_SYSTEM` summarizer prompt instead of the session's system
  prompt;
- only `messages[1..tail_start]` — the head, with the session's system prompt
  stripped and the recent tail excluded;
- no `tools[]` block at all ("we only want prose back").

Four independent reasons the cached prefix cannot match. So compaction pays
**full input price on the entire head**, at the most expensive moment in a
session, and pays it again on each shrink stage when the summarization request
itself overflows. The code already admits the cost in a comment: _"each doomed
attempt is a full upload of the whole history."_

Codex does the opposite (`run_compact_task_inner_impl`): it clones the live
history, appends the compaction instruction as an ordinary user message, and
sends it under `sess.get_base_instructions()` — the session's own instructions.
The prefix is byte-identical to the turn before it, so the cache hits and it
pays for the appended instruction plus output.

On a long conversation with Anthropic pricing, cached input is a small fraction
of base rate. hrdr currently pays the full rate exactly when the conversation is
largest.

**What this does NOT buy: a warm cache after compaction.** `Agent::compact`
calls `refresh_system_prompt_in_place()` on purpose, so a memory note saved
during the session is in the rebuilt index rather than only in the history being
summarized away. That changes the system prompt, and the history is replaced
wholesale, so the turn AFTER a compaction starts cold whatever we do. The saving
here is on the compaction request itself — which is a full-history upload,
repeated per shrink stage — not on the turn that follows it.

---

## Settled decisions

### Compact in place, with the request shape unchanged

The compaction call sends the session's own system prompt, its `tools[]`, and
its history with the instruction appended. Nothing about the request shape
changes relative to an ordinary turn.

### No `tool_choice` — decided 2026-08-04 by the owner

The tempting shortcut is `tool_choice: "none"`: the tools stay in the request so
the prefix survives, while the model is forbidden from calling one.
**Rejected.**

Anthropic's documented cache hierarchy is `tools → system → messages`, and a
change invalidates that level and every level after it. Their "what invalidates
the cache" table says `tool_choice` _"only affect[s] message blocks"_ — so it
keeps the **cheap** half (tools, system) valid and invalidates the **expensive**
half (messages), which is exactly backwards for a long conversation.

Leaving the request byte-identical is also provider-independent: it needs no
per-provider caching research, and cannot be worse anywhere. hrdr has no
`tool_choice` support today (zero hits in `hrdr-llm/src`), and this decision
means none is needed — which also removes what would have been a three-backend
change (`client.rs`, `anthropic.rs`, `codex.rs`).

Source:
[Anthropic prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)

### Prose is guaranteed by instruction plus a hard guard

With `tools[]` present and no `tool_choice`, the model _could_ return a tool
call. Two layers handle it:

1. The compaction instruction states explicitly that only prose may be returned
   and that no tool may be called.
2. **A tool call returned during compaction is never executed.** It is a failed
   attempt, retried through the existing `RetryBudget`. Executing one would run
   a side effect the user never asked for, at the worst possible moment in a
   session. This behaviour must be documented at the call site, not just here.

### The summary is a distinguished message, not a user turn

**Verified 2026-08-04:** compaction rebuilds history as
`[system, ChatMessage::user(continuation), ...tail]`, and that continuation is a
PLAIN user message. Nothing marks it apart from something the user typed — only
the prose opening _"This session is being continued from an earlier
conversation…"_. Two consequences, both live today:

- **The lossy chain.** `compaction_tail_start` treats every `Role::User` message
  as a turn boundary, so once the summary is old enough to fall in the head, the
  next compaction summarizes it again — a summary of a summary, degrading
  silently with nothing erroring.
- **It steals a tail slot.** Counting as a turn boundary means that with
  `compaction_tail_turns = 2`, the summary itself occupies one of the two turns
  kept verbatim. Immediately after a compaction the "recent tail" is the summary
  plus ONE real turn — halving the working state that was the whole reason for
  preferring whole turns over codex's user-messages-only rebuild.

The fix uses a seam hrdr already has: `MessageOrigin` distinguishes `User` from
`Steering` from `BackgroundResult` — the same shape of problem, a message that
is structurally a user turn without being the user speaking. Add a `Summary`
variant, then:

1. **Never a turn boundary.** `compaction_tail_start` skips it when counting
   turns, so the tail keeps two REAL turns.
2. **Replace, never re-summarize.** Pull any existing summary out of the head
   and hand its text to the summarizer as prior context — fold it into the new
   one — then emit a single summary that supersedes it. **Invariant: exactly one
   summary in history at any time, always covering the session start through the
   tail.** Stronger than codex, which keeps user messages verbatim but has no
   equivalent guarantee against summary-of-summary.
3. **It carries the reason.** Work item 2 lands here naturally: the tagged
   message holds the `CompactionReason`, so provenance survives into the
   transcript and across a resume.

**Prerequisite to verify before building this:** that `MessageOrigin`
round-trips through session serialization. If it does not, a resumed session
loses the tag, the summary reverts to an ordinary user message, and the chain
returns — silently, and only on resumed sessions, which is the worst way to find
out.

### Keep hrdr's whole-turn tail

Codex rebuilds post-compaction history as three parts: the initial context, then
the user's own messages (newest-first against a 20k-token budget, truncating the
last one that does not fit rather than dropping it), then the summary appended
as a final user message. Assistant replies and tool output survive only through
the summary.

hrdr keeps recent **whole turns** — user, assistant and tool results together,
bounded by `preserve_recent_tokens`, never splitting a tool call from its
result. That is better for a coding agent resuming mid-task, and it stays.

### Keep the local shrink ladder

On overflow codex removes one history item and retries, which can cost many
round trips on a badly oversized history. hrdr computes
`first_viable_compact_stage` locally and skips stages that plainly cannot fit,
so it does not pay for doomed uploads. That stays too.

---

## Work items, in the order worth doing

1. **Compact in place, against the live prefix.** Where the money is. Requires
   the instruction wording and the never-execute guard above.
2. **A `CompactionReason`, logged AND persisted into the post-compaction
   transcript.** Today the summary lands with no provenance: nothing can tell a
   user-requested `/compact` from an overflow rescue, including a resumed
   session. Codex carries
   `UserRequested | ContextLimit | ModelDownshift | CompHashChanged` through to
   a counter tagged `(reason, implementation, outcome)` plus a `warn!` naming
   both models. hrdr has one implementation, so it needs `reason` and `outcome`.
3. **Frame the reinjected summary.** Codex's
   `prompts/templates/compact/summary_prefix.md` tells the resuming model the
   summary came from another model and to build on it _"and avoid duplicating
   work"_ — precisely the failure a compacted agent has. hrdr reinjects with no
   framing at all. Cheap, additive, no mechanism.
4. **Trigger on the body, not the total.** hrdr measures total prompt tokens
   against the window, but the prefix (system prompt, tools, memory) is exactly
   what compaction cannot reclaim — and hrdr's prefix is deliberately large and
   stable. The trigger therefore fires early and reclaims less than the number
   implied. Codex has `AutoCompactTokenLimitScope::{Total, BodyAfterPrefix}`.
5. **Compact with the OUTGOING model on a model switch, before adopting the new
   one.** hrdr already handles a downshift reactively — verified:
   `set_model_ref` → `adopt_resolved` invalidates the cached window, and the
   next `maybe_self_compact` fires against the new smaller trigger. But the
   summarizing is then done by the **incoming** model, on a history that may not
   fit its window, with a cold cache. Codex compacts with the previous model's
   turn context first (`maybe_run_previous_model_inline_compact`), then retries
   with the current model if that fails.

   Its retry arm carries a reusable error taxonomy —
   `should_retry_with_current_model` returns true for `InvalidRequest`,
   `UnexpectedStatus`, `ContextWindowExceeded`, `UsageLimitReached`,
   `ServerOverloaded`, `InternalServerError`, `RetryLimit`. That is the
   "model-specific, try a different model" class as distinct from "transient,
   retry the same request". **Worth adopting even with a single model**, because
   it separates "give up and say so plainly" from "retry" — hrdr currently
   retries several permanently-failing cases.

---

## Open questions

Ranked. Question 1 (what a second compaction does to an existing summary) was
**answered on 2026-08-04** — see
[The summary is a distinguished message](#the-summary-is-a-distinguished-message-not-a-user-turn)
above. It is now a design decision rather than an unknown, and it turned out to
have a second consequence nobody had predicted (the stolen tail slot).

1. **`InitialContextInjection`.** Codex re-injects something after a history
   wipe and suppresses it on the model-switch paths (`DoNotInject`), via
   `build_compaction_initial_context()`. hrdr rebuilds its system sections on
   every request so it may get this for free — **that is an assumption, not a
   finding.**
2. **Nothing can measure the win.** The entire case for item 1 is cache
   economics, and hrdr has no prompt-size introspection (a standing backlog
   gap), so it would ship on argument alone. This is the "change of MECHANISM"
   case: the code compiles and passes identically whether the cache hits or not.
   Wants a cached-vs-uncached input token figure before and after.
3. **Sub-agents and `/compact`.** A delegated agent's history is one long turn
   with no second `role:"user"` message — `mega_turn_tail_start` exists for
   exactly that shape — and must keep working. `/compact`'s optional steering
   instructions need a home in the new in-place request.

---

## Provenance

Read closely: hrdr's `compaction.rs`; codex's `compact.rs`
(`run_compact_task_inner_impl`, `build_compacted_history_with_limit`,
`run_inline_auto_compact_task`), `compact_model_fallback.rs`,
`session/turn.rs`'s switch paths, and the compaction prompt templates.

**Not read:** `compact_remote_v2.rs` (~30 KB) and `compact_remote.rs` (~18 KB),
codex's provider-gated server-side path — unlikely to be portable, deliberately
skipped.

**Two items in this document were corrected after first being described from
filenames alone:** `comp_hash` is a per-model compaction-_compatibility_ hash,
not a prompt-cache hash; and the model fallback is the retry arm of the
model-switch path, not a general safety net for any failed compaction. Open the
code before trusting a name.
