//! Turn execution, tool dispatch, streaming, retries, and cleanup.

use super::*;

/// With this many tool rounds left in a turn, the model is told to wrap up
/// (appended to the last tool result of that round).
const WRAP_UP_WARNING_ROUNDS: usize = 3;

/// Fraction of the tool-round budget after which the model gets one *soft*
/// warning — numerator over [`CHECKPOINT_WARNING_DEN`].
///
/// [`WRAP_UP_WARNING_ROUNDS`] is far too late to act on: three rounds is enough
/// to write a summary, not to commit half-finished work and re-plan the rest.
/// Transcripts show the hard stop landing mid-plan with no warning at all — an
/// autonomous run died roughly a quarter of the way through its list, leaving
/// uncommitted edits and nothing sequenced. At 80% there is still real budget
/// left to checkpoint with.
const CHECKPOINT_WARNING_NUM: usize = 4;
const CHECKPOINT_WARNING_DEN: usize = 5;

/// The round after which the soft checkpoint warning fires, or `None` when the
/// budget is too small for it to be worth anything (the 80% mark lands on the
/// last round, where [`WRAP_UP_WARNING_ROUNDS`] already speaks).
pub(crate) fn checkpoint_warning_round(max_steps: usize) -> Option<usize> {
    let at = (max_steps * CHECKPOINT_WARNING_NUM).div_ceil(CHECKPOINT_WARNING_DEN);
    (at >= 1 && at < max_steps).then_some(at)
}

/// Capacity of the per-tool and shared live-output channels used to forward
/// [`ToolContext::stream`](hrdr_tools::ToolContext::stream) chunks to the UI
/// (see `run_tool_batch`). This is advisory progress output, not the
/// authoritative tool result, so both channels are bounded rather than
/// unbounded: a tool that emits output faster than the UI drains it (e.g. a
/// shell command printing millions of lines) must never queue without limit.
/// 1024 lines is generous for a normal burst — far more than a screen's worth
/// — while keeping the per-in-flight-tool buffer small and fixed; anything past
/// the cap is dropped (`try_send` returns `Full`), never queued or blocked on.
///
/// This bounds the two channels this pipeline owns (`ctx.stream` and the shared
/// forwarder), which fully defeats a synchronous emit tight-loop. The frontend's
/// own `AgentEvent` queue downstream is a separate, still-unbounded hop, so a
/// *streaming* flood can still grow memory there under a lagging renderer — a
/// known follow-up (bound/coalesce that queue), not covered here.
const UI_STREAM_CAP: usize = 1024;

/// Consecutive identical failures after which the exact same call is refused
/// without executing (small models loop on verbatim retries).
const REPEAT_REFUSE_AFTER: u32 = 2;

/// Consecutive identical calls — **whatever their outcome** — after which the
/// model is told it is repeating itself (see [`RepeatGuard`]).
///
/// Three, not two: the second identical call is often a real check rather than a
/// loop (re-`read` a file after editing it, re-run the test that just failed).
/// The third is where the pattern stops being a check — nothing between call two
/// and call three changed the world, so call three cannot learn anything call two
/// didn't. Opencode picked the same number independently (`session/processor.ts`
/// fires on the last 3 tool parts being identical).
const REPEAT_NUDGE_AFTER: u32 = 3;

/// Byte budget for per-turn **relevance recall** injected alongside the opening
/// user message (the full text of the most relevant memories). 4 KiB stays small
/// next to the always-loaded pointer index (~25 KB) while giving the model room
/// for a few complete facts; recall truncates/drops entries to never exceed it.
const MEMORY_RECALL_BUDGET: usize = 4 * 1024;

/// Anti-loop breaker: tracks the last call (tool + raw args), how many times
/// that *exact same* call has been made in a row, and how many of those in a row
/// failed. Any intervening different call resets both counters, so a legitimate
/// `test → edit → test` cycle never trips anything.
///
/// Two failure modes, two answers:
///
/// - **Failing** verbatim retries are refused outright after
///   [`REPEAT_REFUSE_AFTER`] — the call has nothing to offer and small models
///   will retry it forever.
/// - **Succeeding** identical calls are the quieter wedge: `read` the same file,
///   `grep` the same pattern, re-run a `cargo test` that exits 0. Nothing errors,
///   so nothing notices, and the round budget and cost cap drain at full speed.
///   Those get a nudge on the result after [`REPEAT_NUDGE_AFTER`] and nothing
///   more — refusing a call that works would break real work, and hrdr is
///   autonomous, so there is nobody to ask.
#[derive(Default)]
pub(crate) struct RepeatGuard {
    key: Option<u64>,
    /// Consecutive calls with `key`, successes included.
    calls: u32,
    /// Consecutive *failures* with `key`; a success zeroes it but keeps `key`.
    failures: u32,
}

fn call_key(name: &str, raw_args: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    raw_args.hash(&mut h);
    h.finish()
}

impl RepeatGuard {
    /// The refusal message when this call must not run again (it already
    /// failed [`REPEAT_REFUSE_AFTER`]+ times in a row), else `None`.
    pub(crate) fn refusal(&self, name: &str, raw_args: &str) -> Option<String> {
        (self.key == Some(call_key(name, raw_args)) && self.failures >= REPEAT_REFUSE_AFTER).then(
            || {
                format!(
                    "refused without running: this exact {name} call already failed {} \
                     times in a row — change the arguments or the approach; if you're \
                     stuck, stop and tell the user what you tried",
                    self.failures
                )
            },
        )
    }

    /// Record a call's outcome; returns the nudge to append to the result the
    /// model sees, when this call is a repeat worth telling it about.
    ///
    /// `repeatable` is the tool's own opt-out (see
    /// [`Tool::repeatable`](hrdr_tools::Tool::repeatable)): a poll answering the
    /// same question until the answer changes is *supposed* to be identical, so
    /// it never earns the success nudge. It still earns the failure nudge —
    /// polling that keeps erroring is a loop whatever the tool.
    pub(crate) fn record(
        &mut self,
        name: &str,
        raw_args: &str,
        ok: bool,
        repeatable: bool,
    ) -> Option<String> {
        let k = call_key(name, raw_args);
        if self.key != Some(k) {
            self.key = Some(k);
            self.calls = 1;
            self.failures = u32::from(!ok);
            return None;
        }
        self.calls += 1;
        if !ok {
            self.failures += 1;
            // Gated on the streak length rather than "this is a repeat" so a
            // success in the middle of the streak still costs a full pair of
            // failures before the nudge returns — the same point the refusal
            // arms at.
            return (self.failures >= REPEAT_REFUSE_AFTER).then(|| {
                format!(
                    "\n[note: this exact call has failed {} times in a row — change the input \
                     or approach instead of retrying it verbatim]",
                    self.failures
                )
            });
        }
        self.failures = 0;
        (!repeatable && self.calls >= REPEAT_NUDGE_AFTER).then(|| {
            format!(
                "\n[note: you have now made this exact {name} call {} times in a row. It \
                 succeeded and nothing changed in between, so making it again cannot tell you \
                 anything new — change approach, or stop and report what you have]",
                self.calls
            )
        })
    }
}

