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

use hrdr_agent::{Agent, AgentConfig, AgentRegistry, Entry, MAIN_KEY, PaneId, PaneSet, Steer};
use hrdr_app::{self, StatusInputs};
use hrdr_protocol::{PaneTranscript, ServerFrame, WirePane, WirePaneId};
use tokio::sync::{Mutex, Notify, broadcast};

use crate::convert::{
    build_entries, build_notice, build_panes, build_snapshot, build_status, wire_entry_view,
    wire_pane, wire_pane_id, wire_status,
};

/// How many frames the replay buffer holds (for resume-after-reconnect).
const REPLAY_CAP: usize = 1024;

/// How long a `git branch` lookup is reused before it is taken again.
///
/// The status bar is rebuilt on every tick (up to 10×/s while a turn streams) and
/// the branch comes from a subprocess, so the answer — *including a `None` for a
/// directory that is not a repository* — is cached for this long.
const BRANCH_CACHE_TTL: Duration = Duration::from_secs(5);

/// Minimum gap between mid-turn session saves (see
/// [`WebSession::persist_mid_turn`]). A long turn is persisted as it goes, but not
/// on every one of its rounds.
const MIDTURN_SAVE_INTERVAL: Duration = Duration::from_secs(5);

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
                } else if s.main_turn_running() {
                    // Mid-turn durability: a crash during a long turn must not lose
                    // the rounds the agent has already committed (the turn-end
                    // `persist` above is otherwise the only save).
                    s.maybe_persist_mid_turn();
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
    live: AgentRegistry,
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
    /// The config this session was built from — kept so a `/resume` can resolve the
    /// provider a saved conversation ran on (`AgentConfig::resolve_provider`).
    cfg: AgentConfig,
    /// Handle of an in-flight main-pane turn (spawned by the server). A compaction
    /// pass lives in this slot too, exactly as it does in the TUI's `turn_handle`.
    main_turn_handle: Option<tokio::task::JoinHandle<()>>,
    /// The session's active open-lock (held for lifetime, dropped on /new).
    active_lock: Option<hrdr_app::SessionLock>,
    /// Branch name (or the absence of one), cached for [`BRANCH_CACHE_TTL`].
    branch_cache: Option<(Option<String>, Instant)>,
    /// The newest history snapshot the running main turn has committed
    /// ([`hrdr_agent::AgentEvent::History`]), for [`WebSession::persist_mid_turn`].
    /// Written from the turn task's event callback, taken by the save.
    mid_turn_history: Arc<std::sync::Mutex<Option<Vec<hrdr_agent::Message>>>>,
    /// When the last mid-turn save ran, so they are rate-limited to
    /// [`MIDTURN_SAVE_INTERVAL`].
    last_midturn_save: Option<Instant>,

    tick_notify: Arc<Notify>,
}

