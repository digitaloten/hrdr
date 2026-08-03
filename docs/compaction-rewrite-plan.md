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

**Prerequisite — CHECKED 2026-08-04, it passes.** `MessageOrigin` round-trips
through session serialization, by deliberate mechanism rather than by luck. The
field carries `#[serde(default, skip_serializing)]`, so the ordinary `Serialize`
impl never emits it — that is what keeps it off the provider wire, where it is
not a real field. The session file instead uses
`#[serde(with = "persisted_messages")]` (`hrdr-agent/src/session.rs`), whose
serializer re-inserts the internal fields the wire impl drops:
`reasoning_content`, `anthropic_thinking_blocks`, `responses_reasoning_items`
and `origin`, commented _"Preserve internal origin marker so real user turns
stay distinguishable from injected context after a session resume."_
Deserialization is the plain derive, and `#[serde(default)]` means an older file
with no `origin` key loads as `User`.

Two things the `Summary` variant must respect:

- **The serializer writes `origin` only when it differs from `User`** — a size
  optimisation that round-trips correctly precisely because `User` is also the
  `#[default]`. `Summary != User`, so it is written and read back. But this
  makes the default load-bearing: **`User` must stay `#[default]`**, or every
  omitted message silently mislabels on load.
- **A round-trip test ships with the feature.** Save a session holding a
  `Summary`-tagged message, load it, assert the tag survived — and prove the
  test fails when the tag is dropped. Without it, a later change to
  `persisted_messages` could quietly stop emitting `origin` and nothing would
  notice until a resumed session began chaining summaries again.

### Initial context needs nothing from us — but the ORDER might

**Investigated 2026-08-04, closing the open question.** Codex's "initial
context" is the repo/environment state (`WorldState`), and it lives **inside the
history**, so a compaction that replaces history destroys it and it has to be
put back. Hence the enum. Its doc comment states both cases:

- Pre-turn and manual compaction use `DoNotInject` — _"they replace history with
  a summary and clear `reference_context_item`, so the next regular turn will
  fully reinject initial context after compaction."_
- Mid-turn compaction MUST use `BeforeLastUserMessage`, _"because the model is
  trained to see the compaction summary as the last item in history after
  mid-turn compaction; we therefore inject initial context into the replacement
  history just above the last real user message."_

**hrdr needs none of this, and that is architectural rather than lucky.** The
equivalent state lives in hrdr's SYSTEM PROMPT — environment, memory, AGENTS.md,
skills — as ordered named sections rebuilt per request, and `Agent::compact`
already calls `refresh_system_prompt_in_place()` precisely so a note saved this
session is in the rebuilt index. Codex has to re-inject because its context is
history-resident; ours cannot be destroyed by a history rewrite. Codex's other
half of the same hygiene — clearing `reference_context_item` — hrdr already has
as `reset_read_files()`, on the same line of reasoning: file contents the model
read now live only in the summary, so require fresh reads before further edits.

**Summary ordering — decided 2026-08-04 by the owner: chronological, summary
first.** hrdr builds `[system, summary, ...tail]`, and the continuation prose
says so (_"the most recent messages follow it verbatim"_). Codex's mid-turn
shape is the opposite — summary LAST — because their models are trained on that
layout.

The owner's reasoning, and it is the stronger one: **the position should be
temporally true.** The summary covers what happened BEFORE the last few full
turns, so placing it before them is simply where it belongs in the conversation;
the tail then reads forward from it in order. That shape needs no special
training to be understood, which is why it works across providers. Codex's
layout is probably better on GPT models specifically, and hrdr is not a GPT-only
harness.

Not revisited without evidence that a provider we care about behaves worse with
summary-first — and note that adopting codex's order would mean betting the
layout on one vendor's post-training.

### Compaction reports its own cache saving — resolved 2026-08-04

The original worry was that item 1's entire case is cache economics and nothing
could prove it worked. Owner's answer, and it is better than a manual benchmark:
**read the cache figures off the provider response and put them in a `::Notice`
in the transcript after every compaction**, so the mechanism reports on itself
in normal use.

**The data already exists.** `hrdr-llm/src/catalog.rs` prices cached tokens
today — `cache_read` and `cache_write` rates, a `cache_creation` input, and a
cost function discounting reads at roughly 0.1x input, with a documented mapping
between provider field names and the catalog's. The counts are parsed and priced
already; they are simply never surfaced.

**The one structural obstacle:** `Agent::compact` cannot emit events. Its own
comment says so — _"`compact` has no event sink (it is called from overflow
recovery and from `/compact` alike) … the caller's own notice covers the
outcome."_ So the notice cannot come from inside it. It fits the existing shape
instead: `compact()` already returns `(before, after)` message counts and each
caller emits its own notice from them. Widen that return to a small struct
carrying the cache figures, and each caller folds them into the notice it
already writes — which also means an overflow rescue and a `/compact` can report
differently, and that is exactly where work item 2's `CompactionReason` belongs.
One change, both purposes.

**What to report:** tokens served from cache versus tokens charged at full rate
for the compaction request, the summarization's own output tokens (unchanged by
this work, and the honest denominator for "what did this cost"), and the
estimated cost. The number that proves the mechanism is the **cache-read
fraction** — near zero today, and it should jump to most of the prompt once the
request shape matches an ordinary turn. If it does not move, the change did not
work, and that is visible on the first compaction rather than at the end of a
billing period.

**Two caveats it must respect:**

- **Cache reporting is provider-specific.** Anthropic reports it directly,
  OpenAI-compatible endpoints report `cached_tokens` under a details object, and
  many local or gateway endpoints report nothing. The notice must degrade
  honestly — "not reported" rather than a zero, because absent and zero mean
  opposite things and one of them looks like the change failed.
- **It measures the compaction request only.** Compaction rewrites history and
  refreshes the system prompt, so the turn AFTER it starts cold regardless. The
  wording must not imply otherwise.

**Paired with a CI check, because a notice is not a test.** You cannot unit-test
a cache hit — it needs a real provider and two sequential requests — but you CAN
test the property that causes it: build a normal request and a compaction
request from the same agent state and assert the system prompt and `tools[]` are
equal, and that the messages differ only by the appended instruction. That goes
red the moment anyone reintroduces a separate summarizer prompt or strips the
tools, which is the regression that would silently restore today's cost. The
notice then confirms real behaviour in use; the test stops it regressing.

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

1. **Sub-agents and `/compact` — two traps in the in-place design.**

   **The appended instruction creates a turn boundary that did not exist.** A
   delegated agent's history is one long turn with no second `role:"user"`
   message — `mega_turn_tail_start` exists for exactly that shape. But item 1
   appends the compaction instruction AS a user message, so the very shape that
   function was written for changes at the moment compaction runs, and tail
   selection may take a different path than it does today. That might make the
   mega-turn split unnecessary during compaction, or it might land the boundary
   somewhere useless. **Decide it while building item 1, not after** — finding
   it late means reworking the tail logic. The test is concrete: a delegated
   agent's history, compacted, asserting the tail is what was intended.

   **The instruction must not survive into the rebuilt history.** Once the
   summary comes back, the appended message has to be dropped explicitly, or
   every compacted session carries a fake user turn reading "summarize the
   conversation so far" — which the model will take as something the user asked
   for. Today's implementation cannot have this bug because the request is built
   separately; the in-place design introduces the possibility.

   `/compact`'s optional steering instructions are the easy half: append them to
   the same user message as the compaction instruction, which keeps the request
   shape unchanged and needs no new plumbing (they already arrive as
   `Option<&str>`).

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
