//! The request → response seam between a tool and the user.
//!
//! Everything else in hrdr flows one way: a tool emits, a frontend renders, and
//! the user's replies arrive as steering the agent picks up when it next looks.
//! An approval cannot work that way — the tool has to stop and be answered
//! before it decides how to run — so this is the one place a tool blocks on a
//! human.
//!
//! Which makes the failure mode obvious and worth stating first: **a gate that
//! can hang a turn is worse than no gate at all.** So it models whether anyone is
//! even *able* to answer. Nothing registers in this slice, so every request is
//! denied on the spot rather than waiting out a timeout — which is not a
//! placeholder but the permanently correct behaviour for a headless run, where
//! there is no user to ask.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::escalation::EscalationPolicy;

/// How long a frontend that CAN answer gets before the request is denied.
///
/// Only ever reached when someone is listening: with no listener the answer is
/// immediate. A minute is long enough to read a command and decide, and short
/// enough that a user who walked away costs one confined run rather than a turn
/// that never ends.
pub const APPROVAL_TIMEOUT_SECS: u64 = 60;

/// What the user said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Yes, this once.
    Once,
    /// Yes, and stop asking for this rule for the rest of the session.
    Session,
    /// No.
    Deny,
}

/// A consent decision that has been made, for the durable record.
///
/// The approval *question* is UI state and deliberately not persisted — by the
/// time anyone replays a transcript it has been answered. The **answer** is a
/// different thing: it is the moment a human agreed to move the boundary, and
/// without it nothing on disk says a command ran with less confinement than the
/// session's policy describes. Recorded for denials too, since "the user was
/// asked and said no" is exactly as much a fact about the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscalationDecision {
    /// The command verbatim, as approved.
    pub command: String,
    /// What the user was told they were agreeing to.
    pub reason: String,
    /// The labels the answer was keyed on.
    pub rules: Vec<String>,
    /// Whether it was granted, and whether it was remembered.
    pub decision: ApprovalDecision,
}

/// Decisions made by this agent, waiting to be drained into its transcript.
///
/// Same shape and same reasoning as [`SandboxNotices`](crate::SandboxNotices):
/// a tool call deep in `hrdr-tools` has something the agent must record, and no
/// way to emit an event itself. This is the agent's OWN channel — a sibling's
/// consent is a sibling's record.
#[derive(Debug, Default)]
pub struct EscalationLog {
    entries: Mutex<Vec<EscalationDecision>>,
}

