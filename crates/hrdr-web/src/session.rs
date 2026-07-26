//! `WebSession` — a network-free session host that owns the agent, folds
//! panes, and emits `ServerFrame` deltas on a broadcast channel.
//!
//! This is the headless equivalent of the TUI's `App`: it owns the agent, the
//! live registry, the pane set, and the event loop that reconciles them into a
//! sequence-numbered stream of wire frames. No HTTP, no WS — just the session
//! engine.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hrdr_agent::{Agent, AgentConfig, Entry, LiveSubagents, MAIN_KEY, PaneId, PaneSet, Steer};
use hrdr_app::{self, StatusInputs};
use hrdr_protocol::{PaneTranscript, ServerFrame, WirePane, WirePaneId};
use tokio::sync::{Mutex, Notify, broadcast};

use crate::convert::{
    build_entries, build_notice, build_panes, build_snapshot, build_status, wire_entry_view,
    wire_pane, wire_pane_id, wire_status,
};

/// How many frames the replay buffer holds (for resume-after-reconnect).
const REPLAY_CAP: usize = 1024;

/// Shared, cloneable handle to a `WebSession`.
#[derive(Clone)]
pub struct SharedSession(Arc<Mutex<WebSession>>);

impl SharedSession {
    /// Construct a `WebSession`, spawn the tick task, and return the shared handle.
    pub async fn start(config: AgentConfig) -> anyhow::Result<Self> {
        let session = WebSession::new(config).await?;
        let shared = Self(Arc::new(Mutex::new(session)));

        let tick_self = shared.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = async {
                        let s = tick_self.0.lock().await;
                        let n = s.tick_notify.clone();
                        drop(s);
                        n.notified().await;
                    } => {}
                }
                let mut s = tick_self.0.lock().await;
                s.tick();

                // After tick, check if main turn finished and needs persistence.
                let turn_done = s.main_turn_handle.as_ref().is_some_and(|h| h.is_finished());
                if turn_done {
                    let handle = s.main_turn_handle.take().unwrap();
                    // join to avoid leaking
                    drop(handle);
                    s.persist();
                    // Check for pending steers to relaunch.
                    if !s.live.pending(MAIN_KEY).is_empty() {
                        s.spawn_pending_main_turn();
                    }
                }
            }
        });

        Ok(shared)
    }

    /// Lock and run a closure synchronously.
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, WebSession> {
        self.0.lock().await
    }
}

/// The session engine: owns the agent, folds panes, broadcasts deltas.
pub struct WebSession {
    // ── agent ──
    agent: Arc<tokio::sync::Mutex<Agent>>,
    steering: hrdr_agent::SteeringQueue,
    live: LiveSubagents,
    panes: PaneSet,

    // ── sub-agent transcript dir cell (set after first save assigns an id) ──
    subagent_dir: Arc<std::sync::Mutex<Option<PathBuf>>>,

    // ── broadcast ──
    broadcast: broadcast::Sender<ServerFrame>,
    seq: u64,
    replay: VecDeque<ServerFrame>,

    // ── content-hash tracking (per-pane: last sent hashes) ──
    sent: HashMap<PaneId, Vec<u64>>,

    // ── last-serialized panes + status (for change detection) ──
    last_panes_json: String,
    last_status_json: String,

    // ── misc ──
    show_thinking: bool,
    cwd: PathBuf,
    /// Handle of an in-flight main-pane turn (spawned by the server).
    main_turn_handle: Option<tokio::task::JoinHandle<()>>,
    /// The session's active open-lock (held for lifetime, dropped on /new).
    active_lock: Option<hrdr_app::SessionLock>,
    /// Branch name, cached for 5s.
    branch_cache: Option<(String, Instant)>,

    tick_notify: Arc<Notify>,
}