/// Render a tool's error for the model: the full `anyhow` context chain, not
/// just the outermost frame.
///
/// `{e}` prints only the last `.context(...)`, which is the summary a *human*
/// wants and the opposite of what the model needs — "invalid edit args" without
/// "missing field `old_string`" gives it nothing to correct. `{e:#}` appends
/// each source, `outer: inner: root`.
pub(crate) fn tool_error_text(e: &anyhow::Error) -> String {
    format!("Error: {e:#}")
}

/// Case-insensitive substring scan of an error's display string against a set
/// of marker phrases — the shared shape of the classifiers below.
fn err_mentions(e: &anyhow::Error, needles: &[&str]) -> bool {
    let msg = e.to_string().to_ascii_lowercase();
    needles.iter().any(|n| msg.contains(n))
}

/// Whether an error looks like a transient network/server failure worth
/// retrying (connection issues `request failed`/`timed out`/…, 429, or 5xx).
///
/// Checks the typed [`hrdr_llm::ChatError`] first. A typed error's `message`
/// carries the server's own response body (or, for a mid-stream error object,
/// the server's own error text) — arbitrary data that happens to contain a
/// word like "connection" or "reset" as part of an unrelated, permanent 400
/// isn't evidence of a transient failure, so the broad substring scan below is
/// **not** applied to it; `kind` alone decides. Only errors that never went
/// through the typed path at all — raw transport/network failures (a reqwest
/// send failure, a dropped connection mid-read) or a legacy plain-text error —
/// fall back to the substring scan, where those same marker words genuinely
/// describe the transport-level failure itself.
pub(crate) fn is_transient(e: &anyhow::Error) -> bool {
    if let Some(ce) = e.downcast_ref::<hrdr_llm::ChatError>() {
        return ce.kind == hrdr_llm::ChatErrorKind::Transient;
    }
    err_mentions(
        e,
        &[
            "request failed", // reqwest send() failure (network)
            "timed out",
            "connection",
            "reset",
            "broken pipe",
            "returned 429", // rate limited
            "returned 500",
            "returned 502",
            "returned 503",
            "returned 504",
            "returned 529",      // Anthropic "Overloaded"
            "overloaded",        // Anthropic mid-stream overloaded_error
            "incomplete stream", // stream truncated without terminal marker
        ],
    )
}

/// Whether an error is the server rejecting the request for exceeding the
/// model's context window. The marker phrases are ported from pi's
/// provider-specific overflow patterns (`packages/ai/src/utils/overflow.ts`),
/// covering ~20 OpenAI-compatible backends.
///
/// Checks the typed [`hrdr_llm::ChatError`] first; falls back to a
/// case-insensitive substring scan of the display string for errors that
/// predate the typed form.
pub(crate) fn is_context_overflow(e: &anyhow::Error) -> bool {
    if let Some(ce) = e.downcast_ref::<hrdr_llm::ChatError>() {
        match ce.kind {
            hrdr_llm::ChatErrorKind::Overflow => return true,
            hrdr_llm::ChatErrorKind::Transient => return false,
            // `Other` falls through to the body-text scan: many providers
            // signal context overflow with a 400 + descriptive body, which
            // `classify_status` can't distinguish from an ordinary bad request.
            hrdr_llm::ChatErrorKind::Other => {}
        }
    }
    // Rate-limit / throttling errors sometimes contain overflow-ish wording
    // (e.g. Bedrock's "Throttling: too many tokens") — exclude them first so
    // they retry (via [`is_transient`]) rather than triggering a compaction.
    if err_mentions(
        e,
        &["rate limit", "too many requests", "throttl", "returned 429"],
    ) {
        return false;
    }
    err_mentions(
        e,
        &[
            // Generic phrasings (cover most backends + our own error text).
            "context length",
            "context_length",
            "maximum context",
            "context window",
            "context size",
            "too many tokens",
            "token limit exceeded",
            "reduce the length",
            // Provider-specific (from pi's overflow.ts).
            "prompt is too long",                     // Anthropic
            "request_too_large",                      // Anthropic 413
            "request too large",                      // Anthropic 413 (spaced)
            "returned 413",                           // our formatting of a 413
            "input is too long",                      // Bedrock
            "exceeds the context window",             // OpenAI
            "input token count",                      // Google Gemini
            "maximum prompt length is",               // xAI Grok
            "maximum allowed input length",           // OpenRouter/Poolside
            "longer than the model's context length", // Together AI
            "exceeds the limit of",                   // GitHub Copilot
            "exceeded model token limit",             // Kimi
            "too large for model with",               // Mistral
            "model_context_window_exceeded",          // z.ai
            "configured context size",                // DS4
        ],
    )
}

/// Process-wide counter mixed into jitter so concurrent agents (sub-agents
/// especially) don't get identical jitter from same subsec-nanos and retry
/// in lockstep.
static JITTER_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Map a sequence number to one of 1,000 evenly spaced jitter slots.
pub(crate) fn retry_jitter(seq: u64) -> f64 {
    0.75 + f64::from((seq % 1_000) as u32) / 2_000.0
}

/// Exponential backoff for retry `attempt` (1-based), capped at 8s, with
/// ±25% jitter so parallel agents (sub-agents especially) tripping the same
/// rate limit don't retry in lockstep and re-trip it together.
pub(crate) fn retry_backoff(attempt: usize) -> std::time::Duration {
    let secs = (0.5 * 2f64.powi((attempt as i32 - 1).max(0))).min(8.0);
    // Every call increments the atomic counter, so concurrent agents receive
    // adjacent jitter slots. The counter cycles evenly through all 1,000.
    let seq = JITTER_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::time::Duration::from_secs_f64(secs * retry_jitter(seq))
}

/// The server-requested wait from a `Retry-After` header, if the client embedded
/// one in the error as `retry-after: <seconds>s` (see the client's rate-limit
/// error formatting). Clamped to 60s so a hostile/oversized value can't stall the
/// turn. Only the integer-seconds form is parsed (the HTTP-date form is ignored).
///
/// Checks the typed [`hrdr_llm::ChatError`] first; falls back to a text scan
/// of the display string for errors that predate the typed form.
pub(crate) fn retry_after_hint(e: &anyhow::Error) -> Option<std::time::Duration> {
    if let Some(ce) = e.downcast_ref::<hrdr_llm::ChatError>() {
        return ce.retry_after;
    }
    let msg = e.to_string().to_ascii_lowercase();
    let after = msg.split("retry-after:").nth(1)?;
    let secs: u64 = after
        .trim_start()
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    (secs > 0).then(|| std::time::Duration::from_secs(secs.min(60)))
}

