#![cfg(target_arch = "wasm32")]

use dioxus::prelude::*;
use hrdr_protocol::{
    ClientMsg, ServerFrame, ServerMsg, WireApprovalDecision, WirePane, WirePaneId, WireStatus,
};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{MessageEvent, WebSocket};

use hrdr_ui::state;

static WS: std::sync::Mutex<Option<WebSocket>> = std::sync::Mutex::new(None);

/// One open escalation question: may this command run OUTSIDE the OS sandbox?
#[derive(Clone, PartialEq)]
struct ApprovalPrompt {
    /// Quoted back when answering, so a late answer can never resolve some
    /// *other* request.
    id: String,
    /// The command verbatim. Rendered whole — see `approval_modal`.
    command: String,
    reason: String,
    rules: Vec<String>,
    /// Whether "approve for the session" will really be honoured as standing. The
    /// button is omitted when it would not be — a choice that quietly means
    /// something narrower than its label is worse than no choice.
    allow_session: bool,
}

/// The two approve buttons stay inert for this long after the dialog appears.
///
/// The browser's version of the terminal's reflexive-Enter hazard: a click that
/// was already on its way to whatever was under the cursor lands on a dialog
/// that appeared mid-turn, unannounced. Enforced by the browser's own animation
/// clock (see `APPROVAL_CSS`), not by a timer this app has to get right — and
/// only on the two answers that grant something. Deny is live from the first
/// frame, because denying early costs one confined run.
const APPROVAL_ARM_MS: u32 = 600;

/// Styles for the approval modal, built from [`APPROVAL_ARM_MS`] so the delay and
/// the sentence describing it to the user cannot drift apart.
///
/// `pointer-events: none` until the animation's last step is what makes the
/// arming delay real rather than cosmetic: an early click passes straight
/// through the button to the backdrop, which denies.
fn approval_css() -> String {
    format!(
        "@keyframes hrdr-approval-arm {{ \
           0%, 99% {{ pointer-events: none; opacity: 0.45; }} \
           100% {{ pointer-events: auto; opacity: 1; }} \
         }} \
         .hrdr-approve {{ animation: hrdr-approval-arm {APPROVAL_ARM_MS}ms linear forwards; }}"
    )
}

#[derive(Clone)]
struct UiState {
    transcript: Vec<state::ViewEntry>,
    panes: Vec<WirePane>,
    active: WirePaneId,
    status: Option<WireStatus>,
    show_thinking: bool,
    connected: bool,
    /// The question on screen. While it is `Some`, the composer is inert: this
    /// dialog must not be answerable by accident while typing a message.
    approval: Option<ApprovalPrompt>,
    /// Questions that arrived while another was on screen.
    ///
    /// Two shell calls in one tool batch can both be eligible, and each blocks
    /// its own call until answered — so a second request can never be dropped,
    /// and two dialogs cannot both own the screen. It queues, and answering one
    /// opens the next.
    approval_queue: std::collections::VecDeque<ApprovalPrompt>,
}

