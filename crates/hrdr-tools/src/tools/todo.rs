use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{TodoItem, Tool, ToolContext};

// ---- todo ----

pub struct TodoTool;

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }
    fn description(&self) -> &'static str {
        "Replace the task list for the current work. Use it to plan and track multi-step \
         coding tasks: mark exactly one item `in_progress`, the rest \
         `pending`/`completed`/`cancelled`."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The full task list, replacing whatever was there before.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "The task, in a few words."},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"], "description": "pending: not started. in_progress: exactly one item at a time. completed: done. cancelled: abandoned."},
                            "evidence": {"type": "string", "description": "How you verified this item: the command you ran and what it reported (e.g. `cargo test -p x: 12 passed`). REQUIRED to move an item to `completed` — the call is rejected without it. Omit for every other status."}
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }
    /// `read_only` here means what it means everywhere else in the registry:
    /// *does not mutate the working tree*. `todo` replaces a `Vec<TodoItem>`
    /// behind a mutex in the agent's own [`ToolContext`] — no file, no process,
    /// nothing outside this agent's memory. Classifying it as mutating cost a
    /// read-only agent the tool while the unconditional prompt kept telling it to
    /// plan with `todo`, which is the one combination that cannot be right.
    fn read_only(&self) -> bool {
        true
    }
    /// …but opt back out of concurrency, which `read_only` would otherwise
    /// imply. Each call *replaces* the whole list, so two of them in one batch
    /// are order-sensitive: run concurrently, whichever mutex acquisition landed
    /// last would decide the surviving list. Sequential keeps "the last call the
    /// model made is the list it gets", which is what the turn-end TODO checks
    /// then read back.
    fn concurrent(&self) -> bool {
        false
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let mut items = parse_todos(args).context("invalid todo args")?;
        // Reject *before* the list is replaced, so a refused call leaves the
        // previous list exactly as it was and the retry is a straight re-send.
        {
            let prior = ctx
                .todos
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(err) = unevidenced_completions(&prior, &items) {
                bail!(err);
            }
            assign_ids(&prior, &mut items);
        }
        let rendered = render_todos(&items);
        // A poisoned lock must not silently report success with a stale list.
        *ctx.todos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = items;
        Ok(rendered)
    }
}

/// Forgivingly extract the todo list from `todo` arguments. The schema is
/// the standard `{"todos": [{content, status}, …]}`, but smaller models often
/// echo the JSON-Schema shape into the value or drop/rename the wrapper, so we
/// also accept `{"todos": {"items": […]}}` (the schema-echo mistake), a bare
/// `{"items": […]}` / `{"tasks": […]}`, and a top-level array.
pub(crate) fn parse_todos(args: serde_json::Value) -> Result<Vec<TodoItem>> {
    let arr = match args {
        Value::Array(a) => a,
        Value::Object(mut m) => {
            let v = m
                .remove("todos")
                .or_else(|| m.remove("items"))
                .or_else(|| m.remove("tasks"))
                .ok_or_else(|| anyhow!("expected a `todos` array of {{content, status}} items"))?;
            match v {
                Value::Array(a) => a,
                // `{"todos": {"items": […]}}` — the model copied the schema's
                // `items` keyword instead of emitting a bare array.
                Value::Object(mut inner) => {
                    match inner.remove("items").or_else(|| inner.remove("todos")) {
                        Some(Value::Array(a)) => a,
                        _ => bail!("`todos` must be an array of {{content, status}} items"),
                    }
                }
                // A single item object instead of a one-element array.
                other => vec![other],
            }
        }
        _ => bail!("expected an object with a `todos` array"),
    };
    arr.into_iter().map(parse_item).collect()
}

