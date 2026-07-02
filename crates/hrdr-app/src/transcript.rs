//! The transcript data model shared by hrdr's frontends: the [`Entry`] enum (one
//! rendered item in the conversation) plus the representation-independent queries
//! over a slice of entries — search, message counting/indexing, and text/JSON
//! export. How an `Entry` is painted is the frontend's business; what counts as a
//! "message", how `/find` matches, and the export formats are shared here so the
//! TUI and GUI stay consistent.

use chrono::{DateTime, Local};
use hrdr_agent::{Message, MessageRole};

/// One rendered item in the transcript.
pub enum Entry {
    User(String),
    Assistant(String),
    Reasoning(String),
    Tool {
        id: String,
        name: String,
        args: String,
        result: String,
        ok: bool,
        done: bool,
        /// Show the full result instead of a truncated preview (`/expand`).
        expanded: bool,
    },
    System(String),
    /// Final per-turn stats line, appended below the last output.
    Stats(String),
    /// A unified diff (e.g. `/diff`), rendered with diff coloring.
    Diff(String),
}

impl Entry {
    /// The displayable text of a user/assistant message, if this entry is one.
    /// These are the only entries that count as numbered "messages" for `/find`,
    /// `/goto`, `/copy msg N`, and export.
    pub fn message_text(&self) -> Option<&str> {
        match self {
            Entry::User(s) | Entry::Assistant(s) => Some(s),
            _ => None,
        }
    }
}

/// Rebuild display entries from a restored message history (`/resume`, startup
/// auto-resume) — shared so the TUI and GUI reconstruct identically. User and
/// non-empty assistant texts become entries; each assistant `tool_calls` entry
/// is paired with its `role:"tool"` result by call id (the `Error:` prefix
/// convention marks a failed call). Other roles are skipped. Frontends map the
/// returned entries into their own representation (the TUI stores them as-is,
/// the GUI wraps each in its reactive signals).
pub fn messages_to_entries(msgs: &[Message]) -> Vec<Entry> {
    use std::collections::HashMap;
    // Map tool_call_id → (result, ok) from the tool-result messages.
    let mut results: HashMap<&str, (&str, bool)> = HashMap::new();
    for m in msgs {
        if m.role == MessageRole::Tool
            && let (Some(id), Some(content)) = (&m.tool_call_id, &m.content)
        {
            results.insert(id, (content, !content.starts_with("Error:")));
        }
    }
    let mut out = Vec::new();
    for m in msgs {
        match m.role {
            MessageRole::User => {
                if let Some(c) = &m.content {
                    out.push(Entry::User(c.clone()));
                }
            }
            MessageRole::Assistant => {
                if let Some(c) = &m.content
                    && !c.is_empty()
                {
                    out.push(Entry::Assistant(c.clone()));
                }
                for call in m.tool_calls.iter().flatten() {
                    let (result, ok) = results
                        .get(call.id.as_str())
                        .map(|(r, ok)| (r.to_string(), *ok))
                        .unwrap_or_default();
                    out.push(Entry::Tool {
                        id: call.id.clone(),
                        name: call.function.name.clone(),
                        args: call.function.arguments.clone(),
                        result,
                        ok,
                        done: true,
                        expanded: false,
                    });
                }
            }
            _ => {}
        }
    }
    out
}

/// 1-based message numbers whose user/assistant text contains `query`
/// (case-insensitive substring). Message numbers count only user/assistant
/// entries, matching the numbering the frontends display.
pub fn find_hits(entries: &[Entry], query: &str) -> Vec<usize> {
    let needle = query.to_ascii_lowercase();
    let mut num = 0;
    let mut hits = Vec::new();
    for e in entries {
        if let Some(s) = e.message_text() {
            num += 1;
            if s.to_ascii_lowercase().contains(&needle) {
                hits.push(num);
            }
        }
    }
    hits
}

/// Number of user/assistant messages in the transcript.
pub fn message_count(entries: &[Entry]) -> usize {
    entries
        .iter()
        .filter(|e| e.message_text().is_some())
        .count()
}

/// The text of the Nth (1-based) user/assistant message, if any.
pub fn nth_message_text(entries: &[Entry], n: usize) -> Option<String> {
    if n == 0 {
        return None;
    }
    entries
        .iter()
        .filter_map(Entry::message_text)
        .nth(n - 1)
        .map(str::to_string)
}

