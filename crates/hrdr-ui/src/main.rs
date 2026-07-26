#![cfg(target_arch = "wasm32")]

use dioxus::prelude::*;
use hrdr_protocol::{ClientMsg, ServerFrame, ServerMsg, WirePane, WirePaneId, WireStatus};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{MessageEvent, WebSocket};

mod state;

static WS: std::sync::Mutex<Option<WebSocket>> = std::sync::Mutex::new(None);

#[derive(Clone)]
struct UiState {
    transcript: Vec<state::ViewEntry>,
    panes: Vec<WirePane>,
    active: WirePaneId,
    status: Option<WireStatus>,
    show_thinking: bool,
    connected: bool,
}

fn App() -> Element {
    let mut ui = use_signal(|| UiState {
        transcript: vec![],
        panes: vec![],
        active: WirePaneId::Main,
        status: None,
        show_thinking: true,
        connected: false,
    });
    let mut input = use_signal(String::new);
    let mut last_seq = use_signal(|| 0u64);
    let mut reconnect_count = use_signal(|| 0u32);

    let connect = move || {
        let window = web_sys::window().unwrap();
        let loc = window.location();
        let search = loc.search().unwrap_or_default();
        let token = search.strip_prefix("?token=").unwrap_or("");
        let protocol = if loc.protocol().unwrap_or_default() == "https:" { "wss" } else { "ws" };
        let host = loc.host().unwrap_or_default();
        let ws_url = format!("{protocol}://{host}/ws?token={token}");

        let ws = WebSocket::new(&ws_url).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ServerFrame>();
        *WS.lock().unwrap() = Some(ws.clone());

        let onmsg = Closure::<dyn Fn(MessageEvent)>::new(move |e: MessageEvent| {
            if let Ok(data) = e.data().dyn_into::<js_sys::JsString>() {
                let s: String = data.into();
                if let Ok(frame) = serde_json::from_str::<ServerFrame>(&s) {
                    let _ = tx.send(frame);
                }
            }
        });
        ws.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
        onmsg.forget();
        rx
    };

    use_effect(move || {
        spawn(async move {
            let mut rx = connect();
            while let Some(frame) = rx.recv().await {
                last_seq.set(frame.seq);
                ui.write().apply_frame(&frame);
            }
            // Disconnected — attempt reconnect with backoff.
            ui.write().connected = false;
            reconnect_count += 1;
            let delay = (reconnect_count() * 1000).min(30000) as u64;
            gloo::timers::callback::Timeout::new(delay, move || {
                // Reconnect will re-trigger the effect.
            }).forget();
        });
    });

    let send_msg = move |text: String| {
        let text = text.trim().to_string();
        if text.is_empty() { return; }
        let pane = ui.read().active;
        let msg = if text.starts_with('/') {
            ClientMsg::Command { pane, line: text.clone() }
        } else {
            ClientMsg::Submit { pane, text: text.clone() }
        };
        if let Ok(json) = serde_json::to_string(&msg) {
            if let Some(ws) = WS.lock().unwrap().as_ref() {
                let _ = ws.send_with_str(&json);
            }
        }
    };

    let on_send = move |_| { let t = input(); send_msg(t); input.set(String::new()); };
    let on_key = move |evt: KeyboardEvent| {
        if evt.key() == Key::Enter && !evt.shift_key() {
            evt.prevent_default();
            let t = input(); send_msg(t); input.set(String::new());
        }
    };
    let switch_pane = move |id: WirePaneId| {
        send_msg(format!("/switch {id:?}"));
        let msg = ClientMsg::SwitchPane { pane: id };
        if let Ok(json) = serde_json::to_string(&msg) {
            if let Some(ws) = WS.lock().unwrap().as_ref() {
                let _ = ws.send_with_str(&json);
            }
        }
    };

    let s = ui.read();
    if !s.connected {
        return rsx! { div { style: "height:100dvh;display:flex;align-items:center;justify-content:center;flex-direction:column;font-family:system-ui;background:#1a1a2e;color:#e0e0e0;",
            h1 { "hrdr web" }
            p { "Connecting…" }
        }};
    }

    // Status bar
    let status_html = render_status(&s.status);
    // Active pane turn loader
    let turn_html = render_turn_loader(&s.active_pane());
    // Todo panel
    let todos_html = render_todos(&s.active_pane());

    rsx! {
        div { style: "display:flex;flex-direction:column;height:100dvh;font-family:system-ui;background:#1a1a2e;color:#e0e0e0;",

            // Status bar
            div { style: "display:flex;justify-content:space-between;padding:0.25rem 0.5rem;background:#16213e;font-size:13px;min-height:24px;overflow:hidden;",
                div { style: "display:flex;gap:0.5rem;overflow:hidden;", dangerous_inner_html: status_html.0 }
                div { style: "display:flex;gap:0.5rem;", dangerous_inner_html: status_html.1 }
            }

            // Pane tabs
            if s.panes.len() > 1 {
                div { style: "display:flex;background:#0f3460;overflow-x:auto;",
                    for pane in &s.panes {
                        div {
                            style: format!(
                                "padding:0.25rem 0.75rem;cursor:pointer;font-size:13px;{}",
                                if pane.id == s.active { "background:#16213e;color:#e94560;" } else { "" }
                            ),
                            onclick: { let id = pane.id; move |_| switch_pane(id) },
                            "{pane_marker(pane.status)} {pane.title}"
                        }
                    }
                }
            }

            // Turn loader
            div { dangerous_inner_html: turn_html }

            // Todo panel
            div { dangerous_inner_html: todos_html }

            // Transcript
            div { id:"transcript", style:"flex:1;overflow-y:auto;padding:1rem;",
                for entry in s.transcript.iter() {
                    div { class:"entry entry-{entry.css_class()}", style:"margin-bottom:0.25rem;padding:0.25rem 0;line-height:1.5;", dangerous_inner_html: entry.html() }
                }
                div { id:"transcript-bottom" }
            }

            // Input bar
            div { style:"display:flex;padding:0.5rem;background:#16213e;border-top:1px solid #0f3460;",
                input {
                    value:"{input}", placeholder:"Type a message or /command…", autofocus:"true",
                    style:"flex:1;padding:0.5rem;background:#0f3460;color:#e0e0e0;border:none;border-radius:4px;font-family:monospace;font-size:14px;",
                    oninput: move |evt| input.set(evt.value()), onkeydown: on_key,
                }
                button { style:"padding:0.5rem 1rem;margin-left:0.5rem;background:#e94560;color:white;border:none;border-radius:4px;cursor:pointer;font-size:14px;", onclick: on_send, "Send" }
            }
        }
    }
}

