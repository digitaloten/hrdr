//! App state, the async event loop, and agent orchestration.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use hrdr_agent::{Agent, AgentConfig, AgentEvent, Todo};
use hrdr_editor::{EditorEngine, VimEngine};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::Tui;
use crate::ui;

/// One rendered item in the transcript.
pub(crate) enum Entry {
    User(String),
    Assistant(String),
    Reasoning(String),
    Tool {
        id: String,
        name: String,
        args: String,
        result: String,
        ok: bool,
        done: bool,
    },
    System(String),
}

/// Messages from the background agent task back to the UI loop.
enum TurnMsg {
    Event(AgentEvent),
    /// Turn finished; `Some` carries an error string.
    Done(Option<String>),
}

pub(crate) struct App {
    agent: Arc<tokio::sync::Mutex<Agent>>,
    pub(crate) editor: Box<dyn EditorEngine>,
    pub(crate) transcript: Vec<Entry>,
    pub(crate) running: bool,
    pub(crate) status: String,
    pub(crate) model: String,
    /// Handle to the in-flight turn task; `abort()` cancels it.
    turn_handle: Option<JoinHandle<()>>,
    /// Transcript scroll offset in raw lines from the natural bottom.
    /// 0 = auto-follow (pin to newest content).
    pub(crate) scroll_offset: usize,
    /// Height of the transcript area as measured during the last draw; used
    /// by key handlers to compute half-page scroll amounts.
    pub(crate) transcript_height: u16,
    /// Shared TODO list updated live by the `todo_write` tool.
    pub(crate) todos: Arc<Mutex<Vec<Todo>>>,
    tx: mpsc::UnboundedSender<TurnMsg>,
    rx: Option<mpsc::UnboundedReceiver<TurnMsg>>,
    should_quit: bool,
}

