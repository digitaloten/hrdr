//! The clock on an agent's turn — *per agent*, not per session.
//!
//! How long the model has been working, when its first token arrived, how many it
//! has produced, whether it is inferring or waiting on a tool: these describe **a
//! turn on an agent**. Every agent has turns, so every agent has these.
//!
//! They used to live in the frontend, which meant they existed only for the main
//! agent — so a sub-agent's view borrowed the main agent's clock. Watching a
//! sub-agent work showed you the *main* agent's spinner, throughput and elapsed
//! time, and a sub-agent grinding away under an idle main agent showed no loader
//! at all.
//!
//! Kept beside the event record it is derived from: [`crate::LiveAgents::record`]
//! feeds every event through [`TurnStats::record`], so the figures are right with
//! no UI attached.

use std::time::{Duration, Instant, SystemTime};

use crate::AgentEvent;

/// The live state of one agent's turn.
///
/// Two clocks, deliberately: `infer_*` is how long the model has been *on the
/// hook* for this turn (everything but the tool calls it waited on) — what the
/// spinner and the elapsed figure read. `decode_*` is the far narrower window in
/// which it was actually emitting tokens, and it is the only denominator
/// throughput may use. They differ by every wait that produces no tokens:
/// prefill, retry backoff, a compaction call, turn-end hooks, the local
/// bookkeeping between rounds. Dividing tokens by the first is what made a fast
/// model read as one that slows down over a long turn — every round adds another
/// prefill to the denominator and none of it to the numerator.
#[derive(Debug, Clone, Copy, Default)]
pub struct TurnStats {
    /// When the turn began (monotonic — for durations).
    pub started: Option<Instant>,
    /// When the turn began (wall clock — for "started 2m ago").
    pub started_at: Option<SystemTime>,
    /// When the first token arrived — the provider's time-to-first-token.
    pub first_token_at: Option<Instant>,
    /// Output tokens the provider reported for the rounds that have finished —
    /// text, reasoning *and* tool-call arguments, which is the whole point of
    /// preferring its count to a delta tally (see [`Self::out_tokens`]).
    banked_tokens: usize,
    /// Streamed deltas since the last usage report: the live stand-in for the
    /// round in flight, discarded once the provider's real count lands.
    live_deltas: usize,
    /// Tool calls in flight this round. While any is running the model is idle.
    pub tools_running: usize,
    /// Model working time banked from earlier stretches of this turn.
    infer_banked: Duration,
    /// Start of the stretch the model is working *now*, if it is.
    infer_started: Option<Instant>,
    /// Measured token-generation time banked from the rounds that have finished.
    decode_banked: Duration,
    /// Start of the round in flight's *observed* generation, if it has started
    /// streaming text or reasoning. Only ever an estimate for the round on
    /// screen — a round that streams nothing but tool-call arguments never sets
    /// it — and always thrown away in favour of the measured figure on `Usage`.
    decode_live: Option<Instant>,
    /// Prompt tokens served from cache on the latest call, when reported.
    pub last_cached_tokens: Option<u32>,
    /// Completion tokens spent on reasoning on the latest call, when reported.
    pub last_reasoning_tokens: Option<u32>,
}

impl TurnStats {
    /// A turn is starting: reset the clock and put the model to work.
    pub fn begin(&mut self) {
        let now = Instant::now();
        *self = Self {
            started: Some(now),
            started_at: Some(SystemTime::now()),
            infer_started: Some(now),
            ..Default::default()
        };
    }

    /// Fold one of this agent's events into the clock.
    pub fn record(&mut self, ev: &AgentEvent) {
        match ev {
            // A streamed delta: the first one is time-to-first-token, and it
            // starts the generation clock for the round on screen.
            AgentEvent::Text(_) | AgentEvent::Reasoning(_) => {
                self.first_token_at.get_or_insert_with(Instant::now);
                // Only inside a turn: a delta recorded against an agent with no
                // turn open (a replay, a test) must not start a clock that
                // nothing will ever close.
                if self.started.is_some() {
                    self.decode_live.get_or_insert_with(Instant::now);
                }
                self.live_deltas += 1;
            }
            // The model has handed off: it is idle until every tool of this round
            // returns, so its clock stops rather than inflating the turn with time
            // it spent waiting.
            AgentEvent::ToolStart { .. } => {
                self.tools_running += 1;
                if self.tools_running == 1 {
                    self.pause();
                }
            }
            // The last tool of the round returned: it is about to ask the model
            // again, so it is working from here.
            AgentEvent::ToolEnd { .. } => {
                self.tools_running = self.tools_running.saturating_sub(1);
                if self.tools_running == 0 {
                    self.resume();
                }
            }
            // A round just closed: bank what it really produced, and how long it
            // really spent producing it. Both figures replace this round's live
            // estimates rather than adding to them.
            AgentEvent::Usage {
                completion_tokens,
                cached_prompt_tokens,
                reasoning_tokens,
                decode_ms,
                ..
            } => {
                self.decode_live = None;
                self.decode_banked += Duration::from_millis(*decode_ms as u64);
                // A server that reports no completion tokens (some local ones
                // report zeros) leaves the delta tally as the round's best
                // estimate — never zero out a count we did observe.
                self.banked_tokens += match *completion_tokens {
                    0 => self.live_deltas,
                    n => n as usize,
                };
                self.live_deltas = 0;
                self.last_cached_tokens = *cached_prompt_tokens;
                self.last_reasoning_tokens = *reasoning_tokens;
            }
            AgentEvent::TurnDone => self.end(),
            _ => {}
        }
    }