#[allow(non_snake_case)]
fn App() -> Element {
    let mut ui = use_signal(|| UiState {
        transcript: vec![],
        panes: vec![],
        active: WirePaneId::Main,
        status: None,
        show_thinking: true,
        connected: false,
        approval: None,
        approval_queue: std::collections::VecDeque::new(),
    });
    let mut input = use_signal(String::new);
    let mut last_seq = use_signal(|| 0u64);
    let mut reconnect_count = use_signal(|| 0u32);

    let connect = move || {
        let window = web_sys::window().unwrap();
        let loc = window.location();
        let search = loc.search().unwrap_or_default();
        let token = search.strip_prefix("?token=").unwrap_or("");
        let protocol = if loc.protocol().unwrap_or_default() == "https:" {
            "wss"
        } else {
            "ws"
        };
        let host = loc.host().unwrap_or_default();
        let ws_url = format!("{protocol}://{host}/ws?token={token}");

        let ws = WebSocket::new(&ws_url).unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<ServerFrame>();
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
            let delay = (reconnect_count() * 1000).min(30000);
            gloo::timers::callback::Timeout::new(delay, move || {
                // Reconnect will re-trigger the effect.
            })
            .forget();
        });
    });

    let send_msg = move |text: String| {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let pane = ui.read().active;
        let msg = if text.starts_with('/') {
            ClientMsg::Command {
                pane,
                line: text.clone(),
            }
        } else {
            ClientMsg::Submit {
                pane,
                text: text.clone(),
            }
        };
        if let Ok(json) = serde_json::to_string(&msg) {
            if let Some(ws) = WS.lock().unwrap().as_ref() {
                let _ = ws.send_with_str(&json);
            }
        }
    };

    let on_send = move |_| {
        // Modal: while a command is waiting on a yes/no, nothing else sends.
        if ui.read().approval.is_some() {
            return;
        }
        let t = input();
        send_msg(t);
        input.set(String::new());
    };
    let on_key = move |evt: KeyboardEvent| {
        if ui.read().approval.is_some() {
            return;
        }
        if evt.key() == Key::Enter && !evt.modifiers().shift() {
            evt.prevent_default();
            let t = input();
            send_msg(t);
            input.set(String::new());
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
    let turn_html = render_turn_loader(s.active_pane());
    // Todo panel
    let todos_html = render_todos(s.active_pane());

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

            // Input bar. Disabled while a question is open: the dialog must not be
            // answerable — or ignorable — by carrying on typing.
            div { style:"display:flex;padding:0.5rem;background:#16213e;border-top:1px solid #0f3460;",
                input {
                    value:"{input}", placeholder:"Type a message or /command…", autofocus:"true",
                    disabled: s.approval.is_some(),
                    style:"flex:1;padding:0.5rem;background:#0f3460;color:#e0e0e0;border:none;border-radius:4px;font-family:monospace;font-size:14px;",
                    oninput: move |evt| input.set(evt.value()), onkeydown: on_key,
                }
                button {
                    style:"padding:0.5rem 1rem;margin-left:0.5rem;background:#e94560;color:white;border:none;border-radius:4px;cursor:pointer;font-size:14px;",
                    disabled: s.approval.is_some(),
                    onclick: on_send, "Send"
                }
            }

            // The escalation modal, last in the tree and on top of everything.
            if let Some(prompt) = s.approval.as_ref() {
                {approval_modal(ui, prompt, s.approval_queue.len())}
            }
        }
    }
}

