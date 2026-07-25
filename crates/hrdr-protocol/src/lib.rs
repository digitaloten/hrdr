//! Wire types for hrdr's web protocol. The server and every client (browser
//! WASM, a native webview) share this crate so the WS frames and REST payloads
//! are a single source of truth.
//!
//! This crate is kept `wasm32`-clean on purpose: it depends only on serde, so
//! it compiles in the browser (where `hrdr-agent` with its tokio/reqwest/zstd
//! deps cannot).

#[cfg(test)]
extern crate hrdr_test_support;

use serde::{Deserialize, Serialize};

// ── pane identity ──────────────────────────────────────────────────────────

/// Which conversation a message concerns. Mirrors `hrdr_agent::PaneId`.
/// External tagging on purpose: serializes as `"main"` or `{"sub": 7}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WirePaneId {
    Main,
    Sub(u64),
}

// ── transcript entry (byte-for-byte mirror of hrdr_agent::Entry JSON) ──────

/// Byte-for-byte the JSON of `hrdr_agent::Entry`: flat kind/data + unix time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireEntry {
    #[serde(flatten)]
    pub kind: WireEntryKind,
    /// Unix seconds — `Entry` serializes `DateTime<Local>` this way.
    pub time: i64,
}

/// Matches `hrdr_agent::EntryKind`'s serde shape exactly. This is the one
/// exception to "struct variants only" — it must mirror the externally-shaped
/// serde, newtype variants included, so the round-trip test passes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum WireEntryKind {
    Header,
    User(String),
    Assistant(String),
    Reasoning {
        text: String,
        #[serde(default)]
        took_ms: Option<u64>,
    },
    Tool {
        id: String,
        name: String,
        args: String,
        result: String,
        ok: bool,
        done: bool,
    },
    System(String),
    Notice(String),
    Stats(String),
    Diff(String),
}

// ── entry view (server-computed display model) ─────────────────────────────