impl UiState {
    fn apply_frame(&mut self, frame: &ServerFrame) {
        match &frame.msg {
            ServerMsg::Snapshot { transcripts, panes, active, status, show_thinking, .. } => {
                self.transcript.clear();
                for pt in transcripts { for ev in &pt.entries { self.transcript.push(state::entry_to_view(ev)); } }
                self.panes = panes.clone();
                self.active = *active;
                self.status = Some(status.clone());
                self.show_thinking = *show_thinking;
                self.connected = true;
            }
            ServerMsg::Entries { from, entries, .. } => {
                self.transcript.truncate(*from);
                for ev in entries { self.transcript.push(state::entry_to_view(ev)); }
            }
            ServerMsg::Panes { panes, active } => {
                self.panes = panes.clone();
                self.active = *active;
            }
            ServerMsg::Status { status } => { self.status = Some(status.clone()); }
            ServerMsg::Resumed { .. } => {}
            _ => {}
        }
    }

    fn active_pane(&self) -> Option<&WirePane> {
        self.panes.iter().find(|p| p.id == self.active)
    }
}

fn pane_marker(s: &hrdr_protocol::WirePaneStatus) -> &'static str {
    match s {
        hrdr_protocol::WirePaneStatus::Running => "⏳",
        hrdr_protocol::WirePaneStatus::Idle => "·",
        hrdr_protocol::WirePaneStatus::Done => "✓",
    }
}