impl WebSession {
    /// Construct the session engine — no network, no event loop, just the
    /// agent and the fold machinery, mirroring the TUI's `publish_main_agent`.
    pub async fn new(mut config: AgentConfig) -> anyhow::Result<Self> {
        let (broadcast, _) = broadcast::channel(256);

        // Sub-agent transcript dir cell — created before the agent.
        let subagent_dir: Arc<std::sync::Mutex<Option<PathBuf>>> = Default::default();
        config.child_transcript_dir = Some(subagent_dir.clone());

        let cwd = config.cwd.clone();
        // Kept for provider resolution on `/resume` (the identity a saved
        // conversation ran on has to be resolvable to an endpoint here).
        let cfg = config.clone();

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

        let live = AgentRegistry::new();
        live.register_session(
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
            cfg,
            main_turn_handle: None,
            active_lock: None,
            branch_cache: None,
            mid_turn_history: Default::default(),
            last_midturn_save: None,
            tick_notify: Arc::new(Notify::new()),
        })
    }

    /// Subscribe to the broadcast — returns the current snapshot and a
    /// receiver for all future frames.
    ///
    /// Takes `&mut self` because the snapshot consumes a sequence number like
    /// any other frame (see `build_snapshot`).
    pub fn subscribe(&mut self) -> (ServerFrame, broadcast::Receiver<ServerFrame>) {
        let rx = self.broadcast.subscribe();
        let snap = self.build_snapshot();
        (snap, rx)
    }

    /// Replay buffered frames after `seq`, or `None` when the cursor is behind
    /// the buffer (a gap the client can only recover from with a snapshot).
    ///
    /// The cursor need not name a frame that is still in — or ever was in — the
    /// buffer: seqs are globally monotonic across broadcast and per-connection
    /// frames, so a client's cursor may be the seq of a snapshot that was never
    /// buffered (snapshots are per-connection and never pushed to `replay`).
    /// The rules are therefore stated in terms of ordering, not membership:
    ///
    /// 1. cursor at or past the last emitted seq → already current, no frames.
    /// 2. otherwise, if the buffer holds newer frames and starts no later than
    ///    `seq + 1` (i.e. nothing between the cursor and the buffer was
    ///    dropped) → every buffered frame after the cursor.
    /// 3. otherwise → gap.
    pub fn replay_after(&self, seq: u64) -> Option<Vec<ServerFrame>> {
        // 1. Client is current (this also covers an empty buffer at seq 0).
        if seq >= self.seq {
            return Some(vec![]);
        }
        // 2. Contiguous tail available?
        let oldest = self.replay.front().map(|f| f.seq);
        if let Some(oldest) = oldest
            && oldest <= seq + 1
            && self.replay.back().is_some_and(|f| f.seq > seq)
        {
            let frames: Vec<ServerFrame> = self
                .replay
                .iter()
                .filter(|f| f.seq > seq)
                .cloned()
                .collect();
            return Some(frames);
        }
        // 3. Gap — the caller must send a fresh snapshot.
        None
    }

    // ── snapshot ───────────────────────────────────────────────────────────

    /// Build a full-state snapshot frame.
    ///
    /// The snapshot is a frame like any other, so it takes its own sequence
    /// number: reusing `seq + 1` without advancing would collide with the next
    /// broadcast frame. It is *not* pushed to the replay buffer and *not*
    /// broadcast — snapshots are per-connection. Seqs stay globally monotonic
    /// across broadcast and per-connection frames, so the `last_seq` a client
    /// learns from a snapshot is always a valid `Resume` cursor.
    pub fn build_snapshot(&mut self) -> ServerFrame {
        let seq = self.next_seq();

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
        let main_running = self.main_turn_running();
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

            // Something to send when the tail is new — or when the transcript got
            // SHORTER than what this client was last told (a `/clear`, or a
            // `/resume` into an earlier conversation): the frame then carries no
            // entries and tells the client to truncate at `from`.
            if from < transcript.len() || sent_hashes.len() > transcript.len() {
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

    /// Submit a message to a pane (the `async` face the WS handler calls).
    pub async fn submit(&mut self, wire_pane: WirePaneId, text: String) {
        self.submit_sync(crate::convert::core_pane_id(wire_pane), text);
    }

    /// Submit a message to a pane — nothing here awaits, so slash commands
    /// (`CommandHost::send_prompt`, which is a sync trait method) reach the very
    /// same routing rule instead of open-coding a second one.
    pub(crate) fn submit_sync(&mut self, pane_id: PaneId, text: String) {
        let key = pane_id.key();

        // Same expansion either way; only the session agent's own read-state is
        // updated for `@file` inlines, since another pane's message is delivered to
        // that agent, not to this handle.
        let sent = match pane_id.is_main() {
            true => hrdr_app::prepare_outgoing_via(&self.agent, &text),
            false => hrdr_app::prepare_outgoing_relayed(&self.agent, &text),
        };
        let steer = Steer::new(sent, text);

        // Only the *session's* conversation has an id to reserve — it is what names
        // the saved file. Idempotent, so it does not care whether this message opens
        // a turn or steers one already running.
        if pane_id.is_main() {
            self.reserve_session_id(&steer.sent);
            self.last_midturn_save = None;
        }

        // One send for every agent. Whether this message steers a turn in flight or
        // starts a fresh one is `send_prompt`'s decision, not a shape this frontend
        // reimplements per pane.
        let delivered = self.live.send_prompt(key, steer, self.event_hook(key));
        match delivered {
            None => {
                let seq = self.next_seq();
                let frame = build_notice(seq, "that agent has finished and been released".into());
                self.emit_raw(frame);
            }
            // A fresh turn: keep its handle so `cancel` can abort it. A steer landed
            // on a turn already running, whose handle we already hold — replacing it
            // with `None` there would report the running turn as finished.
            Some(d) => {
                if let Some(handle) = d.into_handle() {
                    self.main_turn_handle = Some(handle);
                }
            }
        }

        self.tick_notify.notify_one();
        self.tick();
    }

    /// How this frontend surfaces a turn on `key`, for [`AgentRegistry::start_turn`]
    /// and [`AgentRegistry::send_prompt`] alike: stash the session conversation's
    /// committed rounds for the mid-turn save, and wake the tick loop.
    ///
    /// Nothing here folds an event into a view — the event is already in the
    /// agent's own record, and a pane is built by replaying that.
    fn event_hook(&self, key: u64) -> impl FnMut(hrdr_agent::AgentEvent) + Send + 'static {
        let history = self.mid_turn_history.clone();
        let notify = self.tick_notify.clone();
        move |ev| {
            // The agent committed a round and sent its history: stash it for the
            // mid-turn save (the agent lock is held for the whole turn, so this is
            // the only way to see it). Only the session's own conversation is saved
            // that way — a delegated agent snapshots itself beside its transcript.
            if key == MAIN_KEY
                && let hrdr_agent::AgentEvent::History(messages) = &ev
                && let Ok(mut cell) = history.lock()
            {
                *cell = Some(messages.clone());
            }
            notify.notify_one();
        }
    }

    // ── cancel ─────────────────────────────────────────────────────────────

    /// Cancel a running turn.
    pub fn cancel(&mut self, wire_pane: WirePaneId) {
        let pane_id = crate::convert::core_pane_id(wire_pane);
        let key = pane_id.key();

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
        self.save_main_state();
    }

    /// Mid-turn durability, mirroring the TUI's `persist_mid_turn`: the running turn
    /// holds the agent lock, so [`Self::persist`]'s `try_lock` read would skip —
    /// save the newest history snapshot the agent itself committed
    /// ([`hrdr_agent::AgentEvent::History`], stashed by the turn task's callback)
    /// instead. With this, a crash mid-turn loses at most the round in flight.
    ///
    /// What such a save can capture: the agent's committed history (so the messages
    /// on disk are real, not a stale copy), the TODO list, and the cwd. What it
    /// cannot: anything only the locked agent knows — its live token/cost counters
    /// come back through the registry, and `state.transcript` is not written to the
    /// `.json` at all (it is appended to the sibling `<id>.jsonl` as events land, so
    /// the *displayed* conversation is already durable without this).
    ///
    /// Returns whether a snapshot was available and saved.
    pub fn persist_mid_turn(&mut self) -> bool {
        let Some(messages) = self
            .mid_turn_history
            .lock()
            .ok()
            .and_then(|mut cell| cell.take())
        else {
            return false;
        };
        let todos = self
            .panes
            .main()
            .todos
            .lock()
            .map(|t| t.clone())
            .unwrap_or_default();
        // `state.cwd` is only synced by the turn-end save; on the very first turn it
        // would still be empty, which files the session under the wrong cwd slug.
        let cwd = self.cwd.display().to_string();
        let state = &mut self.panes.main_mut().state;
        state.messages = messages;
        state.todos = todos;
        state.cwd = cwd;
        // What the turn-end `sync_from` would name it — this save may be the one that
        // mints the id (a hidden `/init` turn never reserved one).
        if state.name.is_empty() {
            state.name = hrdr_app::session_name_from(&state.messages);
        }
        self.save_main_state();
        true
    }

    /// Run a mid-turn save if one is due (called from the tick task while a main
    /// turn is in flight).
    pub fn maybe_persist_mid_turn(&mut self) {
        if self
            .last_midturn_save
            .is_some_and(|t| t.elapsed() < MIDTURN_SAVE_INTERVAL)
        {
            return;
        }
        if self.persist_mid_turn() {
            self.last_midturn_save = Some(Instant::now());
        }
    }

    // ── internals ──────────────────────────────────────────────────────────

    /// Write the main pane's state to disk and adopt what the save assigned (the
    /// freshly-minted id and its open-lock). The one save path — every caller
    /// decides what the state should say *before* calling it.
    fn save_main_state(&mut self) {
        let state = self.panes.main().state.clone();
        // A failed save is silent here (as it has always been): the frontend has no
        // "autosave failed" channel yet, and the next save retries.
        if let Ok(Some(o)) = hrdr_app::save_session(&state) {
            self.panes.main_mut().state.id = Some(o.id.clone());
            if let Some(lock) = o.open_lock {
                self.active_lock = Some(lock);
            }
            self.refresh_subagent_dir();
        }
    }

    /// Claim this session's id — and with it the sub-agent transcript dir — before
    /// the turn runs, mirroring the TUI's `reserve_session_id`.
    ///
    /// The message is only *enqueued* on the agent at this point, so the agent's
    /// history does not contain it yet: this save must NOT go through
    /// [`Self::persist`], whose `sync_from` would replace the seeded mirror with the
    /// agent's older (here: empty) history and leave a session file with no preview.
    fn reserve_session_id(&mut self, sent: &str) {
        if self.panes.main().state.id.is_some() {
            return;
        }
        // An *empty* turn carries no message of its own (it hands the agent something
        // already in its history). Seeding the mirror with an empty user message
        // would create a session whose first turn is blank.
        if sent.trim().is_empty() {
            return;
        }
        let cwd = self.cwd.display().to_string();
        let state = &mut self.panes.main_mut().state;
        state.messages.push(hrdr_agent::Message::user(sent));
        // Both are otherwise only filled by `sync_from` at the end of the turn — and
        // this save is what mints the id, so an unnamed state would file the session
        // under the wrong cwd slug with a nameless id.
        state.cwd = cwd;
        if state.name.is_empty() {
            state.name = hrdr_app::session_name_from(&state.messages);
        }
        self.save_main_state();
    }

    fn refresh_subagent_dir(&self) {
        if let Some(id) = &self.panes.main().state.id {
            let cwd_str = self.cwd.display().to_string();
            let dir = hrdr_app::child_transcript_dir(&cwd_str, id);
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
            PaneId::MAIN => self.panes.main().transcript(),
            PaneId(k) => {
                if let Some(p) = self.panes.subs().iter().find(|p| p.id == PaneId(k)) {
                    p.transcript()
                } else {
                    self.panes.main().transcript()
                }
            }
        }
    }

    fn wire_panes(&self) -> Vec<WirePane> {
        let main_running = self.main_turn_running();
        let mut out = vec![wire_pane(self.panes.main(), main_running)];

        for sub in self.panes.subs() {
            let key = match sub.id {
                PaneId::MAIN => unreachable!(),
                PaneId(k) => k,
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

    fn build_wire_status(&mut self) -> hrdr_protocol::WireStatus {
        // Both before the pane borrow: the branch lookup writes its cache.
        let branch = self.cached_branch();
        let dir = hrdr_app::display_dir(&self.cwd);
        let pane = self.panes.active_pane();

        let inputs = StatusInputs {
            dir: &dir,
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

    /// The current branch, from a cache that is refreshed at most every
    /// [`BRANCH_CACHE_TTL`].
    ///
    /// `git_branch` forks a subprocess and the status bar is rebuilt on every tick,
    /// so the *absence* of a branch is cached too — a non-repository cwd must not
    /// fork git ten times a second either.
    fn cached_branch(&mut self) -> Option<String> {
        if let Some((branch, taken)) = &self.branch_cache
            && taken.elapsed() < BRANCH_CACHE_TTL
        {
            return branch.clone();
        }
        let branch = hrdr_app::git_branch(&self.cwd);
        self.branch_cache = Some((branch.clone(), Instant::now()));
        branch
    }

    /// Launch a main-pane turn: `run` drains whatever is queued as its opener (an
    /// opener-less call hands the agent something already in its history).
    pub(crate) fn spawn_pending_main_turn(&mut self) {
        self.last_midturn_save = None;
        let done_notify = self.tick_notify.clone();
        // The turn itself is the registry's — `begin_turn`, the run, the panic
        // guard, recording every event on the agent's own entry, the terminal
        // `TurnDone`. What is left here is what only this frontend knows, and it is
        // the same hook a submitted message uses.
        self.main_turn_handle = self.live.start_turn(
            MAIN_KEY,
            self.event_hook(MAIN_KEY),
            move |_outcome| async move {
                done_notify.notify_one();
            },
        );
    }

    // ── session switching (resume) ─────────────────────────────────────────

    /// Swap in a loaded session's state wholesale — the headless twin of the TUI's
    /// `App::adopt_state`, and the only place a resumed conversation is installed.
    ///
    /// The caller must hand in the **locked main agent**: the history swap is part of
    /// the adoption, not a task that races it. (The web host used to set the messages
    /// from a detached `tokio::spawn` and then save immediately — whichever won, the
    /// save could write the OLD conversation into the RESUMED session's file.)
    ///
    /// Two fields are not simply overwritten, matching the TUI:
    ///
    /// * `context_window` — a saved one is a stand-in until the endpoint is
    ///   re-probed, so it never clobbers a window this process already knows.
    /// * `model` / `provider` — the session's identity WINS, and a pre-`provider://model`
    ///   file (`provider_unset`) means "this model, on the provider in force".
    ///
    /// Repointing the agent AT that provider is the caller's last step
    /// (`hrdr_app::restore_session_provider`), because it needs the whole
    /// `CommandHost`.
    pub(crate) fn adopt_state(
        &mut self,
        agent: &mut Agent,
        state: hrdr_app::SessionState,
        id: Option<String>,
    ) {
        let probed_window = self.panes.main().state.usage.context_window;
        let base_url = std::mem::take(&mut self.panes.main_mut().state.base_url);
        // The identity in force right now — the provider an OLD session file (one
        // that named a model but no provider) means by "this model".
        let in_force = self.panes.main().state.model.clone();

        // The state *is* the main pane's — transcript, counters and all — so adopting
        // a session is one assignment.
        self.panes.main_mut().state = state.restored();
        let state = &mut self.panes.main_mut().state;
        state.id = id;
        state.base_url = base_url;
        state.usage.context_window = probed_window.or(state.usage.context_window);
        if state.provider_unset {
            state.model = hrdr_agent::ModelSpec::ModelOnly(state.model.model().to_string())
                .apply(&in_force)
                .expect("a bare model id always resolves");
            state.provider_unset = false;
        }

        // Drop the outgoing session's transcript writer before pointing the dirs at
        // the incoming one, so a resumed session doesn't append onto the file we just
        // left; `refresh_subagent_dir` re-attaches against the adopted id.
        self.detach_transcript();
        self.refresh_subagent_dir();
        // The pane is rebuilt from the registry every tick, main agent included — so
        // the resumed session's counters have to land there too, or the next tick
        // quietly restores the ones we just replaced.
        self.publish_main_agent(agent);

        // Hand the parts whose runtime owners live elsewhere back to them: the chat
        // history and the session's spend to the agent (so it counts on from there),
        // the TODOs to the shared list.
        let (messages, todos, spent) = {
            let s = &self.panes.main().state;
            (s.messages.clone(), s.todos.clone(), s.usage.cost_usd)
        };
        agent.set_messages(messages);
        agent.set_session_cost(spent);
        if let Ok(mut t) = self.panes.main().todos.lock() {
            *t = todos;
        }
        // A mid-turn snapshot from the session we just left must not be saved into
        // this one.
        if let Ok(mut cell) = self.mid_turn_history.lock() {
            *cell = None;
        }
        self.last_midturn_save = None;
    }

    /// Switch the tools' working directory (in-process only — no parent shell is
    /// involved): the agent's cwd, the one this session reports, and the caches keyed
    /// to it. Mirrors the TUI's `apply_cwd`.
    pub(crate) fn apply_cwd(&mut self, agent: &mut Agent, new: PathBuf) {
        agent.set_cwd(new.clone());
        self.cwd = new;
        self.branch_cache = None; // a different directory, a different branch
        self.panes.main_mut().state.cwd = self.cwd.display().to_string();
    }

    /// Keep the main agent's registry entry in step with its pane's state — the
    /// counters half of the TUI's `publish_main_agent`.
    ///
    /// `register_session` is idempotent (a no-op once the entry exists), so what this
    /// actually does after startup is seed the resumed session's usage. What the
    /// agent is *running on* is deliberately not written here: the agent publishes
    /// that itself (`Agent::attach_live`), and a copy kept here is a copy that can be
    /// wrong — which is exactly how a resumed session's provider label used to reach
    /// the status bar while the agent kept talking to the endpoint it launched with.
    fn publish_main_agent(&mut self, agent: &mut Agent) {
        let (reference, base_url, usage) = {
            let s = &self.panes.main().state;
            (s.model.clone(), s.base_url.clone(), s.usage)
        };
        self.live.register_session(
            self.agent.clone(),
            self.steering.clone(),
            reference.model().to_string(),
            Some(reference.provider().to_string()),
            base_url,
            usage,
        );
        self.live.update(MAIN_KEY, |e| e.usage = usage);
        // Adopt the entry (idempotent) so every later change republishes into it.
        agent.attach_live(self.live.clone(), MAIN_KEY);
    }

    // ── public accessors ──────────────────────────────────────────────────

    /// Whether a main-pane turn (or a compaction, which occupies the same slot) is
    /// in flight.
    pub fn main_turn_running(&self) -> bool {
        self.main_turn_handle
            .as_ref()
            .is_some_and(|h| !h.is_finished())
    }

    /// The config this session was built from (provider resolution).
    pub fn cfg(&self) -> &AgentConfig {
        &self.cfg
    }

    pub fn notify_tick(&self) {
        self.tick_notify.notify_one();
    }

    pub fn live(&self) -> &AgentRegistry {
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

#[cfg(test)]
mod tests {
    use super::*;
    use hrdr_agent::{AgentEvent, AgentRegistry, MAIN_KEY};
    use hrdr_protocol::{WireEntryKind, WireToolBody};

    /// Build a bare WebSession for tests. We create a real (minimal) Agent
    /// so the Mutex holds a valid value — it is never actually run.
    fn test_session() -> (WebSession, broadcast::Receiver<ServerFrame>) {
        test_session_at(PathBuf::from("/tmp"))
    }

    /// A test session rooted at `cwd` — the session-file tests each want their own
    /// directory (the file id is derived per cwd slug).
    ///
    /// The endpoint is a closed loopback port: a turn these tests start must fail
    /// immediately rather than reach out over the network.
    fn test_session_at(cwd: PathBuf) -> (WebSession, broadcast::Receiver<ServerFrame>) {
        let (broadcast, rx) = broadcast::channel(256);
        let cfg = AgentConfig {
            cwd: cwd.clone(),
            base_url: "http://127.0.0.1:1/v1".to_string(),
            ..Default::default()
        };
        let agent = Agent::new(cfg.clone()).expect("minimal test agent");
        let (model, provider, base_url) = (
            agent.model_name(),
            Some(agent.provider_name().to_string()),
            agent.endpoint_base_url(),
        );
        let agent = Arc::new(tokio::sync::Mutex::new(agent));

        let live = AgentRegistry::new();
        // Register main so sync() can find it.
        live.register_session(
            agent.clone(),
            hrdr_agent::steering_queue(),
            model,
            provider,
            base_url,
            hrdr_agent::AgentUsage::default(),
        );

        let session = WebSession {
            agent,
            steering: hrdr_agent::steering_queue(),
            live: live.clone(),
            panes: PaneSet::new(),
            subagent_dir: Default::default(),
            broadcast,
            seq: 0,
            replay: VecDeque::with_capacity(REPLAY_CAP),
            sent: HashMap::new(),
            last_panes_json: String::new(),
            last_status_json: String::new(),
            show_thinking: true,
            cwd,
            cfg,
            main_turn_handle: None,
            active_lock: None,
            branch_cache: None,
            mid_turn_history: Default::default(),
            last_midturn_save: None,
            tick_notify: Arc::new(Notify::new()),
        };
        (session, rx)
    }

    /// Whether any saved/held message says `text`.
    fn says(messages: &[hrdr_agent::Message], text: &str) -> bool {
        messages
            .iter()
            .any(|m| m.content.as_deref().is_some_and(|c| c.contains(text)))
    }

    /// The status bar is rebuilt on every tick; the branch behind it is not looked
    /// up again within the cache window — not even for a directory that has no
    /// branch at all (the answer `None` is cached too, or a non-repo cwd would fork
    /// git ten times a second).
    #[test]
    fn branch_lookup_is_cached_between_status_rebuilds() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut session, _rx) = test_session_at(tmp.path().to_path_buf());
        assert!(session.branch_cache.is_none(), "nothing looked up yet");

        let _ = session.build_wire_status();
        let (branch, taken) = session.branch_cache.clone().expect("the lookup was cached");
        assert!(branch.is_none(), "a temp dir is not a repository");

        let _ = session.build_wire_status();
        let (_, taken_again) = session.branch_cache.expect("still cached");
        assert_eq!(
            taken, taken_again,
            "the second rebuild was served from the cache — no second git call"
        );
    }

    /// Submitting on a fresh session claims its id *and* its preview: the file is on
    /// disk with the user's message in it before the turn has produced anything.
    ///
    /// Regression: the reserve pushed the message into the mirror and then saved
    /// through `persist`, whose `sync_from` replaced it with the agent's history —
    /// which does not contain the message yet (it is only enqueued). The result was a
    /// session file with an empty preview.
    #[test]
    fn submit_reserves_a_session_file_with_the_user_message() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        let (mut session, _rx) = test_session_at(cwd.clone());

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            session
                .submit(hrdr_protocol::WirePaneId::Main, "reserve me".into())
                .await;
        });

        let id = session
            .panes
            .main()
            .state
            .id
            .clone()
            .expect("the submit reserved a session id");
        let path = hrdr_app::session_file_path(&cwd.display().to_string(), &id);
        assert!(path.exists(), "no session file at {}", path.display());
        let saved = hrdr_app::Session::load_path(&path).expect("the reserved session loads");
        assert!(
            says(&saved.state.messages, "reserve me"),
            "the saved session has no preview: {:?}",
            saved.state.messages
        );
    }

    /// A mid-turn save writes the history the AGENT committed (the snapshot its
    /// `History` event carried), not the stale mirror — and reports whether it had
    /// anything to write.
    #[test]
    fn mid_turn_save_writes_the_committed_history() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        let (mut session, _rx) = test_session_at(cwd.clone());

        assert!(
            !session.persist_mid_turn(),
            "no committed round yet → nothing to save"
        );

        *session.mid_turn_history.lock().unwrap() = Some(vec![
            hrdr_agent::Message::user("mid turn"),
            hrdr_agent::Message::assistant("round one"),
        ]);
        assert!(session.persist_mid_turn(), "a snapshot was there to save");

        let id = session
            .panes
            .main()
            .state
            .id
            .clone()
            .expect("the mid-turn save minted an id");
        let path = hrdr_app::session_file_path(&cwd.display().to_string(), &id);
        let saved = hrdr_app::Session::load_path(&path).expect("the mid-turn save loads");
        assert!(
            says(&saved.state.messages, "mid turn"),
            "the committed round was not persisted: {:?}",
            saved.state.messages
        );
        assert!(
            session.mid_turn_history.lock().unwrap().is_none(),
            "the snapshot is consumed by the save"
        );
    }

    /// Resuming a session must not destroy it.
    ///
    /// Regression: the swap set the pane's state, handed the agent its messages from
    /// a DETACHED task, and saved immediately — so the save could (and in a
    /// single-threaded runtime always did) run against an agent that still held the
    /// previous conversation, and write that over the resumed session's file. The
    /// history is now applied through a lock taken before anything is swapped, so the
    /// next save round-trips the same conversation.
    #[test]
    fn resume_keeps_the_restored_conversation() {
        use hrdr_app::CommandHost;

        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        let cwd_str = cwd.display().to_string();
        let (mut session, _rx) = test_session_at(cwd.clone());

        // Save a session on the identity this process is already on, so the resume
        // needs no provider repoint (that path is asynchronous by nature).
        let identity: hrdr_agent::ModelRef = session.live.with(|v| {
            let e = v
                .iter()
                .find(|e| e.key == MAIN_KEY)
                .expect("main registered");
            format!("{}://{}", e.provider.clone().unwrap_or_default(), e.model)
                .parse()
                .expect("the agent's own identity parses")
        });
        let state = hrdr_app::SessionState {
            name: "known".to_string(),
            model: identity,
            cwd: cwd_str.clone(),
            messages: vec![
                hrdr_agent::Message::user("keep me"),
                hrdr_agent::Message::assistant("kept"),
            ],
            ..Default::default()
        };
        let outcome = hrdr_app::save_session(&state)
            .expect("the fixture saves")
            .expect("a user message makes it saveable");
        let id = outcome.id.clone();
        // Release the fixture's open-lock so the resume can take it.
        drop(outcome);
        let path = hrdr_app::session_file_path(&cwd_str, &id);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (line_tx, _line_rx) = tokio::sync::mpsc::unbounded_channel();
            let loaded = hrdr_app::Session::load_path(&path).expect("the fixture loads");
            let mut host = crate::host::WebHost {
                session: &mut session,
                line_tx,
            };
            host.resume(id.clone(), loaded);
        });

        assert_eq!(
            session.panes.main().state.id.as_deref(),
            Some(id.as_str()),
            "the resumed session's id is adopted"
        );
        assert!(
            says(&session.panes.main().state.messages, "keep me"),
            "the restored conversation is in the pane"
        );
        assert!(
            says(&session.agent.blocking_lock().messages_owned(), "keep me"),
            "the agent was handed the restored history synchronously"
        );

        // The save that follows a resume (the tick's turn-end `persist`) must write
        // the SAME conversation back.
        session.persist();
        let back = hrdr_app::Session::load_path(&path).expect("the resumed session still loads");
        assert!(
            says(&back.state.messages, "keep me"),
            "the resumed session's file was overwritten: {:?}",
            back.state.messages
        );
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

    /// The cursor rules: current, mid-buffer tail, and a genuine gap.
    #[test]
    fn replay_after_cursor_rules() {
        let (mut session, _rx) = test_session();
        for i in 0..5 {
            let seq = session.next_seq();
            session.emit_raw(build_notice(seq, format!("msg {i}")));
        }
        // Buffer now holds seqs 1..=5, last emitted seq is 5.

        let tail = session
            .replay_after(5)
            .expect("cursor at head is not a gap");
        assert!(tail.is_empty(), "cursor == last seq → nothing to replay");

        let tail = session.replay_after(3).expect("mid-buffer cursor replays");
        assert_eq!(
            tail.iter().map(|f| f.seq).collect::<Vec<_>>(),
            vec![4, 5],
            "mid-buffer cursor → the frames after it"
        );

        let tail = session
            .replay_after(0)
            .expect("a buffer starting at seq 1 is contiguous with cursor 0");
        assert_eq!(tail.len(), 5);

        // Drop the two oldest frames: the buffer now starts at seq 3.
        session.replay.pop_front();
        session.replay.pop_front();
        assert!(
            session.replay_after(0).is_none(),
            "cursor before the buffer → gap"
        );
        assert!(
            session.replay_after(1).is_none(),
            "cursor two frames before the buffer start → gap"
        );
        assert!(
            session.replay_after(2).is_some(),
            "cursor adjacent to the buffer start → contiguous"
        );
    }

    /// A snapshot's seq is never in the replay buffer, but it is still a valid
    /// resume cursor: it is newer than everything buffered, so the client is
    /// current — a gap response (full snapshot) would be wrong.
    #[test]
    fn replay_after_accepts_snapshot_seq() {
        let (mut session, _rx) = test_session();
        for i in 0..3 {
            let seq = session.next_seq();
            session.emit_raw(build_notice(seq, format!("msg {i}")));
        }
        let snap = session.build_snapshot();
        assert_eq!(snap.seq, 4, "snapshot takes the next seq");
        assert!(
            !session.replay.iter().any(|f| f.seq == snap.seq),
            "snapshots are per-connection and never buffered"
        );

        let tail = session
            .replay_after(snap.seq)
            .expect("a snapshot seq is a valid cursor, not a gap");
        assert!(tail.is_empty());
    }

    /// Snapshots consume a seq like every other frame, so the frames a later
    /// tick broadcasts carry strictly higher seqs — no duplicates on the wire.
    #[test]
    fn snapshot_seq_is_monotonic_with_broadcast_frames() {
        let (mut session, mut rx) = test_session();
        let live = session.live.clone();

        let snap = session.build_snapshot();
        assert!(snap.seq >= 1, "snapshot takes a real seq");
        assert!(rx.try_recv().is_err(), "snapshot is not broadcast");
        assert!(session.replay.is_empty(), "snapshot is not buffered");

        live.record(MAIN_KEY, &AgentEvent::Text("hi".into()));
        session.tick();
        let frames = drain_vec(&mut rx);
        assert!(!frames.is_empty(), "tick should broadcast frames");
        for f in &frames {
            assert!(
                f.seq > snap.seq,
                "frame seq {} must exceed the snapshot's {}",
                f.seq,
                snap.seq
            );
        }
        for pair in frames.windows(2) {
            assert!(
                pair[1].seq > pair[0].seq,
                "broadcast seqs must strictly increase"
            );
        }

        let snap2 = session.build_snapshot();
        assert!(
            snap2.seq > frames.last().unwrap().seq,
            "a later snapshot advances past the broadcast frames"
        );
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
