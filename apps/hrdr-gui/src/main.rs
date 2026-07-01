//! `hrdr-gui` — a floem desktop frontend for the agentic coding harness.
//!
//! This is a **scaffold / proof-of-concept**. It drives the same UI-agnostic
//! core the TUI uses — `hrdr_agent::Agent` — rendering its streamed
//! [`AgentEvent`]s in a floem window. As GUI features grow, the parts shared
//! with the TUI (transcript model, slash commands, sessions, …) get lifted out
//! of `hrdr-tui` into a shared crate that both frontends consume.

use std::sync::Arc;

use floem::ext_event::create_signal_from_tokio_channel;
use floem::prelude::*;
use floem::reactive::create_effect;
use floem::views::Decorators;
use hrdr_agent::{Agent, AgentConfig, AgentEvent};
use tokio::sync::Mutex as TokioMutex;

/// A rendered transcript message. `text` is a signal so streamed tokens update
/// the view in place without rebuilding the list.
#[derive(Clone)]
struct Msg {
    id: u64,
    role: String,
    text: RwSignal<String>,
}

/// UI-thread message from a running turn (mirrors the TUI's `TurnMsg`).
#[derive(Clone)]
enum UiMsg {
    Event(AgentEvent),
    /// Turn finished; `Some` carries an error string.
    Done(Option<String>),
}

fn main() -> anyhow::Result<()> {
    // A tokio runtime, entered on this (UI) thread so floem's tokio-channel
    // bridge and our per-turn agent tasks can `tokio::spawn`. Held for the
    // program's lifetime.
    let rt = tokio::runtime::Runtime::new()?;
    let _guard = rt.enter();

    let config = AgentConfig::load();
    let agent = Arc::new(TokioMutex::new(Agent::new(config)?));

    floem::launch(move || app_view(agent));
    Ok(())
}

fn app_view(agent: Arc<TokioMutex<Agent>>) -> impl IntoView {
    let messages: RwSignal<Vec<Msg>> = create_rw_signal(Vec::new());
    let input = create_rw_signal(String::new());
    let running = create_rw_signal(false);
    let next_id = create_rw_signal(0u64);

    // One long-lived channel bridges background turns → the UI thread.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<UiMsg>();
    let events = create_signal_from_tokio_channel(rx);
    create_effect(move |_| {
        let Some(msg) = events.get() else { return };
        match msg {
            UiMsg::Event(AgentEvent::Text(t)) => {
                if let Some(sig) = messages.with_untracked(|m| m.last().map(|msg| msg.text)) {
                    sig.update(|s| s.push_str(&t));
                }
            }
            UiMsg::Event(_) => {}
            UiMsg::Done(err) => {
                running.set(false);
                if let Some(e) = err {
                    push_msg(messages, next_id, "error", e);
                }
            }
        }
    });

    let send = move || {
        let text = input.get();
        if text.trim().is_empty() || running.get() {
            return;
        }
        input.set(String::new());
        push_msg(messages, next_id, "you", text.clone());
        // Empty assistant message to stream tokens into.
        push_msg(messages, next_id, "assistant", String::new());
        running.set(true);

        let agent = agent.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let tx_ev = tx.clone();
            let result = agent
                .lock()
                .await
                .run(text, move |ev| {
                    let _ = tx_ev.send(UiMsg::Event(ev));
                })
                .await;
            let _ = tx.send(UiMsg::Done(result.err().map(|e| e.to_string())));
        });
    };

    let transcript = scroll(
        dyn_stack(
            move || messages.get(),
            |m: &Msg| m.id,
            |m: Msg| {
                let role = m.role.clone();
                let text_sig = m.text;
                v_stack((
                    text(role).style(|s| s.font_bold().margin_bottom(2.0)),
                    label(move || text_sig.get()),
                ))
                .style(|s| s.margin_bottom(10.0))
            },
        )
        .style(|s| s.flex_col().width_full()),
    )
    .style(|s| s.flex_grow(1.0).width_full().padding(8.0));

    let input_row = h_stack((
        text_input(input).style(|s| s.flex_grow(1.0).padding(6.0)),
        button("Send").on_click_stop(move |_| send()),
    ))
    .style(|s| s.width_full().gap(6.0).padding(8.0));

    v_stack((transcript, input_row)).style(|s| s.width_full().height_full())
}

/// Append a message with a fresh id.
fn push_msg(
    messages: RwSignal<Vec<Msg>>,
    next_id: RwSignal<u64>,
    role: &str,
    text: impl Into<String>,
) {
    let id = next_id.get();
    next_id.set(id + 1);
    let msg = Msg {
        id,
        role: role.to_string(),
        text: create_rw_signal(text.into()),
    };
    messages.update(|m| m.push(msg));
}