impl WebSession {
    /// Construct the session engine — no network, no event loop, just the
    /// agent and the fold machinery, mirroring the TUI's `publish_main_agent`.
    pub async fn new(mut config: AgentConfig) -> anyhow::Result<Self> {
        let (broadcast, _) = broadcast::channel(256);

        // Sub-agent transcript dir cell — created before the agent.
        let subagent_dir: Arc<std::sync::Mutex<Option<PathBuf>>> = Default::default();
        config.subagent_transcript_dir = Some(subagent_dir.clone());

        let cwd = config.cwd.clone();

        let agent = Arc::new(tokio::sync::Mutex::new(Agent::new(config)?));
        let steering = hrdr_agent::steering_queue();

        let (model_name, provider, base_url, usage) = {
            let a = agent.lock().await;
            (
                a.model_name(),
                Some(a.provider_name().to_string()),
                a.endpoint_base_url(),
                hrdr_agent::AgentUsage::default(),
            )
        };

        let live = LiveSubagents::new();
        live.register_main(
            agent.clone(),
            steering.clone(),
            model_name,
            provider,
            base_url,
            usage,
        );

        // attach_live
        {
            let mut a = agent.lock().await;
            a.attach_live(live.clone(), MAIN_KEY);
        }

        // connect MCP servers, push notices into the main pane transcript.
        let notices = agent.lock().await.connect_mcp().await;
        let mut panes = PaneSet::new();
        {
            let main_pane = panes.main_mut();
            for n in notices {
                main_pane.transcript_mut().push(Entry::system(n));
            }
        }

        Ok(Self {
            agent,
            steering,
            live,
            panes,
            subagent_dir,
            broadcast,
            seq: 0,
            replay: VecDeque::with_capacity(REPLAY_CAP),
            sent: HashMap::new(),
            last_panes_json: String::new(),
            last_status_json: String::new(),
            show_thinking: true,
            cwd,
            main_turn_handle: None,
            active_lock: None,
            branch_cache: None,
            tick_notify: Arc::new(Notify::new()),
        })
    }

    /// Subscribe to the broadcast — returns the current snapshot and a
    /// receiver for all future frames.
    pub fn subscribe(&self) -> (ServerFrame, broadcast::Receiver<ServerFrame>) {
        let rx = self.broadcast.subscribe();
        let snap = self.build_snapshot();
        (snap, rx)
    }

    /// Replay buffered frames after `seq`.
    pub fn replay_after(&self, seq: u64) -> Option<Vec<ServerFrame>> {
        // If the exact seq is in the buffer, return everything after it.
        if self.replay.iter().any(|f| f.seq == seq) {
            let frames: Vec<ServerFrame> = self
                .replay
                .iter()
                .filter(|f| f.seq > seq)
                .cloned()
                .collect();
            return Some(frames);
        }
        // seq not in buffer. If buffer is empty, return Some(empty).
        // Otherwise the seq is before the buffer start → gap → None.
        if self.replay.is_empty() {
            Some(vec![])
        } else {
            None
        }
    }

    // ── snapshot ───────────────────────────────────────────────────────────

    pub fn build_snapshot(&self) -> ServerFrame {
        let seq = self.seq + 1; // don't advance seq for snapshot (caller manages)

        let panes_vec: Vec<WirePane> = self.wire_panes();
        let active = wire_pane_id(self.panes.active());
        let status = self.build_wire_status();
        let transcripts = self.build_transcripts();
        let session_name = self.panes.main().state.name.clone();

        build_snapshot(
            seq,
            self.panes.main().state.id.clone(),
            session_name,
            self.cwd.display().to_string(),
            &panes_vec,
            active,
            status,
            transcripts,
            self.show_thinking,
        )
    }

    // ── tick ───────────────────────────────────────────────────────────────

