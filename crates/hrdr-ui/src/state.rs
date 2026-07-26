//! Pure frame→state reducer for the Dioxus client. No DOM, no WASM — just
//! the logic that turns `ServerFrame` messages into a `Vec<ViewEntry>`.
//!
//! Host-runnable unit tests live at the bottom — `cargo test` inside
//! `crates/hrdr-ui` on the host target must pass.

use hrdr_protocol::{ServerFrame, ServerMsg};

/// One rendered entry in the client-side transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewEntry {
    /// The CSS class suffix (e.g. "user", "assistant", "tool").
    css_class: String,
    /// HTML content to render.
    html: String,
}

impl ViewEntry {
    pub fn css_class(&self) -> &str {
        &self.css_class
    }

    pub fn html(&self) -> &str {
        &self.html
    }
}

/// Apply one `ServerFrame` to the client-side transcript state.
pub fn apply_frame(frame: &ServerFrame, transcript: &mut Vec<ViewEntry>) {
    match &frame.msg {
        ServerMsg::Snapshot { transcripts, .. } => {
            transcript.clear();
            for pt in transcripts {
                for ev in &pt.entries {
                    transcript.push(entry_to_view(ev));
                }
            }
        }
        ServerMsg::Entries {
            from, entries, ..
        } => {
            let from = *from;
            // Truncate from `from` to the end.
            transcript.truncate(from);
            // Append new entries.
            for ev in entries {
                transcript.push(entry_to_view(ev));
            }
        }
        ServerMsg::Resumed {} => {
            // Nothing to do — the frames that follow will update us.
        }
        _ => {
            // Panes, Status, Notice, SetInput, Error — not transcript content.
            // The UI handles these separately via dedicated signals.
        }
    }
}

fn entry_to_view(ev: &hrdr_protocol::WireEntryView) -> ViewEntry {
    let entry = &ev.entry;
    let (css_class, html) = match &entry.kind {
        hrdr_protocol::WireEntryKind::Header => (
            "heading".to_string(),
            "<h1 style=\"text-align:center; color:#e94560;\">hrdr</h1>".to_string(),
        ),
        hrdr_protocol::WireEntryKind::User(text) => (
            "user".to_string(),
            format!(
                "<strong>You</strong> {}",
                html_escape(text)
            ),
        ),
        hrdr_protocol::WireEntryKind::Assistant(text) => {
            ("assistant".to_string(), html_escape(text))
        }
        hrdr_protocol::WireEntryKind::Reasoning { text, took_ms } => {
            let label = match took_ms {
                Some(ms) => format!("Thought for {:.1}s", *ms as f64 / 1000.0),
                None => "Thinking…".to_string(),
            };
            (
                "reasoning".to_string(),
                format!(
                    "<details><summary style=\"color:#666; cursor:pointer;\">{label}</summary><pre style=\"color:#666; font-style:italic; margin-top:0.5rem;\">{}</pre></details>",
                    html_escape(text)
                ),
            )
        }
        hrdr_protocol::WireEntryKind::Tool {
            name,
            args,
            result,
            ok,
            done,
            ..
        } => {
            let status = if !done {
                "⏳".to_string()
            } else if *ok {
                "✓".to_string()
            } else {
                "✗".to_string()
            };
            let tool_display = if let Some(td) = &ev.tool {
                format!("<pre style=\"color:#f0a500; margin:0.25rem 0;\">{}</pre>", html_escape(&td.headline))
            } else {
                String::new()
            };
            let result_html = if *done {
                if let Some(lines) = &ev.diff_lines {
                    // Diff output — colored.
                    let colored: String = lines
                        .iter()
                        .map(|l| {
                            let cls = match l.kind {
                                hrdr_protocol::WireDiffLineKind::Add => "diff-add",
                                hrdr_protocol::WireDiffLineKind::Remove => "diff-remove",
                                hrdr_protocol::WireDiffLineKind::Hunk => "diff-hunk",
                                hrdr_protocol::WireDiffLineKind::Meta => "",
                            };
                            format!("<span class=\"{cls}\">{}</span>\n", html_escape(&l.text))
                        })
                        .collect();
                    format!("<pre style=\"font-family:monospace; margin:0.25rem 0;\">{colored}</pre>")
                } else {
                    format!("<pre style=\"font-family:monospace; margin:0.25rem 0; white-space:pre-wrap;\">{}</pre>", html_escape(result))
                }
            } else {
                String::new()
            };
            (
                "tool".to_string(),
                format!(
                    "<strong style=\"color:#f0a500;\">{status} {name}</strong> {tool_display}{result_html}"
                ),
            )
        }
        hrdr_protocol::WireEntryKind::System(text) => (
            "system".to_string(),
            format!("<em>{}</em>", html_escape(text)),
        ),
        hrdr_protocol::WireEntryKind::Notice(text) => (
            "system".to_string(),
            format!("<em style=\"color:#888;\">{}</em>", html_escape(text)),
        ),
        hrdr_protocol::WireEntryKind::Stats(text) => (
            "system".to_string(),
            format!("<small style=\"color:#888;\">{}</small>", html_escape(text)),
        ),
        hrdr_protocol::WireEntryKind::Diff(text) => {
            let colored: String = text
                .lines()
                .map(|line| {
                    let cls = classify_line(line);
                    format!("<span class=\"{cls}\">{}</span>\n", html_escape(line))
                })
                .collect();
            (
                "diff".to_string(),
                format!("<pre style=\"font-family:monospace;\">{colored}</pre>"),
            )
        }
    };

    ViewEntry { css_class, html }
}

