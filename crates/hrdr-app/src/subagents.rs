//! Representation-independent sub-agent panel model shared by hrdr's frontends.
//!
//! [`SubAgentPanel`] maintains the live list of running blocking `task`
//! sub-agents, updated via its event-fold methods as `ToolStart`/`ToolOutput`/
//! `ToolEnd` events arrive. [`panel_items`] merges the blocking list with
//! detached background tasks from the shared registry to produce a unified
//! [`Vec<PanelItem>`] ready for rendering by any frontend.

use std::collections::HashSet;
use std::sync::Mutex;

use hrdr_tools::BackgroundTask;

/// A running blocking `task` sub-agent, shown live in the sub-agent panel until
/// it finishes. `log` is the full streamed progress/output; the panel shows
/// the tail collapsed, all of it expanded.
#[derive(Default, Clone)]
pub struct SubAgentLog {
    /// The task tool-call id (matches the `ToolOutput`/`ToolEnd` id).
    pub id: String,
    /// Accumulated live output (starts with the `↳ task …` header line).
    pub log: String,
    /// Whether the panel shows this agent's full log (toggled by a click/tap).
    pub expanded: bool,
}

/// Stateful holder for the live list of blocking sub-agents, updated by the
/// event-fold methods as `ToolStart`/`ToolOutput`/`ToolEnd` events arrive.
#[derive(Default)]
pub struct SubAgentPanel {
    /// Live blocking sub-agents in arrival order.
    pub agents: Vec<SubAgentLog>,
}

impl SubAgentPanel {
    /// A `task` tool call started: push a new live entry.
    pub fn on_tool_start(&mut self, id: String) {
        self.agents.push(SubAgentLog {
            id,
            log: String::new(),
            expanded: false,
        });
    }

    /// Streamed output chunk for `id`: append to the matching entry's log.
    pub fn on_tool_output(&mut self, id: &str, chunk: &str) {
        if let Some(sa) = self.agents.iter_mut().find(|s| s.id == id) {
            sa.log.push_str(chunk);
        }
    }

    /// A `task` tool call ended: remove it from the live panel (its result is
    /// now in the transcript entry).
    pub fn on_tool_end(&mut self, id: &str) {
        self.agents.retain(|s| s.id != id);
    }

    /// Clear all live entries (e.g. at turn end, in case an interrupted turn
    /// left entries without a matching `ToolEnd`).
    pub fn clear(&mut self) {
        self.agents.clear();
    }

    /// Toggle the expanded state of the entry at `idx` (panel row click).
    pub fn toggle(&mut self, idx: usize) {
        if let Some(sa) = self.agents.get_mut(idx) {
            sa.expanded = !sa.expanded;
        }
    }
}

/// What a click on a sub-agent panel row targets: a blocking sub-agent (by
/// index in [`SubAgentPanel::agents`]) or a detached background task (by its
/// registry id).
#[derive(Clone, Copy)]
pub enum PanelHit {
    /// Index into [`SubAgentPanel::agents`].
    Blocking(usize),
    /// Registry id of a detached background task.
    Background(u64),
}

/// One row in the sub-agent panel: a blocking sub-agent or a detached
/// background task, unified for rendering.
#[derive(Clone)]
pub struct PanelItem {
    /// First line of the log, used as the panel row title.
    pub title: String,
    /// Full log text; body lines are shown tail-first when collapsed.
    pub log: String,
    /// Whether this row is currently expanded to show the full log.
    pub expanded: bool,
    /// `true` for a finished background task (renders with a completion marker).
    pub done: bool,
    /// What a click on this row targets (for toggle and hit-testing).
    pub hit: PanelHit,
}

/// Tail lines shown per collapsed sub-agent entry in the panel.
pub const SUBAGENT_TAIL_LINES: usize = 4;

/// Content rows one panel item occupies: a header line plus body lines (all
/// when expanded, else the last [`SUBAGENT_TAIL_LINES`]). Kept in sync with
/// [`panel_item_body`] without allocating the lines.
pub fn panel_item_rows(item: &PanelItem) -> usize {
    let rest = item.log.lines().count().saturating_sub(1);
    let body = if item.expanded {
        rest
    } else {
        rest.min(SUBAGENT_TAIL_LINES)
    };
    1 + body
}

/// Header line for one panel row: expansion indicator (`▸`/`▾`), completion
/// badge (`✓ ` when done), then the title.
pub fn panel_item_header(item: &PanelItem) -> String {
    let indicator = if item.expanded { "▾" } else { "▸" };
    let badge = if item.done { "✓ " } else { "" };
    format!("{indicator} {badge}{}", item.title)
}

/// Body lines for one panel row (the log minus its header line), each trimmed
/// of trailing whitespace: all of them when expanded, else the last
/// [`SUBAGENT_TAIL_LINES`] (matching [`panel_item_rows`]).
pub fn panel_item_body(item: &PanelItem) -> Vec<String> {
    let rest: Vec<&str> = item.log.lines().skip(1).collect();
    let shown = if item.expanded {
        &rest[..]
    } else {
        &rest[rest.len().saturating_sub(SUBAGENT_TAIL_LINES)..]
    };
    shown.iter().map(|l| l.trim_end().to_string()).collect()
}

