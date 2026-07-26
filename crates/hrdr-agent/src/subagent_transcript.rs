//! Per-sub-agent transcript: an append-only JSONL log of one delegated `task`
//! run, written independently of the parent session so a sub-agent that dies
//! mid-run leaves all completed work recoverable on disk.
//!
//! Each line is a [`Record`] — a complete, serializable projection of the
//! sub-agent's `AgentEvent` stream: tool calls keep their full args and results,
//! so the on-disk record shows exactly which files and paths a tool touched. On
//! read, each `Record` maps back to an `AgentEvent` and folds through the SAME
//! [`crate::apply_event`] reducer as the main transcript, so a sub-agent's
//! durable record renders identically to the main agent's.
//!
//! Persistence is best-effort: every write error is swallowed, because writing
//! a transcript must never break the actual sub-agent run. A brand-new,
//! never-saved session has no id yet, so the very first sub-agent spawned
//! before the first autosave is not persisted (the dir cell is still empty).
//!
//! Best-effort is *not* licence to leave a half-written line behind, though: a
//! torn line costs two records (the fragment, and whatever is appended onto it)
//! and breaks every line-by-line reader of the file. So the two sides hold a
//! line-atomicity contract — [`SubagentTranscript::write`] either lands a whole
//! record or rolls its bytes back, and the readers here skip a damaged line
//! instead of stopping at it (a torn file from an older build must still resume).

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// How a sub-agent was spawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnKind {
    Blocking,
    Background,
}

/// Terminal status of a sub-agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndStatus {
    Ok,
    Failed,
    Panicked,
    Cancelled,
}

/// One line in a sub-agent transcript. A complete, serializable projection of
/// the sub-agent's `AgentEvent` stream — tool calls keep their full args and
/// results — plus the `Start`/`End`/`Error` framing needed for orphan
/// detection. Serialized with a `t` discriminator so a reader can dispatch on
/// the record kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Record {
    Start {
        model: String,
        label: String,
        kind: SpawnKind,
        prompt: String,
    },
    Reasoning {
        text: String,
    },
    Text {
        chunk: String,
    },
    ToolStart {
        id: String,
        name: String,
        args: String,
    },
    ToolOutput {
        id: String,
        chunk: String,
    },
    ToolEnd {
        id: String,
        name: String,
        result: String,
        ok: bool,
    },
    Notice {
        msg: String,
    },
    Steered {
        text: String,
    },
    Error {
        msg: String,
    },
    End {
        status: EndStatus,
        /// Byte length of the sub-agent's trimmed text output at the terminal
        /// point — the same measure on the blocking and background paths, so runs
        /// are comparable. `0` on `Panicked`/`Cancelled`, where the output was
        /// never collected. A size hint only; it gates nothing.
        bytes: usize,
    },
}

impl Record {
    /// Project a live agent event onto the transcript record to persist, if any.
    /// The write side of the sub-agent transcript: keeps tool args and results
    /// intact. Bulky bookkeeping (`Usage`, `History`) and non-transcript signals
    /// (`TurnDone`, `TodoUpdated`) are dropped.
    pub fn from_event(ev: &crate::AgentEvent) -> Option<Record> {
        use crate::AgentEvent;
        match ev {
            AgentEvent::Reasoning(t) => Some(Record::Reasoning { text: t.clone() }),
            AgentEvent::Text(t) => Some(Record::Text { chunk: t.clone() }),
            AgentEvent::ToolStart { id, name, args } => Some(Record::ToolStart {
                id: id.clone(),
                name: name.clone(),
                args: args.clone(),
            }),
            AgentEvent::ToolOutput { id, chunk } => Some(Record::ToolOutput {
                id: id.clone(),
                chunk: chunk.clone(),
            }),
            AgentEvent::ToolEnd {
                id,
                name,
                result,
                ok,
            } => Some(Record::ToolEnd {
                id: id.clone(),
                name: name.clone(),
                result: result.clone(),
                ok: *ok,
            }),
            AgentEvent::Notice(n) => Some(Record::Notice { msg: n.clone() }),
            AgentEvent::Steered(s) => Some(Record::Steered { text: s.clone() }),
            AgentEvent::Usage { .. }
            | AgentEvent::History(_)
            | AgentEvent::TodoUpdated(_)
            | AgentEvent::TurnDone => None,
        }
    }