/// The escalation-approval dialog: what is being asked, the command **verbatim**,
/// what approving actually grants, the rule that matched, and the three answers.
///
/// The security-shaped choices, which mirror the TUI's modal:
///
/// * The command is wrapped and scrolled, **never truncated**. Everything else
///   here elides happily; this may not. The user is consenting to *this* command
///   running with no sandbox, and `git push … ; rm -rf ~` with the tail cut off
///   is a dialog that lies about what it is asking for.
/// * Deny is the default and the safest action: it is first in the tab order,
///   it takes focus when the dialog opens (so a keypress aimed at the message box
///   lands on "no"), and it is what Esc, a click on the backdrop, and an early
///   click on a not-yet-armed approve button all do.
/// * The grant is described in plain words — this runs *outside* the sandbox, as
///   you, with your files and keys. Nothing is "elevated"; the confinement is
///   simply removed.
/// * The matched rules are named, so "for the session" has a visible meaning.
fn approval_modal(ui: Signal<UiState>, prompt: &ApprovalPrompt, queued: usize) -> Element {
    let rules = if prompt.rules.is_empty() {
        "—".to_string()
    } else {
        prompt.rules.join(", ")
    };
    let rule_word = if prompt.rules.len() == 1 {
        "rule"
    } else {
        "rules"
    };
    rsx! {
        div {
            // Keyed on the request, so a *new* question mounts fresh DOM and the
            // arming animation starts over. Without it the next dialog inherits
            // the finished animation of the last one and is live on arrival.
            key: "{prompt.id}",
            tabindex: "-1",
            style: "position:fixed;inset:0;z-index:1000;display:flex;align-items:center;\
                    justify-content:center;background:rgba(8,8,20,0.82);padding:1rem;",
            // Dismissing a dialog is not consenting to it: a click that misses the
            // panel (including one that passed through a still-inert approve
            // button) denies.
            onclick: move |_| answer_approval(ui, WireApprovalDecision::Deny),
            onkeydown: move |evt: KeyboardEvent| {
                if evt.key() == Key::Escape {
                    answer_approval(ui, WireApprovalDecision::Deny);
                }
            },
            div {
                style: "max-width:44rem;width:100%;max-height:88dvh;overflow-y:auto;\
                        background:#16213e;border:1px solid #e94560;border-radius:6px;\
                        padding:1rem;font-family:system-ui;box-shadow:0 8px 40px rgba(0,0,0,0.6);",
                // Clicks inside the panel are not "clicking away".
                onclick: move |evt| evt.stop_propagation(),

                // Mounted with the dialog and torn down with it, so the arming
                // animation is defined exactly while there is something to arm.
                style { {approval_css()} }

                div { style:"color:#e94560;font-weight:bold;font-size:16px;margin-bottom:0.5rem;",
                    "⚠ Run this command with less confinement?"
                }

                div { style:"color:#888;font-size:12px;text-transform:uppercase;letter-spacing:0.05em;", "Command" }
                // Wrapped and scrolled — never shortened, and never through
                // `dangerous_inner_html`: this is text, and it is the one string
                // on the page that must arrive exactly as the server sent it.
                pre {
                    style: "margin:0.25rem 0 0.75rem 0;padding:0.5rem;background:#0f3460;\
                            border-radius:4px;color:#e0e0e0;font-family:monospace;font-size:13px;\
                            white-space:pre-wrap;overflow-wrap:anywhere;word-break:break-word;\
                            max-height:32dvh;overflow-y:auto;",
                    "{prompt.command}"
                }

                // The severity of the grant, and the only place it is described.
                // There is deliberately no hard-coded "runs with NO sandbox at
                // all" above: that was true of the only rung that existed when it
                // was written and false of the two narrower ones added since, so a
                // dialog carrying it would have promised the user's whole
                // filesystem away while the command actually ran fully confined.
                div { style:"color:#f0a500;font-size:14px;line-height:1.5;margin-bottom:0.75rem;",
                    "{prompt.reason}"
                }
                if prompt.allow_session {
                    div { style:"color:#888;font-size:13px;line-height:1.5;margin-bottom:0.75rem;",
                        "Matched {rule_word}: "
                        code { style:"color:#4ecca3;", "{rules}" }
                        " — this is what “approve for the session” would remember, and every \
                         later command matching it would then run the same way without asking."
                    }
                }

                if queued > 0 {
                    div { style:"color:#f0a500;font-size:13px;margin-bottom:0.5rem;",
                        "{queued} more waiting — each blocks its own command until answered."
                    }
                }

                // Deny first: first in the tab order, focused on open, widest.
                div { style:"display:flex;flex-wrap:wrap;gap:0.5rem;margin-top:0.75rem;",
                    button {
                        autofocus: "true",
                        style:"padding:0.5rem 1rem;background:#4ecca3;color:#0a1020;border:none;\
                               border-radius:4px;cursor:pointer;font-size:14px;font-weight:bold;",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            answer_approval(ui, WireApprovalDecision::Deny);
                        },
                        "Deny — run it inside the sandbox"
                    }
                    button {
                        class: "hrdr-approve",
                        style:"padding:0.5rem 1rem;background:#0f3460;color:#e0e0e0;\
                               border:1px solid #e94560;border-radius:4px;cursor:pointer;font-size:14px;",
                        onclick: move |evt| {
                            evt.stop_propagation();
                            answer_approval(ui, WireApprovalDecision::Once);
                        },
                        "Approve once"
                    }
                    if prompt.allow_session {
                        button {
                            class: "hrdr-approve",
                            style:"padding:0.5rem 1rem;background:#0f3460;color:#e0e0e0;\
                                   border:1px solid #e94560;border-radius:4px;cursor:pointer;font-size:14px;",
                            onclick: move |evt| {
                                evt.stop_propagation();
                                answer_approval(ui, WireApprovalDecision::Session);
                            },
                            "Approve for the session"
                        }
                    }
                }
                div { style:"color:#666;font-size:12px;margin-top:0.5rem;",
                    "Esc, or clicking outside, denies. The approve buttons come alive \
                     {APPROVAL_ARM_MS}ms after this dialog appears, so a click already on its way \
                     somewhere else cannot grant anything."
                }
            }
        }
    }
}

impl UiState {
    fn apply_frame(&mut self, frame: &ServerFrame) {
        match &frame.msg {
            ServerMsg::Snapshot {
                transcripts,
                panes,
                active,
                status,
                show_thinking,
                ..
            } => {
                self.transcript.clear();
                for pt in transcripts {
                    for ev in &pt.entries {
                        self.transcript.push(state::entry_to_view(ev));
                    }
                }
                self.panes = panes.clone();
                self.active = *active;
                self.status = Some(status.clone());
                self.show_thinking = *show_thinking;
                self.connected = true;
            }
            ServerMsg::Entries { from, entries, .. } => {
                self.transcript.truncate(*from);
                for ev in entries {
                    self.transcript.push(state::entry_to_view(ev));
                }
            }
            ServerMsg::Panes { panes, active } => {
                self.panes = panes.clone();
                self.active = *active;
            }
            ServerMsg::Status { status } => {
                self.status = Some(status.clone());
            }
            ServerMsg::Resumed { .. } => {}
            // A tool call is blocked on this one right now. It is the only frame
            // this client has to *act* on rather than render — nothing is folded
            // into the transcript, and the dialog it opens is the only thing that
            // can unblock the call.
            ServerMsg::ApprovalRequested {
                id,
                command,
                reason,
                rules,
                allow_session,
            } => {
                let prompt = ApprovalPrompt {
                    id: id.clone(),
                    command: command.clone(),
                    reason: reason.clone(),
                    rules: rules.clone(),
                    allow_session: *allow_session,
                };
                match self.approval {
                    Some(_) => self.approval_queue.push_back(prompt),
                    None => self.approval = Some(prompt),
                }
            }
            // Answered by another tab, or timed out inside the blocked call. Take
            // the dialog down: leaving it up solicits consent for a decision that
            // has already been made, and the next click would answer nothing.
            ServerMsg::ApprovalClosed { id } => {
                if self.approval.as_ref().is_some_and(|p| &p.id == id) {
                    self.approval = self.approval_queue.pop_front();
                } else {
                    self.approval_queue.retain(|p| &p.id != id);
                }
            }
            _ => {}
        }
    }