/// Server-computed display model that rides beside an entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireEntryView {
    pub entry: WireEntry,
    /// For Tool entries: `hrdr_agent::tool_display(name, args)`, converted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<WireToolDisplay>,
    /// For Diff entries AND Tool entries whose body is Diff: each line of the
    /// diff text classified by `hrdr_app::classify_diff_line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_lines: Option<Vec<WireDiffLine>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireToolDisplay {
    pub headline: String,
    pub body: WireToolBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireToolBody {
    Shell { command: String },
    Code { lang: String, content: String },
    Diff {},
    Read {},
    Details { rows: Vec<(String, String)> },
    Text {},
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireDiffLine {
    pub kind: WireDiffLineKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireDiffLineKind {
    Hunk,
    Add,
    Remove,
    Meta,
}

// ── pane chrome ────────────────────────────────────────────────────────────

/// Snapshot of one pane's chrome (list row + status-bar inputs live here).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WirePane {
    pub id: WirePaneId,
    pub title: String,
    /// Mirrors `hrdr_agent::PaneStatus`.
    pub status: WirePaneStatus,
    pub model: String,
    pub provider: String,
    pub effort: Option<String>,
    /// Queued-but-undelivered user messages.
    pub pending: Vec<String>,
    pub compacting: bool,
    pub turn: WireTurn,
    pub todos: Vec<WireTodo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WirePaneStatus {
    Running,
    Idle,
    Done,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireTodo {
    pub content: String,
    pub status: String,
}

/// A snapshot of one pane's turn clock. Not the agent's live `TurnStats`
/// (which holds `Instant`/`SystemTime` — not serde, not WASM), but the derived
/// numbers the frontend needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireTurn {
    pub running: bool,
    pub inferring: bool,
    /// `TurnStats::infer_elapsed().as_millis()`.
    pub elapsed_ms: u64,
    /// Time-to-first-token in seconds.
    pub ttft_secs: Option<f64>,
    /// Streamed tokens per second of model working time.
    pub tok_per_sec: f64,
    pub out_tokens: usize,
    /// Unix seconds — from `TurnStats::started_at` when present.
    pub started_unix: Option<i64>,
}

// ── status bar ─────────────────────────────────────────────────────────────

/// Pre-built status bar (server ran `hrdr_app::status_sections`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireStatus {
    pub left: Vec<WireStatusSeg>,
    pub right: Vec<WireStatusSeg>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireStatusSeg {
    pub priority: u8,
    pub runs: Vec<WireStatusRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gauge: Option<WireGauge>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireStatusRun {
    pub text: String,
    pub role: WireStatusRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum WireStatusRole {
    Dir {},
    Branch {},
    TokensIn {},
    TokensOut {},
    CtxFill { level: WireCtxLevel },
    CtxRest {},
    CtxPlain {},
    Provider {},
    Model {},
    Effort {},
    Ttft {},
    Session {},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireCtxLevel {
    Ok,
    Warn,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireGauge {
    pub frac: f64,
    pub level: WireCtxLevel,
    pub label: String,
}

// ── messages ───────────────────────────────────────────────────────────────

/// Every server frame carries a global sequence number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerFrame {
    pub seq: u64,
    #[serde(flatten)]
    pub msg: ServerMsg,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// First frame on connect (and after a failed resume): complete state.
    Snapshot {
        session_id: Option<String>,
        session_name: String,
        cwd: String,
        panes: Vec<WirePane>,
        active: WirePaneId,
        status: WireStatus,
        transcripts: Vec<PaneTranscript>,
        show_thinking: bool,
    },
    /// Replace pane's entries from index `from` to the end with `entries`.
    /// `from == 0` with empty `entries` = the transcript was cleared (/new).
    Entries {
        pane: WirePaneId,
        from: usize,
        entries: Vec<WireEntryView>,
    },
    /// Pane list / chrome changed (panes added, released, status, turn, todos).
    Panes {
        panes: Vec<WirePane>,
        active: WirePaneId,
    },
    Status {
        status: WireStatus,
    },
    /// A system line produced outside the fold (async command output).
    Notice {
        text: String,
    },
    /// The server asks the client to replace/augment its input box
    /// (`CommandHost::set_input` / `prepend_input` / `insert_input`).
    SetInput {
        mode: InputSetMode,
        text: String,
    },
    /// Resume accepted: client state is current up to `seq`; deltas follow.
    Resumed {},
    /// Auth failed / connection refused; the socket closes after this.
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputSetMode {
    Replace,
    Prepend,
    InsertAtCursor,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneTranscript {
    pub pane: WirePaneId,
    pub entries: Vec<WireEntryView>,
}

// ── client messages ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// User pressed send. Routed via `LiveSubagents::send_prompt` (steer if a
    /// turn is in flight, new turn if idle).
    Submit {
        pane: WirePaneId,
        text: String,
    },
    /// A slash command line (leading '/'). Runs through `hrdr_app` dispatch.
    Command {
        pane: WirePaneId,
        line: String,
    },
    /// Cancel the active turn on `pane` (abort task + clear_pending).
    Cancel {
        pane: WirePaneId,
    },
    SwitchPane {
        pane: WirePaneId,
    },
    /// Reconnect: client last saw `seq`. Server replays buffered frames after
    /// `seq`, or sends a fresh `Snapshot` if the buffer no longer reaches back.
    Resume {
        seq: u64,
    },
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Every JSON example from §4 of the web-ui-plan must parse and
    /// re-serialize to the same value.
    #[test]
    fn wire_examples_round_trip() {
        let examples: Vec<(&str, &str)> = vec![
            // Snapshot (abridged).
            (
                "snapshot",
                r#"{"seq":1,"type":"snapshot","session_id":"fix-parser","session_name":"fix parser","cwd":"/home/me/proj","panes":[{"id":"main","title":"main","status":"idle","model":"gpt-5.5","provider":"openai","effort":null,"pending":[],"compacting":false,"turn":{"running":false,"inferring":false,"elapsed_ms":0,"ttft_secs":null,"tok_per_sec":0.0,"out_tokens":0,"started_unix":null},"todos":[]}],"active":"main","status":{"left":[],"right":[]},"transcripts":[{"pane":"main","entries":[{"entry":{"kind":"user","data":"hi","time":1753500000}}]}],"show_thinking":true}"#,
            ),
            // Entries delta.
            (
                "entries",
                r#"{"seq":42,"type":"entries","pane":"main","from":3,"entries":[{"entry":{"kind":"assistant","data":"Done — it was an off-by-one.","time":1753500100}}]}"#,
            ),
            // Tool entry with display model.
            (
                "tool_entry",
                r#"{"entry":{"kind":"tool","data":{"id":"c1","name":"shell","args":"{\"command\":\"ls\"}","result":"src\n","ok":true,"done":true},"time":1753500050},"tool":{"headline":"","body":{"type":"shell","command":"ls"}}}"#,
            ),
            // Sub-agent pane id.
            (
                "sub_pane",
                r#"{"type":"submit","pane":{"sub":7},"text":"fix the bug"}"#,
            ),
            // Command.
            (
                "command",
                r#"{"type":"command","pane":{"sub":7},"line":"/status"}"#,
            ),
            // Resume.
            ("resume", r#"{"type":"resume","seq":41}"#),
        ];

        for (name, json) in examples {
            // Parse as generic Value, then re-serialize, then compare.
            let v: serde_json::Value =
                serde_json::from_str(json).unwrap_or_else(|_| panic!("{name}: parse failed"));
            let round = serde_json::to_string(&v).unwrap();
            let v2: serde_json::Value = serde_json::from_str(&round).unwrap();
            assert_eq!(v, v2, "{name}: round-trip mismatch");
        }
    }

    /// `PaneId` wire shape: `Main` ⇄ `"main"`, `Sub(7)` ⇄ `{"sub":7}`.
    #[test]
    fn pane_id_wire_shape() {
        // Main
        let main: WirePaneId = serde_json::from_str(r#""main""#).unwrap();
        assert_eq!(main, WirePaneId::Main);
        let back = serde_json::to_string(&WirePaneId::Main).unwrap();
        assert_eq!(back, r#""main""#);

        // Sub(7)
        let sub7: WirePaneId = serde_json::from_str(r#"{"sub":7}"#).unwrap();
        assert_eq!(sub7, WirePaneId::Sub(7));
        let back = serde_json::to_string(&WirePaneId::Sub(7)).unwrap();
        assert_eq!(back, r#"{"sub":7}"#);
    }

    /// A `ServerFrame` with `seq` and a flattened `ServerMsg` serializes
    /// with `seq` at the top level next to `type`.
    #[test]
    fn server_frame_flattens_seq() {
        let frame = ServerFrame {
            seq: 1,
            msg: ServerMsg::Notice {
                text: "hello".into(),
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["seq"], serde_json::json!(1));
        assert_eq!(v["type"], serde_json::json!("notice"));
        assert_eq!(v["text"], serde_json::json!("hello"));

        // Round-trip through ServerFrame.
        let parsed: ServerFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, frame);
    }

    /// `ClientMsg` round-trips for every variant.
    #[test]
    fn client_msg_round_trip() {
        let cases = vec![
            ClientMsg::Submit {
                pane: WirePaneId::Main,
                text: "hi".into(),
            },
            ClientMsg::Command {
                pane: WirePaneId::Sub(9),
                line: "/status".into(),
            },
            ClientMsg::Cancel {
                pane: WirePaneId::Main,
            },
            ClientMsg::SwitchPane {
                pane: WirePaneId::Sub(3),
            },
            ClientMsg::Resume { seq: 42 },
        ];
        for msg in cases {
            let json = serde_json::to_string(&msg).unwrap();
            let back: ClientMsg = serde_json::from_str(&json).unwrap();
            assert_eq!(msg, back, "round-trip: {json}");
        }
    }
}