/// Drain a chat stream into an [`Accumulator`], emitting `Reasoning` and `Text`
/// deltas as they arrive. Shared by the turn loop, the budget-exhausted wrap-up
/// round, and (with a no-op sink) the one-off compaction call.
pub(crate) async fn drain_stream<F: FnMut(AgentEvent)>(
    stream: &mut ChatStream,
    on_event: &mut F,
) -> Result<Accumulator> {
    let mut acc = Accumulator::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        // Empty deltas are dropped rather than forwarded, in BOTH directions.
        // Servers do send them: a Qwen3-style backend keeps emitting
        // `reasoning_content: ""` on every content chunk once it stops thinking
        // (and `content: ""` while it is still thinking), where other providers
        // omit the field entirely. Either one, forwarded, silently shreds the
        // transcript — the frontend only merges a delta into the previous entry
        // when that entry is the matching kind, so an empty event of the *other*
        // kind lands in between and forces a new block per chunk. An empty
        // `Text` also closes the open reasoning block, fragmenting reasoning the
        // same way. Both render as nothing, so the only visible symptom is one
        // `#N assistant` header per token group.
        //
        // `acc.push` is still called for every chunk — it accumulates content
        // and tool-call fragments; only the *event* is suppressed.
        if let Some(choice) = chunk.choices.first()
            && let Some(r) = &choice.delta.reasoning_content
            && !r.is_empty()
        {
            on_event(AgentEvent::Reasoning(r.clone()));
        }
        if let Some(text) = acc.push(&chunk)
            && !text.is_empty()
        {
            on_event(AgentEvent::Text(text));
        }
    }
    Ok(acc)
}

/// Repair a history left dangling by an interrupted turn. An assistant message
/// with `tool_calls` must be followed by a `role:"tool"` result for every call
/// id, or strict servers (OpenAI, and infr) reject the next request. Any
/// tool-calling assistant message missing results (the turn was cancelled
/// mid tool-call) gets a stub result appended for each unanswered id, inserted
/// right after that turn's existing results so ordering stays correct.
///
/// Scans the **whole** history, not just the most recent tool-calling turn: a
/// resumed or hand-edited session can carry an older dangling turn buried
/// earlier in the messages (e.g. two interrupted turns before a save), and
/// leaving it unrepaired would keep the session permanently invalid even after
/// the newest turn is fixed.
pub(crate) fn repair_dangling_tool_calls(messages: &mut Vec<ChatMessage>) {
    let mut idx = 0;
    while idx < messages.len() {
        if messages[idx].role != Role::Assistant || messages[idx].tool_calls.is_none() {
            idx += 1;
            continue;
        }
        let call_ids: Vec<String> = messages[idx]
            .tool_calls
            .as_ref()
            .map(|calls| calls.iter().map(|c| c.id.clone()).collect())
            .unwrap_or_default();
        // This turn's own results are the contiguous run of `role:"tool"`
        // messages immediately following it — the next non-tool message starts
        // a different turn, so it can't answer this one's calls.
        let mut end = idx + 1;
        while end < messages.len() && messages[end].role == Role::Tool {
            end += 1;
        }
        let answered: std::collections::HashSet<&str> = messages[idx + 1..end]
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        let missing: Vec<String> = call_ids
            .into_iter()
            .filter(|id| !answered.contains(id.as_str()))
            .collect();
        let inserted = missing.len();
        for (offset, id) in missing.into_iter().enumerate() {
            messages.insert(end + offset, ChatMessage::tool_result(id, "[interrupted]"));
        }
        idx = end + inserted;
    }
}

