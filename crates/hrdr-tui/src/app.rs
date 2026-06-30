//! App state, the async event loop, and agent orchestration.

use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use hrdr_agent::{Agent, AgentConfig, AgentEvent};
use hrdr_editor::{EditorEngine, VimEngine};
use tokio::sync::{Mutex, mpsc};

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
    agent: Arc<Mutex<Agent>>,
    pub(crate) editor: Box<dyn EditorEngine>,
    pub(crate) transcript: Vec<Entry>,
    pub(crate) running: bool,
    pub(crate) status: String,
    pub(crate) model: String,
    tx: mpsc::UnboundedSender<TurnMsg>,
    rx: Option<mpsc::UnboundedReceiver<TurnMsg>>,
    should_quit: bool,
}

impl App {
    pub(crate) fn new(config: AgentConfig) -> Result<Self> {
        let model = config.model.clone();
        let agent = Agent::new(config)?;
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Self {
            agent: Arc::new(Mutex::new(agent)),
            editor: Box::new(VimEngine::new()),
            transcript: vec![Entry::System(
                "hrdr ready. Type a task — Insert to type, Esc for Normal, Enter to send, \
                 Ctrl+C to quit."
                    .to_string(),
            )],
            running: false,
            status: "ready".to_string(),
            model,
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

        // Ctrl+C / Ctrl+Q always quit, even mid-turn.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q'))
        {
            self.should_quit = true;
            return;
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
            self.spawn_turn(input);
            return;
        }

        self.editor.feed_key(key);
    }

    fn spawn_turn(&mut self, input: String) {
        self.running = true;
        self.status = "thinking…".to_string();
        let agent = self.agent.clone();
        let tx = self.tx.clone();
        let tx_events = tx.clone();
        tokio::spawn(async move {
            let mut a = agent.lock().await;
            let result = a
                .run(input, |ev| {
                    let _ = tx_events.send(TurnMsg::Event(ev));
                })
                .await;
            let _ = tx.send(TurnMsg::Done(result.err().map(|e| e.to_string())));
        });
    }

    fn on_turn_msg(&mut self, msg: TurnMsg) {
        match msg {
            TurnMsg::Event(ev) => self.apply_event(ev),
            TurnMsg::Done(err) => {
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