    /// The turn is over: stop the clock.
    pub fn end(&mut self) {
        self.pause();
        self.tools_running = 0;
    }

    fn pause(&mut self) {
        if let Some(t) = self.infer_started.take() {
            self.infer_banked += t.elapsed();
        }
        // The generation clock stops with it. Normally `Usage` has already
        // closed the round; this covers a turn that ends without one (an error,
        // a cancellation) so a half-open clock can't run on.
        if let Some(t) = self.decode_live.take() {
            self.decode_banked += t.elapsed();
        }
    }

    fn resume(&mut self) {
        self.infer_started.get_or_insert_with(Instant::now);
    }

    /// Whether the model is working *right now* — as opposed to waiting on a tool
    /// call, or not being in a turn at all. This is what a loader shows.
    pub fn inferring(&self) -> bool {
        self.infer_started.is_some()
    }

    /// How long the model has actually worked this turn: the banked stretches plus
    /// the one in progress. Excludes time spent waiting on tool calls.
    pub fn infer_elapsed(&self) -> Duration {
        self.infer_banked
            + self
                .infer_started
                .map(|t| t.elapsed())
                .unwrap_or(Duration::ZERO)
    }

    /// How long the model spent actually emitting tokens this turn: the measured
    /// generation window of each finished round, plus the round in flight.
    ///
    /// Strictly less than [`Self::infer_elapsed`] — it excludes every wait that
    /// yields no tokens, which is what makes it the only honest denominator for
    /// throughput.
    pub fn decode_elapsed(&self) -> Duration {
        self.decode_banked
            + self
                .decode_live
                .map(|t| t.elapsed())
                .unwrap_or(Duration::ZERO)
    }

    /// Output tokens produced this turn, across every round.
    ///
    /// The provider's own count for each finished round, so tool-call arguments
    /// and reasoning are in it — a delta tally sees neither, and a turn spent
    /// writing files is nearly all tool-call arguments. Only the round in flight
    /// is a delta estimate, and it is replaced the moment that round reports.
    pub fn out_tokens(&self) -> usize {
        self.banked_tokens + self.live_deltas
    }

    /// Time-to-first-token, in seconds.
    pub fn ttft(&self) -> Option<f64> {
        let (start, first) = (self.started?, self.first_token_at?);
        Some(first.duration_since(start).as_secs_f64())
    }