    fn active_pane(&self) -> Option<&WirePane> {
        self.panes.iter().find(|p| p.id == self.active)
    }
}

/// Answer the open question and move on to any queued one.
///
/// Keyed by the request's own id, so an answer sent after its request expired
/// resolves *nothing* rather than the next person's question — the server checks
/// the same thing at the gate.
fn answer_approval(mut ui: Signal<UiState>, decision: WireApprovalDecision) {
    let id = {
        let mut s = ui.write();
        let Some(prompt) = s.approval.take() else {
            return;
        };
        let next = s.approval_queue.pop_front();
        s.approval = next;
        prompt.id
    };
    let msg = ClientMsg::AnswerApproval { id, decision };
    if let Ok(json) = serde_json::to_string(&msg)
        && let Some(ws) = WS.lock().unwrap().as_ref()
    {
        let _ = ws.send_with_str(&json);
    }
}

fn pane_marker(s: hrdr_protocol::WirePaneStatus) -> &'static str {
    match s {
        hrdr_protocol::WirePaneStatus::Running => "⏳",
        hrdr_protocol::WirePaneStatus::Idle => "·",
        hrdr_protocol::WirePaneStatus::Done => "✓",
    }
}

fn render_status(status: &Option<WireStatus>) -> (String, String) {
    let Some(s) = status else {
        return (String::new(), String::new());
    };
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
        return format!(
            "<span style=\"background:#333;border-radius:3px;padding:0 4px;\"><span style=\"background:{color};width:{pct}%;display:inline-block;border-radius:2px;\">&nbsp;</span><span style=\"font-size:11px;padding:0 2px;\">{}</span></span>",
            gauge.label
        );
    }
    let runs: String = seg
        .runs
        .iter()
        .map(|r| {
            let role_style = status_role_style(&r.role);
            format!(
                "<span style=\"{role_style}\">{}</span>",
                r.text.replace('<', "&lt;")
            )
        })
        .collect();
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
        hrdr_protocol::WireStatusRole::Session {} => {
            "background:#f0a500;color:#1a1a2e;padding:0 4px;border-radius:2px;"
        }
    }
}

fn render_turn_loader(pane: Option<&WirePane>) -> String {
    let Some(p) = pane else { return String::new() };
    if !p.turn.running {
        return String::new();
    }
    let inferring = if p.turn.inferring { "⚡" } else { "⏳" };
    format!(
        "<div style=\"font-size:13px;color:#888;padding:0.25rem 0.5rem;\">{inferring} {:.1} tok/s · {:.1}s elapsed</div>",
        p.turn.tok_per_sec,
        p.turn.elapsed_ms as f64 / 1000.0
    )
}

fn render_todos(pane: Option<&WirePane>) -> String {
    let Some(p) = pane else { return String::new() };
    if p.todos.is_empty() {
        return String::new();
    }
    let items: String = p.todos.iter().map(|t| {
        let icon = match t.status.as_str() {
            "completed" => "✓",
            "in_progress" => "⏳",
            "cancelled" => "✗",
            _ => "·",
        };
        format!("<div style=\"font-size:13px;padding:0.25rem;color:#ccc;\">{icon} {} <span style=\"color:#888;\">({})</span></div>", t.content.replace('<', "&lt;"), t.status)
    }).collect();
    format!(
        "<div style=\"margin:0.25rem 0;padding:0.25rem 0.5rem;background:#0f3460;border-radius:4px;\"><strong style=\"font-size:13px;color:#f0a500;\">Tasks</strong>{items}</div>"
    )
}

fn main() {
    dioxus::launch(App);
}
