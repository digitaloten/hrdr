//! Conversion from core types (`hrdr_agent`, `hrdr_app`) to wire types
//! (`hrdr_protocol`). One-way: the web server converts core → wire; the WASM
//! client never sees the core types.
//!
//! Every converter is a pure function that clones the data it needs, so the
//! caller holds no locks across the conversion and the wire types are always
//! self-contained.

use hrdr_agent::{Entry, EntryKind, ToolBody, TurnStats};
use hrdr_app::{
    CtxGauge, CtxLevel, StatusInputs, StatusRole, StatusRun, StatusSeg, classify_diff_line,
};
use hrdr_protocol::{
    InputSetMode, PaneTranscript, ServerFrame, ServerMsg, WireApprovalDecision, WireCtxLevel,
    WireDiffLine, WireDiffLineKind, WireEntry, WireEntryKind, WireEntryView, WireGauge, WirePane,
    WirePaneId, WirePaneStatus, WireStatus, WireStatusRole, WireStatusRun, WireStatusSeg, WireTodo,
    WireToolBody, WireToolDisplay, WireTurn,
};

use hrdr_agent::{Pane, PaneId, PaneStatus};

// ── entry ──────────────────────────────────────────────────────────────────

/// Convert a core `Entry` to a `WireEntry`. The wire types mirror `Entry`'s
/// serde shape byte-for-byte, so the JSON round-trip test in the session
/// module pins this.
pub fn wire_entry(entry: &Entry) -> WireEntry {
    WireEntry {
        kind: wire_entry_kind(&entry.kind),
        time: entry.time.timestamp(),
    }
}

fn wire_entry_kind(kind: &EntryKind) -> WireEntryKind {
    match kind {
        EntryKind::Header => WireEntryKind::Header,
        EntryKind::User(s) => WireEntryKind::User(s.clone()),
        EntryKind::Assistant(s) => WireEntryKind::Assistant(s.clone()),
        EntryKind::Reasoning { text, took_ms } => WireEntryKind::Reasoning {
            text: text.clone(),
            took_ms: *took_ms,
        },
        EntryKind::Tool {
            id,
            name,
            args,
            result,
            ok,
            done,
            ..
        } => WireEntryKind::Tool {
            id: id.clone(),
            name: name.clone(),
            args: args.clone(),
            result: result.clone(),
            ok: *ok,
            done: *done,
        },
        EntryKind::System(s) => WireEntryKind::System(s.clone()),
        EntryKind::Notice(s) => WireEntryKind::Notice(s.clone()),
        EntryKind::Stats(s) => WireEntryKind::Stats(s.clone()),
        EntryKind::Diff(s) => WireEntryKind::Diff(s.clone()),
    }
}

/// Build the full display model for one entry: core entry → wire entry, plus
/// tool classification and diff-line coloring when applicable.
pub fn wire_entry_view(entry: &Entry) -> WireEntryView {
    let wire = wire_entry(entry);
    let tool = tool_display_for(entry);
    let diff_lines = diff_lines_for(entry, tool.as_ref());
    WireEntryView {
        entry: wire,
        tool,
        diff_lines,
    }
}

/// Classify a Tool entry via `hrdr_agent::tool_display` and convert to wire.
fn tool_display_for(entry: &Entry) -> Option<WireToolDisplay> {
    match &entry.kind {
        EntryKind::Tool { name, args, .. } => {
            let td = hrdr_agent::tool_display(name, args);
            Some(WireToolDisplay {
                headline: td.headline,
                body: wire_tool_body(td.body),
            })
        }
        _ => None,
    }
}

fn wire_tool_body(body: ToolBody) -> WireToolBody {
    match body {
        ToolBody::Shell { command } => WireToolBody::Shell { command },
        ToolBody::Code { lang, content } => WireToolBody::Code { lang, content },
        ToolBody::Diff => WireToolBody::Diff {},
        ToolBody::Read => WireToolBody::Read {},
        ToolBody::Details(rows) => WireToolBody::Details { rows },
        ToolBody::Text => WireToolBody::Text {},
    }
}

/// Classify every line of a Diff entry (or a done Tool whose body is Diff).
fn diff_lines_for(entry: &Entry, _tool: Option<&WireToolDisplay>) -> Option<Vec<WireDiffLine>> {
    // Tool entries whose body is Diff AND done: classify the result.
    if let EntryKind::Tool {
        result, done, name, ..
    } = &entry.kind
        && *done
    {
        let display = hrdr_agent::tool_display(name, "");
        if matches!(display.body, ToolBody::Diff) {
            return Some(classify_lines(result));
        }
    }

    // Diff entries: classify the text.
    if let EntryKind::Diff(text) = &entry.kind {
        return Some(classify_lines(text));
    }

    None
}

