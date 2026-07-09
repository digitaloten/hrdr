//! Shared slash-command layer. The command *implementations* live here, behind
//! the [`CommandHost`] trait, so every frontend drives the exact same logic and
//! gains new commands for free — a frontend just implements the host
//! capabilities (emit a line, access the agent, clipboard, sessions, …) and
//! calls [`dispatch`]. Frontend-coupled commands (scrolling, find/goto, expand,
//! theme/timestamps, editor) stay in the frontends and are handled before
//! delegating here.
//!
//! Async work (network, subprocess, filesystem, agent lock) is expressed as a
//! [`LineFuture`] the host spawns; its returned string (if non-empty) is shown
//! as a system line. This keeps the layer uniform across a sync-polled frontend
//! (the TUI) and an async-locked one, which both hold the agent as
//! `Arc<tokio::sync::Mutex>`.

mod compaction;
mod conversation;
mod dispatch;
mod helpers;
mod host;
mod model;
mod types;

pub use compaction::*;
pub use conversation::*;
pub use dispatch::*;
pub use helpers::*;
pub use host::*;
pub use model::*;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;
    use hrdr_agent::Message;

    #[test]
    fn unreachable_guidance_shows_setup_paths() {
        let msg = unreachable_guidance("http://localhost:8080/v1", "connection refused");
        // Names the failed endpoint + the underlying error…
        assert!(msg.contains("http://localhost:8080/v1"));
        assert!(msg.contains("connection refused"));
        // …then points at both a local server and the /login wizard.
        assert!(msg.contains("infr serve"));
        assert!(msg.contains("llama-server"));
        assert!(msg.contains("/login"));
    }

    #[test]
    fn auto_compact_threshold_and_messages() {
        // Reserved 200 → fires at window − reserved = 800. Below/at/over,
        // disabled, and missing inputs.
        assert!(should_auto_compact(Some(800), Some(1000), 200, true));
        assert!(should_auto_compact(Some(950), Some(1000), 200, true));
        assert!(!should_auto_compact(Some(799), Some(1000), 200, true));
        assert!(!should_auto_compact(Some(999), Some(1000), 200, false)); // disabled
        assert!(!should_auto_compact(None, Some(1000), 200, true));
        assert!(!should_auto_compact(Some(999), None, 200, true));
        // Reserved larger than a quarter-window is clamped, so the trigger
        // never collapses to 0: with window 1000 it fires at 75% (750), not 0.
        assert!(should_auto_compact(Some(750), Some(1000), 5000, true));
        assert!(!should_auto_compact(Some(749), Some(1000), 5000, true));
        // Message formatting covers the three outcomes.
        assert_eq!(compaction_message(&Ok((2, 2))), "nothing to compact yet");
        assert!(compaction_message(&Ok((10, 2))).contains("compacted: 10 → 2"));
        assert!(compaction_message(&Err("boom".into())).contains("[compact failed] boom"));
    }

    #[test]
    fn conversation_export_covers_only_user_assistant() {
        let msgs = vec![
            Message::user("hello"),
            Message::assistant("hi there"),
            Message::assistant(""), // empty assistant (tool-call turn) skipped
        ];
        let md = conversation_to_markdown(&msgs);
        assert!(md.contains("## User\nhello"));
        assert!(md.contains("## Assistant\nhi there"));
        assert!(!md.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(&conversation_to_json(&msgs)).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[1]["role"], "assistant");
    }
}