    /// The main tick: fold events, diff transcripts, rebuild panes+status.
    pub fn tick(&mut self) {
        // 1. Tell the registry whether the main agent is running.
        let main_running = self
            .main_turn_handle
            .as_ref()
            .is_some_and(|h| !h.is_finished());
        self.live.update(MAIN_KEY, |e| e.running = main_running);
        self.panes.sync(&self.live);

        // 2. Diff each pane's transcript against last-sent hashes.
        // Collect changes then emit after releasing borrows.
        let pane_ids: Vec<PaneId> = self.pane_ids();
        let mut changes: Vec<(PaneId, usize, Vec<_>)> = Vec::new();

        for &pane_id in &pane_ids {
            // Clone the transcript, then drop the borrow before touching sent.
            let transcript = {
                // Access panes via a helper that borrows self immutably.

                self.transcript_clone(pane_id)
            };
            let sent_hashes = self.sent.entry(pane_id).or_default();

            let from = (0..sent_hashes.len().min(transcript.len()))
                .find(|&i| sent_hashes[i] != transcript[i].content_hash)
                .unwrap_or_else(|| {
                    if transcript.len() != sent_hashes.len() {
                        sent_hashes.len().min(transcript.len())
                    } else {
                        transcript.len()
                    }
                });

            if from < transcript.len()
                || (from == 0 && transcript.is_empty() && !sent_hashes.is_empty())
            {
                let entries: Vec<_> = transcript[from..].iter().map(wire_entry_view).collect();
                changes.push((pane_id, from, entries));
            }
        }

        // Emit entries changes.
        for (pane_id, from, entries) in &changes {
            let wire_id = wire_pane_id(*pane_id);
            let seq = self.next_seq();
            let frame = build_entries(seq, wire_id, *from, entries.clone());
            self.emit_raw(frame);

            // Update sent hashes.
            // Read transcript first, then update sent.
            let transcript = self.transcript_clone(*pane_id);
            let sent_hashes = self.sent.entry(*pane_id).or_default();
            sent_hashes.truncate(*from);
            for e in &transcript[*from..] {
                sent_hashes.push(e.content_hash);
            }
        }

        // 3. Rebuild panes + status; emit if changed.
        let panes_vec: Vec<WirePane> = self.wire_panes();
        let panes_json = serde_json::to_string(&panes_vec).unwrap_or_default();
        if panes_json != self.last_panes_json {
            let active = wire_pane_id(self.panes.active());
            let seq = self.next_seq();
            let frame = build_panes(seq, panes_vec.clone(), active);
            self.emit_raw(frame);
            self.last_panes_json = panes_json;
        }

        let status = self.build_wire_status();
        let status_json = serde_json::to_string(&status).unwrap_or_default();
        if status_json != self.last_status_json {
            let seq = self.next_seq();
            let frame = build_status(seq, status);
            self.emit_raw(frame);
            self.last_status_json = status_json;
        }

        // 4. Prune sent-map entries for panes that no longer exist.
        let existing: Vec<PaneId> = self.pane_ids();
        self.sent.retain(|id, _| existing.contains(id));
    }

    // ── submit ─────────────────────────────────────────────────────────────

    /// Submit a message to a pane.
    pub async fn submit(&mut self, wire_pane: WirePaneId, text: String) {
        let pane_id = crate::convert::core_pane_id(wire_pane);
        let key = pane_id_to_key(pane_id);

        let sent = hrdr_app::prepare_outgoing_via(&self.agent, &text);
        let steer = Steer::new(sent, text);

        if key == MAIN_KEY {
            let running = self
                .main_turn_handle
                .as_ref()
                .is_some_and(|h| !h.is_finished());
            if running {
                self.live.send_prompt(key, steer, |_ev| {});
            } else {
                self.reserve_session_id(&steer.sent);
                self.live.enqueue(MAIN_KEY, steer);
                self.spawn_pending_main_turn();
            }
        } else {
            let notify = self.tick_notify.clone();
            let delivered = self.live.send_prompt(key, steer, move |_ev| {
                notify.notify_one();
            });
            if delivered.is_none() {
                let seq = self.next_seq();
                let frame = build_notice(seq, "that agent has finished and been released".into());
                self.emit_raw(frame);
            }
        }

        self.tick_notify.notify_one();
        self.tick();
    }

    // ── cancel ─────────────────────────────────────────────────────────────

    /// Cancel a running turn.
    pub fn cancel(&mut self, wire_pane: WirePaneId) {
        let pane_id = crate::convert::core_pane_id(wire_pane);
        let key = pane_id_to_key(pane_id);

        if key == MAIN_KEY
            && let Some(handle) = self.main_turn_handle.take()
        {
            handle.abort();
        }
        self.live.clear_pending(key);
        self.live.end_turn(key);

        let seq = self.next_seq();
        let frame = build_notice(seq, "turn cancelled".into());
        self.emit_raw(frame);

        self.persist();
        self.tick();
    }

    // ── switch pane ────────────────────────────────────────────────────────

    pub fn switch_pane(&mut self, wire_pane: WirePaneId) {
        let pane_id = crate::convert::core_pane_id(wire_pane);
        self.panes.focus(pane_id);
        self.tick();
    }