fn classify_lines(text: &str) -> Vec<WireDiffLine> {
    text.lines()
        .map(|line| WireDiffLine {
            kind: wire_diff_kind(classify_diff_line(line)),
            text: line.to_string(),
        })
        .collect()
}

fn wire_diff_kind(k: hrdr_app::DiffLineKind) -> WireDiffLineKind {
    match k {
        hrdr_app::DiffLineKind::Hunk => WireDiffLineKind::Hunk,
        hrdr_app::DiffLineKind::Add => WireDiffLineKind::Add,
        hrdr_app::DiffLineKind::Remove => WireDiffLineKind::Remove,
        hrdr_app::DiffLineKind::Meta => WireDiffLineKind::Meta,
    }
}

// ── pane ───────────────────────────────────────────────────────────────────

/// Convert a core `Pane` to a `WirePane`. `running` is whether a turn is
/// in flight on this pane (the caller knows this from the live registry).
pub fn wire_pane(pane: &Pane, running: bool) -> WirePane {
    WirePane {
        id: wire_pane_id(pane.id),
        title: pane.title().to_string(),
        status: wire_pane_status(pane.status),
        model: pane.model().to_string(),
        provider: pane.provider().to_string(),
        effort: pane.effort.clone(),
        pending: pane.pending.clone(),
        compacting: pane.compacting,
        turn: wire_turn(&pane.turn, running),
        todos: wire_todos(pane),
    }
}