impl App {
    pub(crate) fn new(config: AgentConfig) -> Result<Self> {
        let model = config.model.clone();
        let agent = Agent::new(config)?;
        let todos = agent.todos();
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Self {
            agent: Arc::new(tokio::sync::Mutex::new(agent)),
            editor: Box::new(VimEngine::new()),
            transcript: vec![Entry::System(
                "hrdr ready. Type a task — Insert to type, Esc for Normal, Enter to send, \
                 Ctrl+C to quit (Esc in Normal cancels a running turn)."
                    .to_string(),
            )],
            running: false,
            status: "ready".to_string(),
            model,
            turn_handle: None,
            scroll_offset: 0,
            transcript_height: 24,
            todos,
            tx,
            rx: Some(rx),
            should_quit: false,
        })
    }

    pub(crate) async fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        let mut events = EventStream::new();
        let mut rx = self.rx.take().expect("run called once");

        loop {
            terminal.draw(|f| ui::draw(f, self))?;
            if self.should_quit {
                break;
            }

            tokio::select! {
                maybe_ev = events.next() => match maybe_ev {
                    Some(Ok(Event::Key(key))) => self.on_key(key),
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                },
                Some(msg) = rx.recv() => self.on_turn_msg(msg),
            }
        }
        Ok(())
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }

        // Ctrl+C / Ctrl+Q: quit when idle, cancel when running.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') if self.running => {
                    self.cancel_turn();
                    return;
                }
                KeyCode::Char('c') | KeyCode::Char('q') => {
                    self.should_quit = true;
                    return;
                }
                // Transcript scroll — Ctrl+U/Ctrl+D in Normal mode (vim convention).
                KeyCode::Char('u') if self.editor.mode_label() == "NORMAL" => {
                    let half = (self.transcript_height / 2).max(1) as usize;
                    self.scroll_offset = self.scroll_offset.saturating_add(half);
                    return;
                }
                KeyCode::Char('d') if self.editor.mode_label() == "NORMAL" => {
                    let half = (self.transcript_height / 2).max(1) as usize;
                    self.scroll_offset = self.scroll_offset.saturating_sub(half);
                    return;
                }
                _ => {}
            }
        }

        // Esc in Normal mode while running → cancel the in-flight turn.
        if self.running
            && key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && self.editor.mode_label() == "NORMAL"
        {
            self.cancel_turn();
            return;
        }

        // PageUp / PageDown scroll the transcript (any mode).
        if key.modifiers.is_empty() {
            match key.code {
                KeyCode::PageUp => {
                    let page = self.transcript_height.max(1) as usize;
                    self.scroll_offset = self.scroll_offset.saturating_add(page);
                    return;
                }
                KeyCode::PageDown => {
                    let page = self.transcript_height.max(1) as usize;
                    self.scroll_offset = self.scroll_offset.saturating_sub(page);
                    return;
                }
                _ => {}
            }
        }

        // Enter in Normal mode submits the input buffer.
        if !self.running
            && key.code == KeyCode::Enter
            && key.modifiers.is_empty()
            && self.editor.mode_label() == "NORMAL"
        {
            let input = self.editor.content();
            if input.trim().is_empty() {
                return;
            }
            self.transcript.push(Entry::User(input.clone()));
            self.editor.set_content("");
            // Reset scroll to auto-follow on new submission.
            self.scroll_offset = 0;
            self.spawn_turn(input);
            return;
        }

        self.editor.feed_key(key);
    }

    /// Abort the in-flight agent task, update status, push a cancel marker.
    fn cancel_turn(&mut self) {
        if let Some(handle) = self.turn_handle.take() {
            handle.abort();
        }
        self.running = false;
        self.status = "cancelled".to_string();
        self.transcript
            .push(Entry::System("[cancelled]".to_string()));
    }

    fn spawn_turn(&mut self, input: String) {
        self.running = true;
        self.status = "thinking…".to_string();
        let agent = self.agent.clone();
        let tx = self.tx.clone();
        let tx_events = tx.clone();
        let handle = tokio::spawn(async move {
            let mut a = agent.lock().await;
            let result = a
                .run(input, |ev| {
                    let _ = tx_events.send(TurnMsg::Event(ev));
                })
                .await;
            let _ = tx.send(TurnMsg::Done(result.err().map(|e| e.to_string())));
        });
        self.turn_handle = Some(handle);
    }

    fn on_turn_msg(&mut self, msg: TurnMsg) {
        match msg {
            TurnMsg::Event(ev) => {
                // Ignore buffered events after cancellation.
                if self.running {
                    self.apply_event(ev);
                }
            }
            TurnMsg::Done(err) => {
                if !self.running {
                    // Stale Done from an aborted task; discard.
                    return;
                }
                self.turn_handle = None;
                self.running = false;
                match err {
                    Some(e) => {
                        self.status = format!("error: {e}");
                        self.transcript.push(Entry::System(format!("[error] {e}")));
                    }
                    None => self.status = "ready".to_string(),
                }
            }
        }
    }

    fn apply_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Text(t) => match self.transcript.last_mut() {
                Some(Entry::Assistant(s)) => s.push_str(&t),
                _ => self.transcript.push(Entry::Assistant(t)),
            },
            AgentEvent::Reasoning(t) => match self.transcript.last_mut() {
                Some(Entry::Reasoning(s)) => s.push_str(&t),
                _ => self.transcript.push(Entry::Reasoning(t)),
            },
            AgentEvent::ToolStart { id, name, args } => {
                self.status = format!("running {name}…");
                self.transcript.push(Entry::Tool {
                    id,
                    name,
                    args,
                    result: String::new(),
                    ok: true,
                    done: false,
                });
            }
            AgentEvent::ToolEnd {
                id,
                result,
                ok,
                name: _,
            } => {
                for entry in self.transcript.iter_mut().rev() {
                    if let Entry::Tool {
                        id: tid,
                        result: r,
                        ok: o,
                        done,
                        ..
                    } = entry
                        && *tid == id
                        && !*done
                    {
                        *r = result;
                        *o = ok;
                        *done = true;
                        break;
                    }
                }
            }
            AgentEvent::TurnDone => {
                self.status = "ready".to_string();
            }
        }
    }
}
