//! `WebHost` — implements `hrdr_app::CommandHost` for the web server so
//! every shared slash command works over WS.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hrdr_agent::{Agent, AgentEvent, EntryKind, MAIN_KEY, ModelRef, PaneId, Session, Steer};
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

impl CommandHost for WebHost<'_> {
    fn info(&mut self, line: String) {
        let _ = self.line_tx.send((LineKind::System, line));
    }

    fn agent(&self) -> Arc<tokio::sync::Mutex<Agent>> {
        let pane_id = self.session.panes().active();
        let key = match pane_id {
            PaneId::Main => MAIN_KEY,
            PaneId::Sub(k) => k,
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
        let key = pane_id_to_key(pane_id);
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

    fn resume(&mut self, id: String, _session: Session) {
        if self.session.live().is_running(MAIN_KEY) || self.session.live().is_compacting(MAIN_KEY) {
            self.info(hrdr_app::RESUME_BUSY_MSG.to_string());
            return;
        }

        let cwd = self.session.cwd().clone();
        let path = hrdr_app::session_file_path(&cwd.display().to_string(), &id);
        match Session::open_path(&path) {
            Ok((sess, lock)) => {
                let state = sess.state.restored();
                let main = self.session.panes_mut().main_mut();
                main.state = state;
                main.state.id = Some(id);
                main.state.cwd = cwd.display().to_string();

                let msgs = main.state.messages.clone();
                let agent = self.session.agent().clone();
                let live = self.session.live().clone();
                tokio::spawn(async move {
                    let mut a = agent.lock().await;
                    a.set_messages(msgs);
                    a.attach_live(live, MAIN_KEY);
                });

                self.session.set_active_lock(Some(lock));
                self.session.detach_transcript();
                self.session.persist();
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

    fn send_prompt(&mut self, prompt: String, show_as_user: bool) {
        let agent = self.session.agent().clone();
        let live = self.session.live().clone();
        let steering = self.session.steering().clone();
        let notify = self.session.tick_notify().clone();
        let key = pane_id_to_key(self.session.panes().active());

        let sent = hrdr_app::prepare_outgoing_via(&agent, &prompt);

        if show_as_user {
            let steer = Steer::new(sent, prompt);
            live.enqueue(key, steer);
        } else {
            if let Ok(mut a) = agent.try_lock() {
                a.push_user_note(sent);
            } else {
                let a2 = agent.clone();
                let s = sent.clone();
                tokio::spawn(async move { a2.lock().await.push_user_note(s) });
            }
        }

        live.begin_turn(key);
        tokio::spawn(async move {
            let _guard = hrdr_agent::RunGuard::new(live.clone(), key);
            let mut a = agent.lock().await;
            if let Err(e) = a
                .run(steering, |ev| {
                    live.record(key, &ev);
                    notify.notify_one();
                })
                .await
            {
                live.record(key, &AgentEvent::Notice(format!("[error] {e:#}")));
                live.record(key, &AgentEvent::TurnDone);
            }
            notify.notify_one();
        });
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

    fn start_compaction(&mut self, instructions: Option<String>) {
        let agent = self.session.agent().clone();
        let live = self.session.live().clone();
        let line_tx = self.line_tx.clone();
        let notify = self.session.tick_notify().clone();
        let key = MAIN_KEY;

        live.begin_turn(key);
        tokio::spawn(async move {
            let result = hrdr_app::run_compaction(agent, instructions).await;
            live.record(key, &AgentEvent::TurnDone);
            match result {
                Ok(_) => {
                    let _ = line_tx.send((LineKind::System, "compaction finished".into()));
                }
                Err(e) => {
                    let _ = line_tx.send((LineKind::System, format!("compaction failed: {e}")));
                }
            }
            notify.notify_one();
        });
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
        let active = self.session.panes().active();
        if let Some(pane) = self.session.panes_mut().pane_mut(active) {
            pane.effort = Some(label);
        }
    }
}

fn pane_id_to_key(id: PaneId) -> u64 {
    match id {
        PaneId::Main => MAIN_KEY,
        PaneId::Sub(k) => k,
    }
}