    // ── persistence ────────────────────────────────────────────────────────

    /// Persist the session state (mirrors TUI `autosave`).
    pub fn persist(&mut self) {
        let (msgs, cwd_str) = match self.agent.try_lock() {
            Ok(a) => (a.messages_owned(), a.cwd().display().to_string()),
            Err(_) => return,
        };
        let todos = self
            .panes
            .main()
            .todos
            .lock()
            .map(|t| t.clone())
            .unwrap_or_default();
        self.panes.main_mut().state.sync_from(msgs, todos, cwd_str);

        let state = self.panes.main().state.clone();
        let saved = hrdr_app::save_session(&state);

        if let Ok(Some(o)) = saved {
            self.panes.main_mut().state.id = Some(o.id.clone());
            if let Some(lock) = o.open_lock {
                self.active_lock = Some(lock);
            }
            self.refresh_subagent_dir();
        }
    }

    /// Persist mid-turn: the agent lock is held, so just save from pane state.
    pub fn persist_mid_turn(&mut self) {
        let state = self.panes.main().state.clone();
        let saved = hrdr_app::save_session(&state);
        if let Ok(Some(o)) = saved {
            self.panes.main_mut().state.id = Some(o.id.clone());
            if let Some(lock) = o.open_lock {
                self.active_lock = Some(lock);
            }
            self.refresh_subagent_dir();
        }
    }

    // ── internals ──────────────────────────────────────────────────────────

    fn reserve_session_id(&mut self, sent: &str) {
        if self.panes.main().state.id.is_some() {
            return;
        }
        self.panes
            .main_mut()
            .state
            .messages
            .push(hrdr_agent::Message::user(sent));
        self.persist();
    }

    fn refresh_subagent_dir(&self) {
        if let Some(id) = &self.panes.main().state.id {
            let cwd_str = self.cwd.display().to_string();
            let dir = hrdr_app::subagent_transcript_dir(&cwd_str, id);
            if let Ok(mut cell) = self.subagent_dir.lock() {
                *cell = Some(dir);
            }
            let jsonl = hrdr_app::session_transcript_path(&cwd_str, id);
            self.live.attach_transcript(MAIN_KEY, &jsonl);
        }
    }

    #[allow(dead_code)]
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Public version — used by the server when it holds the lock.
    pub fn next_seq_internal(&mut self) -> u64 {
        self.next_seq()
    }

    fn emit_raw(&mut self, frame: ServerFrame) {
        self.replay.push_back(frame.clone());
        if self.replay.len() > REPLAY_CAP {
            self.replay.pop_front();
        }
        let _ = self.broadcast.send(frame);
    }

    /// Public version — used by the server when it holds the lock.
    pub fn emit_internal(&mut self, frame: ServerFrame) {
        self.emit_raw(frame);
    }

    fn transcript_clone(&self, id: PaneId) -> Vec<Entry> {
        self.pane_transcript(id).clone()
    }

    fn pane_ids(&self) -> Vec<PaneId> {
        let mut ids = vec![self.panes.main().id];
        ids.extend(self.panes.subs().iter().map(|p| p.id));
        ids
    }

    fn pane_transcript(&self, id: PaneId) -> &Vec<Entry> {
        match id {
            PaneId::Main => self.panes.main().transcript(),
            PaneId::Sub(k) => {
                if let Some(p) = self.panes.subs().iter().find(|p| p.id == PaneId::Sub(k)) {
                    p.transcript()
                } else {
                    self.panes.main().transcript()
                }
            }
        }
    }

    fn wire_panes(&self) -> Vec<WirePane> {
        let main = self.panes.main();
        let main_running = self
            .main_turn_handle
            .as_ref()
            .is_some_and(|h| !h.is_finished());
        let mut out = vec![wire_pane(main, main_running)];

        for sub in self.panes.subs() {
            let key = match sub.id {
                PaneId::Main => unreachable!(),
                PaneId::Sub(k) => k,
            };
            let running = self.live.is_running(key);
            out.push(wire_pane(sub, running));
        }
        out
    }

