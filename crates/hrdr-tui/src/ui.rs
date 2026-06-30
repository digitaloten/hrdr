//! Rendering: transcript + vim input pane + status line.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::{App, Entry};

const TOOL_RESULT_PREVIEW_LINES: usize = 8;

pub(crate) fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(7),
        Constraint::Length(1),
    ])
    .split(area);

    draw_transcript(f, app, chunks[0]);
    draw_input(f, app, chunks[1]);
    draw_status(f, app, chunks[2]);
}

fn draw_transcript(f: &mut Frame, app: &App, area: Rect) {
    let lines = transcript_lines(app);
    let total = lines.len();
    let scroll = total.saturating_sub(area.height as usize) as u16;
    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(para, area);
}

fn draw_input(f: &mut Frame, app: &mut App, area: Rect) {
    let mode = app.editor.mode_label();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" input [{mode}] "))
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);
    app.editor.render(f, inner);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let dot = if app.running { "●" } else { "○" };
    let text = format!(
        "{dot} {}  │  model: {}  │  Esc=normal  Enter=send  Ctrl+C=quit",
        app.status, app.model
    );
    let para = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    f.render_widget(para, area);
}

fn transcript_lines(app: &App) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = Vec::new();
    for entry in &app.transcript {
        match entry {
            Entry::User(text) => push_text(
                &mut out,
                Span::styled("❯ ", Style::default().fg(Color::Cyan).bold()),
                text,
                Style::default().fg(Color::Cyan),
            ),
            Entry::Assistant(text) => push_text(
                &mut out,
                Span::raw(""),
                text,
                Style::default().fg(Color::White),
            ),
            Entry::Reasoning(text) => push_text(
                &mut out,
                Span::styled("· ", Style::default().fg(Color::DarkGray)),
                text,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
            Entry::Tool {
                name,
                args,
                result,
                ok,
                done,
                ..
            } => push_tool(&mut out, name, args, result, *ok, *done),
            Entry::System(text) => push_text(
                &mut out,
                Span::raw(""),
                text,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
        }
        out.push(Line::raw(""));
    }
    out
}

fn push_text(out: &mut Vec<Line<'static>>, prefix: Span<'static>, text: &str, style: Style) {
    for (i, raw) in text.split('\n').enumerate() {
        if i == 0 {
            out.push(Line::from(vec![
                prefix.clone(),
                Span::styled(raw.to_string(), style),
            ]));
        } else {
            out.push(Line::from(Span::styled(raw.to_string(), style)));
        }
    }
}

fn push_tool(
    out: &mut Vec<Line<'static>>,
    name: &str,
    args: &str,
    result: &str,
    ok: bool,
    done: bool,
) {
    let mark = if !done {
        ("…", Color::Yellow)
    } else if ok {
        ("✓", Color::Green)
    } else {
        ("✗", Color::Red)
    };
    let args_preview = truncate_inline(args, 80);
    out.push(Line::from(vec![
        Span::styled(format!("{} ", mark.0), Style::default().fg(mark.1)),
        Span::styled(name.to_string(), Style::default().fg(Color::Yellow).bold()),
        Span::styled(
            format!(" {args_preview}"),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    if done && !result.is_empty() {
        for line in result.lines().take(TOOL_RESULT_PREVIEW_LINES) {
            out.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(Color::DarkGray),
            )));
        }
        let extra = result
            .lines()
            .count()
            .saturating_sub(TOOL_RESULT_PREVIEW_LINES);
        if extra > 0 {
            out.push(Line::from(Span::styled(
                format!("  … (+{extra} more lines)"),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
}

fn truncate_inline(s: &str, max: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let truncated: String = one_line.chars().take(max).collect();
        format!("{truncated}…")
    }
}