/// Render the not-yet-`completed`/`cancelled` TODO items as `[ ] content` / `[~] content`
/// lines, one per item — mirrors the checkbox rendering `todo`'s own tool
/// produces (see `render_todos` in `hrdr-tools::tools::todo`), minus the
/// completed/cancelled items, since those are exactly what a turn-end nudge needs to
/// call out.
pub(crate) fn render_unfinished_todos(todos: &[TodoItem]) -> String {
    todos
        .iter()
        .filter(|t| !matches!(t.status.as_str(), "completed" | "cancelled"))
        .map(|t| {
            let mark = if t.status.as_str() == "in_progress" {
                "~"
            } else {
                " "
            };
            format!("[{mark}] {}", t.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Guard against an assistant turn carrying neither text nor a tool call.
///
/// `Accumulator::into_message` leaves both `content` and `tool_calls` unset
/// when the model's reply was genuinely empty (e.g. a `stop` with no delta
/// and no tool call), which serializes as a bare `{"role":"assistant"}` on
/// the wire. Some strict OpenAI-compatible servers 400 on *any* request whose
/// history contains one of those, wedging every later request in the
/// session. A short placeholder keeps the message round-trippable; nothing
/// else about it (in particular, no `tool_calls`) changes, so no
/// tool-call/result pairing invariant is affected.
pub(crate) fn ensure_assistant_has_content(msg: &mut ChatMessage) {
    let empty_text = msg
        .content
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty();
    if empty_text && msg.tool_calls.is_none() {
        msg.content = Some("(no response)".to_string());
    }
}

/// Human-readable elapsed time, magnitude-relative: the two largest adjacent
/// units — hours+minutes, minutes+seconds, or seconds+milliseconds — or just
/// milliseconds under one second. Examples: `53ms`, `5s 12ms`, `1m 31s`,
/// `1h 32m`. The coarse unit gives the magnitude; the finer one keeps
/// precision without a wall of units.
pub(crate) fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms >= 3_600_000 {
        format!("{}h {}m", ms / 3_600_000, (ms % 3_600_000) / 60_000)
    } else if ms >= 60_000 {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1_000)
    } else if ms >= 1_000 {
        format!("{}s {}ms", ms / 1_000, ms % 1_000)
    } else {
        format!("{ms}ms")
    }
}

/// State the turn-end TODO nudge arms so the rounds after it can tell
/// *reconciliation* from *deletion*.
///
/// Transcripts show the failure this catches: nudged about four unfinished
/// items, the model called `todo` once with a single `completed` item — "all
/// done" — and the bookkeeping was square by erasure. The todo tool replaces the
/// whole list, so content strings are the only identity items have; a shrinking
/// list plus a named item gone missing is the honest signal, and it is checked
/// only while a nudge is outstanding.
struct TodoShrinkWatch {
    /// Contents of the items the nudge named as unfinished.
    unfinished: Vec<String>,
    /// Length of the whole list when the nudge was sent.
    len: usize,
}

impl Agent {
    /// The current TODO list's item contents and length, for [`TodoShrinkWatch`].
    fn todo_snapshot(&self) -> (Vec<String>, usize) {
        let items = self
            .ctx
            .todos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            items.iter().map(|t| t.content.clone()).collect(),
            items.len(),
        )
    }

    /// Run one user turn to completion, emitting events as it goes. `steering` is
    /// a shared queue the caller can push to mid-turn (see [`SteeringQueue`]);
    /// pass [`steering_queue()`] when there's no interactive steering.
    pub async fn run<F>(&mut self, steering: SteeringQueue, mut on_event: F) -> Result<()>
    where
        F: FnMut(AgentEvent),
    {
        // Anything raised when there was no turn to carry it — the model pre-flight,
        // at construction or on a switch — is said before the turn it might explain.
        for notice in self.take_pending_notices() {
            on_event(AgentEvent::Notice(notice));
        }
        // A previous turn interrupted mid tool-call can leave the history ending
        // with an assistant `tool_calls` message whose results are missing —
        // strict servers reject that. Backfill stubs before the new user turn.
        repair_dangling_tool_calls(&mut self.messages);
        // Drain the turn opener from the queue — the same queue a mid-turn steer
        // lands on. A normal turn has one waiting (the caller enqueued it); an
        // opener-less turn — nothing queued — exists only to hand the agent
        // something already in its history (a `!command`'s output, a landed
        // background result), so it skips delivery and proceeds straight to the
        // loop.
        let opening = steering
            .lock()
            .map(|mut q| q.pop_front())
            .unwrap_or_default();
        if let Some(opening) = opening {
            self.deliver_user_message(opening, /*opening*/ true, &mut on_event)
                .await?;
        }
        let defs = self.tools.defs();
        // Allow one automatic compaction per turn when the context overflows.
        let mut overflow_compacted = false;
        // Anti-loop breaker for verbatim retries — failing (refused) or
        // succeeding-but-going-nowhere (nudged).
        let mut repeat = RepeatGuard::default();
        // At most one turn-end nudge (see below) per turn — a genuinely blocked
        // or deferring model must still be able to stop.
        let mut nudged_this_turn = false;
        // One soft checkpoint warning per turn at 80% of the round budget.
        let mut checkpoint_warned = false;
        // Armed by the turn-end nudge: the unfinished items it named, plus the
        // list length at that moment. The rounds that follow the nudge are
        // watched for the list *shrinking* — see `TodoShrinkWatch`.
        let mut todo_watch: Option<TodoShrinkWatch> = None;

        for step in 0..self.max_steps {
            // Deliver any steering messages submitted since the last request — a
            // mid-turn correction reaches the model after the current tool round.
            self.drain_steering(&steering, &mut on_event).await;
            // Fold in any detached background sub-agent results that have landed.
            self.drain_background(&mut on_event);
            // Reclaim stale tool output before compacting or building the next
            // request — the cheap, no-model-call first line of defence against
            // context ballooning (compaction below is the expensive fallback).
            // Pressure-gated and ROI-checked, not continuous: rewriting old
            // messages invalidates the provider's prompt cache for nearly the
            // whole conversation, so this is only even attempted once usage
            // nears the compaction trigger, and only applied when the reclaim
            // buys real runway (see the doc comment above `PRUNE_PROTECT_TOKENS`
            // for the full reasoning).
            if self.auto_prune
                && let Some(usage) = self.last_prompt_tokens
            {
                // Same inputs `maybe_self_compact` uses below — one trigger, so
                // the two decisions can't drift apart.
                self.ensure_context_window();
                if let Some(window) = self.context_window
                    && prune_under_pressure(usage, window, self.compaction_reserved)
                {
                    let (victims, reclaimable) =
                        plan_prune(&self.messages, PRUNE_PROTECT_TOKENS, PRUNE_KEEP_TURNS);
                    if !victims.is_empty()
                        && prune_meets_roi(usage, window, self.compaction_reserved, reclaimable)
                    {
                        apply_prune(&mut self.messages, &victims);
                        on_event(AgentEvent::Notice(format!(
                            "context filling — pruned ~{reclaimable} tokens of old tool \
                             output, deferring compaction"
                        )));
                        // `last_prompt_tokens` describes the *previous* request — it
                        // doesn't know about the prune we just applied. Left as-is,
                        // `maybe_self_compact` right below would read that stale,
                        // pre-prune figure and compact anyway on this very round,
                        // making the prune pure loss (cache invalidated for nothing).
                        // Both numbers are estimates (`estimate_tokens` here vs
                        // whatever tokenizer produced the original reading), which is
                        // fine for a threshold heuristic — this only needs to be
                        // roughly right.
                        self.last_prompt_tokens = Some(usage.saturating_sub(reclaimable));
                    }
                    // Else: the plan doesn't clear the ROI bar — no mutation, and
                    // deliberately no notice. Pressure builds gradually, so this
                    // branch would otherwise fire (silently) every round while
                    // pressure is on but ROI isn't met; compaction is left to
                    // handle it once usage actually reaches the trigger.
                }
            }
            // Compact before the next request if this agent manages its own
            // context and is close to filling it (a small local model reading a
            // lot of files gets there fast). Pruning above gets first shot at
            // relieving pressure — cheap, no model call — so this expensive
            // fallback (a summarizer call plus a full cache nuke) only fires
            // when pruning couldn't buy enough runway, or usage is already past
            // what pruning alone can save.
            self.maybe_self_compact(&mut on_event).await;
            // Cost budget: stop before issuing another model call once the
            // session's estimated spend (incl. sub-agents) reaches the cap.
            if let Err(error) = self.budget_preflight().await {
                on_event(AgentEvent::Notice(error.to_string()));
                return Err(error);
            }
            // Stream one assistant turn, accumulating text + tool calls. The
            // connect is retried on transient errors and auto-compacted once on
            // a context-length overflow. Mid-stream failures are retried too
            // (history is unchanged at that point, so re-requesting is safe).
            let acc = self
                .connect_and_drain(&defs, &mut overflow_compacted, &mut on_event)
                .await?;
            if let Some(warning) = hrdr_llm::take_client_warning() {
                on_event(AgentEvent::Notice(warning));
            }
            // The sandbox's own degradation channel: a shell command that ran
            // with less OS confinement than its mode promised says so here,
            // once per process, through the same event the frontends already
            // render.
            if let Some(warning) = hrdr_tools::take_sandbox_notice() {
                on_event(AgentEvent::Notice(warning));
            }

            // Emit usage for the status bar + auto-compaction. Prefer the
            // server's reported counts; when it doesn't send any (e.g. a server
            // that ignores `stream_options.include_usage`), fall back to a rough
            // estimate so the context bar and compaction still work — an estimate
            // beats a stale/zero reading, and the overflow-retry path covers any
            // under-estimate.
            let (
                prompt_tokens,
                completion_tokens,
                cached_prompt_tokens,
                cost_usd,
                session_cost_usd,
            ) = self.account_usage(&acc).await;
            self.last_prompt_tokens = Some(prompt_tokens);
            on_event(AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
                cached_prompt_tokens,
                reasoning_tokens: acc.usage.as_ref().and_then(|u| u.reasoning_tokens()),
                cost_usd,
                session_cost_usd,
                cost_partial: self.session_cost_partial(),
            });

            // The reply hit the output cap — warn so a silently-truncated answer
            // or edit isn't mistaken for a complete one (raise `max_tokens` on the
            // Anthropic backend, or the model's cap otherwise). The *model* is
            // told too, on this round's last tool result (see below): it otherwise
            // resumes believing it emitted everything it meant to.
            let truncated = acc.truncated();
            if truncated {
                on_event(AgentEvent::Notice(
                    "⚠ response truncated at the output limit — it may be incomplete \
                     (raise max_tokens if this recurs)"
                        .to_string(),
                ));
            }

            let mut assistant = acc.into_message();
            ensure_assistant_has_content(&mut assistant);
            let tool_calls = assistant.tool_calls.clone().unwrap_or_default();
            self.messages.push(assistant);

            if tool_calls.is_empty() {
                // A degraded high-context model sometimes ends its turn on a
                // promise instead of doing the work — "I'll implement now",
                // zero tool calls, TODO items left dangling, and no background
                // sub-agent still doing that work. Give it exactly one chance
                // per turn to either finish the list or explicitly defer it,
                // instead of silently accepting the promise as done.
                if !nudged_this_turn && self.bg_handle_count() == 0 {
                    let unfinished: Vec<TodoItem> = self
                        .ctx
                        .todos
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .iter()
                        .filter(|t| {
                            t.status.as_str() != "completed" && t.status.as_str() != "cancelled"
                        })
                        .cloned()
                        .collect();
                    if !unfinished.is_empty() {
                        nudged_this_turn = true;
                        // Watch the rounds that follow for the list being
                        // *emptied* instead of reconciled (see below).
                        todo_watch = Some(TodoShrinkWatch {
                            unfinished: unfinished.iter().map(|t| t.content.clone()).collect(),
                            len: self.todo_snapshot().1,
                        });
                        on_event(AgentEvent::Notice(format!(
                            "turn ended with {} unfinished TODOs — nudging the model to \
                             finish or defer explicitly",
                            unfinished.len()
                        )));
                        self.push_user_message(
                            format!(
                                "[Your turn was about to end, but these TODO items are not \
                                 finished:\n{}\nEither continue now and complete them, or \
                                 reconcile the list item by item with the todo tool: send the \
                                 full list back with every one of these items still in it, \
                                 each marked `completed` or `cancelled` — and say plainly, to \
                                 the user, why you cancelled what you cancelled. Do not \
                                 replace, collapse, or drop items to make the list look \
                                 finished; a shorter list is not a resolved list.]",
                                render_unfinished_todos(&unfinished)
                            ),
                            MessageOrigin::Nudge,
                        );
                        continue;
                    }
                }
                // The model answered without calling a tool: the turn is over,
                // even if a steering message is pending. It has no tool result to
                // ride in on, so the frontend sends it as a turn of its own —
                // steering redirects work in progress, it doesn't extend a turn
                // the model already finished.
                self.fire_turn_end_hooks(&mut on_event).await;
                self.release_finished_subagents();
                self.age_todos();
                on_event(AgentEvent::TurnDone);
                return Ok(());
            }

            // Execute the requested tools, feeding results back. Runs of
            // consecutive concurrency-safe calls (reads/searches/fetches, and
            // `task` sub-agents) execute concurrently; a file-mutating call is a
            // barrier, run alone — so a read after a write still observes the
            // write, and results always land in call order.
            let mut idx = 0;
            while idx < tool_calls.len() {
                let concurrent = self.tools.is_concurrent(&tool_calls[idx].function.name);
                let mut end = idx + 1;
                while concurrent
                    && end < tool_calls.len()
                    && self.tools.is_concurrent(&tool_calls[end].function.name)
                {
                    end += 1;
                }
                let batch = &tool_calls[idx..end];
                idx = end;

                // One path for both: a read-only run executes concurrently, a
                // lone mutating call is a one-element batch. The refusal check,
                // arg parse, streamed output, and in-order results all live in
                // `run_tool_batch`.
                self.run_tool_batch(batch, &mut repeat, &mut on_event).await;
            }

            // A truncated reply is the one thing the model cannot notice itself:
            // it was cut off, so whatever it meant to emit after this point never
            // reached us, and next round it reads its own message as complete.
            // Ride this round's last tool result — the same channel as the budget
            // notes below — so it re-issues what is missing instead of assuming it
            // ran. Before the History snapshot, so a resume sees the note too.
            //
            // A truncated reply with *no* tool calls has no result to ride on and
            // ends the turn where it stands; that case is left to the user-facing
            // Notice above, since there is no next round to correct.
            if truncated
                && let Some(last) = self.messages.last_mut()
                && let Some(content) = &mut last.content
            {
                content.push_str(
                    "\n\n[note: your reply was cut off at the output limit — anything you \
                     intended after this point, including further tool calls, was lost and \
                     never ran. Re-issue what is missing rather than assuming it happened, and \
                     keep the next reply shorter.]",
                );
            }

            // Backstop for the turn-end TODO nudge: the model can "satisfy" it by
            // replacing the list with one collapsed "all done" item instead of
            // resolving the items it was shown. Fires at most once per turn (the
            // watch is disarmed either way), and only on the unambiguous shape —
            // the list got *shorter* and an item the nudge named is gone
            // altogether, not merely reworded or re-statused.
            if let Some(watch) = todo_watch.take() {
                let (current, len) = self.todo_snapshot();
                let removed: Vec<&String> = watch
                    .unfinished
                    .iter()
                    .filter(|c| !current.contains(c))
                    .collect();
                if len < watch.len && !removed.is_empty() {
                    on_event(AgentEvent::Notice(format!(
                        "{} nudged TODO items were removed rather than resolved — asking \
                         the model to restore them",
                        removed.len()
                    )));
                    let list = removed
                        .iter()
                        .map(|c| format!("- {c}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.push_user_message(
                        format!(
                            "[These TODO items were removed from the list rather than \
                             resolved:\n{list}\nDeleting an item is not finishing it. Send the \
                             list back through the todo tool with each of them restored and \
                             marked `completed` or `cancelled`, and tell the user which ones \
                             you did not actually do.]"
                        ),
                        MessageOrigin::Nudge,
                    );
                } else {
                    // Not a shrink (yet) — keep watching the rest of the turn.
                    todo_watch = Some(watch);
                }
            }

            // Mid-turn durability: every result of this round is committed, so
            // hand the frontend a history snapshot to persist. A crash from
            // here on loses at most the next round.
            on_event(AgentEvent::History(self.messages.clone()));

            // 80% of the budget: one soft warning, early enough that the model
            // can still commit what's done and sequence what's left. Rides the
            // last tool result of the round, exactly like the wrap-up note below
            // — the model reads it with that round's results.
            let used = step + 1;
            if !checkpoint_warned
                && checkpoint_warning_round(self.max_steps) == Some(used)
                && let Some(last) = self.messages.last_mut()
                && let Some(content) = &mut last.content
            {
                checkpoint_warned = true;
                content.push_str(&format!(
                    "\n\n[note: you've used {used} of {max} tool rounds this turn — checkpoint \
                     your work (commit what's done) and sequence what remains; the turn ends \
                     at {max}]",
                    max = self.max_steps
                ));
            }

            // Near the budget: tell the model so it wraps up instead of
            // getting cut off mid-plan.
            let remaining = self.max_steps - step - 1;
            if remaining == WRAP_UP_WARNING_ROUNDS
                && let Some(last) = self.messages.last_mut()
                && let Some(content) = &mut last.content
            {
                content.push_str(&format!(
                    "\n\n[note: only {remaining} tool rounds remain this turn — finish up \
                     and summarize]"
                ));
            }
        }

        // Budget exhausted: instead of failing the turn, run one final round
        // with no tools so the model must answer in text.
        on_event(AgentEvent::Notice(format!(
            "tool-round limit reached ({}) — asking the model to wrap up",
            self.max_steps
        )));
        self.messages.push(ChatMessage::user(
            "[The tool-call budget for this turn is exhausted. Do not request more tool \
             calls. Summarize what you accomplished and what remains to be done.]"
                .to_string(),
        ));
        // No `tools` are sent for this round (the model must answer in text),
        // but the turn's history is full of tool_use/tool_result blocks from
        // the rounds that already ran — the native Anthropic backend 400s any
        // request carrying those without a `tools` definition. Flatten the
        // protocol out for this request only; the real history (with the tool
        // protocol intact) is restored right after, so later turns still see
        // accurate tool-call pairing.
        let flattened = flatten_tool_protocol(&self.messages);
        if let Err(error) = self.budget_preflight().await {
            on_event(AgentEvent::Notice(error.to_string()));
            return Err(error);
        }
        let real_messages = std::mem::replace(&mut self.messages, flattened);
        let acc = self
            .connect_and_drain(&[], &mut overflow_compacted, &mut on_event)
            .await;
        self.messages = real_messages;
        let acc = acc?;
        let (prompt_tokens, completion_tokens, cached_prompt_tokens, cost_usd, session_cost_usd) =
            self.account_usage(&acc).await;
        on_event(AgentEvent::Usage {
            prompt_tokens,
            completion_tokens,
            cached_prompt_tokens,
            reasoning_tokens: acc
                .usage
                .as_ref()
                .and_then(|usage| usage.reasoning_tokens()),
            cost_usd,
            session_cost_usd,
            cost_partial: self.session_cost_partial(),
        });
        let mut wrap_up_reply = acc.into_message();
        ensure_assistant_has_content(&mut wrap_up_reply);
        self.messages.push(wrap_up_reply);
        self.fire_turn_end_hooks(&mut on_event).await;
        self.release_finished_subagents();
        self.age_todos();
        on_event(AgentEvent::TurnDone);
        Ok(())
    }

    /// Deliver one queued user message into the turn: (opening only) run the
    /// `user_prompt` hook, then emit [`AgentEvent::Steered`] carrying the display
    /// form and push the (possibly hook-augmented) `sent` text into history.
    ///
    /// The single path a user message takes to reach the model, whether it opens
    /// a turn or steers one already in flight — so a normal message and a steering
    /// message are the same thing (a queued message), differing only in *when*
    /// they are drained. Both announce themselves with `Steered`, so every user
    /// turn is in the event stream.
    ///
    /// Returns `Err` only when a `user_prompt` hook blocks the turn (opening
    /// only); a mid-turn steer never runs the hook, so it never blocks.
    pub(crate) async fn deliver_user_message<F: FnMut(AgentEvent)>(
        &mut self,
        msg: Steer,
        opening: bool,
        on_event: &mut F,
    ) -> Result<()> {
        let mut sent = msg.sent;
        // The user's original text, before any hook/recall augmentation — recall
        // keys on what the user actually typed, not the expanded form.
        let query = sent.clone();
        // `user_prompt` hooks see the message before the turn starts: a block
        // (exit 2) fails the turn before anything enters history; hook stdout
        // rides along as extra context for the model (the frontend still displays
        // only what the user typed). This fires for the turn opener, not for a
        // mid-turn steer — preserving today's behavior.
        if opening
            && !sent.trim().is_empty()
            && self.has_event_hooks(hrdr_tools::HookEvent::UserPrompt)
        {
            let payload = serde_json::json!({
                "event": "user_prompt",
                "prompt": sent,
                "cwd": self.ctx.cwd.display().to_string(),
                "model": self.client.model,
            });
            let out = hrdr_tools::run_event_hooks(
                &self.event_hooks,
                hrdr_tools::HookEvent::UserPrompt,
                None,
                &payload,
                &self.ctx.cwd,
            )
            .await;
            for note in out.notes {
                on_event(AgentEvent::Notice(note));
            }
            if let Some(reason) = out.block {
                bail!("blocked by user_prompt hook: {reason}");
            }
            if !out.context.is_empty() {
                sent.push_str("\n\n[hook context]\n");
                sent.push_str(&out.context.join("\n"));
            }
        }
        // Relevance recall (opening only, same shape as the hook context above):
        // surface the full text of the memories most relevant to the user's
        // original message into the model-facing `sent`, so it has the facts, not
        // just the always-loaded pointer index. Keyed on `query` (what the user
        // typed), never the hook-augmented text. `recall` is sync `std::fs` over
        // small files and best-effort — safe to call here without `spawn_blocking`.
        // The transcript/`Steered` display stays `msg.display`; a mid-turn steer
        // never recalls (guarded on `opening`, exactly like the hook).
        if opening
            && !query.trim().is_empty()
            && let Some(block) = hrdr_tools::memory::recall(
                self.ctx.memory_project.as_deref(),
                self.ctx.memory_global.as_deref(),
                &query,
                MEMORY_RECALL_BUDGET,
            )
        {
            sent.push_str("\n\n");
            sent.push_str(&block);
        }
        // The model reads the expanded (`sent`) form; the transcript shows what was
        // typed (`display`). A real opener is a `User` turn; a mid-turn correction
        // is tagged `Steering` so pruning/session serialization can still tell them
        // apart (both count as turn boundaries — see `plan_prune`).
        on_event(AgentEvent::Steered(msg.display));
        let origin = if opening {
            MessageOrigin::User
        } else {
            MessageOrigin::Steering
        };
        self.push_user_message(sent, origin);
        Ok(())
    }

    /// Emit the `ToolEnd` event and push the tool-result message for a
    /// completed call (shared by the sequential and concurrent paths). Feeds
    /// the repeat breaker, appending its nudge to a repeated call — failing or
    /// succeeding.
    fn finish_tool_call<F: FnMut(AgentEvent)>(
        &mut self,
        call: &hrdr_llm::ToolCall,
        elapsed: std::time::Duration,
        result: Result<String>,
        repeat: &mut RepeatGuard,
        on_event: &mut F,
    ) {
        let (ok, mut body) = match result {
            Ok(s) => (true, s),
            Err(e) => (false, tool_error_text(&e)),
        };
        let repeatable = self.tools.is_repeatable(&call.function.name);
        if let Some(nudge) = repeat.record(
            &call.function.name,
            &call.function.arguments,
            ok,
            repeatable,
        ) {
            body.push_str(&nudge);
        }
        on_event(AgentEvent::ToolEnd {
            id: call.id.clone(),
            name: call.function.name.clone(),
            result: body.clone(),
            ok,
        });
        // The `todo` tool replaces the shared list; emit the new state so every
        // listener — including this agent's own event log — records the update.
        if call.function.name == "todo" {
            let todos = self
                .ctx
                .todos
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            on_event(AgentEvent::TodoUpdated(todos));
        }
        // Record the call's wall-clock cost for the MODEL, appended after
        // (outside) any untrusted-content wrapper the tool added — trusted
        // harness metadata, present on failures too. Kept out of the ToolEnd
        // display event above: `(took 0ms)` on every instant tool is just noise
        // in the transcript, and the model is what asked for the timing.
        let recorded = format!("{body}\n\n(took {})", format_duration(elapsed));
        self.messages
            .push(ChatMessage::tool_result(call.id.clone(), recorded));
    }

    /// Run a batch of tool calls, forwarding each call's streamed output as
    /// `ToolOutput` events (attributed by call id) while they run. A read-only
    /// run executes concurrently; a lone mutating call is a one-element batch.
    /// Results are emitted and recorded in call order.
    async fn run_tool_batch<F: FnMut(AgentEvent)>(
        &mut self,
        batch: &[hrdr_llm::ToolCall],
        repeat: &mut RepeatGuard,
        on_event: &mut F,
    ) {
        // One shared (id, chunk) channel; each call gets a private sink whose
        // chunks a forwarder task tags with the call id.
        //
        // Both channels are bounded — this is advisory live-progress output,
        // not the tool result, so a producer that outruns the UI consumer
        // (e.g. a shell command emitting millions of lines) must never queue
        // unboundedly. UI_STREAM_CAP buffers a normal burst; past that,
        // `ctx.emit`'s `try_send` (see `ToolContext::emit`) drops lines
        // rather than blocking the tool, and the forwarder below does the
        // same into `shared_tx` — dropping at either stage just means the UI
        // sees gaps in the live stream, never the model or the tool result.
        let (shared_tx, mut shared_rx) =
            tokio::sync::mpsc::channel::<(String, String)>(UI_STREAM_CAP);
        let mut futs = Vec::with_capacity(batch.len());
        for call in batch {
            on_event(AgentEvent::ToolStart {
                id: call.id.clone(),
                name: call.function.name.clone(),
                args: call.function.arguments.clone(),
            });
            let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(UI_STREAM_CAP);
            let fwd_tx = shared_tx.clone();
            let fwd_id = call.id.clone();
            tokio::spawn(async move {
                while let Some(chunk) = rx.recv().await {
                    let _ = fwd_tx.try_send((fwd_id.clone(), chunk));
                }
            });
            let mut ctx = self.ctx.clone();
            ctx.stream = Some(tx);
            // So a `task` call can tag the background entry it spawns with the
            // transcript entry it came from.
            ctx.call_id = Some(call.id.clone());
            let name = call.function.name.clone();
            let raw_args = call.function.arguments.clone();
            // Cheap clone (Arc-backed registry) so the futures don't borrow
            // `self` — results are recorded with `&mut self` right after.
            let tools = self.tools.clone();
            let hooks = Arc::clone(&self.event_hooks);
            // A refused call (repeat breaker) resolves immediately instead of
            // executing; boxing keeps the join order == call order.
            type TimedResult = (std::time::Duration, Result<String>);
            let fut: std::pin::Pin<Box<dyn std::future::Future<Output = TimedResult> + Send>> =
                match repeat.refusal(&name, &raw_args) {
                    // A refused call never ran, so its cost is zero.
                    Some(msg) => {
                        Box::pin(
                            async move { (std::time::Duration::ZERO, Err(anyhow::anyhow!(msg))) },
                        )
                    }
                    None => Box::pin(async move {
                        let start = std::time::Instant::now();
                        let res: Result<String> = async move {
                            let args: serde_json::Value = if raw_args.trim().is_empty() {
                                serde_json::json!({})
                            } else {
                                match serde_json::from_str(&raw_args) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        return Err(anyhow::anyhow!(
                                            "invalid tool arguments JSON: {e}"
                                        ));
                                    }
                                }
                            };
                            // `pre_tool` hooks can veto the call (exit 2): the
                            // model sees the hook's reason as the tool error.
                            if hooks
                                .iter()
                                .any(|h| h.event == hrdr_tools::HookEvent::PreTool)
                            {
                                let payload = serde_json::json!({
                                    "event": "pre_tool",
                                    "tool": name,
                                    "args": args,
                                    "cwd": ctx.cwd.display().to_string(),
                                });
                                let out = hrdr_tools::run_event_hooks(
                                    &hooks,
                                    hrdr_tools::HookEvent::PreTool,
                                    Some(&name),
                                    &payload,
                                    &ctx.cwd,
                                )
                                .await;
                                if let Some(reason) = out.block {
                                    return Err(anyhow::anyhow!(
                                        "blocked by pre_tool hook: {reason}"
                                    ));
                                }
                                for note in out.notes {
                                    ctx.emit(format!("{note}\n"));
                                }
                            }
                            let mut res = tools.execute(&name, args.clone(), &ctx).await;
                            // `post_tool` hooks see the (bounded) result; their
                            // complaints ride back to the model with it.
                            if hooks
                                .iter()
                                .any(|h| h.event == hrdr_tools::HookEvent::PostTool)
                            {
                                let (ok, result_text) = match &res {
                                    Ok(r) => (true, hrdr_tools::truncate_inline(r, 30_000)),
                                    Err(e) => (false, e.to_string()),
                                };
                                let payload = serde_json::json!({
                                    "event": "post_tool",
                                    "tool": name,
                                    "args": args,
                                    "ok": ok,
                                    "result": result_text,
                                    "cwd": ctx.cwd.display().to_string(),
                                });
                                let out = hrdr_tools::run_event_hooks(
                                    &hooks,
                                    hrdr_tools::HookEvent::PostTool,
                                    Some(&name),
                                    &payload,
                                    &ctx.cwd,
                                )
                                .await;
                                let notes: Vec<String> =
                                    out.notes.into_iter().chain(out.block).collect();
                                if !notes.is_empty() {
                                    let joined = notes.join("\n");
                                    res = match res {
                                        Ok(r) => Ok(format!("{r}\n{joined}")),
                                        Err(e) => Err(anyhow::anyhow!("{e}\n{joined}")),
                                    };
                                }
                            }
                            res
                        }
                        .await;
                        (start.elapsed(), res)
                    }),
                };
            futs.push(fut);
        }
        drop(shared_tx); // forwarders hold the remaining senders

        let joined = futures_util::future::join_all(futs);
        tokio::pin!(joined);
        let results = loop {
            tokio::select! {
                r = &mut joined => break r,
                Some((id, chunk)) = shared_rx.recv() => {
                    on_event(AgentEvent::ToolOutput { id, chunk });
                }
            }
        };
        // Drain chunks buffered between the last poll and completion.
        while let Ok((id, chunk)) = shared_rx.try_recv() {
            on_event(AgentEvent::ToolOutput { id, chunk });
        }
        for (call, (elapsed, result)) in batch.iter().zip(results) {
            self.finish_tool_call(call, elapsed, result, repeat, on_event);
        }
    }

    /// Stream one assistant turn, retrying both the connect and any transient
    /// mid-stream failure with the same backoff the connect path uses. History
    /// is unchanged when `drain_stream` fails, so a clean re-request is safe.
    async fn connect_and_drain<F: FnMut(AgentEvent)>(
        &mut self,
        defs: &[ToolDef],
        overflow_compacted: &mut bool,
        on_event: &mut F,
    ) -> Result<Accumulator> {
        const MAX_DRAIN_RETRIES: usize = 3;
        let mut drain_attempt = 0usize;
        loop {
            let mut stream = self
                .connect_stream(defs, overflow_compacted, on_event)
                .await?;
            match drain_stream(&mut stream, on_event).await {
                Ok(acc) => return Ok(acc),
                Err(e) if is_transient(&e) && drain_attempt < MAX_DRAIN_RETRIES => {
                    drain_attempt += 1;
                    let delay =
                        retry_after_hint(&e).unwrap_or_else(|| retry_backoff(drain_attempt));
                    on_event(AgentEvent::Notice(format!(
                        "stream interrupted — retrying in {:.0}s \
                         (attempt {drain_attempt}/{MAX_DRAIN_RETRIES})",
                        delay.as_secs_f64()
                    )));
                    tokio::time::sleep(delay).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Before a request, inject fresh OAuth credentials for trusted ChatGPT, or
    /// strip any stale OAuth state when this is not ChatGPT.
    ///
    /// The gate is [`ResolvedModel::is_codex_oauth`] — the trusted kind AND the
    /// canonical endpoint, one definition — so a custom shadow, or a ChatGPT
    /// identity anywhere else, never receives the bearer/account header. On the
    /// non-ChatGPT path
    /// the resolved provider's own headers are restored (dropping any
    /// `ChatGPT-Account-Id` left over from a prior ChatGPT turn); the API key is
    /// left untouched (it is the key provider's real credential).
    pub(crate) async fn refresh_oauth_if_needed(&mut self) {
        if !self.resolved.is_codex_oauth() {
            // Defensive: ensure no stale bearer/account header survives a switch
            // away from ChatGPT. Idempotent for a steady-state key provider.
            if self.client.extra_headers_contains("ChatGPT-Account-Id") {
                self.client.set_headers(self.resolved.headers().to_vec());
            }
            return;
        }
        // A failed refresh leaves the previous state untouched; the authenticated
        // catalog/health path surfaces a genuine auth warning.
        if let Ok(access) =
            oauth::coordinated_oauth_access(self.resolved.kind(), self.resolved.base_url()).await
        {
            self.client.set_api_key(Some(access.access));
            let mut headers = self.resolved.headers().to_vec();
            if let Some(id) = access.account_id {
                headers.push(("ChatGPT-Account-Id".to_string(), id));
            }
            self.client.set_headers(headers);
        }
    }

    /// Open a chat stream, retrying transient network/server errors with
    /// exponential backoff and auto-compacting once on a context-length
    /// overflow. Emits `Notice` events for each recovery attempt.
    async fn connect_stream<F: FnMut(AgentEvent)>(
        &mut self,
        defs: &[ToolDef],
        overflow_compacted: &mut bool,
        on_event: &mut F,
    ) -> Result<ChatStream> {
        self.refresh_oauth_if_needed().await;
        const MAX_RETRIES: usize = 4;
        let mut attempt = 0usize;
        loop {
            match self.client.chat_stream(&self.messages, defs).await {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    // Context overflow → compact once, then retry.
                    if is_context_overflow(&e) && !*overflow_compacted && self.messages.len() > 2 {
                        on_event(AgentEvent::Notice(
                            "context window exceeded — compacting and retrying".to_string(),
                        ));
                        let (before, after) = self.compact(None).await?;
                        *overflow_compacted = true;
                        // `compact` reports `before == after` on every no-op path
                        // (nothing to summarize, or splitting the mega-turn bought
                        // nothing). Retrying then would resend the exact request
                        // that just failed and hit the same overflow again, having
                        // burned the single retry this branch allows — so fail
                        // clearly now instead of falling through to a generic
                        // "background task failed" once the caller gives up.
                        if after >= before {
                            bail!(
                                "context window exceeded and the current turn is too \
                                 large to compact ({after} messages, nothing left to \
                                 shrink) — {e}"
                            );
                        }
                        continue;
                    }
                    // Transient network/server error → backoff and retry. Honor a
                    // server `Retry-After` when present, else exponential backoff.
                    if is_transient(&e) && attempt < MAX_RETRIES {
                        attempt += 1;
                        let delay = retry_after_hint(&e).unwrap_or_else(|| retry_backoff(attempt));
                        on_event(AgentEvent::Notice(format!(
                            "network error — retrying in {:.0}s (attempt {attempt}/{MAX_RETRIES})",
                            delay.as_secs_f64()
                        )));
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }
    /// Release sub-agents whose work is done, whose answers the main agent has,
    /// and that nobody is looking at.
    ///
    /// At **turn end**, not per tool round. A blocking sub-agent is marked done and
    /// delivered inside the very round that spawned it (its answer *is* the tool
    /// result), so pruning mid-loop dropped it before the user could so much as see
    /// its row — the retained agent was unreachable in practice unless they were
    /// already looking at it. Holding until the turn ends gives the frontend the
    /// whole turn to pin the one being read.
    ///
    /// Running inside the agent, rather than leaving it to the frontend, is what
    /// keeps a headless run (which pins nothing) from leaking agents.
    fn release_finished_subagents(&mut self) {
        self.live_subagents.prune();
    }

    /// Age out TODOs that have been finished for `todo_ttl` turns.
    ///
    /// The TODO list is the agent's own state — the model re-reads it every turn —
    /// so ageing belongs here, not in a frontend. It used to run only in the TUI,
    /// which meant a headless run and every delegated sub-agent carried their
    /// finished items forever and paid for them in context on every request.
    fn age_todos(&mut self) {
        self.todo_turn += 1;
        if let Ok(mut todos) = self.ctx.todos.lock() {
            age_completed_todos(
                &mut todos,
                &mut self.todo_completed_at,
                self.todo_turn,
                self.todo_ttl,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrdr_llm::{ChatChunk, ChunkChoice, Delta};

    fn chunk(content: Option<&str>, reasoning: Option<&str>) -> ChatChunk {
        ChatChunk {
            choices: vec![ChunkChoice {
                delta: Delta {
                    content: content.map(str::to_string),
                    reasoning_content: reasoning.map(str::to_string),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
            anthropic_thinking_blocks: vec![],
        }
    }

    async fn events_for(chunks: Vec<ChatChunk>) -> (Vec<AgentEvent>, Accumulator) {
        let mut stream: ChatStream =
            Box::pin(futures_util::stream::iter(chunks.into_iter().map(Ok)));
        let mut seen = Vec::new();
        let acc = drain_stream(&mut stream, &mut |ev| seen.push(ev))
            .await
            .unwrap();
        (seen, acc)
    }

    /// An empty delta is dropped rather than forwarded — in both directions.
    ///
    /// Regression: a Qwen3-style backend keeps emitting `reasoning_content: ""`
    /// on every content chunk once it stops thinking, and `content: ""` while it
    /// is still thinking. Providers that omit the field deserialize to `None` and
    /// never reach here. Forwarded, each empty event lands between two deltas of
    /// the *other* kind, and the frontend only merges into the previous entry
    /// when it is the matching kind — so the reply came out as one
    /// `#N assistant` header per token group, with reasoning shredded the same
    /// way. Both empties render as nothing, so the split was the only symptom.
    #[tokio::test]
    async fn empty_deltas_are_not_forwarded_as_events() {
        let (events, _) = events_for(vec![
            chunk(None, Some("thinking")),
            chunk(Some(""), None), // must not close the reasoning block
            chunk(None, Some(" harder")),
            chunk(Some("answer"), Some("")), // empty reasoning must not split text
            chunk(Some(" more"), Some("")),
        ])
        .await;

        let reasoning: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Reasoning(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        let text: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();

        assert_eq!(
            reasoning,
            vec!["thinking", " harder"],
            "no empty reasoning event may be forwarded"
        );
        assert_eq!(
            text,
            vec!["answer", " more"],
            "no empty text event may be forwarded"
        );
    }

    /// Suppressing the *event* must not suppress accumulation: `acc.push` still
    /// runs for every chunk, so the assembled reply is unaffected.
    #[tokio::test]
    async fn empty_deltas_still_accumulate_into_the_final_message() {
        let (_, acc) = events_for(vec![
            chunk(Some("hello"), Some("")),
            chunk(Some(""), None),
            chunk(Some(" world"), Some("")),
        ])
        .await;
        assert_eq!(acc.into_message().content.as_deref(), Some("hello world"));
    }
}