fn classify_line(line: &str) -> &'static str {
    if line.starts_with("+++") || line.starts_with("---") {
        ""
    } else if line.starts_with('@') {
        "diff-hunk"
    } else if line.starts_with('+') {
        "diff-add"
    } else if line.starts_with('-') {
        "diff-remove"
    } else {
        ""
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrdr_protocol::{
        PaneTranscript, ServerFrame, ServerMsg, WireEntry, WireEntryKind, WireEntryView,
        WirePaneId,
    };

    #[test]
    fn snapshot_replaces_state() {
        let mut t = Vec::new();
        let frame = ServerFrame {
            seq: 1,
            msg: ServerMsg::Snapshot {
                session_id: None,
                session_name: String::new(),
                cwd: String::new(),
                panes: vec![],
                active: WirePaneId::Main,
                status: hrdr_protocol::WireStatus {
                    left: vec![],
                    right: vec![],
                },
                transcripts: vec![PaneTranscript {
                    pane: WirePaneId::Main,
                    entries: vec![WireEntryView {
                        entry: WireEntry {
                            kind: WireEntryKind::User("hi".into()),
                            time: 0,
                        },
                        tool: None,
                        diff_lines: None,
                    }],
                }],
                show_thinking: true,
            },
        };
        apply_frame(&frame, &mut t);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].css_class, "user");
    }

    #[test]
    fn entries_truncates_and_extends() {
        let mut t = vec![
            ViewEntry { css_class: "user".into(), html: "a".into() },
            ViewEntry { css_class: "assistant".into(), html: "b".into() },
            ViewEntry { css_class: "user".into(), html: "c".into() },
        ];
        let frame = ServerFrame {
            seq: 2,
            msg: ServerMsg::Entries {
                pane: WirePaneId::Main,
                from: 1,
                entries: vec![
                    WireEntryView {
                        entry: WireEntry {
                            kind: WireEntryKind::Assistant("B".into()),
                            time: 0,
                        },
                        tool: None,
                        diff_lines: None,
                    },
                    WireEntryView {
                        entry: WireEntry {
                            kind: WireEntryKind::Assistant("C".into()),
                            time: 0,
                        },
                        tool: None,
                        diff_lines: None,
                    },
                ],
            },
        };
        apply_frame(&frame, &mut t);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].html, "a");
        assert_eq!(t[1].html, "B");
        assert_eq!(t[2].html, "C");
    }

    #[test]
    fn entries_from_0_sets_first() {
        let mut t = vec![];
        let frame = ServerFrame {
            seq: 1,
            msg: ServerMsg::Entries {
                pane: WirePaneId::Main,
                from: 0,
                entries: vec![WireEntryView {
                    entry: WireEntry {
                        kind: WireEntryKind::Assistant("hello".into()),
                        time: 0,
                    },
                    tool: None,
                    diff_lines: None,
                }],
            },
        };
        apply_frame(&frame, &mut t);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn resumed_does_nothing() {
        let mut t = vec![ViewEntry { css_class: "user".into(), html: "x".into() }];
        let frame = ServerFrame {
            seq: 5,
            msg: ServerMsg::Resumed {},
        };
        apply_frame(&frame, &mut t);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn html_escape_works() {
        assert_eq!(html_escape("<script>"), "&lt;script&gt;");
        assert_eq!(html_escape("a & b"), "a &amp; b");
        assert_eq!(html_escape("\"quoted\""), "&quot;quoted&quot;");
    }
}
