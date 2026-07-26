//! Pure frame→state reducer for the Dioxus client. No DOM, no WASM — just
//! the logic that turns `ServerFrame` messages into a `Vec<ViewEntry>`.
//!
//! Renders markdown (pulldown-cmark, raw-HTML stripped), tool blocks from
//! `WireEntryView.tool`, diff coloring from `diff_lines`, reasoning collapse,
//! and dim styling for system/notice/stats lines.

use hrdr_protocol::{ServerFrame, ServerMsg, WireDiffLineKind};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

/// One rendered entry in the client-side transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewEntry {
    css_class: String,
    html: String,
}

impl ViewEntry {
    pub fn css_class(&self) -> &str { &self.css_class }
    pub fn html(&self) -> &str { &self.html }
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
        ServerMsg::Entries { from, entries, .. } => {
            transcript.truncate(*from);
            for ev in entries {
                transcript.push(entry_to_view(ev));
            }
        }
        ServerMsg::Resumed {} => {}
        _ => {}
    }
}

fn entry_to_view(ev: &hrdr_protocol::WireEntryView) -> ViewEntry {
    let entry = &ev.entry;
    let (css_class, html) = match &entry.kind {
        hrdr_protocol::WireEntryKind::Header => (
            "heading".into(),
            "<h1 style=\"text-align:center; color:#e94560; margin:2rem 0;\">hrdr</h1>".into(),
        ),
        hrdr_protocol::WireEntryKind::User(text) => (
            "user".into(),
            format!("<strong style=\"color:#e94560;\">You</strong> <div style=\"margin-top:0.25rem;\">{}</div>", render_markdown(text)),
        ),
        hrdr_protocol::WireEntryKind::Assistant(text) => (
            "assistant".into(),
            render_markdown(text),
        ),
        hrdr_protocol::WireEntryKind::Reasoning { text, took_ms } => {
            let label = match took_ms {
                Some(ms) => format!("Thought for {:.1}s", *ms as f64 / 1000.0),
                None => "Thinking…".into(),
            };
            (
                "reasoning".into(),
                format!(
                    "<details><summary style=\"color:#666; cursor:pointer; font-style:italic;\">{label}</summary><div style=\"color:#888; font-style:italic; border-left:2px solid #666; padding-left:0.5rem; margin:0.5rem 0;\">{}</div></details>",
                    render_markdown(text)
                ),
            )
        }
        hrdr_protocol::WireEntryKind::Tool {
            name,
            result,
            ok,
            done,
            ..
        } => {
            let status = if !done { "⏳" } else if *ok { "✓" } else { "✗" };
            let status_color = if !done { "#888" } else if *ok { "#4ecca3" } else { "#e94560" };

            // Tool display model.
            let tool_body_html = if let Some(td) = &ev.tool {
                render_tool_body(&td.body)
            } else {
                String::new()
            };

            // Result output.
            let result_html = if *done {
                if let Some(lines) = &ev.diff_lines {
                    render_diff_lines(lines)
                } else {
                    format!("<pre style=\"font-family:monospace; margin:0.25rem 0; white-space:pre-wrap; color:#ccc; font-size:13px;\">{}</pre>", html_escape(result))
                }
            } else {
                String::new()
            };

            (
                "tool".into(),
                format!(
                    "<div style=\"margin:0.5rem 0;\"><span style=\"color:{status_color}; font-weight:bold;\">{status} {name}</span>{tool_body_html}{result_html}</div>"
                ),
            )
        }
        hrdr_protocol::WireEntryKind::System(text) => (
            "system".into(),
            format!("<em style=\"color:#888;\">{}</em>", html_escape(text)),
        ),
        hrdr_protocol::WireEntryKind::Notice(text) => (
            "notice".into(),
            format!("<div style=\"color:#888; font-style:italic; margin:0.25rem 0;\">{}</div>", html_escape(text)),
        ),
        hrdr_protocol::WireEntryKind::Stats(text) => (
            "stats".into(),
            format!("<div style=\"color:#666; font-size:13px; margin:0.25rem 0;\">{}</div>", html_escape(text)),
        ),
        hrdr_protocol::WireEntryKind::Diff(text) => (
            "diff".into(),
            {
                let lines: Vec<_> = text.lines().map(|line| {
                    let cls = classify_line(line);
                    (cls, line)
                }).collect();
                let html: String = lines.iter().map(|(cls, line)| {
                    format!("<span class=\"{cls}\">{}</span>\n", html_escape(line))
                }).collect();
                format!("<pre style=\"font-family:monospace; font-size:13px; line-height:1.4;\">{html}</pre>")
            },
        ),
    };
    ViewEntry { css_class, html }
}