    /// Map this record back to the `AgentEvent` the shared reducer expects, if
    /// it carries transcript content. The read side of the sub-agent transcript.
    ///
    /// `Start` opens the folded transcript with the task as a user turn
    /// (`Steered`), matching the live path (`delegation.rs` records the prompt as
    /// a `Steered` event before the run). `Error` surfaces as a `Notice`. `End`
    /// is pure framing and folds to nothing.
    pub fn as_event(&self) -> Option<crate::AgentEvent> {
        use crate::AgentEvent;
        match self {
            Record::Start { prompt, .. } => Some(AgentEvent::Steered(prompt.clone())),
            Record::Reasoning { text } => Some(AgentEvent::Reasoning(text.clone())),
            Record::Text { chunk } => Some(AgentEvent::Text(chunk.clone())),
            Record::ToolStart { id, name, args } => Some(AgentEvent::ToolStart {
                id: id.clone(),
                name: name.clone(),
                args: args.clone(),
            }),
            Record::ToolOutput { id, chunk } => Some(AgentEvent::ToolOutput {
                id: id.clone(),
                chunk: chunk.clone(),
            }),
            Record::ToolEnd {
                id,
                name,
                result,
                ok,
            } => Some(AgentEvent::ToolEnd {
                id: id.clone(),
                name: name.clone(),
                result: result.clone(),
                ok: *ok,
            }),
            Record::Notice { msg } => Some(AgentEvent::Notice(msg.clone())),
            Record::Steered { text } => Some(AgentEvent::Steered(text.clone())),
            Record::Error { msg } => Some(AgentEvent::Notice(msg.clone())),
            Record::End { .. } => None,
        }
    }
}

/// An open append-only transcript file for one sub-agent run.
pub struct SubagentTranscript {
    file: File,
    path: std::path::PathBuf,
    /// The file ends **mid-record**: a previous append got a partial write that
    /// could not be rolled back, or the file we opened already ended without a
    /// newline (a torn tail left by an earlier process). The next record is
    /// prefixed with `\n` so the fragment stays one broken line instead of
    /// swallowing the next record too. See [`Self::write`].
    torn: bool,
}

impl SubagentTranscript {
    /// The transcript file's path, so a caller can point a reader at it later.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SubagentTranscript {
    /// Create `dir/<id>.jsonl` for one run, creating `dir` if needed.
    ///
    /// **Exclusive.** A run owns its file outright: if `id` is already taken this
    /// returns [`std::io::ErrorKind::AlreadyExists`] so the caller can pick the
    /// next id (see `open_next` in `lib.rs`). Opening in plain append mode would
    /// be wrong — the transcript dir is keyed by *session id* and so survives a
    /// resume, while the id counter restarts at 0 in each process, so a resumed
    /// session would append a fresh run onto a previous run's file. That yields a
    /// file with two `Start`s and two `End`s, and makes [`is_complete`] report a
    /// genuinely orphaned run as complete — defeating the whole point of the log.
    pub fn create(dir: &Path, id: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        // The transcript holds the sub-agent's full prompt and output. Keep the
        // directory owner-only; the file inherits protection from it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        // Owner-only too, not just the directory: a transcript is a verbatim
        // prompt/output log ([`hrdr_llm::owner_only_options`] documents what that
        // buys on each platform).
        let mut opts = hrdr_llm::owner_only_options();
        opts.create_new(true).append(true);
        let path = dir.join(format!("{id}.jsonl"));
        let file = opts.open(&path)?;
        Ok(Self {
            file,
            path,
            // Exclusively created: the file is empty, so it starts on a boundary.
            torn: false,
        })
    }

