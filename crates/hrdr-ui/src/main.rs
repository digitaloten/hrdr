#![cfg(target_arch = "wasm32")]

//! hrdr web UI — Dioxus WASM SPA.

use dioxus::prelude::*;
use hrdr_protocol::{ClientMsg, ServerFrame, WirePaneId};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{MessageEvent, WebSocket};

mod state;

static WS: std::sync::Mutex<Option<WebSocket>> = std::sync::Mutex::new(None);

fn App() -> Element {
    let mut transcript = use_signal(Vec::<state::ViewEntry>::new);
    let mut connected = use_signal(|| false);
    let mut input = use_signal(String::new);

    use_effect(move || {
        spawn(async move {
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

            let ws = match WebSocket::new(&ws_url) {
                Ok(ws) => ws,
                Err(e) => {
                    gloo::dialogs::alert(&format!("Connect failed: {:?}", e));
                    return;
                }
            };

            let (rx_tx, mut rx_rx) = tokio::sync::mpsc::unbounded_channel::<ServerFrame>();
            let ws_clone = ws.clone();
            *WS.lock().unwrap() = Some(ws_clone);

            let onmsg = Closure::<dyn Fn(MessageEvent)>::new(move |e: MessageEvent| {
                if let Ok(data) = e.data().dyn_into::<js_sys::JsString>() {
                    let s: String = data.into();
                    if let Ok(frame) = serde_json::from_str::<ServerFrame>(&s) {
                        let _ = rx_tx.send(frame);
                    }
                }
            });
            ws.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
            onmsg.forget();

            let onopen = Closure::<dyn Fn()>::new(move || {
                // Connected
            });
            ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
            onopen.forget();

            connected.set(true);

            while let Some(frame) = rx_rx.recv().await {
                let mut t = transcript();
                state::apply_frame(&frame, &mut t);
                transcript.set(t);
            }
        });
    });

    let send_msg = move |text: String| {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let pane = WirePaneId::Main;
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
        let t = input();
        send_msg(t);
        input.set(String::new());
    };

    let on_key = move |evt: KeyboardEvent| {
        if evt.key() == Key::Enter && !evt.shift_key() {
            evt.prevent_default();
            let t = input();
            send_msg(t);
            input.set(String::new());
        }
    };

    if !connected() {
        return rsx! {
            div { style: "height: 100dvh; display: flex; align-items: center; justify-content: center; flex-direction: column; font-family: system-ui; background: #1a1a2e; color: #e0e0e0;",
                h1 { "hrdr web" }
                p { "Connecting…" }
            }
        };
    }

    rsx! {
        div {
            style: "display: flex; flex-direction: column; height: 100dvh; font-family: system-ui; background: #1a1a2e; color: #e0e0e0;",

            // Transcript
            div {
                id: "transcript",
                style: "flex: 1; overflow-y: auto; padding: 1rem;",
                onmounted: move |_| {
                    if let Some(el) = web_sys::window()
                        .and_then(|w| w.document())
                        .and_then(|d| d.get_element_by_id("transcript"))
                    {
                        el.set_scroll_top(el.scroll_height());
                    }
                },
                for entry in transcript() {
                    div {
                        class: "entry entry-{entry.css_class()}",
                        style: "margin-bottom: 0.25rem; padding: 0.25rem 0; line-height: 1.5;",
                        dangerous_inner_html: entry.html(),
                    }
                }
                div { id: "transcript-bottom" }
            }

            // Input bar
            div {
                style: "display: flex; padding: 0.5rem; background: #16213e; border-top: 1px solid #0f3460;",
                input {
                    value: "{input}",
                    placeholder: "Type a message or /command…",
                    autofocus: "true",
                    style: "flex: 1; padding: 0.5rem; background: #0f3460; color: #e0e0e0; border: none; border-radius: 4px; font-family: monospace; font-size: 14px;",
                    oninput: move |evt| input.set(evt.value()),
                    onkeydown: on_key,
                }
                button {
                    style: "padding: 0.5rem 1rem; margin-left: 0.5rem; background: #e94560; color: white; border: none; border-radius: 4px; cursor: pointer; font-size: 14px;",
                    onclick: on_send,
                    "Send"
                }
            }
        }
    }
}

fn main() {
    dioxus::web::launch(App);
}