    /// Output tokens per second of *generation* time — see [`Self::decode_elapsed`].
    /// Neither the tool calls it waited on nor the prefill it sat through count
    /// against the model here.
    pub fn tok_per_sec(&self) -> f64 {
        let secs = self.decode_elapsed().as_secs_f64();
        match self.out_tokens() {
            0 => 0.0,
            n if secs > 0.0 => n as f64 / secs,
            _ => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_start() -> AgentEvent {
        AgentEvent::ToolStart {
            id: "c1".into(),
            name: "bash".into(),
            args: "{}".into(),
        }
    }

    fn tool_end() -> AgentEvent {
        AgentEvent::ToolEnd {
            id: "c1".into(),
            name: "bash".into(),
            result: String::new(),
            ok: true,
        }
    }

    /// The clock stops while the model waits on a tool call — otherwise a turn that
    /// spent a minute in `bash` reads as a minute of slow inference.
    #[test]
    fn the_model_clock_stops_while_a_tool_runs() {
        let mut t = TurnStats::default();
        t.begin();
        assert!(t.inferring(), "a turn opens with the model working");

        t.record(&tool_start());
        assert!(!t.inferring(), "it is idle while a tool runs");
        let banked = t.infer_elapsed();
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(
            t.infer_elapsed(),
            banked,
            "and the clock does not advance while it waits"
        );

        t.record(&tool_end());
        assert!(t.inferring(), "it is working again once the round returns");
    }

    /// A round with several tools is one idle stretch, not one per tool.
    #[test]
    fn a_round_of_several_tools_is_one_idle_stretch() {
        let mut t = TurnStats::default();
        t.begin();
        t.record(&tool_start());
        t.record(&tool_start());
        t.record(&tool_end());
        assert!(!t.inferring(), "still waiting on the second tool");
        t.record(&tool_end());
        assert!(t.inferring());
    }

    #[test]
    fn deltas_count_toward_throughput_and_the_first_one_is_ttft() {
        let mut t = TurnStats::default();
        t.begin();
        assert_eq!(t.ttft(), None, "nothing has streamed yet");
        t.record(&AgentEvent::Text("a".into()));
        t.record(&AgentEvent::Reasoning("b".into()));
        assert_eq!(t.out_tokens(), 2, "reasoning is output too");
        assert!(t.ttft().is_some(), "the first delta is time-to-first-token");
        assert!(t.tok_per_sec() > 0.0);
    }

    fn usage(completion_tokens: u32, decode_ms: u32) -> AgentEvent {
        AgentEvent::Usage {
            prompt_tokens: 1000,
            completion_tokens,
            decode_ms,
            cached_prompt_tokens: None,
            reasoning_tokens: None,
            cost_usd: None,
            session_cost_usd: None,
            cost_partial: false,
        }
    }

    /// Throughput is measured over the model's *generation* window, not its
    /// working time. This is the whole bug: a turn's working time also holds the
    /// prefill wait before each round streams, and that wait grows with the
    /// context, so dividing by it made a model that was still generating at full
    /// speed read as one steadily slowing down.
    #[test]
    fn throughput_ignores_the_prefill_the_round_waited_through() {
        let mut t = TurnStats::default();
        t.begin();
        // Stand in for a long prefill: working time accrues, no tokens arrive.
        std::thread::sleep(Duration::from_millis(60));
        t.record(&AgentEvent::Text("a".into()));
        // The round reports: 20 tokens generated over 10ms of streaming.
        t.record(&usage(20, 10));

        assert_eq!(t.decode_elapsed(), Duration::from_millis(10));
        assert!(
            t.infer_elapsed() > t.decode_elapsed(),
            "the wait is in the working time and nowhere else"
        );
        assert_eq!(t.out_tokens(), 20);
        assert!(
            (t.tok_per_sec() - 2000.0).abs() < 1.0,
            "20 tokens in 10ms is 2000 tok/s, whatever the wait around it: {}",
            t.tok_per_sec()
        );
    }

    /// A round whose entire output is one tool call streams no text and no
    /// reasoning — so an event-driven clock never starts, and an event-driven
    /// tally counts nothing, while the model in fact spent that round emitting
    /// argument JSON. Both figures come from the round's own report instead.
    #[test]
    fn a_tool_call_only_round_is_counted_and_timed() {
        let mut t = TurnStats::default();
        t.begin();
        t.record(&usage(300, 100));
        assert_eq!(t.out_tokens(), 300, "tool-call arguments are output");
        assert_eq!(t.decode_elapsed(), Duration::from_millis(100));
        assert!(
            (t.tok_per_sec() - 3000.0).abs() < 1.0,
            "{}",
            t.tok_per_sec()
        );
    }

    /// The provider's count replaces the round's delta estimate rather than
    /// adding to it — otherwise every round would be counted twice over.
    #[test]
    fn a_reported_round_replaces_its_own_estimate() {
        let mut t = TurnStats::default();
        t.begin();
        t.record(&AgentEvent::Text("a".into()));
        t.record(&AgentEvent::Text("b".into()));
        assert_eq!(t.out_tokens(), 2, "deltas stand in until the round reports");
        t.record(&usage(500, 50));
        assert_eq!(t.out_tokens(), 500, "not 502");
        // And the next round accumulates on top.
        t.record(&AgentEvent::Text("c".into()));
        assert_eq!(t.out_tokens(), 501);
        t.record(&usage(100, 50));
        assert_eq!(t.out_tokens(), 600);
        assert_eq!(
            t.decode_elapsed(),
            Duration::from_millis(100),
            "both rounds"
        );
    }

    /// Some local servers report zeros. Banking that would erase a count we
    /// watched arrive, so the delta tally stands for that round.
    #[test]
    fn a_round_reporting_no_tokens_keeps_what_was_observed() {
        let mut t = TurnStats::default();
        t.begin();
        t.record(&AgentEvent::Text("a".into()));
        t.record(&AgentEvent::Reasoning("b".into()));
        t.record(&usage(0, 40));
        assert_eq!(t.out_tokens(), 2);
        assert!(t.tok_per_sec() > 0.0, "and it still reads as throughput");
    }

    /// The generation clock must not keep running once a turn stops — a turn
    /// that ends without a closing `Usage` (cancelled, errored) would otherwise
    /// bank wall time forever and sink the rate to nothing.
    #[test]
    fn ending_a_turn_mid_stream_closes_the_generation_clock() {
        let mut t = TurnStats::default();
        t.begin();
        t.record(&AgentEvent::Text("a".into()));
        t.end();
        let decoded = t.decode_elapsed();
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(t.decode_elapsed(), decoded);
    }

    #[test]
    fn a_finished_turn_stops_the_clock() {
        let mut t = TurnStats::default();
        t.begin();
        t.record(&AgentEvent::TurnDone);
        assert!(!t.inferring(), "a finished turn is not still generating");
        let elapsed = t.infer_elapsed();
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(t.infer_elapsed(), elapsed, "and its clock is frozen");
    }
}