    /// Open `path` for appending, creating it (and its parent dirs) if absent.
    ///
    /// **Non-exclusive**, unlike [`create`](Self::create): the main agent's
    /// transcript has a *stable* id across resumes — the file survives with the
    /// session and the process reattaches to it — so a resumed session must
    /// continue appending to its existing jsonl, not refuse it. (A sub-agent's id
    /// counter restarts each process, which is exactly why its runs own their
    /// files outright.) The existing records stay put; new events land after them,
    /// and a later [`read_transcript`] folds old and new together in order.
    pub fn append(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            // The transcript holds the agent's full prompt and output; keep its
            // directory owner-only (the file inherits from the mode below).
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        // Same owner-only policy as [`create`](Self::create) — see
        // [`hrdr_llm::owner_only_options`].
        let mut opts = hrdr_llm::owner_only_options();
        opts.create(true).append(true);
        let file = opts.open(path)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            // A file that does not end in a newline was cut mid-record (a crash
            // or a full disk during an earlier append). Resuming onto it must not
            // glue this session's first record onto that fragment.
            torn: ends_mid_line(path),
        })
    }

    /// Append one record as **one whole line** and flush. All errors are
    /// swallowed: a failed transcript write must never break the agent's run.
    ///
    /// A torn line loses two records, not one — the fragment and whatever gets
    /// appended after it — and makes the whole line unparsable, so the guarantee
    /// here is that a record either lands in full or leaves no trace:
    ///
    /// * The record is serialized into a single buffer *including* its trailing
    ///   newline and written with one [`append_all`] loop — never a `write!` per
    ///   field, never a flush that could split it.
    /// * A partial write (the real case seen in the wild: `ENOSPC` with 21 bytes
    ///   of a `Reasoning` record on disk, the next record appended straight onto
    ///   the fragment) is **rolled back** to the pre-write length, so the file
    ///   still ends on a record boundary. Only this handle appends to the file,
    ///   under the `Mutex` its owner holds, so the truncated tail can only ever
    ///   be our own partial bytes.
    /// * If even the rollback fails, [`Self::torn`] is set and the next record
    ///   opens with a newline — the damage stays confined to one line, which
    ///   [`read_transcript`] skips.
    pub fn write(&mut self, rec: &Record) {
        let Ok(json) = serde_json::to_string(rec) else {
            return;
        };
        let mut line = String::with_capacity(json.len() + 2);
        if self.torn {
            line.push('\n');
        }
        line.push_str(&json);
        line.push('\n');
        // The record boundary to roll back to if this append tears.
        let before = self.file.metadata().map(|m| m.len()).ok();
        match append_all(&mut self.file, line.as_bytes()) {
            Ok(()) => self.torn = false,
            // Nothing reached the disk: the file is untouched, so it is exactly as
            // torn (or not) as it was before.
            Err(0) => {}
            Err(_partial) => {
                let rolled = before.is_some_and(|len| self.file.set_len(len).is_ok());
                if !rolled {
                    self.torn = true;
                }
            }
        }
        let _ = self.file.flush();
    }
}

/// [`Write::write_all`], but reporting how many bytes made it out when it fails.
/// `write_all` collapses that to a bare error, and rolling a partial record back
/// needs to know whether anything landed at all.
fn append_all(w: &mut impl Write, buf: &[u8]) -> Result<(), usize> {
    let mut done = 0;
    while done < buf.len() {
        match w.write(&buf[done..]) {
            // A zero-length write makes no progress; treat it as a stalled write
            // rather than spinning forever.
            Ok(0) => return Err(done),
            Ok(n) => done += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(done),
        }
    }
    Ok(())
}

/// Whether `path` ends mid-record — non-empty and not terminated by a newline.
/// Read through its own handle: the transcript's own handle is append/write-only.
/// Unreadable or absent counts as "not torn": a fresh file starts on a boundary.
fn ends_mid_line(path: &Path) -> bool {
    let Ok(mut f) = File::open(path) else {
        return false;
    };
    let Ok(len) = f.seek(SeekFrom::End(0)) else {
        return false;
    };
    if len == 0 {
        return false;
    }
    if f.seek(SeekFrom::Start(len - 1)).is_err() {
        return false;
    }
    let mut last = [0u8; 1];
    f.read_exact(&mut last).is_ok() && last[0] != b'\n'
}