fn render_markdown(text: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(text, opts);

    let mut html = String::new();
    let mut in_raw_html = false;

    for event in parser {
        match event {
            Event::Html(raw) | Event::InlineHtml(raw) => {
                // Drop raw HTML for safety.
                in_raw_html = true;
                let _ = raw;
            }
            Event::Start(tag) => {
                in_raw_html = false;
                match tag {
                    Tag::Paragraph => html.push_str("<p style=\"margin:0.25rem 0;\">"),
                    Tag::Heading { level, .. } => {
                        let size = match level {
                            pulldown_cmark::HeadingLevel::H1 => "1.5rem",
                            pulldown_cmark::HeadingLevel::H2 => "1.3rem",
                            pulldown_cmark::HeadingLevel::H3 => "1.1rem",
                            _ => "1rem",
                        };
                        html.push_str(&format!("<h{level} style=\"margin:0.5rem 0; font-size:{size};\">"));
                    }
                    Tag::BlockQuote(_) => html.push_str("<blockquote style=\"border-left:3px solid #666; padding-left:0.5rem; color:#aaa; margin:0.25rem 0;\">"),
                    Tag::CodeBlock(_) => html.push_str("<pre style=\"background:#0f3460; padding:0.5rem; border-radius:4px; font-family:monospace; font-size:13px; overflow-x:auto;\"><code>"),
                    Tag::List(_) => html.push_str("<ul style=\"margin:0.25rem 0; padding-left:1.5rem;\">"),
                    Tag::Item => html.push_str("<li>"),
                    Tag::Emphasis => html.push_str("<em>"),
                    Tag::Strong => html.push_str("<strong>"),
                    Tag::Strikethrough => html.push_str("<s>"),
                    Tag::Link { dest_url, .. } => {
                        html.push_str(&format!("<a href=\"{dest_url}\" style=\"color:#4ecca3;\">"));
                    }
                    _ => {}
                }
            }
            Event::End(tag) => {
                match tag {
                    TagEnd::Paragraph => html.push_str("</p>"),
                    TagEnd::Heading(level) => html.push_str(&format!("</h{level}>")),
                    TagEnd::BlockQuote(_) => html.push_str("</blockquote>"),
                    TagEnd::CodeBlock => html.push_str("</code></pre>"),
                    TagEnd::List(_) => html.push_str("</ul>"),
                    TagEnd::Item => html.push_str("</li>"),
                    TagEnd::Link => html.push_str("</a>"),
                    _ => {}
                }
            }
            Event::Text(t) => {
                if !in_raw_html {
                    html.push_str(&html_escape(&t));
                }
            }
            Event::Code(s) => {
                html.push_str(&format!("<code style=\"background:#0f3460; padding:0.1em 0.3em; border-radius:3px; font-family:monospace; font-size:13px;\">{}</code>", html_escape(&s)));
            }
            Event::SoftBreak => html.push('\n'),
            Event::HardBreak => html.push_str("<br>"),
            Event::Rule => html.push_str("<hr style=\"border-color:#333;\">"),
            Event::FootnoteReference(_) | Event::TaskListMarker(_) => {}
            _ => {}
        }
    }
    html
}

fn render_tool_body(body: &hrdr_protocol::WireToolBody) -> String {
    match body {
        hrdr_protocol::WireToolBody::Shell { command } => {
            format!("<pre style=\"color:#f0a500; font-family:monospace; margin:0.25rem 0;\">$ {}</pre>", html_escape(command))
        }
        hrdr_protocol::WireToolBody::Code { lang, content } => {
            format!("<pre style=\"background:#0f3460; padding:0.5rem; border-radius:4px; font-family:monospace; font-size:13px; overflow-x:auto;\" class=\"lang-{}\">{}</pre>", html_escape(lang), html_escape(content))
        }
        hrdr_protocol::WireToolBody::Diff {} => String::new(),
        hrdr_protocol::WireToolBody::Read {} => String::new(),
        hrdr_protocol::WireToolBody::Details { rows } => {
            let rows_html: String = rows.iter().map(|(k, v)| {
                format!("<tr><td style=\"color:#f0a500; padding:0 0.5rem 0 0; white-space:nowrap; vertical-align:top;\">{}</td><td style=\"color:#ccc;\">{}</td></tr>", html_escape(k), html_escape(v))
            }).collect();
            format!("<table style=\"margin:0.25rem 0; font-size:13px;\">{rows_html}</table>")
        }
        hrdr_protocol::WireToolBody::Text {} => String::new(),
    }
}