impl EscalationLog {
    pub(crate) fn push(&self, decision: EscalationDecision) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(decision);
    }

    /// Take everything recorded so far. Draining rather than reading, because a
    /// decision must be persisted exactly once.
    pub fn take(&self) -> Vec<EscalationDecision> {
        std::mem::take(&mut *self.entries.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// One question waiting on a human, as a frontend receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// Answer with this ([`ApprovalGate::answer`]). Unique for the session.
    pub id: String,
    /// The exact command, verbatim — what the user is being asked to allow must
    /// be what actually runs, with no normalization in between.
    pub command: String,
    /// One line saying why it is being asked, ready to render as-is.
    pub reason: String,
    /// The rules it matched. Shown so "always allow" can name what it is
    /// remembering, and it is what the session memory is keyed on.
    pub rules: Vec<String>,
    /// Whether an [`ApprovalDecision::Session`] answer will actually be honoured
    /// as standing.
    ///
    /// `false` for a derived rule (see
    /// [`request_with_memory`](ApprovalGate::request_with_memory)), where the gate
    /// downgrades that answer to a one-off. A frontend must not offer a choice
    /// that silently means something else, so this is on the request rather than
    /// left to prose in `reason`: the button has to go, not be explained away.
    pub allow_session: bool,
}

#[derive(Default)]
struct GateState {
    /// How many frontends said they can answer. A count rather than a flag
    /// because the web UI and the TUI can both be attached to one session.
    listeners: usize,
    /// In-flight requests, by id.
    pending: HashMap<String, oneshot::Sender<ApprovalDecision>>,
    /// Rules the user approved for the session.
    approved_rules: HashSet<String>,
}

/// The escalation approval gate: it holds the eligibility rules, publishes
/// requests to whoever is rendering, and hands the answer back to the waiting
/// tool call.
///
/// Held behind an `Arc` on [`ToolContext::approvals`](crate::ToolContext::approvals)
/// so every tool call shares one — and, crucially, so a frontend can reach it
/// *without* the agent lock. A turn task holds that lock for its whole duration
/// (the same reason mid-turn history is *pushed* at frontends rather than read
/// back off the agent), and an answer that had to wait for the turn to end could
/// never arrive in time to unblock the turn.
///
/// Only the main agent gets one; see `Agent::new`.
pub struct ApprovalGate {
    policy: EscalationPolicy,
    timeout: Duration,
    /// Requests out to the frontend. Unbounded, and it must stay that way: the
    /// producer is a tool call that is about to block on the answer, so dropping
    /// or delaying a request is dropping the thing it is waiting for. The bound
    /// in practice is the one command in flight per tool call.
    tx: mpsc::UnboundedSender<ApprovalRequest>,
    state: Mutex<GateState>,
    next_id: AtomicU64,
}

impl ApprovalGate {
    /// A gate and the stream of requests to render. The agent keeps the
    /// receiver and turns each request into an
    /// `AgentEvent::ApprovalRequested`.
    pub fn new(policy: EscalationPolicy) -> (Arc<Self>, mpsc::UnboundedReceiver<ApprovalRequest>) {
        Self::with_timeout(policy, Duration::from_secs(APPROVAL_TIMEOUT_SECS))
    }

    /// [`new`](Self::new) with the wait made explicit, so a test can prove the
    /// timeout path without spending a minute in it.
    pub fn with_timeout(
        policy: EscalationPolicy,
        timeout: Duration,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<ApprovalRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                policy,
                timeout,
                tx,
                state: Mutex::new(GateState::default()),
                next_id: AtomicU64::new(1),
            }),
            rx,
        )
    }

    pub fn policy(&self) -> &EscalationPolicy {
        &self.policy
    }

    fn state(&self) -> std::sync::MutexGuard<'_, GateState> {
        // A poisoned lock costs an approval, not the session — the same trade
        // `SandboxNotices` makes, and here it fails toward denial.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Declare that this frontend will render approval requests and can answer
    /// them. Until something does, every request is denied immediately.
    pub fn register_frontend(&self) {
        self.state().listeners += 1;
    }

    /// Undo [`register_frontend`](Self::register_frontend) — a frontend that is
    /// detaching must say so, or the requests it can no longer see start
    /// costing a full timeout each.
    pub fn unregister_frontend(&self) {
        let mut state = self.state();
        state.listeners = state.listeners.saturating_sub(1);
    }

    /// Whether anyone is in a position to answer.
    pub fn can_answer(&self) -> bool {
        self.state().listeners > 0
    }

    /// [`request_with_memory`](Self::request_with_memory) with "for the session"
    /// available — the up-front, allowlisted path, where the rule the user is
    /// approving was written by hand in advance.
    pub async fn request(&self, command: &str, rules: &[String], reason: &str) -> ApprovalDecision {
        self.request_with_memory(command, rules, reason, true).await
    }

    /// Ask, and wait for the answer.
    ///
    /// Never waits when nobody can answer, and never waits longer than
    /// [`timeout`](Self::with_timeout) when someone can. Both paths end in
    /// [`ApprovalDecision::Deny`]: the whole feature fails closed, so the worst
    /// case is the command running exactly as confined as it does today.
    ///
    /// `remember` is whether a "for the session" answer may become a standing
    /// grant. False makes the gate treat that answer as
    /// [`Once`](ApprovalDecision::Once) and record nothing, which is what a
    /// *derived* rule needs: a label like `sh` — from `curl … | sh`, whose
    /// segments are each individually offerable — would otherwise be remembered
    /// and silently auto-approve every later `sh` without showing the user a
    /// thing. A rule somebody wrote into config in advance carries an intent that
    /// a label inferred from one failing command does not.
    pub async fn request_with_memory(
        &self,
        command: &str,
        rules: &[String],
        reason: &str,
        remember: bool,
    ) -> ApprovalDecision {
        let (id, rx) = {
            let mut state = self.state();
            // Headless, permanently: no channel to a human means the answer is
            // no, and it is no *now*. Waiting out a timeout nobody could ever
            // satisfy would stall every eligible command by a minute.
            if state.listeners == 0 {
                return ApprovalDecision::Deny;
            }
            // Already approved for the session — every rule of them. Partial
            // coverage still asks: `git push && git fetch` where only the push
            // was approved is a command the user has not seen.
            //
            // The emptiness guard is load-bearing, not defensive tidiness: `all`
            // over nothing is `true`, so a caller that passed no rules would be
            // handed a silent session-wide approval it never asked a human about.
            // No caller does today; one arriving later must fail closed.
            if remember
                && !rules.is_empty()
                && rules.iter().all(|r| state.approved_rules.contains(r))
            {
                return ApprovalDecision::Session;
            }
            let id = format!("esc-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
            let (tx, rx) = oneshot::channel();
            state.pending.insert(id.clone(), tx);
            drop(state);
            let request = ApprovalRequest {
                id: id.clone(),
                command: command.to_string(),
                reason: reason.to_string(),
                rules: rules.to_vec(),
                allow_session: remember,
            };
            if self.tx.send(request).is_err() {
                // The receiving end is gone, so the request reached nobody
                // however many listeners are registered. Don't wait for an
                // answer that cannot come.
                self.state().pending.remove(&id);
                return ApprovalDecision::Deny;
            }
            (id, rx)
        };
        match tokio::time::timeout(self.timeout, rx).await {
            // Downgraded rather than refused: the user did say yes, and this run
            // should proceed. What they cannot do is make it standing.
            Ok(Ok(ApprovalDecision::Session)) if !remember => ApprovalDecision::Once,
            Ok(Ok(ApprovalDecision::Session)) => {
                // Keyed on the RULE, not the command: the point of "for the
                // session" is that `git push origin main` and `git push -u
                // origin main` are one decision.
                self.state().approved_rules.extend(rules.iter().cloned());
                ApprovalDecision::Session
            }
            Ok(Ok(decision)) => decision,
            // Timed out, or the sender was dropped without answering. Drop the
            // entry here rather than leaving it for whoever polls `is_pending`:
            // the map holds a `oneshot::Sender` per request, and a session where
            // the user lets prompts expire would otherwise accumulate one
            // apiece with nothing obliged to come back for them.
            _ => {
                self.state().pending.remove(&id);
                ApprovalDecision::Deny
            }
        }
    }

    /// How many requests are still waiting, for the leak assertions in the
    /// tests. A gate that accumulates entries nobody will ever answer is a slow
    /// leak of `oneshot::Sender`s, and the only way to catch that is to count.
    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.state().pending.len()
    }

    /// Whether `id` is still waiting on an answer.
    ///
    /// The timeout fires *inside* [`request`](Self::request), which has no way to
    /// tell a frontend: the only channel out of here carries requests, and
    /// widening it into an enum would ripple through `AgentEvent` and every
    /// frontend that matches on it. So a frontend that is showing a request
    /// checks this instead, and closes the dialog when it goes false — otherwise
    /// the modal sits there soliciting consent for a decision that was made a
    /// minute ago without it.
    ///
    /// False also for an id that was answered, which is the same thing from the
    /// caller's side: nothing is waiting on it.
    pub fn is_pending(&self, id: &str) -> bool {
        let mut state = self.state();
        match state.pending.get(id) {
            // The entry outlived its waiter: `request` timed out and returned, or
            // the whole tool call was cancelled — Ctrl+C on a turn with a prompt
            // open — and its future went with it. Either way the receiver is
            // dropped and nobody is listening. Reaped here rather than left to
            // accumulate over a session.
            Some(tx) if tx.is_closed() => {
                state.pending.remove(id);
                false
            }
            Some(_) => true,
            None => false,
        }
    }

    /// Answer a pending request. `false` when the id is unknown — already
    /// answered, or timed out while the user was deciding.
    pub fn answer(&self, id: &str, decision: ApprovalDecision) -> bool {
        let Some(tx) = self.state().pending.remove(id) else {
            return false;
        };
        tx.send(decision).is_ok()
    }
}

