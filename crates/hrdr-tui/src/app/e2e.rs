//! End-to-end TUI tests.
//!
//! These drive a real [`App`] against a **mock OpenAI-compatible server** — no
//! network, no live model — through the same seams the event loop uses
//! (`on_key` for input, `on_turn_msg` for streamed agent events), then render to
//! a ratatui [`TestBackend`] and assert on the visible buffer. It's a child
//! module of `app`, so it reaches `App`'s private methods and fields directly.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use super::{App, TurnMsg};
use crate::ui;
use hrdr_agent::AgentConfig;

// ---------------------------------------------------------------------------
// Mock OpenAI-compatible server
// ---------------------------------------------------------------------------

/// A scripted reply the mock server returns for one `chat/completions` call.
#[derive(Clone)]
enum MockReply {
    /// Plain assistant text; ends the turn (`finish_reason: "stop"`).
    Text(String),
    /// A single tool call (`finish_reason: "tool_calls"`). The agent runs the
    /// tool then requests again, consuming the next queued reply.
    ToolCall { name: String, args: String },
}

/// A tiny in-process HTTP server speaking just enough of the OpenAI API for the
/// client: `GET …/models` and a streamed (SSE) `POST …/chat/completions`.
/// Replies are popped from a queue per chat request (defaulting to a short text
/// once the queue drains). Runs until dropped.
struct MockServer {
    base_url: String,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn start(replies: Vec<MockReply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1");
        let queue = Arc::new(Mutex::new(VecDeque::from(replies)));

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let queue = queue.clone();
                tokio::spawn(async move {
                    let head = read_request_head(&mut sock).await;
                    let path = head
                        .lines()
                        .next()
                        .unwrap_or("")
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("");
                    let (ctype, payload) = if path.ends_with("/models") {
                        ("application/json", models_body())
                    } else {
                        let reply = queue
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or(MockReply::Text("ok".to_string()));
                        ("text/event-stream", sse_body(&reply))
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
                         Connection: close\r\n\r\n{payload}",
                        payload.len(),
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });

        Self {
            base_url,
            _handle: handle,
        }
    }
}

/// Read an HTTP request's head (up to and including the blank line), then drain
/// its body per `Content-Length` so the client's write completes cleanly before
/// we respond. Returns the header block (the request line is its first line).
async fn read_request_head(sock: &mut tokio::net::TcpStream) -> String {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = match sock.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        data.extend_from_slice(&buf[..n]);
        if let Some(pos) = find(&data, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&data[..pos]).to_string();
            let body_start = pos + 4;
            let have = data.len() - body_start;
            let mut remaining = content_length(&headers).saturating_sub(have);
            while remaining > 0 {
                match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => remaining = remaining.saturating_sub(n),
                }
            }
            return headers;
        }
    }
    String::from_utf8_lossy(&data).to_string()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn content_length(headers: &str) -> usize {
    headers
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0)
}

fn models_body() -> String {
    "{\"object\":\"list\",\"data\":[{\"id\":\"test-model\",\"object\":\"model\",\
     \"owned_by\":\"local\"}]}"
        .to_string()
}

/// Build a full SSE body (role delta → payload → finish → usage → `[DONE]`) for
/// one scripted reply. Sent all at once with `Content-Length`; the client parses
/// it line-by-line regardless of chunking.
fn sse_body(reply: &MockReply) -> String {
    let role = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n";
    let usage = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\
                 \"completion_tokens\":5}}\n\n";
    let done = "data: [DONE]\n\n";
    let (payload, finish) = match reply {
        MockReply::Text(t) => (
            format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n",
                esc(t)
            ),
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        ),
        MockReply::ToolCall { name, args } => (
            format!(
                "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\
                 \"id\":\"call_1\",\"function\":{{\"name\":\"{}\",\"arguments\":\"{}\"}}}}]}}}}]}}\n\n",
                esc(name),
                esc(args)
            ),
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        ),
    };
    format!("{role}{payload}{finish}{usage}{done}")
}

/// Minimal JSON string escaping for values embedded in the canned SSE frames.
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            _ => o.push(c),
        }
    }
    o
}

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