/// A transcript's lines, as decoded text, in order.
///
/// Splits on `\n` over **bytes** rather than using [`BufRead::lines`]: `lines()`
/// yields `Err` for a line that is not valid UTF-8, and the `map_while(ok)` every
/// reader here uses would then silently drop the ENTIRE rest of the file. A torn
/// write can cut a multi-byte character in half (an interrupted append is not
/// character-aligned), so one damaged line must not be able to truncate a resumed
/// transcript. Decoded lossily and left for the caller to parse-or-skip.
fn text_lines(path: &Path) -> Option<impl Iterator<Item = String>> {
    let file = File::open(path).ok()?;
    Some(
        BufReader::new(file)
            .split(b'\n')
            .map_while(Result::ok)
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
    )
}

/// Whether a transcript file ends in an `End` record. A file with no `End` line
/// is an orphan: the sub-agent crashed or is still running. The disk-aware
/// `task_list` reads this to report a resumable run as `done` vs `orphaned`.
///
/// The last *parsable* record decides: a torn fragment at the tail (a full disk
/// during the final append) must not turn a completed run into an orphan.
pub fn is_complete(path: &Path) -> bool {
    let Some(lines) = text_lines(path) else {
        return false;
    };
    let mut last = None;
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<Record>(&line) {
            last = Some(rec);
        }
    }
    matches!(last, Some(Record::End { .. }))
}

/// The opening [`Record::Start`] of a transcript, if it has one. The disk-aware
/// `task_list` uses it to label a run whose `.json` snapshot is absent (a run
/// that crashed before its first `History`-triggered save). `Start` is always
/// the first record a sub-agent run writes, so only the first non-empty line is
/// inspected.
pub fn read_start(path: &Path) -> Option<Record> {
    for line in text_lines(path)? {
        if line.trim().is_empty() {
            continue;
        }
        return match serde_json::from_str::<Record>(&line) {
            Ok(rec @ Record::Start { .. }) => Some(rec),
            _ => None,
        };
    }
    None
}

/// Read a sub-agent transcript file and fold it into a Vec<Entry> using the
/// SAME reducer as the main transcript, so the on-disk sub-agent record renders
/// identically (tool args + results intact). Best-effort: unparsable lines are skipped.
///
/// The MAIN agent's resume path ([`crate::Session::load_path`]) folds its session
/// jsonl through here too — the transcript is no longer embedded in the `.json`.
pub fn read_transcript(path: &Path) -> Vec<crate::Entry> {
    fold_transcript(path).0
}

