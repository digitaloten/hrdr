//! `WebHost` — implements `hrdr_app::CommandHost` for the web server so
//! every shared slash command works over WS.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hrdr_agent::{Agent, EntryKind, MAIN_KEY, ModelRef, PaneId, Session};
use hrdr_app::{CommandHost, ExpandMode, LineKind};
use tokio::sync::mpsc;

use crate::convert::build_set_input;
use crate::session::WebSession;

/// A `CommandHost` that drives a `WebSession`. Short-lived — constructed per
/// `dispatch()` call.
pub struct WebHost<'a> {
    pub session: &'a mut WebSession,
    pub line_tx: mpsc::UnboundedSender<(LineKind, String)>,
}

impl WebHost<'_> {
    /// Repoint the agent at the `(provider, model)` the just-adopted session ran on —
    /// the tail of the TUI's `adopt_state`, factored out of [`CommandHost::resume`].
    ///
    /// One thing stops it: an identity the agent is **already on** needs no switch.
    fn restore_identity(&mut self) {
        let (reference, window) = {
            let s = &self.session.panes().main().state;
            (s.model.clone(), s.usage.context_window)
        };
        // The identity the AGENT is on — what the registry says it publishes, which is
        // the endpoint it is actually talking to.
        let current = self.session.live().with(|v| {
            v.iter()
                .find(|e| e.key == MAIN_KEY)
                .map(|e| (e.provider.clone().unwrap_or_default(), e.model.clone()))
        });
        let unchanged = current.as_ref()
            == Some(&(
                reference.provider().to_string(),
                reference.model().to_string(),
            ));
        if unchanged || reference.model().is_empty() {
            return;
        }
        let name = reference.provider().to_string();
        let model = reference.model().to_string();
        if let Err(e) = hrdr_app::restore_session_provider(self, &name, model, window) {
            self.info(format!(
                "this session ran on provider '{name}', which isn't usable here ({e}) — \
                 staying on the current endpoint; /model to switch"
            ));
        }
        // No endpoint is written into the chrome here (the TUI's `set_active_base_url`
        // step): the main pane's model/provider/endpoint are rebuilt from the registry
        // every tick, and the agent republishes all three itself the moment the switch
        // lands — so the bar names the endpoint the agent reached, not the one it was
        // asked to reach.
    }
}