/// Drives an [`App`] against a [`MockServer`] without the crossterm event loop.
struct Harness {
    app: App,
    rx: mpsc::UnboundedReceiver<TurnMsg>,
    _mock: MockServer,
    _tmp: tempfile::TempDir,
}

impl Harness {
    async fn new(replies: Vec<MockReply>) -> Self {
        let mock = MockServer::start(replies).await;
        let tmp = tempfile::tempdir().unwrap();
        let config = AgentConfig {
            base_url: mock.base_url.clone(),
            model: "test-model".to_string(),
            cwd: tmp.path().to_path_buf(),
            checkpoints: Some("off".to_string()),
            auto_resume: false,
            context_window: Some(1000),
            ..Default::default()
        };
        let mut app = App::new(config).unwrap();
        let rx = app.rx.take().expect("fresh app has its receiver");
        Self {
            app,
            rx,
            _mock: mock,
            _tmp: tmp,
        }
    }

    fn press(&mut self, code: KeyCode) {
        self.app.on_key(KeyEvent::new(code, KeyModifiers::empty()));
    }

    fn type_str(&mut self, s: &str) {
        for c in s.chars() {
            self.press(KeyCode::Char(c));
        }
    }

    /// Type `msg`, press Enter, then pump agent events until the turn settles.
    async fn submit(&mut self, msg: &str) {
        self.type_str(msg);
        self.press(KeyCode::Enter);
        self.pump().await;
    }

    /// Drain the turn channel until the agent is no longer running.
    async fn pump(&mut self) {
        while self.app.running {
            match self.rx.recv().await {
                Some(msg) => self.app.on_turn_msg(msg),
                None => break,
            }
        }
    }

    /// Render the whole UI to a [`TestBackend`] and flatten it to text.
    fn render(&mut self) -> String {
        let mut term = Terminal::new(TestBackend::new(90, 30)).unwrap();
        term.draw(|f| ui::draw(f, &mut self.app)).unwrap();
        buffer_to_string(term.backend().buffer())
    }
}

fn buffer_to_string(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(cell) = buf.cell(Position::new(x, y)) {
                out.push_str(cell.symbol());
            }
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plain_message_gets_a_streamed_reply() {
    let mut h = Harness::new(vec![MockReply::Text(
        "Hello from the mock model.".to_string(),
    )])
    .await;
    h.submit("hi there").await;
    let screen = h.render();
    // The user's message and the assistant's streamed reply both render.
    assert!(
        screen.contains("hi there"),
        "user message missing:\n{screen}"
    );
    assert!(
        screen.contains("Hello from the mock model."),
        "assistant reply missing:\n{screen}"
    );
    // The turn finished — not stuck "running".
    assert!(!h.app.running);
}

#[tokio::test]
async fn tool_call_runs_the_tool_then_finishes() {
    // First reply asks to write a todo; the follow-up turn ends with text.
    let mut h = Harness::new(vec![
        MockReply::ToolCall {
            name: "todo_write".to_string(),
            args: r#"{"todos":[{"content":"write more tests","status":"in_progress"}]}"#
                .to_string(),
        },
        MockReply::Text("Added the todo.".to_string()),
    ])
    .await;
    h.submit("make a plan").await;
    let screen = h.render();
    // The tool call is surfaced, the todo panel shows the item, and the final
    // assistant text lands — proving the full tool round-trip drove two calls.
    assert!(
        screen.contains("todo_write"),
        "tool call missing:\n{screen}"
    );
    assert!(
        screen.contains("write more tests"),
        "todo item missing:\n{screen}"
    );
    assert!(
        screen.contains("Added the todo."),
        "final reply missing:\n{screen}"
    );
    assert!(!h.app.running);
}

#[tokio::test]
async fn slash_help_renders_locally_without_a_turn() {
    let mut h = Harness::new(vec![]).await;
    // `/help` is handled locally — no model turn, so nothing is consumed.
    h.submit("/help").await;
    let screen = h.render();
    // The help text is long and the transcript follows to the bottom, so assert
    // on lines that stay visible there rather than the "Commands" header up top.
    assert!(
        screen.contains("/exit") && screen.contains("reload AGENTS.md"),
        "help output missing:\n{screen}"
    );
    assert!(!h.app.running);
}
