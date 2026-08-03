# Session accounting — plan

**Delete this file when the last slice lands.** What still binds future work
goes to `docs/backlog.md`; what shipped goes to `CHANGELOG.md`. A plan kept past
its implementation is a second, drifting description of the code.

## The symptom

`CompactionReport::notice()` prints a prompt-cache fraction for the
summarization call, and that figure is the only evidence anywhere that
compacting in place — the whole point of the compaction rewrite — still works.
It cannot be read:

- After a tool-call retry the fraction is near 100% because the identical
  previous attempt warmed the cache, not because the live session prefix
  matched.
- At any shrink stage above 0 it is near zero **by construction** — every stage
  rewrites message bodies and gives the cache up. Expected, not a regression,
  and the line does not say which.

The fix is not a better sentence. One call's fraction is a sample; the question
("is prefix caching working for this session?") is about the series. hrdr counts
tokens and dollars per session already and throws the cache figures away.

## What exists today

| Fact                                                                                                                | Where                                                         |
| ------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------- |
| `account_usage` is the one chokepoint every model call passes through — it prices the call and adds to `cost_total` | `hrdr-agent/src/budget.rs:107`                                |
| It computes cache reads and cache writes…                                                                           | `budget.rs:131`, `budget.rs:137`                              |
| …prices both, then **discards the write count** and returns a 5-tuple                                               | `budget.rs:141`, `budget.rs:171`                              |
| Three call sites destructure that tuple positionally                                                                | `turn_loop.rs:568`, `turn_loop.rs:832`, `compaction.rs:925`   |
| `CompactionSpend` is a hand-rolled subset of the same tuple, passed as a `&mut` out-param                           | `compaction.rs`                                               |
| `AgentUsage::record_event` is "the single place an event becomes a number" — and ignores `cached_prompt_tokens`     | `hrdr-agent/src/usage.rs:71`                                  |
| Every emitted event folds through it, for sub-agents and for the pane on screen                                     | `registry.rs:412`, `pane.rs:126`                              |
| `prompt_tokens` is the **inclusive** total (plain + cache read + cache write) after normalization                   | `hrdr-llm/src/anthropic.rs:1025`, test at `anthropic.rs:2001` |
| `/cost` and `/status` read `session_tokens()` and `session_cost()` off the active pane's `AgentUsage`               | `hrdr-app/src/commands/dispatch.rs:514`, `dispatch.rs:94`     |

### The bug this uncovers

**Compaction's calls never emit `AgentEvent::Usage`.** `plain_completion_inner`
calls `account_usage` (so the money lands in `cost_total`) but drains its stream
into a no-op sink and emits nothing. So every compaction's prompt and completion
tokens are missing from `tokens_in`/`tokens_out`, and `/cost` under-reports by
exactly the calls that are largest — a summarization request carries the whole
history. The gap grows with every compaction, and nothing flags it.

That is not a reporting nicety. It is the accounting being wrong.

## Design

### One type for what a call cost

```rust
/// What one model call was billed.
pub(crate) struct CallSpend {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Prompt tokens served from cache, and written into it. `None` means the
    /// provider reported no figure — which is not zero and must never render
    /// as one.
    pub cache_read_tokens: Option<u32>,
    pub cache_write_tokens: Option<u32>,
    pub cost_usd: Option<f64>,
    pub session_cost_usd: Option<f64>,
}
```

`account_usage` returns it. `CompactionSpend` is deleted — it was this type
minus two fields. The `&mut` out-param goes with it: returning the value makes
"a spend exists ⟺ a request succeeded" a fact of the type rather than a comment.

### Cache totals live beside the token totals

`AgentUsage` gains three counters, folded in `record_event` next to
`record_call`:

```rust
pub cache_read_tokens: usize,
pub cache_write_tokens: usize,
/// Prompt tokens from calls whose cache use the provider actually reported —
/// the only honest denominator for `cache_hit_rate`.
pub cache_measured_tokens: usize,
```

`cache_measured_tokens` is the load-bearing one. A session that mixes a
cache-reporting provider with one that reports nothing would otherwise read
artificially low, and "the cache stopped working" is exactly the wrong
conclusion to draw from "the provider stopped saying". A call contributes to it
only when it reported at least one of the two figures.

```rust
/// Fraction of measured prompt tokens served from cache, or `None` when no
/// call this session reported any cache figure at all.
pub fn cache_hit_rate(&self) -> Option<f64>;
```

`AgentEvent::Usage` gains `cache_creation_tokens: Option<u32>` — it already
carries `cached_prompt_tokens`, and the write count has to travel the same way
or `record_event` cannot see it.