    fn build_transcripts(&self) -> Vec<PaneTranscript> {
        self.pane_ids()
            .into_iter()
            .map(|id| {
                let entries: Vec<_> = self
                    .pane_transcript(id)
                    .iter()
                    .map(wire_entry_view)
                    .collect();
                PaneTranscript {
                    pane: wire_pane_id(id),
                    entries,
                }
            })
            .collect()
    }

    fn build_wire_status(&self) -> hrdr_protocol::WireStatus {
        let pane = self.panes.active_pane();
        let branch = self.cached_branch();

        let inputs = StatusInputs {
            dir: &hrdr_app::display_dir(&self.cwd),
            branch: branch.as_deref(),
            tokens_in: pane.state.usage.tokens_in,
            tokens_out: pane.state.usage.tokens_out,
            ctx_used: pane.state.usage.ctx_used(),
            context_window: pane.state.usage.context_window,
            auto_compact_enabled: pane.auto_compact,
            compaction_reserved: pane.compaction_reserved,
            provider: Some(pane.provider()),
            model: pane.model(),
            session: if pane.state.name.is_empty() {
                None
            } else {
                Some(pane.state.name.as_str())
            },
            effort: pane.effort.as_deref(),
            ttft: pane.turn.ttft(),
            nerd_icons: false,
        };

        wire_status(&inputs)
    }

    fn cached_branch(&self) -> Option<String> {
        if let Some((b, t)) = &self.branch_cache
            && t.elapsed() < Duration::from_secs(5)
        {
            return Some(b.clone());
        }
        hrdr_app::git_branch(&self.cwd)
    }

    fn spawn_pending_main_turn(&mut self) {
        self.live.begin_turn(MAIN_KEY);
        let agent = self.agent.clone();
        let steering = self.steering.clone();
        let live = self.live.clone();
        let tick_notify = self.tick_notify.clone();

        let handle = tokio::spawn(async move {
            let _guard = hrdr_agent::RunGuard::new(live.clone(), MAIN_KEY);

            let result = {
                let mut a = agent.lock().await;
                a.run(steering, {
                    let live = live.clone();
                    let notify = tick_notify.clone();
                    move |ev| {
                        live.record(MAIN_KEY, &ev);
                        notify.notify_one();
                    }
                })
                .await
            };

            match result {
                Ok(()) => {
                    live.record(MAIN_KEY, &hrdr_agent::AgentEvent::TurnDone);
                }
                Err(e) => {
                    live.record(
                        MAIN_KEY,
                        &hrdr_agent::AgentEvent::Notice(format!("[error] {e:#}")),
                    );
                    live.record(MAIN_KEY, &hrdr_agent::AgentEvent::TurnDone);
                }
            }
            tick_notify.notify_one();
        });

        self.main_turn_handle = Some(handle);
    }

    // ── public accessors ──────────────────────────────────────────────────

    pub fn notify_tick(&self) {
        self.tick_notify.notify_one();
    }

    pub fn live(&self) -> &LiveSubagents {
        &self.live
    }

    pub fn panes(&self) -> &PaneSet {
        &self.panes
    }

    pub fn panes_mut(&mut self) -> &mut PaneSet {
        &mut self.panes
    }

    pub fn agent(&self) -> &Arc<tokio::sync::Mutex<Agent>> {
        &self.agent
    }

    pub fn steering(&self) -> &hrdr_agent::SteeringQueue {
        &self.steering
    }

    pub fn show_thinking(&self) -> bool {
        self.show_thinking
    }

    pub fn set_show_thinking(&mut self, v: bool) {
        self.show_thinking = v;
    }

    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    pub fn main_turn_handle(&self) -> &Option<tokio::task::JoinHandle<()>> {
        &self.main_turn_handle
    }

    pub fn main_turn_handle_mut(&mut self) -> &mut Option<tokio::task::JoinHandle<()>> {
        &mut self.main_turn_handle
    }

    pub fn tick_notify(&self) -> &Arc<Notify> {
        &self.tick_notify
    }

    pub fn active_lock(&self) -> Option<&hrdr_app::SessionLock> {
        self.active_lock.as_ref()
    }

    pub fn set_active_lock(&mut self, lock: Option<hrdr_app::SessionLock>) {
        self.active_lock = lock;
    }