impl std::fmt::Debug for ApprovalGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalGate")
            .field("rules", &self.policy.rules().len())
            .field("can_answer", &self.can_answer())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(timeout_ms: u64) -> (Arc<ApprovalGate>, mpsc::UnboundedReceiver<ApprovalRequest>) {
        ApprovalGate::with_timeout(
            EscalationPolicy::with_extra(&[]),
            Duration::from_millis(timeout_ms),
        )
    }

    /// The headless answer, and the one this slice ships with everywhere: no
    /// frontend, no wait, no.
    #[tokio::test]
    async fn with_nobody_to_ask_the_answer_is_no_immediately() {
        let (gate, mut rx) = gate(60_000);
        assert!(!gate.can_answer());
        let started = std::time::Instant::now();
        let decision = gate
            .request("git push", &["git push".to_string()], "why")
            .await;
        assert_eq!(decision, ApprovalDecision::Deny);
        // The point of the test: it did not wait out the minute.
        assert!(started.elapsed() < Duration::from_secs(1));
        // And nothing was published — there is nobody to publish to.
        assert!(rx.try_recv().is_err());
    }

    /// Someone can answer and doesn't. The turn must still end.
    #[tokio::test]
    async fn a_listener_that_never_answers_is_a_denial_after_the_timeout() {
        let (gate, mut rx) = gate(50);
        gate.register_frontend();
        let asked = gate.clone();
        let waiting = tokio::spawn(async move {
            asked
                .request("git push", &["git push".to_string()], "why")
                .await
        });
        // The request really did reach the frontend; it just went unanswered.
        let req = rx.recv().await.expect("the request is published");
        assert_eq!(req.command, "git push");
        assert_eq!(req.rules, vec!["git push".to_string()]);
        assert!(!req.id.is_empty());
        // What a frontend showing this request polls: still live now…
        assert!(gate.is_pending(&req.id));
        assert_eq!(waiting.await.unwrap(), ApprovalDecision::Deny);
        // The entry is dropped by `request` itself, at the moment it gives up.
        // Asserted BEFORE `is_pending`, which reaps a dead entry as a side
        // effect of looking — check it after and this passes whether or not
        // `request` cleans up, which is how the leak survived slice 1.
        assert_eq!(
            gate.pending_len(),
            0,
            "the timed-out entry is not leaked: one `oneshot::Sender` per \
             ignored prompt would accumulate for the life of the session"
        );
        // …and not after the timeout, which is how the dialog learns to close.
        assert!(!gate.is_pending(&req.id));
        assert!(!gate.answer(&req.id, ApprovalDecision::Once));
    }

    /// The answering path, and the id round-trip a frontend will use.
    #[tokio::test]
    async fn an_answered_request_resolves_with_the_decision() {
        for decision in [
            ApprovalDecision::Once,
            ApprovalDecision::Session,
            ApprovalDecision::Deny,
        ] {
            let (gate, mut rx) = gate(60_000);
            gate.register_frontend();
            let asked = gate.clone();
            let waiting = tokio::spawn(async move {
                asked
                    .request("git push", &["git push".to_string()], "why")
                    .await
            });
            let req = rx.recv().await.unwrap();
            assert!(gate.answer(&req.id, decision));
            assert_eq!(waiting.await.unwrap(), decision);
            // An id answers exactly once.
            assert!(!gate.answer(&req.id, decision));
        }
    }

    /// "For the session" is keyed on the RULE. If it were keyed on the command
    /// string, `git push origin main` and `git push -u origin main` would ask
    /// separately and the option would be worthless.
    #[tokio::test]
    async fn session_approval_is_remembered_per_rule_not_per_command() {
        let (gate, mut rx) = gate(60_000);
        gate.register_frontend();
        let asked = gate.clone();
        let waiting = tokio::spawn(async move {
            asked
                .request("git push origin main", &["git push".to_string()], "why")
                .await
        });
        let req = rx.recv().await.unwrap();
        gate.answer(&req.id, ApprovalDecision::Session);
        assert_eq!(waiting.await.unwrap(), ApprovalDecision::Session);

        // A different spelling of the same rule is not asked about again.
        let decision = gate
            .request("git push -u origin HEAD", &["git push".to_string()], "why")
            .await;
        assert_eq!(decision, ApprovalDecision::Session);
        assert!(rx.try_recv().is_err(), "the second command re-prompted");

        // …but a rule the user has NOT approved still asks, even alongside one
        // they have.
        let asked = gate.clone();
        let waiting = tokio::spawn(async move {
            asked
                .request(
                    "git push && git fetch",
                    &["git push".to_string(), "git fetch".to_string()],
                    "why",
                )
                .await
        });
        let req = rx.recv().await.expect("the unapproved rule re-prompts");
        gate.answer(&req.id, ApprovalDecision::Deny);
        assert_eq!(waiting.await.unwrap(), ApprovalDecision::Deny);
    }

    /// A frontend that detaches stops being able to answer, and requests go back
    /// to being denied on the spot rather than costing a timeout each.
    #[tokio::test]
    async fn a_detached_frontend_stops_counting() {
        let (gate, _rx) = gate(60_000);
        gate.register_frontend();
        assert!(gate.can_answer());
        gate.unregister_frontend();
        assert!(!gate.can_answer());
        assert_eq!(
            gate.request("git push", &["git push".to_string()], "why")
                .await,
            ApprovalDecision::Deny
        );
    }

    /// The receiver being gone is the same situation as nobody listening, even
    /// though the count says otherwise — deny now rather than wait.
    #[tokio::test]
    async fn a_dropped_receiver_denies_without_waiting() {
        let (gate, rx) = gate(60_000);
        gate.register_frontend();
        drop(rx);
        let started = std::time::Instant::now();
        assert_eq!(
            gate.request("git push", &["git push".to_string()], "why")
                .await,
            ApprovalDecision::Deny
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