/// [`read_transcript`] plus how many non-empty lines had to be skipped because
/// they did not parse (a line torn by a full disk or a crash mid-append). Every
/// intact record before AND after the damage still folds: a resumed transcript is
/// never truncated by one bad line. The count is not logged — it exists so a
/// caller (and the tests) can see salvage happened.
fn fold_transcript(path: &Path) -> (Vec<crate::Entry>, usize) {
    let mut entries = Vec::new();
    let mut skipped = 0usize;
    let Some(lines) = text_lines(path) else {
        return (entries, skipped);
    };
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Record>(&line) else {
            skipped += 1;
            continue;
        };
        // Each record folds through the shared event reducer.
        if let Some(ev) = rec.as_event() {
            crate::apply_event(&mut entries, &ev);
        }
    }
    (entries, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_serializes_with_t_tag_and_snake_case() {
        let start = Record::Start {
            model: "m".into(),
            label: "l".into(),
            kind: SpawnKind::Background,
            prompt: "p".into(),
        };
        let s = serde_json::to_string(&start).unwrap();
        assert!(s.contains(r#""t":"start""#), "got {s}");
        assert!(s.contains(r#""kind":"background""#), "got {s}");
        // Round-trips.
        assert_eq!(serde_json::from_str::<Record>(&s).unwrap(), start);

        // A tool call keeps its full args on the wire (the whole point of the
        // complete projection).
        let tool = Record::ToolStart {
            id: "t1".into(),
            name: "edit".into(),
            args: r#"{"path":"src/main.rs"}"#.into(),
        };
        let s = serde_json::to_string(&tool).unwrap();
        assert!(s.contains(r#""t":"tool_start""#), "got {s}");
        assert!(s.contains("src/main.rs"), "args survive serialization: {s}");
        assert_eq!(serde_json::from_str::<Record>(&s).unwrap(), tool);

        let end = Record::End {
            status: EndStatus::Panicked,
            bytes: 3,
        };
        let s = serde_json::to_string(&end).unwrap();
        assert!(
            s.contains(r#""t":"end""#) && s.contains(r#""status":"panicked""#),
            "got {s}"
        );
    }

    #[test]
    fn write_appends_one_line_per_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = SubagentTranscript::create(dir.path(), "001-x").unwrap();
        t.write(&Record::Start {
            model: "m".into(),
            label: "l".into(),
            kind: SpawnKind::Blocking,
            prompt: "p".into(),
        });
        t.write(&Record::Text {
            chunk: "hello".into(),
        });
        t.write(&Record::End {
            status: EndStatus::Ok,
            bytes: 5,
        });
        let body = std::fs::read_to_string(dir.path().join("001-x.jsonl")).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3, "one line per record: {body:?}");
        for l in &lines {
            serde_json::from_str::<Record>(l).expect("each line is a standalone Record");
        }
    }

    /// The whole point of the complete projection: a tool call's args (the path
    /// it touched) and its result (the diff it produced) survive to disk and
    /// back, folded into an `EntryKind::Tool` by the SAME reducer as the main
    /// transcript.
    #[test]
    fn read_transcript_preserves_tool_args_and_result() {
        use crate::EntryKind;
        let dir = tempfile::tempdir().unwrap();
        let mut t = SubagentTranscript::create(dir.path(), "003-edit").unwrap();
        let args = r#"{"path":"src/lib.rs"}"#;
        let result = "@@ -1 +1 @@\n-old line\n+new line";
        t.write(&Record::Start {
            model: "m".into(),
            label: "edit-task".into(),
            kind: SpawnKind::Blocking,
            prompt: "edit the file".into(),
        });
        t.write(&Record::ToolStart {
            id: "call-1".into(),
            name: "edit".into(),
            args: args.into(),
        });
        t.write(&Record::ToolEnd {
            id: "call-1".into(),
            name: "edit".into(),
            result: result.into(),
            ok: true,
        });
        t.write(&Record::End {
            status: EndStatus::Ok,
            bytes: 0,
        });

        let entries = read_transcript(&dir.path().join("003-edit.jsonl"));
        let tool = entries
            .iter()
            .find_map(|e| match &e.kind {
                EntryKind::Tool {
                    name,
                    args,
                    result,
                    ok,
                    ..
                } => Some((name.clone(), args.clone(), result.clone(), *ok)),
                _ => None,
            })
            .expect("a folded Tool entry");
        assert_eq!(tool.0, "edit");
        assert!(
            tool.1.contains("src/lib.rs"),
            "args (path) survive: {}",
            tool.1
        );
        assert!(
            tool.2.contains("new line"),
            "result (diff) survives: {}",
            tool.2
        );
        assert!(tool.3, "ok flag survives");
    }

    /// A run owns its file. The dir is keyed by session id and survives a resume,
    /// but the id counter restarts at 0 each process — so without exclusive
    /// creation a resumed session's first task would append onto the previous
    /// run's log, producing a file with two `Start`s and making an orphaned run
    /// look complete.
    #[test]
    fn create_refuses_to_reuse_an_existing_run_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = SubagentTranscript::create(dir.path(), "000-sub-task").unwrap();
        first.write(&Record::Start {
            model: "m".into(),
            label: "sub-task".into(),
            kind: SpawnKind::Blocking,
            prompt: "first run".into(),
        });
        // No End: the first run crashed. It must stay an identifiable orphan.
        drop(first);

        let err = match SubagentTranscript::create(dir.path(), "000-sub-task") {
            Ok(_) => panic!("an id already on disk must not be reopened"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);

        let path = dir.path().join("000-sub-task.jsonl");
        assert!(!is_complete(&path), "the crashed run is still an orphan");
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 1, "untouched by the second attempt");
    }

    #[cfg(unix)]
    #[test]
    fn transcript_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("subagents");
        let _t = SubagentTranscript::create(&root, "000-x").unwrap();
        let file_mode = std::fs::metadata(root.join("000-x.jsonl"))
            .unwrap()
            .permissions()
            .mode();
        let dir_mode = std::fs::metadata(&root).unwrap().permissions().mode();
        // The transcript carries the sub-agent's full prompt and output.
        assert_eq!(file_mode & 0o777, 0o600, "transcript must be 0600");
        assert_eq!(dir_mode & 0o777, 0o700, "transcript dir must be 0700");
    }

    #[test]
    fn is_complete_flags_orphan_and_preserves_partial_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("002-x.jsonl");
        // One run holds one handle for its whole life — the file is never
        // reopened, so this mirrors the real spawn paths.
        let mut t = SubagentTranscript::create(dir.path(), "002-x").unwrap();
        t.write(&Record::Start {
            model: "m".into(),
            label: "l".into(),
            kind: SpawnKind::Blocking,
            prompt: "p".into(),
        });
        t.write(&Record::Text {
            chunk: "done work".into(),
        });

        // Mid-run, before any End: an orphan whose completed work is on disk.
        assert!(!is_complete(&path), "no End line => orphan");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("done work"),
            "partial work survives the crash"
        );

        // The terminal event lands on the same handle the run has held all along.
        t.write(&Record::End {
            status: EndStatus::Failed,
            bytes: 9,
        });
        assert!(is_complete(&path), "End line => complete");
    }

    /// A sub-agent transcript is a pure fold of the `AgentEvent` stream. This
    /// round-trips an event sequence (Steered + Text + ToolStart/ToolEnd) through
    /// disk and back, asserting the folded transcript reconstructs in log order
    /// with tool args and results intact.
    #[test]
    fn read_transcript_folds_an_event_stream_in_order() {
        use crate::{AgentEvent, EntryKind};
        let dir = tempfile::tempdir().unwrap();
        let mut t = SubagentTranscript::create(dir.path(), "004-main").unwrap();

        // Written order: user turn, assistant reply, a tool call (args + result).
        t.write(&Record::from_event(&AgentEvent::Steered("audit the config".into())).unwrap());
        t.write(&Record::from_event(&AgentEvent::Text("looking now".into())).unwrap());
        t.write(
            &Record::from_event(&AgentEvent::ToolStart {
                id: "call-1".into(),
                name: "read".into(),
                args: r#"{"path":"config.toml"}"#.into(),
            })
            .unwrap(),
        );
        t.write(
            &Record::from_event(&AgentEvent::ToolEnd {
                id: "call-1".into(),
                name: "read".into(),
                result: "port = 8080".into(),
                ok: true,
            })
            .unwrap(),
        );

        let entries = read_transcript(&dir.path().join("004-main.jsonl"));

        // The event-derived tool entry carries its args AND result.
        let tool = entries
            .iter()
            .find_map(|e| match &e.kind {
                EntryKind::Tool { args, result, .. } => Some((args.clone(), result.clone())),
                _ => None,
            })
            .expect("folded Tool entry present");
        assert!(
            tool.0.contains("config.toml"),
            "tool args survive: {}",
            tool.0
        );
        assert!(tool.1.contains("8080"), "tool result survives: {}", tool.1);

        // The assistant text folded from the event stream is present.
        assert!(
            entries
                .iter()
                .any(|e| matches!(&e.kind, EntryKind::Assistant(s) if s == "looking now")),
            "assistant text present"
        );

        // Overall ordering matches the written sequence.
        let kinds: Vec<&EntryKind> = entries.iter().map(|e| &e.kind).collect();
        assert!(
            matches!(kinds.as_slice(),
                [
                    EntryKind::User(u),
                    EntryKind::Assistant(_),
                    EntryKind::Tool { .. },
                ] if u == "audit the config"
            ),
            "reconstructed in log order: {kinds:?}"
        );
    }

    /// A partial write must be *reported* as partial: `write_all` collapses "21
    /// bytes landed, then ENOSPC" into a bare error, and that is exactly the
    /// information the rollback needs.
    #[test]
    fn append_all_reports_how_many_bytes_landed_before_the_error() {
        /// Accepts `cap` bytes in total, then fails the way a full disk does.
        struct FullDisk {
            cap: usize,
            written: usize,
        }
        impl Write for FullDisk {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let room = self.cap.saturating_sub(self.written);
                if room == 0 {
                    return Err(std::io::Error::other("No space left on device"));
                }
                let n = room.min(buf.len());
                self.written += n;
                Ok(n)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut disk = FullDisk {
            cap: 21,
            written: 0,
        };
        // The real shape of the corruption: 21 bytes of the record on disk.
        assert_eq!(
            append_all(&mut disk, b"{\"t\":\"reasoning\",\"text\":\"x\"}\n"),
            Err(21)
        );

        // Nothing accepted at all is `Err(0)` — the file is still on a boundary.
        let mut none = FullDisk { cap: 0, written: 0 };
        assert_eq!(append_all(&mut none, b"abc"), Err(0));

        // A writer with room takes the whole buffer.
        let mut ok = Vec::new();
        assert_eq!(append_all(&mut ok, b"abc\n"), Ok(()));
        assert_eq!(ok, b"abc\n");
    }

    /// **The bug.** A full disk left 21 bytes of a `Reasoning` record on disk with
    /// no newline (`{"t":"reasoning","tex`), and the next record was appended
    /// straight onto that fragment — one unparsable line, two records lost, and a
    /// line-by-line JSON reader dying on it.
    ///
    /// Reopening a transcript that ends mid-record must start the next record on
    /// its own line, so the damage stays confined to the fragment.
    #[test]
    fn a_torn_tail_does_not_swallow_the_next_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("005-torn.jsonl");
        let mut t = SubagentTranscript::append(&path).unwrap();
        t.write(&Record::Text {
            chunk: "before".into(),
        });
        drop(t);
        // Exactly what the ENOSPC append left behind: a record cut mid-key, no
        // trailing newline.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"{\"t\":\"reasoning\",\"tex").unwrap();
        }

        let mut t = SubagentTranscript::append(&path).unwrap();
        t.write(&Record::ToolStart {
            id: "call-1".into(),
            name: "shell".into(),
            args: "{}".into(),
        });
        t.write(&Record::Text {
            chunk: "after".into(),
        });

        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 4, "the fragment is its own line: {body:?}");
        assert_eq!(
            lines[1], "{\"t\":\"reasoning\",\"tex",
            "the fragment stayed a lone broken line"
        );
        for good in [lines[0], lines[2], lines[3]] {
            serde_json::from_str::<Record>(good).expect("every other line is a standalone Record");
        }

        // Read-back salvage: exactly one line skipped, and BOTH the records before
        // and after the damage fold.
        let (entries, skipped) = fold_transcript(&path);
        assert_eq!(skipped, 1, "only the fragment is lost");
        let text: String = entries
            .iter()
            .filter_map(|e| match &e.kind {
                crate::EntryKind::Assistant(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert!(
            text.contains("before") && text.contains("after"),
            "records on both sides of the tear survive: {entries:?}"
        );
        assert!(
            entries
                .iter()
                .any(|e| matches!(&e.kind, crate::EntryKind::Tool { name, .. } if name == "shell")),
            "the record that followed the fragment is no longer swallowed: {entries:?}"
        );
    }

    /// A tear is not character-aligned, so a torn line can hold half a multi-byte
    /// character. `BufRead::lines()` yields `Err` for that, and the readers'
    /// `map_while(ok)` dropped the ENTIRE rest of the file — a resumed session
    /// silently lost every record after the damage.
    #[test]
    fn read_survives_a_line_that_is_not_valid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("006-mojibake.jsonl");
        let mut raw = Vec::new();
        raw.extend_from_slice(br#"{"t":"text","chunk":"first"}"#);
        raw.push(b'\n');
        // A record cut in the middle of a 3-byte character.
        raw.extend_from_slice(b"{\"t\":\"text\",\"chunk\":\"\xe2\x82\n");
        raw.extend_from_slice(br#"{"t":"text","chunk":"second"}"#);
        raw.push(b'\n');
        raw.extend_from_slice(br#"{"t":"end","status":"ok","bytes":0}"#);
        raw.push(b'\n');
        std::fs::write(&path, &raw).unwrap();

        let (entries, skipped) = fold_transcript(&path);
        assert_eq!(skipped, 1);
        let text: String = entries
            .iter()
            .filter_map(|e| match &e.kind {
                crate::EntryKind::Assistant(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert!(
            text.contains("first") && text.contains("second"),
            "the read did not stop at the bad line: {entries:?}"
        );
        assert!(
            is_complete(&path),
            "a run whose End is behind a torn line is still complete"
        );
    }

    /// A tear at the very tail must not turn a finished run into an orphan: the
    /// last *parsable* record decides.
    #[test]
    fn is_complete_ignores_a_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("007-tail.jsonl");
        let mut t = SubagentTranscript::append(&path).unwrap();
        t.write(&Record::End {
            status: EndStatus::Ok,
            bytes: 1,
        });
        drop(t);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"{\"t\":\"reas").unwrap();
        assert!(is_complete(&path));
    }

    /// One serialized writer per file: every append goes through the one
    /// `Mutex`-held handle, so two tasks hammering the same transcript can never
    /// split each other's lines. Each event is a single buffered write including
    /// its newline — the invariant a torn line would break.
    #[test]
    fn concurrent_writers_on_one_handle_never_tear_a_line() {
        use std::sync::Arc;
        const PER_THREAD: usize = 200;
        let dir = tempfile::tempdir().unwrap();
        let writer = Arc::new(std::sync::Mutex::new(
            SubagentTranscript::create(dir.path(), "008-race").unwrap(),
        ));
        let path = dir.path().join("008-race.jsonl");

        let handles: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|tag| {
                let writer = Arc::clone(&writer);
                std::thread::spawn(move || {
                    for i in 0..PER_THREAD {
                        // Long payloads: a big line is what needs more than one
                        // write syscall, and so what tears.
                        let chunk = format!("{tag}-{i}-{}", "x".repeat(4096));
                        let mut w = writer.lock().unwrap_or_else(|p| p.into_inner());
                        w.write(&Record::Text { chunk });
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            PER_THREAD * 2,
            "one line per record, no splits"
        );
        let mut seen = std::collections::HashSet::new();
        for l in &lines {
            match serde_json::from_str::<Record>(l).expect("every line parses standalone") {
                Record::Text { chunk } => {
                    seen.insert(chunk);
                }
                other => panic!("unexpected record {other:?}"),
            }
        }
        for tag in ["a", "b"] {
            for i in 0..PER_THREAD {
                let want = format!("{tag}-{i}-{}", "x".repeat(4096));
                assert!(seen.contains(&want), "missing {tag}-{i}");
            }
        }
    }

    #[test]
    fn is_complete_is_false_for_missing_file() {
        assert!(!is_complete(Path::new("/nonexistent/does/not/exist.jsonl")));
    }

    #[test]
    fn open_error_is_returned_not_panicked() {
        // A path whose parent cannot be created (a file where a dir is needed).
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let bad_dir = blocker.join("subdir"); // parent is a file
        assert!(SubagentTranscript::create(&bad_dir, "id").is_err());
    }
}