/// Parse one todo item, tolerating `task`/`text`/`title` aliases for the content
/// and a range of status spellings (see [`normalize_status`]).
fn parse_item(v: serde_json::Value) -> Result<TodoItem> {
    let Value::Object(mut m) = v else {
        bail!("each todo must be an object with a `content` string");
    };
    let content = m
        .remove("content")
        .or_else(|| m.remove("task"))
        .or_else(|| m.remove("text"))
        .or_else(|| m.remove("title"))
        .and_then(|c| match c {
            Value::String(s) => Some(s),
            _ => None,
        })
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("each todo needs a non-empty `content` string"))?;
    let status = m
        .remove("status")
        .or_else(|| m.remove("state"))
        .and_then(|s| s.as_str().map(normalize_status))
        .unwrap_or_else(|| "pending".to_string());
    let id = m.remove("id").and_then(|v| v.as_u64()).unwrap_or(0);
    let evidence = m
        .remove("evidence")
        .or_else(|| m.remove("verified_by"))
        .or_else(|| m.remove("verification"))
        .and_then(|e| match e {
            Value::String(s) => Some(s),
            // A model that answers with `true` has said nothing checkable, and
            // treating it as evidence would hand back the free tick the field
            // exists to take away.
            _ => None,
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(TodoItem {
        content,
        id,
        status,
        evidence,
    })
}

/// The error owed when a call moves items to `completed` without saying how they
/// were verified, or `None` when every new completion carries its evidence.
///
/// Scoped to items *newly* completed: an item that was already `completed` in
/// the previous list rides along in every later call, and demanding evidence
/// again would make the list impossible to resend. `cancelled` is exempt —
/// abandoning work is not a claim that it was done.
fn unevidenced_completions(prior: &[TodoItem], next: &[TodoItem]) -> Option<String> {
    let was_completed = |content: &str| {
        prior
            .iter()
            .any(|p| p.content == content && p.status == "completed")
    };
    let offenders: Vec<&str> = next
        .iter()
        .filter(|t| t.status == "completed" && t.evidence.is_none() && !was_completed(&t.content))
        .map(|t| t.content.as_str())
        .collect();
    if offenders.is_empty() {
        return None;
    }
    Some(format!(
        "cannot mark {} completed without `evidence`: {}\n\
         Give each newly completed item an `evidence` string naming the check you ran and what it \
         reported (e.g. \"cargo test -p hrdr-tools: 155 passed\"). If you have not run it yet, run \
         it now and send the list again — leave the item `in_progress` until you have.",
        if offenders.len() == 1 {
            "an item"
        } else {
            "items"
        },
        offenders
            .iter()
            .map(|c| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(", "),
    ))
}

/// Give every incoming item with `id == 0` a stable reference id.
///
/// The model replaces the whole list on every call and rarely echoes the ids
/// back, so the prior list is the source of truth: an item whose content
/// already carried a nonzero id reuses it (ids survive a full-list
/// replacement), and everything else — new items and legacy id-0 items from
/// sessions saved before the field existed — gets a freshly minted id, starting
/// one past the largest id seen across prior ∪ incoming so an echoed new id
/// never collides with a minted one.
fn assign_ids(prior: &[TodoItem], items: &mut [TodoItem]) {
    let mut next = prior
        .iter()
        .chain(items.iter())
        .filter(|t| t.id != 0)
        .map(|t| t.id)
        .max()
        .unwrap_or(0)
        .max(1)
        + 1;
    for t in items.iter_mut() {
        if t.id != 0 {
            continue;
        }
        t.id = prior
            .iter()
            .find(|p| p.content == t.content && p.id != 0)
            .map(|p| p.id)
            .unwrap_or_else(|| {
                let id = next;
                next += 1;
                id
            });
    }
}

/// Map a free-form status string onto one of `pending | in_progress | completed | cancelled`.
/// Unknown values fall back to `pending`, so a bad status never fails the call.
fn normalize_status(s: &str) -> String {
    match s
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '-'], "_")
        .as_str()
    {
        "completed" | "complete" | "done" | "finished" | "x" | "[x]" => "completed",
        "in_progress" | "inprogress" | "doing" | "active" | "current" | "wip" | "started"
        | "ongoing" => "in_progress",
        "cancelled" | "canceled" | "canceling" | "cancelling" | "abandoned" | "skipped"
        | "removed" | "stale" => "cancelled",
        _ => "pending",
    }
    .to_string()
}