/// The number of the first user/assistant message stamped at/after `cutoff`.
/// `times` is parallel to `entries` (index i is entry i's local timestamp).
pub fn first_message_since(
    entries: &[Entry],
    times: &[DateTime<Local>],
    cutoff: DateTime<Local>,
) -> Option<usize> {
    let mut num = 0;
    for (i, e) in entries.iter().enumerate() {
        if e.message_text().is_some() {
            num += 1;
            if times.get(i).is_some_and(|t| *t >= cutoff) {
                return Some(num);
            }
        }
    }
    None
}

/// The transcript as Markdown-ish text (user/assistant/system/diff/tool lines;
/// reasoning and stats are omitted). Used by `/copy all` and `/export`.
pub fn transcript_to_text(entries: &[Entry]) -> String {
    let mut out = String::new();
    for e in entries {
        match e {
            Entry::User(s) => out.push_str(&format!("## User\n{s}\n\n")),
            Entry::Assistant(s) => out.push_str(&format!("## Assistant\n{s}\n\n")),
            Entry::System(s) => out.push_str(&format!("[{s}]\n\n")),
            Entry::Diff(s) => out.push_str(&format!("{s}\n\n")),
            Entry::Tool { name, .. } => out.push_str(&format!("[tool: {name}]\n\n")),
            Entry::Reasoning(_) | Entry::Stats(_) => {}
        }
    }
    out.trim_end().to_string()
}

/// The conversation as a pretty-printed JSON array of `{n, role, time, content}`
/// objects (user/assistant messages only). `times` is parallel to `entries`.
pub fn transcript_to_json(entries: &[Entry], times: &[DateTime<Local>]) -> String {
    let mut arr = Vec::new();
    let mut num = 0;
    for (i, e) in entries.iter().enumerate() {
        let (role, content) = match e {
            Entry::User(s) => ("user", s),
            Entry::Assistant(s) => ("assistant", s),
            _ => continue,
        };
        num += 1;
        let time = times.get(i).map(|t| t.to_rfc3339()).unwrap_or_default();
        arr.push(serde_json::json!({
            "n": num,
            "role": role,
            "time": time,
            "content": content,
        }));
    }
    serde_json::to_string_pretty(&arr).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Entry> {
        vec![
            Entry::System("welcome".into()),
            Entry::User("Fix the parser bug".into()),
            Entry::Reasoning("thinking…".into()),
            Entry::Assistant("Done — it was an off-by-one.".into()),
            Entry::User("thanks".into()),
        ]
    }

    #[test]
    fn message_count_and_nth_skip_non_messages() {
        let e = sample();
        assert_eq!(message_count(&e), 3); // 2 user + 1 assistant
        assert_eq!(
            nth_message_text(&e, 1).as_deref(),
            Some("Fix the parser bug")
        );
        assert_eq!(
            nth_message_text(&e, 2).as_deref(),
            Some("Done — it was an off-by-one.")
        );
        assert_eq!(nth_message_text(&e, 3).as_deref(), Some("thanks"));
        assert_eq!(nth_message_text(&e, 0), None);
        assert_eq!(nth_message_text(&e, 4), None);
    }

    #[test]
    fn find_hits_are_case_insensitive_message_numbers() {
        let e = sample();
        assert_eq!(find_hits(&e, "PARSER"), vec![1]);
        assert_eq!(find_hits(&e, "off-by-one"), vec![2]);
        // Reasoning/system are never matched even if they contain the needle.
        assert_eq!(find_hits(&e, "welcome"), Vec::<usize>::new());
        assert_eq!(find_hits(&e, "thinking"), Vec::<usize>::new());
    }

    #[test]
    fn to_text_omits_reasoning_and_stats() {
        let e = sample();
        let txt = transcript_to_text(&e);
        assert!(txt.contains("## User\nFix the parser bug"));
        assert!(txt.contains("## Assistant\nDone"));
        assert!(txt.contains("[welcome]"));
        assert!(!txt.contains("thinking")); // reasoning dropped
        assert!(!txt.ends_with('\n')); // trailing whitespace trimmed
    }

    #[test]
    fn to_json_covers_only_messages_with_times() {
        use chrono::Duration;
        let e = sample();
        let base = Local::now();
        // Parallel timestamps; only the message entries' times surface.
        let times: Vec<_> = (0..e.len() as i64)
            .map(|i| base + Duration::seconds(i))
            .collect();
        let json = transcript_to_json(&e, &times);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["n"], 1);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[1]["role"], "assistant");
        assert!(!arr[0]["time"].as_str().unwrap().is_empty());
    }
}