fn render_status(status: &Option<WireStatus>) -> (String, String) {
    let Some(s) = status else { return (String::new(), String::new()) };
    let left: String = s.left.iter().map(|seg| render_status_seg(seg)).collect();
    let right: String = s.right.iter().map(|seg| render_status_seg(seg)).collect();
    (left, right)
}

fn render_status_seg(seg: &hrdr_protocol::WireStatusSeg) -> String {
    if let Some(gauge) = &seg.gauge {
        let pct = (gauge.frac * 100.0) as u32;
        let color = match gauge.level {
            hrdr_protocol::WireCtxLevel::Ok => "#4ecca3",
            hrdr_protocol::WireCtxLevel::Warn => "#f0a500",
            hrdr_protocol::WireCtxLevel::Critical => "#e94560",
        };
        return format!("<span style=\"background:#333;border-radius:3px;padding:0 4px;\"><span style=\"background:{color};width:{pct}%;display:inline-block;border-radius:2px;\">&nbsp;</span><span style=\"font-size:11px;padding:0 2px;\">{}</span></span>", gauge.label);
    }
    let runs: String = seg.runs.iter().map(|r| {
        let role_style = status_role_style(&r.role);
        format!("<span style=\"{role_style}\">{}</span>", r.text.replace('<', "&lt;"))
    }).collect();
    runs
}

fn status_role_style(role: &hrdr_protocol::WireStatusRole) -> &'static str {
    match role {
        hrdr_protocol::WireStatusRole::Dir {} => "color:#e94560;",
        hrdr_protocol::WireStatusRole::Branch {} => "color:#4ecca3;",
        hrdr_protocol::WireStatusRole::TokensIn {} => "color:#f0a500;",
        hrdr_protocol::WireStatusRole::TokensOut {} => "color:#4ecca3;",
        hrdr_protocol::WireStatusRole::CtxFill { .. } => "font-weight:bold;",
        hrdr_protocol::WireStatusRole::CtxRest {} => "color:#666;",
        hrdr_protocol::WireStatusRole::CtxPlain {} => "color:#f0a500;",
        hrdr_protocol::WireStatusRole::Provider {} => "color:#888;",
        hrdr_protocol::WireStatusRole::Model {} => "",
        hrdr_protocol::WireStatusRole::Effort {} => "color:#f0a500;",
        hrdr_protocol::WireStatusRole::Ttft {} => "color:#888;",
        hrdr_protocol::WireStatusRole::Session {} => "background:#f0a500;color:#1a1a2e;padding:0 4px;border-radius:2px;",
    }
}

fn render_turn_loader(pane: Option<&WirePane>) -> String {
    let Some(p) = pane else { return String::new() };
    if !p.turn.running { return String::new() }
    let inferring = if p.turn.inferring { "⚡" } else { "⏳" };
    format!(
        "<div style=\"font-size:13px;color:#888;padding:0.25rem 0.5rem;\">{inferring} {:.1} tok/s · {:.1}s elapsed</div>",
        p.turn.tok_per_sec, p.turn.elapsed_ms as f64 / 1000.0
    )
}

fn render_todos(pane: Option<&WirePane>) -> String {
    let Some(p) = pane else { return String::new() };
    if p.todos.is_empty() { return String::new() }
    let items: String = p.todos.iter().map(|t| {
        let icon = match t.status.as_str() {
            "completed" => "✓",
            "in_progress" => "⏳",
            "cancelled" => "✗",
            _ => "·",
        };
        format!("<div style=\"font-size:13px;padding:0.25rem;color:#ccc;\">{icon} {} <span style=\"color:#888;\">({})</span></div>", t.content.replace('<', "&lt;"), t.status)
    }).collect();
    format!("<div style=\"margin:0.25rem 0;padding:0.25rem 0.5rem;background:#0f3460;border-radius:4px;\"><strong style=\"font-size:13px;color:#f0a500;\">Tasks</strong>{items}</div>")
}

fn main() { dioxus::web::launch(App); }