fn wire_todos(pane: &Pane) -> Vec<WireTodo> {
    pane.todos
        .lock()
        .map(|t| {
            t.iter()
                .map(|item| WireTodo {
                    content: item.content.clone(),
                    status: item.status.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn wire_pane_status(status: PaneStatus) -> WirePaneStatus {
    match status {
        PaneStatus::Running => WirePaneStatus::Running,
        PaneStatus::Idle => WirePaneStatus::Idle,
        PaneStatus::Done => WirePaneStatus::Done,
    }
}

/// Convert a core `TurnStats` to `WireTurn`. Caller passes `running` from the
/// pane's status (is a turn in flight right now).
pub fn wire_turn(stats: &TurnStats, running: bool) -> WireTurn {
    WireTurn {
        running,
        inferring: stats.inferring(),
        elapsed_ms: stats.infer_elapsed().as_millis() as u64,
        ttft_secs: stats.ttft(),
        tok_per_sec: stats.tok_per_sec(),
        out_tokens: stats.out_tokens(),
        started_unix: stats.started_at.map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64
        }),
    }
}

// ── pane id ────────────────────────────────────────────────────────────────

pub fn wire_pane_id(id: PaneId) -> WirePaneId {
    match id {
        PaneId::MAIN => WirePaneId::Main,
        PaneId(k) => WirePaneId::Sub(k),
    }
}

pub fn core_pane_id(id: WirePaneId) -> PaneId {
    match id {
        WirePaneId::Main => PaneId::MAIN,
        WirePaneId::Sub(k) => PaneId(k),
    }
}

// ── status ─────────────────────────────────────────────────────────────────

/// Convert status-bar inputs + built sections to wire.
pub fn wire_status(inputs: &StatusInputs) -> WireStatus {
    let left = hrdr_app::status_sections(inputs);
    let right = hrdr_app::status_right_sections(inputs);
    WireStatus {
        left: left.into_iter().map(wire_status_seg).collect(),
        right: right.into_iter().map(wire_status_seg).collect(),
    }
}

fn wire_status_seg(seg: StatusSeg) -> WireStatusSeg {
    WireStatusSeg {
        priority: seg.priority,
        runs: seg.runs.into_iter().map(wire_status_run).collect(),
        gauge: seg.gauge.map(wire_gauge),
    }
}

fn wire_status_run(run: StatusRun) -> WireStatusRun {
    WireStatusRun {
        text: run.text,
        role: wire_status_role(run.role),
    }
}

fn wire_status_role(role: StatusRole) -> WireStatusRole {
    match role {
        StatusRole::Dir => WireStatusRole::Dir {},
        StatusRole::Branch => WireStatusRole::Branch {},
        StatusRole::TokensIn => WireStatusRole::TokensIn {},
        StatusRole::TokensOut => WireStatusRole::TokensOut {},
        StatusRole::CtxFill(level) => WireStatusRole::CtxFill {
            level: wire_ctx_level(level),
        },
        StatusRole::CtxRest => WireStatusRole::CtxRest {},
        StatusRole::CtxPlain => WireStatusRole::CtxPlain {},
        StatusRole::Provider => WireStatusRole::Provider {},
        StatusRole::Model => WireStatusRole::Model {},
        StatusRole::Effort => WireStatusRole::Effort {},
        StatusRole::Ttft => WireStatusRole::Ttft {},
        StatusRole::Session => WireStatusRole::Session {},
    }
}

fn wire_ctx_level(level: CtxLevel) -> WireCtxLevel {
    match level {
        CtxLevel::Ok => WireCtxLevel::Ok,
        CtxLevel::Warn => WireCtxLevel::Warn,
        CtxLevel::Critical => WireCtxLevel::Critical,
    }
}

fn wire_gauge(gauge: CtxGauge) -> WireGauge {
    WireGauge {
        frac: gauge.frac,
        level: wire_ctx_level(gauge.level),
        label: gauge.label,
    }
}

// ── convenience: full snapshot builder ──────────────────────────────────────

/// Build a `ServerMsg::Snapshot` from the live panes and status.
#[allow(clippy::too_many_arguments)]
pub fn build_snapshot(
    seq: u64,
    session_id: Option<String>,
    session_name: String,
    cwd: String,
    panes: &[WirePane],
    active: WirePaneId,
    status: WireStatus,
    transcripts: Vec<PaneTranscript>,
    show_thinking: bool,
) -> ServerFrame {
    ServerFrame {
        seq,
        msg: ServerMsg::Snapshot {
            session_id,
            session_name,
            cwd,
            panes: panes.to_vec(),
            active,
            status,
            transcripts,
            show_thinking,
        },
    }
}

/// Build a `ServerMsg::Entries` frame.
pub fn build_entries(
    seq: u64,
    pane: WirePaneId,
    from: usize,
    entries: Vec<WireEntryView>,
) -> ServerFrame {
    ServerFrame {
        seq,
        msg: ServerMsg::Entries {
            pane,
            from,
            entries,
        },
    }
}

/// Build a `ServerMsg::Panes` frame.
pub fn build_panes(seq: u64, panes: Vec<WirePane>, active: WirePaneId) -> ServerFrame {
    ServerFrame {
        seq,
        msg: ServerMsg::Panes { panes, active },
    }
}

/// Build a `ServerMsg::Status` frame.
pub fn build_status(seq: u64, status: WireStatus) -> ServerFrame {
    ServerFrame {
        seq,
        msg: ServerMsg::Status { status },
    }
}

/// Build a `ServerMsg::Notice` frame.
pub fn build_notice(seq: u64, text: String) -> ServerFrame {
    ServerFrame {
        seq,
        msg: ServerMsg::Notice { text },
    }
}

/// Build a `ServerMsg::SetInput` frame.
pub fn build_set_input(seq: u64, mode: InputSetMode, text: String) -> ServerFrame {
    ServerFrame {
        seq,
        msg: ServerMsg::SetInput { mode, text },
    }
}

// ── escalation approvals ───────────────────────────────────────────────────

/// Build a `ServerMsg::ApprovalRequested` frame.
///
/// The command is moved through untouched — no truncation, no normalization,
/// no eliding for the wire. What the user is asked to allow has to be what
/// runs, and this is the only place the two could drift apart.
pub fn build_approval_requested(
    seq: u64,
    id: String,
    command: String,
    reason: String,
    rules: Vec<String>,
) -> ServerFrame {
    ServerFrame {
        seq,
        msg: ServerMsg::ApprovalRequested {
            id,
            command,
            reason,
            rules,
        },
    }
}

/// Build a `ServerMsg::ApprovalClosed` frame.
pub fn build_approval_closed(seq: u64, id: String) -> ServerFrame {
    ServerFrame {
        seq,
        msg: ServerMsg::ApprovalClosed { id },
    }
}

/// Wire decision → the gate's own enum.
pub fn core_approval_decision(d: WireApprovalDecision) -> hrdr_tools::ApprovalDecision {
    match d {
        WireApprovalDecision::Once => hrdr_tools::ApprovalDecision::Once,
        WireApprovalDecision::Session => hrdr_tools::ApprovalDecision::Session,
        WireApprovalDecision::Deny => hrdr_tools::ApprovalDecision::Deny,
    }
}
