//! Token/cost counters — *per agent*, not per session.
//!
//! Every agent makes its own model calls, so every agent has its own usage: its
//! cumulative tokens, the size of its last prompt (its live context), the window
//! it is working against, and what it has cost. The main agent's copy is the one
//! a single-agent frontend calls "the session's", but that is a coincidence of
//! there being one agent — a delegated sub-agent on a different provider fills a
//! different window at a different price, and the status bar that claims
//! otherwise is lying about whichever agent you are looking at.
//!
//! Kept here (rather than in a frontend's session state) so the figures exist
//! with no UI attached: [`crate::LiveSubagents::send_prompt`] folds each call's
//! usage into the sub-agent's entry, and a frontend reads it off the registry.

use serde::{Deserialize, Serialize};

/// One agent's token and cost counters.
///
/// `tokens_in`/`tokens_out` accumulate over every model call the agent makes.
/// `last_prompt_tokens`/`last_completion_tokens` are the most recent call's usage
/// — the prompt half is the live context size ("X of Y"). `context_window` is the
/// model's advertised maximum, kept so the "of Y" is right immediately on resume,
/// before the endpoint has been re-probed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentUsage {
    #[serde(default)]
    pub tokens_in: usize,
    #[serde(default)]
    pub tokens_out: usize,
    /// Estimated USD spent by this agent, priced from the models.dev catalog; 0
    /// when nothing was priceable.
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub last_prompt_tokens: Option<u32>,
    #[serde(default)]
    pub last_completion_tokens: Option<u32>,
    #[serde(default)]
    pub context_window: Option<u32>,
}

impl AgentUsage {
    /// The latest call's `(prompt, completion)` usage — the shape the frontends
    /// hold it in — or `None` when no call has reported usage yet.
    pub fn last(&self) -> Option<(u32, u32)> {
        Some((self.last_prompt_tokens?, self.last_completion_tokens?))
    }

    /// Record the latest call's usage (`None` clears it, e.g. after `/clear`).
    pub fn set_last(&mut self, last: Option<(u32, u32)>) {
        self.last_prompt_tokens = last.map(|(p, _)| p);
        self.last_completion_tokens = last.map(|(_, c)| c);
    }

    /// Accumulate one model call: add to the running totals and remember it as
    /// the latest.
    pub fn record_call(&mut self, prompt: u32, completion: u32) {
        self.tokens_in += prompt as usize;
        self.tokens_out += completion as usize;
        self.set_last(Some((prompt, completion)));
    }

    /// Fold one [`crate::AgentEvent::Usage`] into these counters. The single
    /// place an event becomes a number, so an agent's counters read the same
    /// whoever is watching it — or when nobody is.
    pub fn record_event(&mut self, ev: &crate::AgentEvent) {
        if let crate::AgentEvent::Usage {
            prompt_tokens,
            completion_tokens,
            session_cost_usd,
            ..
        } = ev
        {
            self.record_call(*prompt_tokens, *completion_tokens);
            if let Some(total) = session_cost_usd {
                self.cost_usd = *total;
            }
        }
    }

    /// The live context size — the last call's prompt tokens.
    pub fn ctx_used(&self) -> usize {
        self.last_prompt_tokens.unwrap_or(0) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_accumulates_totals_and_becomes_the_latest() {
        let mut u = AgentUsage::default();
        assert_eq!(u.last(), None);
        u.record_call(100, 20);
        u.record_call(300, 5);
        assert_eq!(u.tokens_in, 400);
        assert_eq!(u.tokens_out, 25);
        assert_eq!(u.last(), Some((300, 5)), "the latest call, not the sum");
        assert_eq!(u.ctx_used(), 300, "context in use is the last prompt");
        u.set_last(None);
        assert_eq!(u.ctx_used(), 0, "cleared after a /clear or a compaction");
    }

    /// The event is folded into the counters here, so an agent's usage is the
    /// same whether a UI is watching it or not.
    #[test]
    fn a_usage_event_folds_into_the_counters() {
        let mut u = AgentUsage::default();
        u.record_event(&crate::AgentEvent::Usage {
            prompt_tokens: 10,
            completion_tokens: 4,
            cached_prompt_tokens: None,
            reasoning_tokens: None,
            cost_usd: None,
            session_cost_usd: Some(0.5),
        });
        assert_eq!(u.tokens_in, 10);
        assert_eq!(u.tokens_out, 4);
        assert_eq!(u.cost_usd, 0.5);
        // Anything else leaves them alone.
        u.record_event(&crate::AgentEvent::TurnDone);
        assert_eq!(u.tokens_in, 10);
    }
}