Scope note: these go on `AgentUsage` (per agent), not on the `cost_total` arc
(shared across the sub-agent tree). That matches what the counters already mean
— `tokens_in`/`tokens_out` are this agent's, and a sub-agent on another provider
has its own prefix and its own cache behaviour. Money is shared because a budget
is; a cache rate is not.

### Compaction reports like every other call

`Agent::compact` takes an event sink and emits `AgentEvent::Usage` per
summarization attempt, through the existing path. Callers:

| Caller                        | Sink                                              |
| ----------------------------- | ------------------------------------------------- |
| `maybe_self_compact`          | already has `on_event`                            |
| `recover_context_overflow`    | already has `on_event`                            |
| `run_compaction` (`hrdr-app`) | gains one; `spawn_compaction` builds it from `tx` |

The TUI already has the pattern: `launch_turn` wraps `self.tx` in an
`EventSender` and hands the closure to `registry.start_turn`
(`hrdr-tui/src/app.rs:2260`). `spawn_compaction` (`app.rs:2354`) does the same
and routes to `TurnMsg::Event` for the main pane, `TurnMsg::SubAgent(key, ev)`
for a sub-agent's.

Emitting per attempt — not once at the end — is deliberate: a tool-call retry is
a billed call, and the counters should show it. That also makes the under-report
this plan found impossible to reintroduce quietly.

### The notice becomes interpretable

With the series available elsewhere, the compaction line only has to explain its
own sample. `CompactionReport` gains:

```rust
/// Which shrink stage produced the summary, and how many attempts it took.
pub stage: usize,
pub attempts: usize,
```

Both are already locals in `compact`. `notice()` renders them only when they are
not the trivial values, so the common case is unchanged:

```
compacted on request 42 → 6 messages · summary call: 51203 prompt tokens, 94% from cache, 812 output, $0.0412
context window exceeded — compacted 61 → 8 messages · 2 attempts, quarter-history stage · summary call: 18022 prompt tokens, 3% from cache, 900 output, $0.0180
```

A 3% reading next to "quarter-history stage" is a fact about the rescue. A 3%
reading with no stage named is an unanswerable question, which is what it is
now.

## Slices

Each is independently shippable, committed and pushed on its own, with the
workspace green (`cargo clippy --all-targets -- -D warnings`, `cargo fmt --all`,
`cargo test`).

**Slice 1 — `CallSpend`.** Name `account_usage`'s return; delete
`CompactionSpend` and the out-param; spread it at the two `AgentEvent::Usage`
constructions. Pure move, no behaviour change. _Verified by:_ the suite as it
stands. Nothing new can go red because nothing new happens — the point of doing
it first is that the next three slices are then field additions rather than
signature churn.

**Slice 2 — cache counters.** `cache_creation_tokens` on the event; the three
counters and `cache_hit_rate` on `AgentUsage`. _Red first:_ a test folding two
`Usage` events with cache figures and one without, asserting the rate uses only
the measured denominator, and that an all-unreported session yields `None`
rather than `Some(0.0)`.

**Slice 3 — compaction emits `Usage`.** Sink through `compact` →
`run_compaction` → `spawn_compaction`. _Red first:_ a test that compacts and
asserts the emitted events contain a `Usage` whose `prompt_tokens` matches the
report's. It fails today: no event is emitted at all.

**Slice 4 — surface it.** `session_cache()` on the command host; `/cost` and
`/status` print the rate when one is known. _Red first:_ a dispatch test
asserting `/cost` names the rate, and omits the clause entirely when nothing
reported.

**Slice 5 — the symptom.** `stage` and `attempts` on `CompactionReport`;
`notice()` renders them. _Red first:_ a test driving an overflow escalation and
asserting the notice names the stage it landed on, and one driving a tool-call
retry asserting it says two attempts.

## Not in scope

- **Merging compaction's request loop into the turn loop.** They look alike and
  exist for different reasons — the turn loop executes tools, emits per-round
  events and handles steering; compaction must not execute tools and is
  deliberately silent. Sharing the type is right; sharing the loop would grow a
  flag per difference.
- **A live-provider cache test.** The right long-term check for "did the prefix
  cache actually hit" is an `#[ignore]`d integration test against a real key,
  since it needs two sequential requests to a real provider. It needs a secret
  in CI, which is an infrastructure decision, not a code change. Goes to the
  backlog.
- **Persisting cache counters across a resume.** `AgentUsage` is serialized into
  session files, so new fields need `#[serde(default)]` — they get it, and a
  resumed session starts its cache counters at zero rather than reconstructing
  them. Reconstruction would need per-call history nothing stores.