fn render_diff_lines(lines: &[hrdr_protocol::WireDiffLine]) -> String {
    let html: String = lines.iter().map(|l| {
        let cls = match l.kind {
            WireDiffLineKind::Add => "diff-add",
            WireDiffLineKind::Remove => "diff-remove",
            WireDiffLineKind::Hunk => "diff-hunk",
            WireDiffLineKind::Meta => "",
        };
        format!("<span class=\"{cls}\">{}</span>\n", html_escape(&l.text))
    }).collect();
    format!("<pre style=\"font-family:monospace; font-size:13px; line-height:1.4; margin:0.25rem 0;\">{html}</pre>")
}

fn classify_line(line: &str) -> &'static str {
    if line.starts_with("+++") || line.starts_with("---") { "" }
    else if line.starts_with('@') { "diff-hunk" }
    else if line.starts_with('+') { "diff-add" }
    else if line.starts_with('-') { "diff-remove" }
    else { "" }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hrdr_protocol::{
        PaneTranscript, WireEntry, WireEntryKind, WireEntryView, WirePaneId,
    };

    #[test]
    fn snapshot_replaces_state() {
        let mut t = vec![];
        let frame = ServerFrame { seq: 1, msg: ServerMsg::Snapshot {
            session_id: None, session_name: String::new(), cwd: String::new(),
            panes: vec![], active: WirePaneId::Main,
            status: hrdr_protocol::WireStatus { left: vec![], right: vec![] },
            transcripts: vec![PaneTranscript { pane: WirePaneId::Main, entries: vec![
                WireEntryView { entry: WireEntry { kind: WireEntryKind::User("hi".into()), time: 0 }, tool: None, diff_lines: None }
            ]}],
            show_thinking: true,
        }};
        apply_frame(&frame, &mut t);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].css_class, "user");
    }

    #[test]
    fn entries_truncates_and_extends() {
        let mut t = vec![entry("user", "a"), entry("assistant", "b"), entry("user", "c")];
        apply_frame(&ServerFrame { seq: 2, msg: ServerMsg::Entries {
            pane: WirePaneId::Main, from: 1, entries: vec![
                view_entry("B"), view_entry("C")
            ],
        }}, &mut t);
        assert_eq!(t.len(), 3);
        assert!(t[0].html.contains("a"));
        assert!(t[1].html.contains("B"));
        assert!(t[2].html.contains("C"));
    }

    #[test]
    fn markdown_drops_raw_html() {
        let html = render_markdown("hello **world**");
        assert!(html.contains("hello"));
        assert!(html.contains("world"));
        assert!(html.contains("<strong>"), "markdown formatting should work");
        // Raw HTML tags in the input are dropped.
        let html2 = render_markdown("<script>alert(1)</script>");
        assert!(!html2.contains("<script"), "raw html tags are dropped: {html2}");
    }

    #[test]
    fn diff_lines_map_1_to_1() {
        use hrdr_protocol::WireDiffLine;
        let lines = vec![
            WireDiffLine { kind: WireDiffLineKind::Add, text: "+added".into() },
            WireDiffLine { kind: WireDiffLineKind::Remove, text: "-removed".into() },
            WireDiffLine { kind: WireDiffLineKind::Hunk, text: "@@ -1 +1 @@".into() },
        ];
        let html = render_diff_lines(&lines);
        assert!(html.contains("diff-add"));
        assert!(html.contains("diff-remove"));
        assert!(html.contains("diff-hunk"));
    }

    #[test]
    fn resumed_does_nothing() {
        let mut t = vec![entry("user", "x")];
        apply_frame(&ServerFrame { seq: 5, msg: ServerMsg::Resumed {} }, &mut t);
        assert_eq!(t.len(), 1);
    }

    fn entry(css: &str, html: &str) -> ViewEntry {
        ViewEntry { css_class: css.into(), html: html.into() }
    }

    fn view_entry(text: &str) -> WireEntryView {
        WireEntryView {
            entry: WireEntry { kind: WireEntryKind::Assistant(text.into()), time: 0 },
            tool: None, diff_lines: None,
        }
    }
}