/// Apply a click on a panel row: toggle a blocking sub-agent's expansion in
/// `panel`, or flip a background task's id in the `background_expanded` set.
pub fn toggle_panel_hit(
    panel: &mut SubAgentPanel,
    background_expanded: &mut HashSet<u64>,
    hit: PanelHit,
) {
    match hit {
        PanelHit::Blocking(idx) => panel.toggle(idx),
        PanelHit::Background(id) => {
            if !background_expanded.remove(&id) {
                background_expanded.insert(id);
            }
        }
    }
}

/// Collect the panel's rows: blocking sub-agents from `agents` followed by
/// detached background tasks from the shared registry. Background tasks that
/// are done have their result appended to the log as `\n[result] {r}`.
pub fn panel_items(
    agents: &[SubAgentLog],
    background: &Mutex<Vec<BackgroundTask>>,
    background_expanded: &HashSet<u64>,
) -> Vec<PanelItem> {
    let mut items = Vec::new();
    for (i, sa) in agents.iter().enumerate() {
        items.push(PanelItem {
            title: sa
                .log
                .lines()
                .next()
                .unwrap_or("sub-agent…")
                .trim()
                .to_string(),
            log: sa.log.clone(),
            expanded: sa.expanded,
            done: false,
            hit: PanelHit::Blocking(i),
        });
    }
    if let Ok(v) = background.lock() {
        for t in v.iter() {
            let mut log = t.log.clone();
            if t.done
                && let Some(r) = &t.result
            {
                log.push_str(&format!("\n[result] {r}"));
            }
            items.push(PanelItem {
                title: t.log.lines().next().unwrap_or(&t.label).trim().to_string(),
                log,
                expanded: background_expanded.contains(&t.id),
                done: t.done,
                hit: PanelHit::Background(t.id),
            });
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrdr_tools::BackgroundTask;

    fn make_task(
        id: u64,
        label: &str,
        log: &str,
        done: bool,
        result: Option<&str>,
    ) -> BackgroundTask {
        BackgroundTask {
            id,
            label: label.to_string(),
            log: log.to_string(),
            done,
            result: result.map(str::to_string),
            delivered: false,
        }
    }

    #[test]
    fn event_fold_lifecycle() {
        let mut panel = SubAgentPanel::default();
        // Start two agents.
        panel.on_tool_start("id1".to_string());
        panel.on_tool_start("id2".to_string());
        assert_eq!(panel.agents.len(), 2);
        // Stream output to the first.
        panel.on_tool_output("id1", "header line\nsecond line");
        assert_eq!(panel.agents[0].log, "header line\nsecond line");
        // End the first: removed from the live list.
        panel.on_tool_end("id1");
        assert_eq!(panel.agents.len(), 1);
        assert_eq!(panel.agents[0].id, "id2");
        // Clear on turn end.
        panel.clear();
        assert!(panel.agents.is_empty());
    }

    #[test]
    fn panel_items_merges_blocking_and_background() {
        let mut panel = SubAgentPanel::default();
        panel.on_tool_start("block1".to_string());
        panel.on_tool_output("block1", "task: do thing\nrunning…");

        let bg = Mutex::new(vec![make_task(
            10,
            "bg-label",
            "bg task log",
            true,
            Some("done ok"),
        )]);
        let mut expanded = HashSet::new();
        expanded.insert(10u64);

        let items = panel_items(&panel.agents, &bg, &expanded);
        assert_eq!(items.len(), 2);
        // Blocking item first.
        assert_eq!(items[0].title, "task: do thing");
        assert!(!items[0].done);
        assert!(matches!(items[0].hit, PanelHit::Blocking(0)));
        // Background item second: done, appended result, expanded.
        assert!(items[1].log.contains("[result] done ok"));
        assert!(items[1].done);
        assert!(items[1].expanded);
        assert!(matches!(items[1].hit, PanelHit::Background(10)));
    }

    #[test]
    fn panel_item_rows_collapsed_vs_expanded() {
        // 6-line log: 1 header + 5 body lines.
        let log = "line0\nline1\nline2\nline3\nline4\nline5".to_string();
        let collapsed = PanelItem {
            title: "head".to_string(),
            log: log.clone(),
            expanded: false,
            done: false,
            hit: PanelHit::Blocking(0),
        };
        // 1 header + min(5 body, SUBAGENT_TAIL_LINES=4) = 5.
        assert_eq!(panel_item_rows(&collapsed), 5);

        let expanded = PanelItem {
            expanded: true,
            ..collapsed
        };
        // 1 header + 5 body = 6.
        assert_eq!(panel_item_rows(&expanded), 6);
    }

    #[test]
    fn toggle_flips_expanded() {
        let mut panel = SubAgentPanel::default();
        panel.on_tool_start("x".to_string());
        assert!(!panel.agents[0].expanded);
        panel.toggle(0);
        assert!(panel.agents[0].expanded);
        panel.toggle(0);
        assert!(!panel.agents[0].expanded);
        // Out-of-bounds toggle is a no-op.
        panel.toggle(99);
    }
}