fn render_todos(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "(todo list cleared)".to_string();
    }
    let mut out = String::new();
    for t in todos {
        let mark = match t.status.as_str() {
            "completed" => "✓",
            "cancelled" => "✗",
            "in_progress" => "⠋",
            _ => " ",
        };
        out.push_str(&format!("#{} {mark} {}\n", t.id, t.content));
        // Echo the evidence back under its item. It is the model's own claim,
        // and putting it where the user reads the list is what makes an empty
        // one visible to somebody other than the model that wrote it.
        if let Some(e) = t.evidence.as_deref().filter(|_| t.status == "completed") {
            out.push_str(&format!("    ↳ {e}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(content: &str, status: &str, evidence: Option<&str>) -> TodoItem {
        TodoItem {
            content: content.to_string(),
            id: 0,
            status: status.to_string(),
            evidence: evidence.map(str::to_string),
        }
    }

    fn item_with_id(content: &str, status: &str, evidence: Option<&str>, id: u64) -> TodoItem {
        TodoItem {
            content: content.to_string(),
            id,
            status: status.to_string(),
            evidence: evidence.map(str::to_string),
        }
    }

    #[test]
    fn a_new_completion_without_evidence_is_refused_and_names_the_item() {
        let err = unevidenced_completions(
            &[item("fix it", "in_progress", None)],
            &[item("fix it", "completed", None)],
        )
        .expect("a bare tick must be refused");
        assert!(err.contains("`fix it`"), "{err}");
    }

    #[test]
    fn a_new_completion_with_evidence_passes() {
        assert!(
            unevidenced_completions(
                &[item("fix it", "in_progress", None)],
                &[item(
                    "fix it",
                    "completed",
                    Some("cargo test -p hrdr-tools: 155 passed"),
                )],
            )
            .is_none()
        );
    }

    #[test]
    fn an_already_completed_item_rides_along_without_resending_evidence() {
        // Every later call resends the whole list. Demanding evidence again for
        // work already ticked would make the list impossible to resend.
        let prior = [item("fix it", "completed", Some("cargo test: 155 passed"))];
        let next = [
            item("fix it", "completed", None),
            item("next thing", "in_progress", None),
        ];
        assert!(unevidenced_completions(&prior, &next).is_none());
    }

    #[test]
    fn cancelling_needs_no_evidence_but_completing_several_names_them_all() {
        assert!(unevidenced_completions(&[], &[item("dropped", "cancelled", None)]).is_none());
        let err = unevidenced_completions(
            &[],
            &[
                item("one", "completed", None),
                item("two", "completed", Some("ran it")),
                item("three", "completed", None),
            ],
        )
        .expect("two bare ticks must be refused");
        assert!(err.contains("`one`") && err.contains("`three`"), "{err}");
        assert!(
            !err.contains("`two`"),
            "the evidenced item must not be named: {err}"
        );
    }

    #[test]
    fn evidence_must_be_something_checkable_not_a_yes() {
        // `true`, `1`, and whitespace are how a required field gets satisfied
        // without saying anything — which would hand back the free tick this
        // field exists to take away.
        for junk in [json!(true), json!(1), json!("   "), json!(null)] {
            let parsed = parse_todos(json!({"todos": [
                {"content": "fix it", "status": "completed", "evidence": junk}
            ]}))
            .unwrap();
            assert_eq!(
                parsed[0].evidence, None,
                "{junk} should not count as evidence"
            );
            assert!(unevidenced_completions(&[], &parsed).is_some());
        }
    }

    #[test]
    fn the_evidence_is_echoed_under_its_item() {
        let out = render_todos(&[item("fix it", "completed", Some("cargo test: 155 passed"))]);
        assert!(out.contains("✓ fix it"), "{out}");
        assert!(out.contains("↳ cargo test: 155 passed"), "{out}");
        // Not shown for anything but a completion — an in-progress item's
        // "evidence" is a claim about work that is not finished.
        let out = render_todos(&[item("fix it", "in_progress", Some("half a run"))]);
        assert!(!out.contains("↳"), "{out}");
    }

    #[test]
    fn rendered_rows_lead_with_the_stable_id() {
        let out = render_todos(&[item_with_id("fix it", "in_progress", None, 3)]);
        assert!(out.contains("#3 ⠋ fix it"), "{out}");
    }

    #[test]
    fn an_echoed_id_is_kept() {
        let prior: [TodoItem; 0] = [];
        let mut next = vec![item_with_id("fix it", "pending", None, 42)];
        assign_ids(&prior, &mut next);
        assert_eq!(next[0].id, 42, "a model that echoes an id keeps it");
    }

    #[test]
    fn ids_survive_a_full_list_replacement_and_legacy_items_get_minted() {
        // The model replaces the whole list on every call and rarely echoes
        // ids: an item whose content already had an id reuses it, a brand-new
        // item is minted, and a legacy id-0 item (saved before the field
        // existed) is minted on the first call after upgrade.
        let prior = [
            item_with_id("keep me", "pending", None, 7),
            item_with_id("legacy", "pending", None, 0),
        ];
        let mut next = vec![
            item("keep me", "in_progress", None),
            item("brand new", "pending", None),
            item("legacy", "completed", Some("ran it")),
        ];
        assign_ids(&prior, &mut next);
        assert_eq!(next[0].id, 7, "same content reuses the prior id");
        assert_eq!(next[1].id, 8, "a new item gets the next minted id");
        assert_eq!(next[2].id, 9, "a legacy id-0 item is minted too");
        // Minting stays clear of an echoed nonzero id, whatever the order.
        let mut next = vec![
            item("new a", "pending", None),
            item_with_id("echoed", "pending", None, 100),
            item("new b", "pending", None),
        ];
        assign_ids(&prior, &mut next);
        assert_eq!(next[1].id, 100, "the echoed id is untouched");
        assert_eq!(
            next[0].id, 101,
            "minting starts past the largest id seen, echoed ones included"
        );
        assert_eq!(next[2].id, 102, "minting continues in order");
    }
}