    pub fn detach_transcript(&mut self) {
        self.live.detach_transcript(MAIN_KEY);
        if let Ok(mut cell) = self.subagent_dir.lock() {
            *cell = None;
        }
    }
}

fn pane_id_to_key(id: PaneId) -> u64 {
    match id {
        PaneId::Main => MAIN_KEY,
        PaneId::Sub(k) => k,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrdr_agent::{AgentEvent, LiveSubagents, MAIN_KEY};
    use hrdr_protocol::{WireEntryKind, WireToolBody};

    /// Build a bare WebSession for tests. We create a real (minimal) Agent
    /// so the Mutex holds a valid value — it is never actually run.
    fn test_session() -> (WebSession, broadcast::Receiver<ServerFrame>) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (broadcast, rx) = broadcast::channel(256);
        let panes = PaneSet::new();

        let agent = rt.block_on(async {
            Arc::new(tokio::sync::Mutex::new(
                Agent::new(AgentConfig {
                    cwd: PathBuf::from("/tmp"),
                    ..Default::default()
                })
                .expect("minimal test agent"),
            ))
        });

        let live = LiveSubagents::new();
        // Register main so sync() can find it.
        {
            let a = agent.blocking_lock();
            live.register_main(
                agent.clone(),
                hrdr_agent::steering_queue(),
                a.model_name(),
                Some(a.provider_name().to_string()),
                a.endpoint_base_url(),
                hrdr_agent::AgentUsage::default(),
            );
        }

        let session = WebSession {
            agent,
            steering: hrdr_agent::steering_queue(),
            live: live.clone(),
            panes,
            subagent_dir: Default::default(),
            broadcast,
            seq: 0,
            replay: VecDeque::with_capacity(REPLAY_CAP),
            sent: HashMap::new(),
            last_panes_json: String::new(),
            last_status_json: String::new(),
            show_thinking: true,
            cwd: PathBuf::from("/tmp"),
            main_turn_handle: None,
            active_lock: None,
            branch_cache: None,
            tick_notify: Arc::new(Notify::new()),
        };
        (session, rx)
    }

    #[test]
    fn tick_broadcasts_entry_deltas() {
        let mut session;
        let mut rx;
        (session, rx) = test_session();
        let live = session.live.clone();

        live.record(MAIN_KEY, &AgentEvent::Text("he".into()));
        session.tick();

        let mut frames = Vec::new();
        while let Ok(f) = rx.try_recv() {
            frames.push(f);
        }

        let entries_frames: Vec<_> = frames
            .iter()
            .filter(|f| matches!(f.msg, hrdr_protocol::ServerMsg::Entries { .. }))
            .collect();
        assert!(
            !entries_frames.is_empty(),
            "expected at least one Entries frame"
        );

        if let hrdr_protocol::ServerMsg::Entries {
            from, ref entries, ..
        } = entries_frames[0].msg
        {
            assert_eq!(from, 0);
            assert!(!entries.is_empty());
            assert!(matches!(entries[0].entry.kind, WireEntryKind::Assistant(_)));
        }

        // More text — same entry mutates.
        live.record(MAIN_KEY, &AgentEvent::Text("llo".into()));
        session.tick();

        let mut frames2 = Vec::new();
        while let Ok(f) = rx.try_recv() {
            frames2.push(f);
        }
        let entries2: Vec<_> = frames2
            .iter()
            .filter(|f| matches!(f.msg, hrdr_protocol::ServerMsg::Entries { .. }))
            .collect();
        if !entries2.is_empty()
            && let hrdr_protocol::ServerMsg::Entries {
                from, ref entries, ..
            } = entries2[0].msg
        {
            assert_eq!(from, 0);
            if let WireEntryKind::Assistant(ref s) = entries[0].entry.kind {
                assert_eq!(s, "hello");
            }
        }
    }

    #[test]
    fn tick_is_quiet_when_nothing_changed() {
        let (mut session, mut rx) = test_session();
        let live = session.live.clone();

        live.record(MAIN_KEY, &AgentEvent::Text("a".into()));
        session.tick();
        drain(&mut rx);

        session.tick();
        let count2 = drain_count(&mut rx);
        assert_eq!(count2, 0, "second tick should produce no frames");
    }

    #[test]
    fn tool_entries_carry_display_model() {
        let (mut session, mut rx) = test_session();
        let live = session.live.clone();

        live.record(
            MAIN_KEY,
            &AgentEvent::ToolStart {
                id: "c1".into(),
                name: "shell".into(),
                args: r#"{"command":"ls"}"#.into(),
            },
        );
        live.record(
            MAIN_KEY,
            &AgentEvent::ToolEnd {
                id: "c1".into(),
                name: "shell".into(),
                result: "src\n".into(),
                ok: true,
            },
        );
        session.tick();

        let frames: Vec<_> = drain_vec(&mut rx);
        let entries_frames: Vec<_> = frames
            .iter()
            .filter(|f| matches!(f.msg, hrdr_protocol::ServerMsg::Entries { .. }))
            .collect();
        assert!(!entries_frames.is_empty());

        if let hrdr_protocol::ServerMsg::Entries { ref entries, .. } = entries_frames[0].msg {
            let tool_entry = entries
                .iter()
                .find(|e| matches!(e.entry.kind, WireEntryKind::Tool { .. }));
            assert!(tool_entry.is_some(), "expected a tool entry");
            let tool = tool_entry
                .unwrap()
                .tool
                .as_ref()
                .expect("tool should have display model");
            assert!(
                matches!(tool.body, WireToolBody::Shell { .. }),
                "should be Shell variant"
            );
        }
    }

    #[test]
    fn replay_after_returns_gap_or_none() {
        let (mut session, _rx) = test_session();

        for i in 0..(REPLAY_CAP + 10) {
            let seq = session.next_seq();
            let frame = build_notice(seq, format!("msg {i}"));
            session.replay.push_back(frame);
            if session.replay.len() > REPLAY_CAP {
                session.replay.pop_front();
            }
        }

        let first_seq = session.replay.front().unwrap().seq;
        assert!(
            session.replay_after(0).is_none(),
            "seq before buffer = None"
        );
        assert!(
            session.replay_after(first_seq).is_some(),
            "seq in buffer = Some"
        );

        let last_seq = session.replay.back().unwrap().seq;
        let result = session.replay_after(last_seq);
        assert!(result.is_some(), "last seq = Some(empty)");
        assert_eq!(result.unwrap().len(), 0);
    }

    #[test]
    fn wire_entry_matches_core_entry_json() {
        use chrono::Local;
        use hrdr_agent::{Entry, EntryKind};

        let now = Local::now();
        let entries: Vec<Entry> = vec![
            Entry::at(EntryKind::Header, now),
            Entry::at(EntryKind::User("hello".into()), now),
            Entry::at(EntryKind::Assistant("world".into()), now),
            Entry::at(
                EntryKind::Reasoning {
                    text: "thinking...".into(),
                    took_ms: Some(1200),
                },
                now,
            ),
            Entry::at(
                EntryKind::Tool {
                    id: "c1".into(),
                    name: "shell".into(),
                    args: r#"{"command":"ls"}"#.into(),
                    result: "src\n".into(),
                    ok: true,
                    done: true,
                    expanded: false,
                },
                now,
            ),
            Entry::at(EntryKind::System("system msg".into()), now),
            Entry::at(EntryKind::Notice("notice".into()), now),
            Entry::at(EntryKind::Stats("stats line".into()), now),
            Entry::at(EntryKind::Diff("+added\n-removed".into()), now),
        ];

        for entry in &entries {
            let core_json = serde_json::to_value(entry).unwrap();
            let wire_entry = crate::convert::wire_entry(entry);
            let wire_json = serde_json::to_value(&wire_entry).unwrap();
            assert_eq!(core_json, wire_json, "JSON mismatch for {:?}", entry.kind);
        }
    }

    fn drain(rx: &mut broadcast::Receiver<ServerFrame>) {
        while rx.try_recv().is_ok() {}
    }

    fn drain_count(rx: &mut broadcast::Receiver<ServerFrame>) -> usize {
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        n
    }

    fn drain_vec(rx: &mut broadcast::Receiver<ServerFrame>) -> Vec<ServerFrame> {
        let mut out = Vec::new();
        while let Ok(f) = rx.try_recv() {
            out.push(f);
        }
        out
    }
}