impl CommandHost for WebHost<'_> {
    fn info(&mut self, line: String) {
        let _ = self.line_tx.send((LineKind::System, line));
    }

    fn agent(&self) -> Arc<tokio::sync::Mutex<Agent>> {
        let pane_id = self.session.panes().active();
        let key = match pane_id {
            PaneId::MAIN => MAIN_KEY,
            PaneId(k) => k,
        };
        self.session
            .live()
            .handle(key)
            .map(|(a, _)| a)
            .unwrap_or_else(|| self.session.agent().clone())
    }

    fn cwd(&self) -> PathBuf {
        hrdr_app::agent_cwd(self.session.agent())
    }

    fn base_url(&self) -> String {
        self.session.panes().active_pane().state.base_url.clone()
    }

    fn model_ref(&self) -> ModelRef {
        self.session.panes().active_pane().state.model.clone()
    }

    fn set_model_ref(&mut self, reference: ModelRef) {
        let pane_id = self.session.panes().active();
        let key = pane_id.key();
        if let Some(pane) = self.session.panes_mut().pane_mut(pane_id) {
            pane.state.model = reference.clone();
        }
        self.session.live().update(key, |e| {
            e.model = reference.model().to_string();
            e.provider = Some(reference.provider().to_string());
        });
    }

    fn show_thinking(&self) -> bool {
        self.session.show_thinking()
    }

    fn set_show_thinking(&mut self, on: bool) {
        self.session.set_show_thinking(on);
    }

    fn clear_conversation(&mut self) {
        // Abort running main turn.
        if let Some(handle) = self.session.main_turn_handle_mut().take() {
            handle.abort();
        }
        self.session.live().clear_pending(MAIN_KEY);

        // Clear the agent under lock.
        let agent = self.session.agent().clone();
        if let Ok(mut a) = agent.try_lock() {
            a.clear();
        } else {
            tokio::spawn(async move { agent.lock().await.clear() });
        }

        // Clear the steering queue.
        self.session.steering().lock().unwrap().clear();

        // Clear pane state.
        let main = self.session.panes_mut().main_mut();
        main.state.id = None;
        main.state.name.clear();
        main.state.transcript.clear();
        main.state.messages.clear();
        main.state.usage = Default::default();
        main.turn = Default::default();
        main.todos.lock().unwrap().clear();
        main.pending.clear();

        self.session.detach_transcript();
        self.session.set_active_lock(None);

        // Full snapshot broadcast.
        self.session.notify_tick();
    }

    fn session_id(&self) -> Option<String> {
        self.session.panes().main().state.id.clone()
    }

    fn set_session_label(&mut self, name: String) {
        let main = self.session.panes_mut().main_mut();
        main.state.name = name;
        main.state.named_by_user = true;
    }

    fn autosave(&mut self) {
        self.session.persist();
    }

    /// Restore a resolved session — the web twin of the TUI's `resume_locked_path` +
    /// `apply_session`, in the same order and with the same guards.
    ///
    /// Every step is SYNCHRONOUS and ordered. What it replaced set the pane's state,
    /// handed the agent its messages from a detached `tokio::spawn`, and then saved
    /// immediately: if the save won the race it wrote the OLD conversation into the
    /// RESUMED session's file, and if it lost it saved a state whose agent hadn't
    /// caught up. There is no task to lose a race with now — the agent lock is taken
    /// up front, and nothing is swapped unless it is in hand.
    fn resume(&mut self, id: String, session: Session) {
        // A running turn (or a compaction) holds the agent lock, so the history swap
        // could not happen while the session id and transcript did — and the in-flight
        // turn's save would then overwrite the resumed session's file.
        if self.is_busy() {
            self.info(hrdr_app::RESUME_BUSY_MSG.to_string());
            return;
        }
        // Resuming replaces the *session's* conversation, so the view follows it: this
        // also makes `agent()`/`model_ref()` below (which resolve the ACTIVE pane)
        // refer to the conversation being restored rather than to a sub-agent's.
        self.session.panes_mut().focus(PaneId::MAIN);

        // The file lives under its own session's cwd (as in the TUI's `TuiHost::resume`);
        // fall back to ours for a file that never recorded one.
        let cwd = self.session.cwd().clone();
        let session_cwd = if session.state.cwd.is_empty() {
            cwd.display().to_string()
        } else {
            session.state.cwd.clone()
        };
        let path = hrdr_app::session_file_path(&session_cwd, &id);
        // Acquire-new-before-release-old: `open_path` takes the new session's
        // open-lock first, so a session held open elsewhere leaves ours untouched.
        match Session::open_path(&path) {
            Ok((sess, lock)) => {
                // The adoption applies the history through this lock. Without it in
                // hand nothing is swapped at all — a half-applied resume is what
                // corrupts the resumed file.
                let agent = self.session.agent().clone();
                let Ok(mut a) = agent.try_lock() else {
                    drop(lock); // keep the current session rather than release its lock
                    self.info(hrdr_app::RESUME_BUSY_MSG.to_string());
                    return;
                };
                let plan = hrdr_app::resume_plan(
                    &sess.state,
                    &cwd,
                    &self.session.panes().main().state.base_url,
                );
                self.session.set_active_lock(Some(lock)); // releases the previous one
                // The session's cwd is applied FIRST: its `.json`, its `<id>.jsonl`
                // and its sub-agent transcripts all live under that cwd's slug, and
                // `adopt_state` re-attaches the transcript writer. (The TUI applies it
                // afterwards, where the writer has already opened under the old cwd.)
                if let Some(target) = plan.new_cwd.clone() {
                    self.session.apply_cwd(&mut a, target);
                }
                self.session.adopt_state(&mut a, sess.state, Some(id));
                drop(a);

                // The conversation's IDENTITY comes back with it — provider and model
                // together, which is the only way either means anything: resuming a
                // conversation and then talking to a different provider's endpoint is
                // not the same conversation. Repointing the agent is what keeps the
                // thing doing the talking and the thing being displayed the same.
                self.restore_identity();

                // Deliberately NO save here (the TUI doesn't either): the file on disk
                // IS this state, and the next turn-end save writes it from the agent.
                for line in plan.lines {
                    self.info(line);
                }
                self.session.notify_tick();
            }
            Err(hrdr_agent::OpenError::Busy { pid, .. }) => {
                self.info(format!(
                    "session is open in another hrdr instance (pid {pid})"
                ));
            }
            Err(hrdr_agent::OpenError::Load(e)) => {
                self.info(format!("couldn't load session: {e}"));
            }
        }
    }

    fn copy_to_clipboard(&mut self, _text: &str, _label: &str) -> String {
        "clipboard isn't available over the web — select the text in your browser".into()
    }

    fn last_reply(&self) -> Option<String> {
        self.session
            .panes()
            .active_pane()
            .transcript()
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                EntryKind::Assistant(s) => Some(s.clone()),
                _ => None,
            })
    }

    fn transcript_text(&self) -> String {
        hrdr_app::transcript_to_text(self.session.panes().active_pane().transcript())
    }

    fn nth_message_text(&self, n: usize) -> Option<String> {
        self.session
            .panes()
            .active_pane()
            .transcript()
            .iter()
            .filter_map(|e| match &e.kind {
                EntryKind::User(s) => Some(s.clone()),
                EntryKind::Assistant(s) => Some(s.clone()),
                _ => None,
            })
            .nth(n.saturating_sub(1))
    }

    fn line_poster(&self) -> Box<dyn Fn(LineKind, String) + Send> {
        let tx = self.line_tx.clone();
        Box::new(move |kind, text| {
            let _ = tx.send((kind, text));
        })
    }

    fn is_busy(&self) -> bool {
        self.session.live().is_running(MAIN_KEY) || self.session.live().is_compacting(MAIN_KEY)
    }

    /// Launch a turn with `prompt`.
    ///
    /// Both paths keep the agent that RUNS the prompt and the registry key its
    /// transcript, turn clock and queue are recorded under in agreement. What this
    /// replaced always ran the MAIN agent while keying everything to the ACTIVE pane,
    /// so on a sub-pane it drove the wrong agent into the wrong transcript — and it
    /// spawned a second concurrent `run` even when a turn was already in flight.
    fn send_prompt(&mut self, prompt: String, show_as_user: bool) {
        if show_as_user {
            // Exactly the path a client `Submit` takes on the pane being viewed: the
            // registry resolves that pane's own agent, a turn already in flight is
            // steered instead of raced, and a fresh main turn reserves the session id.
            let pane = self.session.panes().active();
            self.session.submit_sync(pane, prompt);
            self.session.notify_tick();
            return;
        }

        // A hidden prompt (`/init`) is the SESSION's, like the TUI's `launch_hidden`:
        // it is never routed to a sub-agent's pane, so agent and key are both main's.
        // `dispatch` already refuses `/init` while busy; re-check, because starting a
        // second concurrent `run` on the same agent is the failure that matters.
        if self.is_busy() || self.session.main_turn_running() {
            self.info("a turn is running — try again when it finishes".to_string());
            return;
        }
        let agent = self.session.agent().clone();
        let sent = hrdr_app::prepare_outgoing_via(&agent, &prompt);
        // The busy check above means the lock is free; the task is only a fallback,
        // and it still lands before `run`'s first request (which waits on the lock).
        if let Ok(mut a) = agent.try_lock() {
            a.push_user_note(sent);
        } else {
            tokio::spawn(async move { agent.lock().await.push_user_note(sent) });
        }
        // Opener-less: the note is already in the agent's history.
        self.session.spawn_pending_main_turn();
        self.session.notify_tick();
    }

    fn set_input(&mut self, text: String) {
        let seq = self.session.next_seq_internal();
        let frame = build_set_input(seq, hrdr_protocol::InputSetMode::Replace, text);
        self.session.emit_internal(frame);
    }

    fn prepend_input(&mut self, text: String) {
        let seq = self.session.next_seq_internal();
        let frame = build_set_input(seq, hrdr_protocol::InputSetMode::Prepend, text);
        self.session.emit_internal(frame);
    }

    fn insert_input(&mut self, text: String) {
        let seq = self.session.next_seq_internal();
        let frame = build_set_input(seq, hrdr_protocol::InputSetMode::InsertAtCursor, text);
        self.session.emit_internal(frame);
    }

    fn set_tool_expansion(&mut self, _mode: ExpandMode) -> String {
        "use the expand toggle on each tool block".into()
    }

    /// Kick off a compaction pass — the web twin of the TUI's `spawn_compaction`
    /// plus its `TurnMsg::Compacted` handler.
    ///
    /// The *compacting* flag is not set here, and must not be: `Agent::compact` raises
    /// it through its live home and clears it on **every** exit (error and cancel
    /// included), so a summarization can never leave a pane spinning forever. What was
    /// missing is the handle: the tick recomputes `running` from
    /// `main_turn_handle`, so a compaction announced with `begin_turn` alone showed as
    /// running for one tick and then went idle — `is_busy()` said no, and a prompt
    /// could start a turn on top of the summarizer. It goes in the same slot a turn's
    /// handle does (as the TUI's `turn_handle` does), which also gives it the turn's
    /// end-of-work handling for free: `end_turn`, a save, and any queued steer relaunched.
    fn start_compaction(&mut self, instructions: Option<String>) {
        let agent = self.session.agent().clone();
        let live = self.session.live().clone();
        let line_tx = self.line_tx.clone();
        let notify = self.session.tick_notify().clone();
        let key = MAIN_KEY;

        live.begin_turn(key);
        let handle = tokio::spawn(async move {
            let result = hrdr_app::run_compaction(agent, instructions).await;
            // Not a `TurnDone` event: compaction is not a turn in the conversation, so
            // nothing is folded into the transcript for it — the turn clock is simply
            // stopped, exactly as the TUI's Compacted handler does.
            live.end_turn(key);
            if result.is_ok() {
                // The context shrank: drop the stale reading so the gauge refreshes on
                // the next turn instead of immediately re-triggering compaction. The
                // pane's usage is rebuilt from the registry, so that is where it lives.
                live.update(key, |e| e.usage.set_last(None));
            }
            let _ = line_tx.send((LineKind::System, hrdr_app::compaction_message(&result)));
            notify.notify_one();
        });
        *self.session.main_turn_handle_mut() = Some(handle);
    }

    // ── overrides ──────────────────────────────────────────────────────────

    fn supports_command(&self, cmd: &str) -> bool {
        !matches!(cmd, "edit" | "paste" | "copy" | "theme" | "reload")
    }

    fn effort(&self) -> Option<String> {
        self.session.panes().active_pane().effort.clone()
    }

    fn session_label(&self) -> Option<String> {
        let name = &self.session.panes().main().state.name;
        if name.is_empty() {
            None
        } else {
            Some(name.clone())
        }
    }

    fn context_usage(&self) -> Option<(u32, u32)> {
        self.session.panes().active_pane().state.usage.last()
    }

    fn context_window(&self) -> Option<u32> {
        self.session
            .panes()
            .active_pane()
            .state
            .usage
            .context_window
    }

    fn session_tokens(&self) -> (usize, usize) {
        let u = &self.session.panes().active_pane().state.usage;
        (u.tokens_in, u.tokens_out)
    }

    fn session_cost(&self) -> f64 {
        self.session.panes().active_pane().state.usage.cost_usd
    }

    fn session_cost_partial(&self) -> bool {
        self.session.panes().active_pane().state.usage.cost_partial
    }

    fn cwd_changed(&mut self, _new: &Path) {}

    fn set_effort(&mut self, label: String) {
        // Effort is the AGENT's, and it publishes the change back into its registry
        // entry — which is what the pane (and so the status bar) is rebuilt from every
        // tick. Writing `pane.effort` here instead, as this used to, changed the chrome
        // for 100ms and never reached the model: the next `panes.sync` restored the
        // agent's value, and the requests kept going out at the old effort.
        let agent = self.agent();
        let notify = self.session.tick_notify().clone();
        tokio::spawn(async move {
            agent.lock().await.set_effort(Some(label));
            notify.notify_one();
        });
    }

    fn resolve_provider(&self, name: &str) -> Option<hrdr_agent::ResolvedProvider> {
        // Needed by `restore_session_provider` (and `/model`): without it every
        // provider is "unknown" and a resumed session's endpoint can never be restored.
        self.session.cfg().resolve_provider(name)
    }
}
