//! Sub-agent delegation, worktrees, and background-task orchestration —
//! extracted from [`Agent`] into its own module to keep `lib.rs` manageable.
//!
//! Holds the `task*` tool family (spawn/list/output/steer/cancel/cleanup/diff),
//! the background-handle registry and detached [`spawn_background`] path, the
//! git worktree lifecycle ([`Worktree`]/[`KeptWorktree`] and their gc/reaping),
//! the sub-agent transcript plumbing, and the per-task config derivation
//! ([`subagent_base_config`], the model-ref overrides, agent-profile resolution).
//! Move-only: behavior is unchanged.

use super::*;

/// Monotonic id source for detached background sub-agents (`task` background mode).
static BG_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Shared list of background-task `JoinHandle`s, keyed by task id.
pub(crate) type BgHandles = Arc<Mutex<Vec<(u64, tokio::task::JoinHandle<()>)>>>;

/// Live sub-agent slots, by capability. Acquired before a `task` spawns and
/// released when it finishes, so the caps bound *concurrent* sub-agents rather
/// than how many a turn may issue in total.
#[derive(Debug, Default)]
pub(crate) struct SubagentSlots {
    read_only: std::sync::atomic::AtomicUsize,
    write: std::sync::atomic::AtomicUsize,
}

impl SubagentSlots {
    /// Take a slot, or `None` when `max` are already running. The compare-and-set
    /// loop matters: several `task` calls in one turn run concurrently, so a
    /// load-then-store would let them all pass a cap of 1.
    pub(crate) fn acquire(self: &Arc<Self>, write: bool, max: usize) -> Option<SubagentSlot> {
        use std::sync::atomic::Ordering;
        let counter = if write { &self.write } else { &self.read_only };
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < max).then_some(n + 1)
            })
            .ok()?;
        Some(SubagentSlot {
            slots: Arc::clone(self),
            write,
        })
    }

    pub(crate) fn live(&self, write: bool) -> usize {
        use std::sync::atomic::Ordering;
        let counter = if write { &self.write } else { &self.read_only };
        counter.load(Ordering::SeqCst)
    }
}

/// A held sub-agent slot; releases on drop, so a panicking or aborted sub-agent
/// can't leak one.
pub(crate) struct SubagentSlot {
    slots: Arc<SubagentSlots>,
    write: bool,
}

impl Drop for SubagentSlot {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        let counter = if self.write {
            &self.slots.write
        } else {
            &self.slots.read_only
        };
        let _ = counter.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            Some(n.saturating_sub(1))
        });
    }
}

/// Create an empty [`BgHandles`] store.
pub(crate) fn bg_handles() -> BgHandles {
    Arc::new(Mutex::new(Vec::new()))
}

/// Spawn `cfg`'s sub-agent detached: it streams into the shared background
/// registry and, on completion, records its result there for the run loop to
/// deliver. Returns immediately with an acknowledgement for the model.
///
/// A write-capable sub-agent is handed its own git `worktree` (its `cfg.cwd` is
/// already pointed at it); the worktree's cleanup is detached here so it survives
/// the run for the parent to review and merge. A read-only sub-agent has
/// `worktree = None` and shares the main dir.
///
/// The task is wrapped in a nested spawn so a panic in the body sets
/// `done = true` with an error message rather than leaving the registry entry
/// live forever. The outer [`JoinHandle`](tokio::task::JoinHandle) is stored in
/// `handles` so [`Agent::clear`] can abort running tasks on session reset.
/// The most of a background sub-agent's final report delivered verbatim into
/// the parent's context, in bytes. The parent needs the answer, not a full
/// re-read of a long run — the durable transcript keeps everything, and an
/// oversized report is middle-truncated (`hrdr_tools::truncate_middle`) with
/// a pointer at the transcript for the rest.
pub(crate) const BACKGROUND_REPORT_MAX_BYTES: usize = 24_000;

/// The git worktree a background sub-agent run operates in.
pub(crate) enum SpawnWorktree {
    /// A freshly created worktree (a `task` spawn): its automatic cleanup is
    /// *detached* here so it outlives the run for the parent to review and merge
    /// (removed only by `task_cancel` / reset). `cfg.cwd` is already pointed at it.
    Fresh(Worktree),
    /// An existing worktree reused by `task_revive`: it is already on disk from
    /// the original run, so it is neither created nor torn down here — the
    /// follow-up's commits stack on the same branch. `repo` is the parent
    /// checkout, where the completion-time size summary's git commands run.
    Existing {
        repo: PathBuf,
        path: PathBuf,
        branch: String,
    },
    /// No worktree — a read-only sub-agent, or a writer sharing the main dir.
    None,
}

/// A sub-agent's prior conversation, restored by `task_revive` into the fresh
/// agent so a follow-up turn continues rather than restarts.
///
/// The persisted `messages` carry the Anthropic signed thinking blocks a Claude
/// sub-agent that died mid-`tool_use` needs to resume byte-exact (the whole
/// reason revive loads the `.json` snapshot, not the display transcript), and the
/// spend/usage seed the run so its cost and gauge count on from where it left off.
pub(crate) struct RestoredContext {
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) session_cost: f64,
    pub(crate) usage: AgentUsage,
}

/// Where a delegated run's state is snapshotted, and everything about the run
/// that never changes once it is spawned.
///
/// A run saves itself twice — on every committed round, and once more when it
/// settles — and those two saves must not be able to describe the same run
/// differently, which is what six separately-captured locals invited. Nothing
/// here is delegation-specific except the destination: the state written is the
/// same [`crate::SessionState`] the session's own agent persists.
struct RunSnapshot {
    /// `<stem>.json`, beside the run's transcript. `None` when there is no
    /// transcript dir to write into (best-effort, the rule the jsonl follows) —
    /// then nothing is snapshotted and the run is not revivable.
    path: Option<PathBuf>,
    name: String,
    read_only: bool,
    model: crate::ModelRef,
    base_url: String,
    cwd: String,
}

impl RunSnapshot {
    /// Write the agent's state beside its transcript.
    ///
    /// The snapshot carries the model-facing `messages` (which the jsonl does not
    /// hold) plus metadata; `transcript` is left EMPTY on purpose — it is the
    /// sibling jsonl, folded back by `read_transcript` on load — so a round never
    /// re-serializes the whole transcript it just appended one line to.
    /// Best-effort: a failed save must never break the run.
    fn save(&self, messages: Vec<ChatMessage>, usage: AgentUsage) {
        let Some(path) = &self.path else {
            return;
        };
        let state = crate::SessionState {
            name: self.name.clone(),
            named_by_user: false,
            read_only: self.read_only,
            model: self.model.clone(),
            base_url: self.base_url.clone(),
            cwd: self.cwd.clone(),
            messages,
            transcript: Vec::new(),
            usage,
            todos: Vec::new(),
            ..Default::default()
        };
        let _ = crate::Session::new(state.persisted()).save_to_path(path);
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_background(
    cfg: AgentConfig,
    prompt: String,
    label: String,
    tool_id: Option<String>,
    slot: SubagentSlot,
    registry: &Arc<Mutex<Vec<hrdr_tools::BackgroundTask>>>,
    handles: &BgHandles,
    cost_total: Arc<std::sync::Mutex<f64>>,
    cost_partial: Arc<std::sync::atomic::AtomicBool>,
    lsp: Option<Arc<hrdr_tools::LspRegistry>>,
    transcript_dir: ChildDirCell,
    live: AgentRegistry,
    // A write-capable sub-agent's isolated worktree (`Fresh` for a new `task`,
    // `Existing` for a `task_revive` continuation), or `None`.
    worktree: SpawnWorktree,
    // A prior conversation to restore before the run (`task_revive`); `None` for a
    // fresh `task`, which starts from an empty context.
    restore: Option<RestoredContext>,
) -> Result<String> {
    use std::sync::atomic::Ordering;
    let id = BG_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let header = format!("↳ task#{id} ({}): {label}", cfg.model.model());
    // Identity for the live registry, taken before `tool_id` is moved into the
    // background-task row below.
    let live_key = AgentRegistry::next_key();
    let tool_id_for_live = tool_id.clone();
    let label_for_live = label.clone();
    let model_for_live = cfg.model.model().to_string();
    let provider_for_live = Some(cfg.model.provider().to_string());
    let base_url_for_live = cfg.base_url.clone();
    // A revived run seeds the pane with its restored counters (so its gauge and
    // cost count on from where it left off); a fresh task starts from zero. The
    // context window is NOT seeded here — the agent publishes its own the moment it
    // attaches below, exactly as the session's agent does.
    let usage_for_live = restore.as_ref().map(|r| r.usage).unwrap_or_default();
    // This run's snapshot identity, captured before `cfg` is moved into
    // `Agent::new` below. One value, so the two save points (every committed
    // round, and once more when the run settles) cannot describe the same run
    // differently. Its `path` is filled in below, once the transcript is open.
    let snapshot = RunSnapshot {
        path: None,
        // The label names the snapshot's session (auto-derived, never user-named).
        name: label.clone(),
        // Capability belongs in the snapshot: without it a `task_revive` of a
        // read-only run cannot tell one from a writer, and rebuilds it
        // write-capable.
        read_only: cfg.read_only,
        model: cfg.model.clone(),
        base_url: cfg.base_url.clone(),
        cwd: cfg.cwd.display().to_string(),
    };
    // Build and register synchronously so `task_steer` can address the id as soon as
    // `task` returns; registration inside the spawned future races the caller.
    let mut sub = Agent::new(cfg)?;
    // A revived sub-agent continues its prior conversation: restore the persisted
    // messages (with the signed thinking blocks a pending tool_use needs) and its
    // running spend, so the follow-up turn stacks on the original run instead of
    // starting from an empty context. `set_messages` rebuilds the system prompt on
    // top of them — the revived run gets today's environment and the current memory
    // index, and a cache split that matches the text it installed.
    if let Some(r) = restore {
        sub.set_messages(r.messages);
        sub.set_session_cost(r.session_cost);
    }
    // Resolve the worktree AFTER the agent is built — so if `Agent::new` fails
    // above, a `Fresh` worktree (still owned by `worktree`) is torn down by its
    // `Drop` rather than being orphaned before it reached any registry. A `Fresh`
    // worktree's automatic cleanup is detached (`keep()`) so it outlives the run
    // for review; an `Existing` one reused by `task_revive` is already persistent
    // — neither created nor removed here, so follow-ups stack on its branch.
    //
    // `repo_cwd` (the parent checkout) is needed at completion time to compute the
    // size summary (`git diff`/`git log` run from there, like `task_diff`, since
    // worktrees share the parent's object store); `worktree` is the (path, branch)
    // recorded on the registry entry.
    let (repo_cwd, worktree): (Option<PathBuf>, Option<(PathBuf, String)>) = match worktree {
        SpawnWorktree::Fresh(w) => {
            let repo = w.repo.clone();
            let kept = w.keep();
            (Some(repo), Some((kept.path, kept.branch)))
        }
        SpawnWorktree::Existing { repo, path, branch } => (Some(repo), Some((path, branch))),
        SpawnWorktree::None => (None, None),
    };
    // (repo cwd, branch) for the completion-time size summary — `None` for a
    // read-only task, which has no worktree/branch to diff.
    let size_summary_target: Option<(PathBuf, String)> = worktree
        .as_ref()
        .map(|(_, branch)| branch.clone())
        .zip(repo_cwd)
        .map(|(branch, repo)| (repo, branch));
    sub.cost_total = cost_total;
    sub.cost_partial = cost_partial;
    sub.ctx.lsp = lsp;
    let steering = steering_queue();
    let sub = Arc::new(tokio::sync::Mutex::new(sub));
    // Open the durable transcript first, so a clone can ride on the live-registry
    // entry (from where `record` writes every event — the delegated run AND any
    // later steered turn) and its path can go onto the background-task row as a
    // `task_output` fallback. `None` when it could not be opened (no session dir
    // yet, or an unwritable one) — best-effort, like every transcript write.
    let transcript: Option<Arc<Mutex<transcript_log::TranscriptLog>>> =
        resolve_child_dir(&transcript_dir)
            .and_then(|dir| open_next_subagent_transcript(&dir, &label))
            .map(|t| Arc::new(Mutex::new(t)));
    let transcript_path = transcript
        .as_ref()
        .and_then(|ts| ts.lock().ok().map(|g| g.path().to_path_buf()));
    live.register(AgentEntry {
        key: live_key,
        bg_id: Some(id),
        tool_id: tool_id_for_live,
        label: label_for_live,
        model: model_for_live.clone(),
        provider: provider_for_live,
        base_url: base_url_for_live,
        effort: None,
        auto_compact: true,
        compaction_reserved: 0,
        todos: Default::default(),
        usage: usage_for_live,
        events: registry::event_log(),
        turn: TurnStats::default(),
        agent: Arc::clone(&sub),
        steering: Arc::clone(&steering),
        running: true,
        compacting: false,
        done: false,
        delivered: false,
        pinned: false,
        // Every event `record`ed against this agent is appended here — its
        // delegated run below, and any steered turn driven later via
        // `send_prompt`, which also goes through `record`. The framing
        // (`Start`/`End`/`Error`) is written directly, from this scope.
        transcript: transcript.clone(),
    });
    // Now that its entry exists, let the agent publish into it: the model,
    // provider, endpoint, effort and context window it is *actually* on, from the
    // agent itself. Attaching before registering published into nothing (a
    // `update` on an absent key is a no-op), which is why this path used to
    // pre-compute a window for the entry by hand.
    //
    // Nothing else holds the lock yet — the run task below is not spawned — so the
    // `try_lock` cannot fail; it is used only because this function is sync.
    if let Ok(mut g) = sub.try_lock() {
        g.attach_live(live.clone(), live_key);
    }
    // The agent's `SessionState` snapshot lives next to its `.jsonl` crash-trail:
    // the sibling `<stem>.json`. No transcript dir (best-effort, same rule the
    // jsonl uses) means no snapshot. This is the resumable/revivable artifact; the
    // jsonl stays as the fine-grained record.
    let snapshot = RunSnapshot {
        path: transcript_path.as_ref().map(|p| p.with_extension("json")),
        ..snapshot
    };
    // The `Start` frame is written synchronously here, BEFORE the run task is
    // spawned, so it precedes every event `record` appends for the run.
    if let Some(ts) = &transcript
        && let Ok(mut t) = ts.lock()
    {
        t.write(&transcript_log::Record::Start {
            model: model_for_live.clone(),
            label: label.clone(),
            prompt: prompt.clone(),
        });
    }
    if let Ok(mut v) = registry.lock() {
        v.push(hrdr_tools::BackgroundTask {
            id,
            tool_id,
            label: label.clone(),
            log: header,
            done: false,
            result: None,
            delivered: false,
            cancelled: false,
            worktree: worktree.as_ref().map(|(path, _)| path.clone()),
            branch: worktree.as_ref().map(|(_, branch)| branch.clone()),
            size_summary: None,
            model: model_for_live.clone(),
            started: Some(std::time::Instant::now()),
            transcript: transcript_path.clone(),
        });
    }
    let ts_inner = transcript.clone();
    let ts_outer = transcript;
    let reg = registry.clone();
    let reg_done = reg.clone();
    // One handle for the inner task (which registers the sub-agent once it
    // exists) and one for the outer guard (which marks it idle on every exit
    // path, including panic and cancellation).
    let live_done = live.clone();
    // The inner task does the actual work; the outer task is the panic guard:
    // it always sets `done = true` + a result, even on panic.
    let handle = tokio::spawn(async move {
        // The slot is released when this task ends — including on abort,
        // since the entire future is dropped.
        let _slot = slot;
        // Single task with catch_unwind so a panic sets done=true and writes a
        // terminal End event rather than crashing and leaving the registry entry
        // live forever. On abort the whole future is dropped — the slot and
        // RunGuard are released, and no stale result reaches the registry or
        // live-subagent store.
        let result = AssertUnwindSafe(async move {
            let mut out = String::new();
            // The contiguous assistant text since the last tool call — reset on
            // every `ToolStart`, appended on every `Text`. At the end of the run
            // this is the sub-agent's final report (its system prompt already
            // tells it that's the hand-off), as opposed to `out`, which is the
            // whole prose stream across every turn including interim narration
            // between tool calls. Only the report belongs in the parent's
            // context; `out` (and the durable transcript) still exist so a
            // run that ends mid-tool-call with no closing text has a fallback.
            let mut final_segment = String::new();
            let result: anyhow::Result<()> = async {
                // Hand the task to the agent as the turn's opening: enqueue it onto
                // the very queue `run` drains. `run` pops it, emits `Steered`, and
                // pushes it into history — so its record opens with the question and
                // not just the answer, exactly as a steered follow-up turn does.
                live.begin_turn(live_key);
                live.enqueue(live_key, crate::Steer::plain(prompt));
                let _run_guard = RunGuard::new(live.clone(), live_key);
                let usage_live = live.clone();
                let mut sub = sub.lock().await;
                loop {
                    sub.run(Arc::clone(&steering), |ev| {
                        // Its run is recorded on its own entry — what it did and what it
                        // spent. This is the *only* way a background sub-agent's work
                        // reaches a frontend: its `task` call returned the instant it was
                        // spawned, so there is no live tool call left to stream through.
                        // `record` also appends the event to this agent's durable
                        // transcript (it holds the writer), so the jsonl is written
                        // exactly once, in order, here — and equally for a steered turn,
                        // which drives `record` through `send_prompt` instead. The
                        // `Start`/`End`/`Error` framing stays written directly, from the
                        // spawn scope, around this run.
                        usage_live.record(live_key, &ev);
                        // On every committed round (a `History` event, emitted with
                        // no dangling tool calls) snapshot this agent's state next
                        // to the jsonl.
                        if let AgentEvent::History(messages) = &ev {
                            snapshot.save(
                                messages.clone(),
                                usage_live.usage(live_key).unwrap_or_default(),
                            );
                        }
                        let chunk = match ev {
                            AgentEvent::Text(t) => {
                                out.push_str(&t);
                                final_segment.push_str(&t);
                                Some(t)
                            }
                            AgentEvent::ToolStart { name, .. } => {
                                // A new tool call starts a fresh segment — whatever
                                // text preceded it was narration, not the report.
                                final_segment.clear();
                                Some(format!("\n· {name}"))
                            }
                            _ => None,
                        };
                        if let Some(c) = chunk
                            && let Ok(mut v) = reg.lock()
                            && let Some(t) = v.iter_mut().find(|t| t.id == id)
                        {
                            t.log.push_str(&c);
                        }
                    })
                    .await?;
                    // A steer may have landed while the turn ran; if so, keep the
                    // agent running and let the next `run` drain it as its opening.
                    // Otherwise the turn is finished. Decided atomically under the
                    // entry lock, so a concurrent steer is never lost.
                    if !live.continue_or_finish(live_key) {
                        break;
                    }
                    live.begin_turn(live_key);
                }
                Ok(())
            }
            .await;
            // Final snapshot from the agent's settled history: the closing assistant
            // text lands AFTER the last `History` event, so the in-loop saves above
            // miss it. Read the retained agent's final messages — the method the
            // session agent's autosave uses.
            if snapshot.path.is_some() {
                let messages = sub.lock().await.messages_owned();
                snapshot.save(messages, live.usage(live_key).unwrap_or_default());
            }
            match result {
                Ok(()) => {
                    let o = out.trim().to_string();
                    if let Some(ts) = &ts_inner
                        && let Ok(mut t) = ts.lock()
                    {
                        // The transcript is the durable full record — its byte
                        // count is the whole run, not the (possibly narrower)
                        // report delivered to the parent below.
                        t.write(&transcript_log::Record::End {
                            status: transcript_log::EndStatus::Ok,
                            bytes: o.len(),
                        });
                    }
                    // Prefer the final segment (the report) over the full prose
                    // stream; fall back to `out` if the run ended mid-tool-call
                    // with no closing text (rare, but the segment would be empty).
                    let segment = final_segment.trim();
                    let report = if segment.is_empty() {
                        o.as_str()
                    } else {
                        segment
                    };
                    if report.is_empty() {
                        "(no text output)".to_string()
                    } else {
                        let over_budget = report.len() > BACKGROUND_REPORT_MAX_BYTES;
                        let mut text =
                            hrdr_tools::truncate_middle(report, BACKGROUND_REPORT_MAX_BYTES);
                        if over_budget && let Some(p) = &transcript_path {
                            text.push_str(&format!(
                                "\n\n(truncated — `task_transcript` with this task's id reads the \
                                 whole run, rendered; the raw file at {} is one JSON record per \
                                 streamed token, don't `read` it)",
                                p.display()
                            ));
                        }
                        text
                    }
                }
                Err(e) => {
                    if let Some(ts) = &ts_inner
                        && let Ok(mut t) = ts.lock()
                    {
                        t.write(&transcript_log::Record::Error {
                            msg: format!("{e:#}"),
                        });
                        t.write(&transcript_log::Record::End {
                            status: transcript_log::EndStatus::Failed,
                            bytes: out.len(),
                        });
                    }
                    format!("(background task failed: {e})")
                }
            }
        })
        .catch_unwind()
        .await;
        let final_result = match result {
            Ok(s) => s,
            Err(panic_err) => {
                let msg = panic_err
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic_err.downcast_ref::<String>().map(|s| s.as_str()))
                    .unwrap_or("(unknown panic)");
                if let Some(ts) = &ts_outer
                    && let Ok(mut t) = ts.lock()
                {
                    t.write(&transcript_log::Record::End {
                        status: transcript_log::EndStatus::Panicked,
                        bytes: 0,
                    });
                }
                format!("(background task panicked: {msg})")
            }
        };
        // Best-effort size summary for the delivery message — computed here (once,
        // at completion, in this async context) rather than in `drain_background`,
        // which is synchronous formatting code and must not shell out. `None` for a
        // read-only task (no worktree/branch) or on any git failure; a missing
        // summary never blocks or fails the delivery.
        let size_summary = match &size_summary_target {
            Some((repo, branch)) => task_size_summary(repo, branch).await,
            None => None,
        };
        if let Ok(mut v) = reg_done.lock()
            && let Some(t) = v.iter_mut().find(|t| t.id == id)
        {
            t.done = true;
            t.result = Some(final_result);
            t.size_summary = size_summary;
        }
        // The sub-agent is idle now (RunGuard's drop inside catch_unwind
        // already sets running=false, done=true), but its answer is still
        // owed to the main agent, so `delivered` stays false — the entry
        // survives the prune until the result is injected via deliver_background.
        live_done.update(live_key, |e| {
            e.running = false;
            e.done = true;
        });
    });
    if let Ok(mut v) = handles.lock() {
        // Best-effort reaping: drop handles for tasks that have already
        // finished. A finished task's result is already recorded in the
        // registry, so dropping the JoinHandle is safe. This keeps the Vec
        // bounded over a long session without requiring an explicit drain.
        // Note: this is best-effort — a panicked task is also considered
        // finished (is_finished returns true) and is reaped here.
        v.retain(|(_, h)| !h.is_finished());
        v.push((id, handle));
    }
    let isolation = if worktree.is_some() {
        " It is write-capable, so it works in its own isolated git worktree; when it \
         finishes you review its changes and merge them into your working dir."
    } else {
        ""
    };
    Ok(format!(
        "Started background task #{id} ({label}) — it runs concurrently in the background. \
         You will be notified automatically, and its result will be delivered to you when it \
         finishes; continue with your other work — do not poll or wait. If you have nothing to \
         do until it finishes, tell the user in one line what it is doing and end your turn.{isolation}"
    ))
}

/// Best-effort size summary for a finished write task: file count and
/// insertions/deletions (`git diff --shortstat`, reformatted as `+ins -del`)
/// plus the commit subjects (`git log --oneline`) — the same two facts
/// `task_diff` would show, surfaced up front in the delivery message so the
/// parent knows the SCALE of the result before deciding how to review it. Run
/// from `repo` (the parent's cwd), like `task_diff`: worktrees share the
/// parent's object store, so `branch` is visible there too. `None` on any git
/// failure or when there is nothing to summarize (no commits) — this must
/// never fail or block a delivery, only enrich it.
pub(crate) async fn task_size_summary(repo: &std::path::Path, branch: &str) -> Option<String> {
    let shortstat_out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--shortstat", &format!("HEAD...{branch}")])
        .output()
        .await
        .ok()?;
    let log_out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "--oneline", &format!("HEAD..{branch}")])
        .output()
        .await
        .ok()?;
    if !shortstat_out.status.success() || !log_out.status.success() {
        return None;
    }
    let shortstat = format_shortstat(&String::from_utf8_lossy(&shortstat_out.stdout));
    let commits = String::from_utf8_lossy(&log_out.stdout).trim().to_string();
    if shortstat.is_none() && commits.is_empty() {
        return None; // nothing landed — no summary worth showing
    }
    let mut out = String::new();
    if let Some(s) = shortstat {
        out.push_str(&format!("  size:     {s}\n"));
    }
    if !commits.is_empty() {
        out.push_str("  commits:\n");
        for line in commits.lines() {
            out.push_str(&format!("    {line}\n"));
        }
    }
    Some(out.trim_end().to_string())
}

/// Reformat `git diff --shortstat`'s output (`" 7 files changed, 182 \
/// insertions(+), 46 deletions(-)"`, with either count clause absent when
/// zero) into the delivery message's compact `"7 files changed, +182 -46"`.
/// `None` for empty input (a branch with commits that net no line changes,
/// e.g. a pure rename).
pub(crate) fn format_shortstat(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let mut parts = raw.split(',');
    let files = parts.next()?.trim().to_string();
    let mut insertions = 0u64;
    let mut deletions = 0u64;
    for clause in parts {
        let clause = clause.trim();
        let Some(n) = clause
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        if clause.contains("insertion") {
            insertions = n;
        } else if clause.contains("deletion") {
            deletions = n;
        }
    }
    Some(format!("{files}, +{insertions} -{deletions}"))
}

/// The shared, lazily-resolved sub-agent transcript directory cell (see
/// [`AgentConfig::child_transcript_dir`]).
pub(crate) type ChildDirCell = Option<std::sync::Arc<std::sync::Mutex<Option<std::path::PathBuf>>>>;

/// Monotonic counter for sub-agent transcript file ids, shared by the blocking
/// and background spawn paths so ids are ordered and unique within a session
/// dir. Separate from `BG_SEQ`, which numbers background-task registry entries.
static SUBAGENT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A transcript file id: `NNN-<slug>`, where `slug` is the sanitized label.
/// `seq` is the pre-fetched counter value.
pub(crate) fn child_transcript_id(seq: u64, label: &str) -> String {
    let lowered: String = label
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = lowered
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug: String = if slug.is_empty() {
        "task".to_string()
    } else {
        slug.chars().take(32).collect()
    };
    format!("{seq:03}-{slug}")
}

/// Read the resolved transcript dir from the shared cell, if the feature is on
/// and a session id has been assigned.
pub(crate) fn resolve_child_dir(cell: &ChildDirCell) -> Option<std::path::PathBuf> {
    cell.as_ref()?.lock().ok()?.clone()
}

/// How many ids to try before giving up on a transcript (best-effort — a run
/// must never fail because we could not name its log).
const SUBAGENT_ID_ATTEMPTS: u64 = 10_000;

/// Open a transcript for one run under `dir`, claiming the next free id.
///
/// The id counter restarts at 0 in every process while `dir` is keyed by session
/// id and survives a resume, so `NNN-<slug>` collides with a previous run's file
/// on the very first task after `/resume` (the default label is `sub-task`, so
/// this is the common case, not a corner). [`TranscriptLog::create`] is
/// exclusive, so a taken id fails and we advance instead of appending a new run
/// onto an old run's log.
///
/// Shared by the blocking and background spawn paths so they cannot drift.
fn open_next_subagent_transcript(
    dir: &std::path::Path,
    label: &str,
) -> Option<transcript_log::TranscriptLog> {
    open_next_subagent_transcript_from(&SUBAGENT_SEQ, dir, label)
}

/// Core of [`open_next_subagent_transcript`] with the id counter injected, so a
/// test can drive it from its own counter instead of poking the process-global
/// one (tests share a process and run in parallel).
pub(crate) fn open_next_subagent_transcript_from(
    seq_source: &std::sync::atomic::AtomicU64,
    dir: &std::path::Path,
    label: &str,
) -> Option<transcript_log::TranscriptLog> {
    for _ in 0..SUBAGENT_ID_ATTEMPTS {
        let seq = seq_source.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = child_transcript_id(seq, label);
        match transcript_log::TranscriptLog::create(dir, &id) {
            Ok(t) => return Some(t),
            // Taken by a previous run (or a concurrent spawn): try the next id.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            // Anything else (unwritable dir, …) is not going to fix itself.
            Err(_) => return None,
        }
    }
    None
}

/// The context window a delegated sub-agent should run against, given the window
/// it would inherit from its parent and the sub-agent's own
/// `(provider, base_url, model)`.
///
/// The Codex endpoint is the only path this fix changes: its account catalog is
/// authoritative and per-model, so a Codex sub-agent ALWAYS re-derives and never
/// carries a wrong inherited preset — the reported overflow (a sub-agent told the
/// old 400k, or a repoint's 272k preset, for a 128k model).
///
/// Every other endpoint keeps the pre-existing behaviour: prefer `inherited`,
/// which may be the parent's endpoint-probed value (a local server's
/// `max_model_len` / `n_ctx`) or a user-configured window — both more exact for
/// this model than a generic catalog — and fall back to the catalog only to fill
/// a gap, never blinding the agent. (A stale `inherited` after a cross-provider
/// `/model` switch is a pre-existing, separately-tracked limitation; correcting it
/// needs the parent's live window published on the delegation runtime.)
/// This is **config**, not display: whatever it returns becomes the child's
/// `AgentConfig::context_window`, which the child then treats as its configured
/// window. What a *running* agent shows comes from the agent itself
/// (`Agent::new` decides it, `publish_chrome` publishes it) — no caller
/// pre-computes a window on an agent's behalf.
pub(crate) fn child_context_window(
    inherited: Option<u32>,
    provider: Option<&str>,
    base_url: &str,
    model: &str,
) -> Option<u32> {
    if base_url == CHATGPT_CODEX_BASE_URL {
        return context_window_for(provider, base_url, model);
    }
    inherited.or_else(|| context_window_for(provider, base_url, model))
}

pub(crate) fn subagent_base_config(config: &AgentConfig) -> AgentConfig {
    let mut base = config.clone();
    base.subagents = false;
    base.mcp = Vec::new();
    // Sub-agents share the parent's language servers (`SubagentTool` hands
    // them its registry Arc) instead of spawning their own set — but still
    // register the LSP tools, which resolve the registry at call time.
    base.lsp = false;
    base.lsp_shared = true;
    // The unnamed default sub-agent runs the main prompt with the full tool set;
    // profiles opt into a persona / read-only scope via `config_for_agent_profile`.
    base.agent_prompt = None;
    base.allowed_tools = None;
    base.read_only = false;
    // Sub-agents never spawn sub-agents, so they never write transcripts.
    base.child_transcript_dir = None;
    // ── The session/sub-agent seam ──────────────────────────────────────────
    // A sub-agent is an agent. It keeps every capability the main agent has;
    // what it may *do* is bounded by its type and permissions (`read_only`,
    // `allowed_tools`), never by the mere fact that it was
    // delegated. Only genuinely structural limits live here:
    //   - it cannot delegate (recursion is bounded to one level), and so
    //   - it writes no sub-agent transcripts of its own.
    // Everything else — memory, compaction, guardrails, hooks, the cost ceiling
    // — is inherited, and the agent works with no UI attached.
    base.delegated = true;
    // The sub-agent model. A bare id is a model on the SAME provider — "Opus
    // drives, Sonnet implements", same endpoint, same key, same bill. A whole
    // `provider://model` moves the sub-agents to another provider, and the endpoint
    // (key, headers, api-version) has to follow it, or they would be sent to the
    // parent's endpoint under another provider's model id.
    // A bare `provider://` takes that provider's DECLARED model — the strict,
    // store-free policy, because a sub-agent's model is not an interactive choice.
    if let Some(spec) = &config.subagent_model
        && let Ok(reference) = strict_spec_ref(config, spec, &config.model)
    {
        let (key, url) = (base.api_key.clone(), base.base_url.clone());
        let parent = AuthContext {
            api_key: key.as_deref(),
            base_url: &url,
        };
        if apply_model_ref(&mut base, reference.clone(), Some(&parent)).is_err() {
            // An unresolvable provider is reported when a `task` actually spawns
            // (where there is somewhere to report it); the identity still stands.
            base.model = reference;
        }
    }
    base
}

/// Move `cfg` onto the identity `reference`: re-derive its endpoint, key,
/// api-version and headers from the provider that identity names, atomically with
/// the identity itself. Endpoint/identity only — does NOT touch persona or tool
/// scope, so it is safe to layer on top of an already-resolved agent profile.
///
/// `parent` is the key-inheritance context (see [`AuthContext`]); passing the
/// caller's own endpoint + key lets a same-endpoint child inherit the credential,
/// and the `same_endpoint` guard inside [`resolve_api_key`] is what stops that key
/// from leaking to a different provider's host.
///
/// The endpoint is re-derived ONLY when the provider changes — because it is a
/// property OF the provider, and a same-provider model change cannot have moved it.
/// (This is now a shortcut rather than a load-bearing rule: re-deriving it would
/// produce the same URL.)
pub(crate) fn apply_model_ref(
    cfg: &mut AgentConfig,
    reference: ModelRef,
    parent: Option<&AuthContext<'_>>,
) -> Result<()> {
    if reference.provider() == cfg.model.provider() {
        cfg.model = reference;
        return Ok(());
    }
    let name = reference.provider().as_str();
    let resolved = resolve(&reference, cfg, parent)?;
    // The provider's CONFIGURED window (a `[providers.*].context_window`, or the
    // ChatGPT preset floor) — a user override, so it outranks the derived one, and
    // it is applied only when the preset actually declares one: most built-ins
    // carry `None`, and overwriting an inherited (probed) window with `None` would
    // blind the agent to how full it is, silently disabling its own compaction.
    if let Some(w) = cfg.resolve_provider(name).and_then(|p| p.context_window) {
        cfg.context_window = Some(w);
    }
    cfg.base_url = resolved.base_url().to_string();
    cfg.api_key = resolved.api_key().map(str::to_string);
    cfg.api_version = resolved.api_version().map(str::to_string);
    cfg.headers = resolved.headers().to_vec();
    cfg.model = reference;
    Ok(())
}

/// The identity a **model spec** names, against the identity `cfg` is already on.
/// This is the **programmatic** entry point — agent profiles (`[[subagent]]`,
/// `agents/*.md`) and the `task` tool's `model` argument.
///
/// The three shapes a source can spell, and only these:
/// - `provider://model` → that exact identity ([`ModelSpec::Full`]);
/// - a bare `model` → [`ModelSpec::ModelOnly`]: same provider, new model;
/// - `provider://` (a provider, no model) → the model that provider itself
///   DECLARES, else an error. NEVER `cfg`'s current model id, which belongs to the
///   provider being left — that silent carry-over is the bug this whole seam
///   exists to kill.
///
/// Note what is deliberately absent: the interactive last-used store
/// ([`model_for_provider`]). A profile is configuration, so it must resolve the
/// same way for everyone — folding in "whatever a human last picked on that
/// provider" would make the same sub-agent run a different model on each
/// developer's machine and a third one in CI. The store is consulted only by the
/// interactive switches (`/login`, the `/model` picker) and by the startup launch
/// fallback, where carrying on with what you were using is precisely the intent.
pub(crate) fn named_spec_ref(cfg: &AgentConfig, spec: Option<&str>) -> Result<Option<ModelRef>> {
    let Some(spec) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let spec: ModelSpec = spec.parse()?;
    strict_spec_ref(cfg, &spec, &cfg.model).map(Some)
}

/// **THE PROGRAMMATIC POLICY** for a [`ModelSpec::ProviderOnly`]: the model that
/// provider itself DECLARES (`[providers.<name>].model`, or a built-in preset's),
/// else an error.
///
/// [`ModelSpec::apply`] answers `None` for that shape precisely so this choice has
/// to be made explicitly, here, by the paths that need a *reproducible* answer.
/// `base` supplies the provider for a bare model id, and nothing else — a
/// `provider://` spec never inherits `base`'s model, which belongs to the provider
/// being LEFT.
pub(crate) fn strict_spec_ref(
    cfg: &AgentConfig,
    spec: &ModelSpec,
    base: &ModelRef,
) -> Result<ModelRef> {
    if let Some(reference) = spec.apply(base) {
        return Ok(reference);
    }
    let ModelSpec::ProviderOnly(p) = spec else {
        unreachable!("apply() answers None only for ProviderOnly");
    };
    let declared = cfg
        .resolve_provider(p.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown provider '{p}' (built-ins: {}, or define [providers.{p}])",
                BUILTIN_PROVIDERS.join(", ")
            )
        })?
        .model;
    let Some(m) = declared else {
        bail!(
            "provider '{p}' needs a model — name one as '{p}://<model>' \
             (it declares no default)"
        );
    };
    Ok(ModelRef::new(p.clone(), &m)?)
}

/// Apply the `task` tool's ad-hoc `model` argument — a [`ModelSpec`] — on top of an
/// already-resolved config (post agent-profile). A bare model id overrides on the
/// provider in force; a `provider://model` (or a `provider://`, which takes the
/// provider's declared model) switches provider too, and that target is auth-gated
/// here — fail fast, before spawning.
pub(crate) fn apply_task_overrides(
    cfg: &mut AgentConfig,
    parent: &AgentConfig,
    spec: Option<&str>,
) -> Result<()> {
    // The identity this delegation runs on.
    //
    // A `task` must be REPRODUCIBLE. When it names a provider but no model, the
    // model comes from what the provider itself declares — never from the
    // interactive last-used store. Consulting that store would make the same
    // delegation resolve to a different model on a developer's machine than in CI,
    // depending on what a human last happened to pick. The last-used fallback is
    // for *interactive* switches (`/login`, the `/model` picker), where "carry on
    // with what I was using" is the whole point; a spawned sub-agent is not that.
    let reference = named_spec_ref(cfg, spec).map_err(|e| anyhow::anyhow!("task: {e:#}"))?;
    let Some(reference) = reference else {
        return Ok(());
    };
    // A change of PROVIDER is what needs gating: the sub-agent is about to be sent
    // to another endpoint, with another credential.
    let switching = reference.provider() != cfg.model.provider();
    if switching {
        let pname = reference.provider().as_str();
        let p = cfg.resolve_provider(pname).ok_or_else(|| {
            anyhow::anyhow!(
                "task: unknown provider '{pname}' (built-ins: {}, or define [providers.{pname}])",
                BUILTIN_PROVIDERS.join(", ")
            )
        })?;
        let current_auth = provider_auth_state(
            pname,
            &p,
            cfg.api_key.as_deref(),
            Some(cfg.base_url.as_str()),
        );
        let parent_auth = provider_auth_state(
            pname,
            &p,
            parent.api_key.as_deref(),
            Some(parent.base_url.as_str()),
        );
        if current_auth == ProviderAuthState::Missing && parent_auth == ProviderAuthState::Missing {
            // Only suggest an env var when the provider actually reads one;
            // key_env-less providers (chatgpt OAuth, a keyless [providers.*])
            // would be sent chasing a var that resolve_api_key never consults.
            let hint = match p.key_env.as_deref() {
                Some(env) => format!("set ${env}, or run /login"),
                None => format!(
                    "run /login, or add an `api_key`/`key_env` to a [providers.{pname}] entry"
                ),
            };
            bail!("task: provider '{pname}' is not configured — {hint}");
        }
    }
    // Key inheritance: the CHILD's own context first (it may already sit on this
    // endpoint), then the parent's. `AuthContext` carries the endpoint each key
    // belongs to, so `resolve_api_key`'s `same_endpoint` guard can refuse to hand
    // a credential to a different provider's host. Snapshotted (owned) because
    // `apply_model_ref` mutates the very config they borrow from.
    let (child_key, child_url) = (cfg.api_key.clone(), cfg.base_url.clone());
    let child_ctx = AuthContext {
        api_key: child_key.as_deref(),
        base_url: &child_url,
    };
    let parent_ctx = AuthContext {
        api_key: parent.api_key.as_deref(),
        base_url: parent.base_url.as_str(),
    };
    let inherited = resolve(&reference, cfg, Some(&child_ctx))
        .ok()
        .and_then(|r| r.api_key().map(str::to_string))
        .or_else(|| {
            resolve(&reference, cfg, Some(&parent_ctx))
                .ok()
                .and_then(|r| r.api_key().map(str::to_string))
        });
    apply_model_ref(cfg, reference, Some(&child_ctx))?;
    if switching {
        cfg.api_key = inherited;
    }
    Ok(())
}

/// Apply a named agent profile onto `base`: (if the profile names a provider)
/// switch the identity — endpoint, auth, headers, and `api-version` follow it — so
/// the agent can run on a **different provider**, then set the persona, tool
/// scope, and runtime knobs. Used both for delegated sub-agents (with a
/// [`subagent_base_config`] base) and for `--agent` primary mode (applied directly
/// onto the main config, keeping delegation + MCP).
pub fn config_for_agent_profile(
    base: &AgentConfig,
    profile: &SubagentProfile,
) -> Result<AgentConfig> {
    let mut cfg = base.clone();
    let spec = profile.model.as_ref().map(ModelSpec::to_string);
    if let Some(reference) = named_spec_ref(&cfg, spec.as_deref())? {
        // The profile's own endpoint inherits the parent's key only across the
        // SAME endpoint (`resolve_api_key`'s guard) — a profile naming another
        // provider must not be handed this one's credential. Snapshotted: the
        // apply below mutates the config these borrow from.
        let (key, url) = (cfg.api_key.clone(), cfg.base_url.clone());
        let parent_ctx = AuthContext {
            api_key: key.as_deref(),
            base_url: &url,
        };
        apply_model_ref(&mut cfg, reference, Some(&parent_ctx))?;
    }
    // Persona + tool scope: an explicit `tools` list wins; otherwise `read_only`
    // (resolved to the read-only tool set in `Agent::new`, which has the registry).
    cfg.agent_prompt = profile.prompt.clone();
    cfg.allowed_tools = profile.tools.clone();
    cfg.read_only = profile.is_read_only();
    // Per-agent runtime knobs, each inheriting the main agent's when omitted.
    if profile.temperature.is_some() {
        cfg.temperature = profile.temperature;
    }
    if profile.effort.is_some() {
        cfg.effort = profile.effort.clone();
    }
    if let Some(s) = profile.max_steps {
        cfg.max_steps = s;
    }
    Ok(cfg)
}

/// The `task` tool: delegate a self-contained sub-task to a fresh sub-agent that
/// has its own context and (optionally) a different model **or provider**. The
/// sub-agent runs to completion and its final text becomes the tool result; its
/// tool activity is streamed to the parent as live output.
pub(crate) struct SubagentTool {
    /// Base policy for derived sub-agents (endpoint/model are overlaid live).
    base: AgentConfig,
    runtime: SharedDelegationRuntime,
    /// Named provider+model profiles selectable via the `agent` argument.
    profiles: Vec<SubagentProfile>,
    /// Description string (leaked once at startup — lists the configured
    /// profiles so the model knows what it can delegate to).
    description: &'static str,
    /// Registry of background-task `JoinHandle`s, shared with the owning
    /// [`Agent`] so it can abort live tasks on `clear()` / session reset.
    pub(crate) bg_handles: BgHandles,
    /// Concurrency caps: `(read-only, write-capable)`.
    caps: (usize, usize),
    /// Slots held by the sub-agents running right now.
    pub(crate) slots: Arc<SubagentSlots>,
    /// The owning agent's session cost counter — every sub-agent spawned here
    /// adds its spend to it, so `/cost` and the `max_cost` budget see the
    /// whole tree, not just the main loop.
    cost_total: Arc<std::sync::Mutex<f64>>,
    /// The owning agent's "cost total is a floor" flag — a sub-agent that runs
    /// an unpriced call (with `allow_unpriced`) sets it, so the whole tree's
    /// reported total admits it excludes unpriced usage.
    cost_partial: Arc<std::sync::atomic::AtomicBool>,
    /// The owning agent's language servers, shared with every sub-agent (the
    /// base config has `lsp = false`, so none builds a registry of its own).
    lsp: Option<Arc<hrdr_tools::LspRegistry>>,
    /// The parent session's transcript dir cell (see
    /// [`AgentConfig::child_transcript_dir`]); read at spawn.
    transcript_dir: ChildDirCell,
    /// Every sub-agent spawned here is registered so the frontend can steer it,
    /// display it, and drive further turns on it. See [`AgentRegistry`].
    live: AgentRegistry,
    /// The parent tree's uncommitted paths as of the last time a spawn warned
    /// about them (see the dirty-tree note in [`Self::spawn_task`]). Kept so a
    /// fan-out of four parallel tasks doesn't repeat one warning four times,
    /// while a *changed* working dir — the model committed some of it, or dirtied
    /// something new — warns again.
    dirty_tree_warned: Arc<std::sync::Mutex<Option<Vec<String>>>>,
}

impl SubagentTool {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        base: AgentConfig,
        runtime: SharedDelegationRuntime,
        profiles: Vec<SubagentProfile>,
        bg_handles: BgHandles,
        cost_total: Arc<std::sync::Mutex<f64>>,
        cost_partial: Arc<std::sync::atomic::AtomicBool>,
        lsp: Option<Arc<hrdr_tools::LspRegistry>>,
        transcript_dir: ChildDirCell,
        live: AgentRegistry,
    ) -> Self {
        let caps = (base.max_readonly_subagents, base.max_write_subagents);
        let mut desc = String::from(
            "Delegate a self-contained sub-task to a fresh sub-agent with its own context. It \
             CANNOT see this conversation or anything you know — it gets only its system prompt \
             and the `prompt` you pass — so make `prompt` complete and standalone. Use it to \
             keep the main context clean: broad exploration, or a focused piece of \
             implementation. The sub-agent has the normal tools (read/write/edit/bash/grep/…) \
             but can't itself delegate. Every task runs in the **background**: this call returns \
             immediately with a task id and the sub-agent's result is delivered to you \
             automatically when it finishes — keep working, spawn more, or (if you can't proceed \
             until it's done) tell the user in one line what it's doing and end your turn. Never \
             poll or wait. Issue several `task` calls at once to run sub-agents in **parallel**. \
             A write-capable sub-agent runs in an isolated git worktree — when it reports back, \
             review its changes and merge them before they affect your working dir (if the \
             project is not a git repo it instead edits your dir directly, and only one write \
             sub-agent runs at a time); a read-only sub-agent shares your dir and changes \
             nothing. Run cheaper/faster work on another `model` (see the `model` parameter)",
        );
        if profiles.is_empty() {
            desc.push('.');
        } else {
            desc.push_str(
                ", or delegate to a specialized `agent`. **Proactively** reach for a matching \
                 agent when a sub-task fits its role (don't wait to be asked) — the ★ ones \
                 especially:\n",
            );
            for p in &profiles {
                // ONE key, so ONE label: `provider · model` for a whole identity, the
                // bare model id for a model on the provider in force, and nothing at
                // all when the profile names neither.
                let mut tags = match &p.model {
                    Some(ModelSpec::Full(r)) => format!("{} · {}", r.provider(), r.model()),
                    Some(ModelSpec::ModelOnly(m)) => m.clone(),
                    // The provider, at whatever model it declares — resolved when the
                    // sub-agent actually spawns, so the label names the provider only.
                    Some(ModelSpec::ProviderOnly(p)) => p.to_string(),
                    None => "main provider".to_string(),
                };
                if p.is_read_only() {
                    tags.push_str(" · read-only");
                }
                let star = if p.is_proactive() { "★ " } else { "" };
                desc.push_str(&format!("- {star}{} ({tags})", p.name));
                if let Some(d) = &p.description {
                    desc.push_str(&format!(" — {d}"));
                }
                desc.push('\n');
            }
        }
        Self {
            base,
            runtime,
            profiles,
            description: Box::leak(desc.into_boxed_str()),
            bg_handles,
            caps,
            slots: Arc::new(SubagentSlots::default()),
            cost_total,
            cost_partial,
            lsp,
            transcript_dir,
            live,
            dirty_tree_warned: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Whether this uncommitted-path set is worth warning about again — see
    /// [`dirty_state_is_news`].
    fn dirty_state_is_news(&self, dirty: &[String]) -> bool {
        dirty_state_is_news(&self.dirty_tree_warned, dirty)
    }
}

/// Whether `dirty` is news against the last dirty state warned about, recording
/// it as the new baseline when it is.
///
/// One warning per distinct dirty state: a fan-out of four parallel tasks shares
/// the one working dir, and four copies of the same note is noise the model
/// learns to skim past — but a tree that *changed* (some of it committed,
/// something new dirtied) is a fresh fact and says so. A poisoned lock warns
/// rather than staying quiet: a repeated nudge beats a missed one.
fn dirty_state_is_news(warned: &std::sync::Mutex<Option<Vec<String>>>, dirty: &[String]) -> bool {
    warned
        .lock()
        .map(|mut seen| {
            let news = seen.as_deref() != Some(dirty);
            if news {
                *seen = Some(dirty.to_vec());
            }
            news
        })
        .unwrap_or(true)
}

#[async_trait::async_trait]
impl hrdr_tools::Tool for SubagentTool {
    fn name(&self) -> &'static str {
        "task"
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn parameters(&self) -> serde_json::Value {
        let mut props = serde_json::json!({
            "description": {
                "type": "string",
                "description": "A 3-6 word label for the sub-task (shown to the user)."
            },
            "prompt": {
                "type": "string",
                "description": "The complete, standalone task for the sub-agent: what to do and exactly what to report back."
            },
            "model": {
                "type": "string",
                "description": "Optional model override, named as `provider://model` or as a bare model id. A bare id (`gpt-5.5-mini`, `deepseek/deepseek-chat`) is that model on the provider you are already on. A `provider://model` (`openrouter://deepseek/deepseek-chat`) also switches the provider — it must be one that is configured and authenticated (a built-in name or a [providers.*] entry); `provider://` on its own uses that provider's configured default model. Defaults to the profile's / configured subagent model, else the main model."
            }
        });
        if !self.profiles.is_empty() {
            let names: Vec<&str> = self.profiles.iter().map(|p| p.name.as_str()).collect();
            props["agent"] = serde_json::json!({
                "type": "string",
                "enum": names,
                "description": "Optional named sub-agent profile (see this tool's description) — runs on that profile's provider + model."
            });
        }
        serde_json::json!({
            "type": "object",
            "properties": props,
            "required": ["prompt"]
        })
    }

    fn read_only(&self) -> bool {
        false
    }

    // Each sub-agent runs in its own isolated context, so multiple `task` calls
    // in one turn run concurrently (parallel exploration/implementation).
    fn concurrent(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let mut prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .filter(|p| !p.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("task needs a non-empty `prompt` argument"))?
            .to_string();

        let mut cfg = self.base.clone();
        let runtime = self
            .runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        // The parent's LIVE resolved endpoint, whole — identity, endpoint, key,
        // api-version and headers together, exactly as the parent resolved them.
        // Overlaying them one at a time is what let a sub-agent end up on one
        // provider's endpoint with another's model.
        let live = runtime.endpoint.resolved;
        cfg.base_url = live.base_url().to_string();
        cfg.api_key = live.api_key().map(str::to_string);
        cfg.api_version = live.api_version().map(str::to_string);
        cfg.headers = live.headers().to_vec();
        cfg.model = live.reference().clone();
        cfg.effort = runtime.endpoint.effort;
        // The parent's *live* endpoint + key, captured before the configured
        // sub-agent model or an agent profile can repoint `cfg` away from it. This —
        // not `self.base` — is the context an ad-hoc provider switch inherits auth
        // from. `self.base` names the endpoint the session *launched* on, and a
        // `/model` switch since then would leave the gate judging a provider against
        // an endpoint the session left long ago: an ad-hoc delegation back to the
        // provider you are currently using could be rejected as "not configured".
        let live_parent = cfg.clone();
        // The configured sub-agent model (`--subagent-model` / `subagent_model`): a
        // bare id rides on the parent's PROVIDER and never changes which endpoint the
        // request is sent to; a whole `provider://model` moves the endpoint with it.
        if let Some(spec) = &runtime.explicit_subagent_model {
            // Strict, store-free: a `provider://` takes that provider's declared
            // model, or the delegation fails — it never takes whatever a human last
            // picked there, which would make this `task` run a different model on
            // every machine.
            let reference = strict_spec_ref(&cfg, spec, live.reference())?;
            let parent_ctx = AuthContext {
                api_key: live.api_key(),
                base_url: live.base_url(),
            };
            apply_model_ref(&mut cfg, reference, Some(&parent_ctx))?;
        }

        if let Some(name) = args.get("agent").and_then(|v| v.as_str())
            && !name.trim().is_empty()
        {
            let profile = self
                .profiles
                .iter()
                .find(|p| p.name.eq_ignore_ascii_case(name.trim()))
                .ok_or_else(|| {
                    let known: Vec<&str> = self.profiles.iter().map(|p| p.name.as_str()).collect();
                    anyhow::anyhow!(
                        "unknown subagent '{name}' (configured: {})",
                        known.join(", ")
                    )
                })?;
            // No `last_model_on` escape here, deliberately: a profile-driven
            // delegation is as programmatic as a `task` arg, so its model must come
            // from the profile, the `task` call, or the provider's own default —
            // never from the interactive last-used store, which would make the same
            // sub-agent run a different model for each developer.
            //
            // Worktree isolation is applied to *every* write-capable sub-agent
            // below, by capability — there is no per-profile opt-in/out.
            cfg = config_for_agent_profile(&cfg, profile)
                .map_err(|e| anyhow::anyhow!("subagent '{}': {e:#}", profile.name))?;
        }
        cfg.cwd = ctx.cwd.clone();
        // Inherit the parent's resolved memory roots, so the sub-agent shares the
        // repo's PROJECT memory. Set here, before a write sub-agent's cwd is
        // repointed at its worktree below — otherwise its project scope would key
        // by the worktree slug and resolve to a throwaway, empty store.
        cfg.memory_roots = ctx.memory_project.clone().zip(ctx.memory_global.clone());
        // ONE argument for the one identity: a bare model id (same provider) or a
        // whole `provider://model`.
        let model_arg = args
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        apply_task_overrides(&mut cfg, &live_parent, model_arg)?;
        if cfg.has_default_model() {
            bail!(
                "no model configured — set `model` in config.toml, $HRDR_MODEL, or pass \
                 `--model` / `--subagent-model` on the CLI"
            );
        }
        // Resolve the window for the sub-agent's OWN (endpoint, model) now that both
        // are final (endpoint overlay, profile, and task overrides all applied). The
        // value inherited from the parent describes the parent's model/provider;
        // carrying it onto a different one is the overflow bug (e.g. a ChatGPT
        // parent's window following a plain delegation onto a smaller model). Runs
        // before both the background and blocking spawns below.
        cfg.context_window = child_context_window(
            cfg.context_window,
            Some(cfg.model.provider().as_str()),
            &cfg.base_url,
            cfg.model.model(),
        );
        let label = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("sub-task")
            .to_string();

        // Every task runs **detached**: spawn and return immediately so the
        // sub-agent never blocks the main conversation. The run loop delivers its
        // result when it lands (the frontend shows live progress). There is no
        // foreground mode — if the parent needs the answer before its next step it
        // acknowledges the task and ends its turn; it is woken on completion.
        //
        // Isolation is decided by capability, not a flag. A write-capable
        // sub-agent runs in its OWN git worktree, so concurrent writers never step
        // on each other or on the main tree. When the project is NOT a git repo,
        // there are no worktrees to hand out, so a writer falls back to sharing the
        // main dir — and then only ONE write-capable sub-agent may run at a time,
        // or two of them would race on the same files. A read-only sub-agent
        // always shares the dir; it changes nothing, so there is nothing to race.
        let write_capable = !cfg.read_only;
        let worktrees_available = write_capable && in_git_repo(&ctx.cwd);

        // Isolation is enforced by pointing the sub-agent's cwd at a private
        // worktree — but that only redirects RELATIVE paths. An absolute path
        // INTO the parent checkout (`<cwd>/sub/path`) in the brief escapes the
        // worktree entirely: with full-FS access and no cwd confinement, the
        // sub-agent's tools operate on it directly, so its edits and commits
        // land in the user's live tree, silently defeating isolation and racing
        // the main agent (it has destroyed untracked user files this way).
        // Rather than reject the brief, rewrite those paths to be
        // project-relative (which resolve inside the worktree) and tell the
        // delegating model what changed so it does it right next time.
        let mut path_rewrite_note: Option<String> = None;
        if worktrees_available {
            let parent_prefix = format!("{}/", ctx.cwd.display());
            if prompt.contains(&parent_prefix) {
                let n = prompt.matches(&parent_prefix).count();
                prompt = prompt.replace(&parent_prefix, "");
                path_rewrite_note = Some(format!(
                    "NOTE: your brief named the parent checkout's absolute path \
                     `{}` ({n} occurrence{}). A write sub-agent runs in its OWN isolated git \
                     worktree, where an absolute path to the parent tree escapes isolation and \
                     would route its edits and commits into your live working directory — so the \
                     harness stripped that prefix, leaving project-relative paths that resolve \
                     inside the worktree. The task ran on the rewritten brief. Write briefs with \
                     project-relative paths (`crates/foo/src/bar.rs`, not \
                     `{}/crates/foo/src/bar.rs`) from the start.",
                    ctx.cwd.display(),
                    if n == 1 { "" } else { "s" },
                    ctx.cwd.display(),
                ));
            }
        }

        // A worktree is a fresh checkout of HEAD, so everything uncommitted in the
        // parent tree is invisible inside it. The failure this catches is a common
        // and expensive one: the parent lays the groundwork itself — a new module,
        // a trait, a renamed symbol, a scaffold every delegated chunk builds on —
        // then delegates without committing it. The sub-agent forks from a HEAD
        // that predates all of it, so it codes against a tree where the thing it
        // was told to extend does not exist. It reports back confused, or
        // reinvents the scaffold, and its diff won't apply cleanly either way.
        //
        // The task is spawned regardless — most dirt is genuinely irrelevant to
        // the brief, and refusing to delegate while a CHANGELOG edit sits unstaged
        // would be its own kind of wrong — but the note rides back with the ack so
        // the model can still `task_cancel`, commit, and re-delegate before the
        // sub-agent has got anywhere.
        let mut dirty_tree_note: Option<String> = None;
        if worktrees_available
            && let Some(dirty) = worktree_dirty_files(&ctx.cwd).await
            && !dirty.is_empty()
            && self.dirty_state_is_news(&dirty)
        {
            dirty_tree_note = Some(format!(
                "NOTE: your working dir has {} uncommitted change{} ({}). This sub-agent's \
                     worktree is a fresh checkout of HEAD, so NONE of it is visible to it. If any \
                     of that work is groundwork this task builds on — a new file, module, trait, \
                     or rename it was told to extend — it is now working against a tree where \
                     that doesn't exist: `task_cancel` it, commit the groundwork, and delegate \
                     again. Before the next fan-out, get the tree clean: commit what the \
                     sub-agents need, and stash or set aside the scratch they don't.",
                dirty.len(),
                if dirty.len() == 1 { "" } else { "s" },
                short_file_list(&dirty, 8),
            ));
        }

        // Bound how many run at once. Read-only agents get the higher cap. A
        // worktree-isolated writer gets the write cap; a shared-dir writer (no git
        // repo) is limited to one at a time so concurrent writers can't collide.
        let (max_readonly, max_write) = self.caps;
        let cap = match (write_capable, worktrees_available) {
            (false, _) => max_readonly,
            (true, true) => max_write,
            (true, false) => 1,
        };
        let kind = if write_capable {
            "write-capable"
        } else {
            "read-only"
        };
        let Some(slot) = self.slots.acquire(write_capable, cap) else {
            let hint = if write_capable && !worktrees_available {
                " (this directory is not a git repo, so write sub-agents run one at a time)"
            } else {
                ""
            };
            bail!(
                "too many sub-agents: {} {kind} already running (limit {cap}){hint}. Wait for one \
                 to finish — you are notified automatically — then try again, or run this work \
                 yourself.",
                self.slots.live(write_capable),
            );
        };

        let worktree = if worktrees_available {
            // Isolate the writer in its own worktree; the parent reviews and merges
            // it when the sub-agent reports back.
            let wt = Worktree::create(&ctx.cwd)
                .await
                .context("creating an isolated worktree for a write-capable sub-agent")?;
            cfg.cwd = wt.path.clone();
            ctx.emit(format!("  · isolated worktree: {}\n", wt.path.display()));
            SpawnWorktree::Fresh(wt)
        } else {
            // Read-only, or a writer with no git repo to isolate into: share the
            // main dir. A shared-dir writer's edits land directly in the working
            // dir (serialized by the cap of 1 above), so there is no worktree to
            // review — the changes are simply already there.
            if write_capable {
                ctx.emit(
                    "  · no git repo — sub-agent works directly in the working dir\n".to_string(),
                );
            }
            SpawnWorktree::None
        };

        // Hand the sub-agent a VERIFIED map of the project's layout. It starts
        // cold — no conversation, no memory of the tree — so it otherwise guesses
        // crate paths from names it invented, and a run has burned millions of
        // tokens grepping directories that never existed; sibling agents that ran
        // `tree` first made zero path errors. This is that tree, already in hand.
        // It rides in the volatile task payload on purpose: the system prompt's
        // sections are ordered least-volatile-first for cache reuse, and per-task
        // text there would break the prefix.
        if let Some(map) = workspace_map(&ctx.cwd) {
            prompt.push_str("\n\n");
            prompt.push_str(&map);
        }

        let ack = spawn_background(
            cfg,
            prompt,
            label,
            ctx.call_id.clone(),
            slot,
            &ctx.background_tasks,
            &self.bg_handles,
            Arc::clone(&self.cost_total),
            Arc::clone(&self.cost_partial),
            self.lsp.clone(),
            self.transcript_dir.clone(),
            self.live.clone(),
            worktree,
            None,
        )?;
        // Surface anything the spawn wants the delegating model to know — a
        // rewritten path, an uncommitted parent tree — alongside the ack.
        let notes: Vec<String> = [path_rewrite_note, dirty_tree_note]
            .into_iter()
            .flatten()
            .collect();
        Ok(match notes.is_empty() {
            true => ack,
            false => format!("{ack}\n\n{}", notes.join("\n\n")),
        })
    }
}

/// Hard cap on the injected workspace map, in bytes. It is per-task context a
/// sub-agent pays for on every turn of its run, so it stays a map, not an
/// inventory: two levels of directories and the workspace crates, nothing else.
pub(crate) const WORKSPACE_MAP_MAX: usize = 1500;

/// Top-level directories (2 levels, dirs only, `.gitignore`-honouring) plus, for
/// a cargo workspace, its member crate paths — the layout a freshly spawned
/// sub-agent would otherwise have to discover or, worse, invent. `None` when
/// there is nothing worth saying (an empty or non-project directory).
///
/// Capped at [`WORKSPACE_MAP_MAX`]: the member list is the part that stops
/// hallucinated crate names, so when the budget runs out it is the directory
/// lines that get elided, not the crates.
pub(crate) fn workspace_map(root: &std::path::Path) -> Option<String> {
    use std::collections::{BTreeMap, BTreeSet};
    // Two levels of directories. `ignore` skips dotdirs and anything
    // `.gitignore`d, so `target/`, `node_modules/` and friends stay out.
    let mut tops: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for entry in ignore::WalkBuilder::new(root)
        .max_depth(Some(2))
        .build()
        .flatten()
    {
        if !entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let mut parts = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string());
        // The root itself has no components — skipped by the `else` below.
        let Some(top) = parts.next() else {
            continue;
        };
        match parts.next() {
            Some(child) => {
                tops.entry(top).or_default().insert(child);
            }
            None => {
                tops.entry(top).or_default();
            }
        }
    }

    // Cargo workspace members, glob patterns expanded to the real directories —
    // the verified spelling of every crate path in the repo.
    let members_line = workspace_members(root).map(|members| {
        format!(
            "cargo workspace members: {}\n",
            short_file_list(&members, 40)
        )
    });

    // An empty or non-project directory has nothing worth a section.
    if tops.is_empty() && members_line.is_none() {
        return None;
    }

    let mut out = String::from("Workspace layout (verified — don't guess paths):\n");
    // Reserve room for the members line and the elision note up front, so the
    // parts that matter most are never the ones cut.
    let reserved = members_line.as_ref().map_or(0, String::len) + 48;
    let budget = WORKSPACE_MAP_MAX.saturating_sub(out.len() + reserved);
    let mut used = 0usize;
    let mut elided = 0usize;
    for (top, children) in &tops {
        let kids: Vec<String> = children.iter().take(12).cloned().collect();
        let more = children.len().saturating_sub(kids.len());
        let mut line = format!("  {top}/");
        if !kids.is_empty() {
            line.push_str(&format!(" → {}", kids.join(", ")));
            if more > 0 {
                line.push_str(&format!(", … +{more}"));
            }
        }
        line.push('\n');
        if used + line.len() > budget {
            elided += 1;
            continue;
        }
        used += line.len();
        out.push_str(&line);
    }
    if elided > 0 {
        out.push_str(&format!("  … and {elided} more top-level dir(s)\n"));
    }
    if let Some(line) = members_line {
        out.push_str(&line);
    }
    // Belt and braces: the reservations above keep this from firing, but a map
    // that grew past the cap must be cut rather than shipped.
    if out.len() > WORKSPACE_MAP_MAX {
        let mut cut = WORKSPACE_MAP_MAX;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("…\n");
    }
    Some(out)
}

/// A cargo workspace's member directories, read from `root/Cargo.toml` and glob-
/// expanded (`crates/*` → the crate dirs that actually exist). `None` when there
/// is no root manifest or no `[workspace]` in it.
fn workspace_members(root: &std::path::Path) -> Option<Vec<String>> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let doc: toml::Value = manifest.parse().ok()?;
    let members = doc.get("workspace")?.get("members")?.as_array()?;
    let mut out: Vec<String> = Vec::new();
    for pattern in members.iter().filter_map(|m| m.as_str()) {
        if pattern.contains('*') {
            let Ok(paths) = glob::glob(&root.join(pattern).to_string_lossy()) else {
                continue;
            };
            let mut hits: Vec<String> = paths
                .flatten()
                .filter(|p| p.join("Cargo.toml").is_file())
                .filter_map(|p| {
                    p.strip_prefix(root)
                        .ok()
                        .map(|r| r.to_string_lossy().replace('\\', "/"))
                })
                .collect();
            hits.sort();
            out.extend(hits);
        } else if root.join(pattern).join("Cargo.toml").is_file() {
            out.push(pattern.to_string());
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Render a background sub-agent's live event log into a human-readable peek
/// (for `task_output`) by folding it through the SHARED transcript reducer, so
/// the peek matches the durable on-disk record (tool args + results intact).
fn peek_events(events: &[AgentEvent]) -> String {
    let mut entries = Vec::new();
    for ev in events {
        crate::apply_event(&mut entries, ev);
    }
    // The SAME rendering `task_transcript` returns. A peek used to go through
    // `transcript_to_text`, which prints `[tool: read]` and drops the arguments
    // and the result — so the two tools described one run in two vocabularies,
    // and the peek was the poorer of them for no reason anybody chose.
    crate::transcript_to_plain_text(&entries, crate::TRANSCRIPT_TOOL_BODY_MAX)
}

/// Compact human duration for `task_list`: `8s`, `3m12s`, `1h4m`.
fn fmt_elapsed(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

/// Whether `path` is one of hrdr's own sub-agent worktrees (`.hrdr/worktrees/…`),
/// so the disk-aware task tools can tell a write run's isolated checkout from a
/// read-only run that just shared the main dir. Mirrors `gc_worktrees`'s inline
/// `.hrdr` + `worktrees` component test.
fn is_hrdr_worktree(path: &std::path::Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".hrdr")
        && path.components().any(|c| c.as_os_str() == "worktrees")
}

/// The branch a worktree checkout is on (`git rev-parse --abbrev-ref HEAD`), so a
/// revive can stack its follow-up on the same branch the original run committed
/// to. `None` on any git failure or a detached HEAD (nothing to continue onto).
async fn current_branch(worktree: &std::path::Path) -> Option<String> {
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "HEAD")
}

/// One sub-agent run found on disk under `subagents/<main-id>/`: the pair of a
/// `<stem>.jsonl` crash-trail and its (optional) `<stem>.json` snapshot.
struct DiskRun {
    /// The `NNN-slug` file stem — the id `task_revive` / `task_output` address it by.
    stem: String,
    /// The run's label (its snapshot's session name, else the jsonl `Start` record).
    label: String,
    /// The recorded cwd — an isolated worktree for a write run, else the main dir.
    cwd: Option<String>,
    /// The run reached a terminal `End` record (`done` vs `running`/`orphaned`).
    done: bool,
}

/// Scan `dir` (`subagents/<main-id>/`) for the sub-agent runs persisted there:
/// one per `<stem>.jsonl`, with its label/cwd taken from the sibling `<stem>.json`
/// snapshot when present (a run that reached its first `History` save) or the
/// Whether `stem` is a well-formed run id — the `NNN-slug` shape
/// [`child_transcript_id`] mints and [`scan_subagent_runs`] surfaces. A
/// `task_output` / `task_revive` id comes from the model, which joins it onto the
/// snapshot dir; rejecting a path separator, `..`, or empty string keeps that
/// lookup inside `subagents/<main-id>/` instead of escaping it.
fn valid_run_stem(stem: &str) -> bool {
    !stem.is_empty()
        && !stem.contains("..")
        && !stem.contains(['/', '\\'])
        && !std::path::Path::new(stem)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
}

/// jsonl's opening `Start` record otherwise. Best-effort: an unreadable dir
/// yields nothing, so the disk fallback silently degrades to the in-memory list.
fn scan_subagent_runs(dir: &std::path::Path) -> Vec<DiskRun> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let done = transcript_log::is_complete(&path);
        // Label + cwd: prefer the snapshot (its name IS the label; its cwd is the
        // worktree), else fall back to the jsonl's opening `Start` record.
        let json = path.with_extension("json");
        let (label, cwd) = match crate::Session::load_path(&json) {
            Ok(s) => (s.state.name, Some(s.state.cwd)),
            Err(_) => match transcript_log::read_start(&path) {
                Some(transcript_log::Record::Start { label, .. }) => (label, None),
                _ => (String::new(), None),
            },
        };
        out.push(DiskRun {
            stem,
            label,
            cwd,
            done,
        });
    }
    out.sort_by(|a, b| a.stem.cmp(&b.stem));
    out
}

/// A sub-agent conversation resolved for `task_revive`: the messages/identity to
/// hydrate a fresh agent from, plus the existing worktree/branch to continue on.
/// Produced live-first (from a retained [`AgentEntry`]) or from disk
/// ([`revive_target_from_disk`]).
struct RevivedState {
    messages: Vec<ChatMessage>,
    /// The identity the run was on — re-resolved to its own endpoint/key on spawn
    /// (it may be a different provider than the parent is on now).
    reference: ModelRef,
    /// The recorded working dir — the run's isolated worktree, or the main dir.
    cwd: PathBuf,
    /// The run's own tool scope: `true` for a read-only sub-agent, whose revived
    /// registry must be pruned the same way its original one was.
    read_only: bool,
    usage: AgentUsage,
    session_cost: f64,
    /// `(worktree, branch)` when the run had an isolated worktree still on disk.
    worktree: Option<(PathBuf, String)>,
    label: String,
}

/// The config a revived run is rebuilt on: the base a `task` spawn uses (always
/// write-capable — see [`subagent_base_config`]), narrowed back to the scope the
/// run actually had.
///
/// `read_only` is the same field [`config_for_agent_profile`] sets from a profile
/// and `Agent::new` resolves into a pruned registry, so a revived `explore` comes
/// back with the reader set it was spawned with rather than the writers its
/// profile deliberately withheld.
fn revive_base_config(base: &AgentConfig, read_only: bool) -> AgentConfig {
    let mut cfg = base.clone();
    cfg.read_only = read_only;
    cfg
}

/// Resolve a `task_revive` id to its persisted state, hydrating from the
/// `<stem>.json` snapshot under `dir` (`subagents/<main-id>/`). This is the
/// disk fallback the input-unification's `SessionState` persistence unlocked:
/// the snapshot carries the real model-facing `messages` (with signed thinking
/// blocks), so a revive continues losslessly rather than from a lossy transcript
/// fold. The recorded worktree is reused when it is still on disk; a write run
/// whose worktree was removed since is refused (its branch and commits are gone)
/// rather than silently continued somewhere else.
async fn revive_target_from_disk(dir: &std::path::Path, stem: &str) -> Result<RevivedState> {
    if !valid_run_stem(stem) {
        bail!("`{stem}` is not a valid run id (see `task_list`)");
    }
    let json = dir.join(format!("{stem}.json"));
    if !json.exists() {
        bail!("no sub-agent run `{stem}` on disk (see `task_list`)");
    }
    let state = crate::Session::load_path(&json)
        .with_context(|| format!("loading sub-agent snapshot for `{stem}`"))?
        .state;
    let cwd = PathBuf::from(&state.cwd);
    let worktree = if is_hrdr_worktree(&cwd) {
        if !cwd.exists() {
            bail!(
                "the isolated worktree for `{stem}` was removed ({}) — its branch and committed \
                 work are gone, so it can't be continued. Nothing was done.",
                cwd.display()
            );
        }
        current_branch(&cwd).await.map(|b| (cwd.clone(), b))
    } else {
        None
    };
    Ok(RevivedState {
        session_cost: state.usage.cost_usd,
        usage: state.usage,
        messages: state.messages,
        reference: state.model,
        label: state.name,
        read_only: state.read_only,
        cwd,
        worktree,
    })
}

/// `task_list`: report the background sub-agents `task` spawned — id, label,
/// status, model, elapsed, and (for a write-capable one) its worktree — so the
/// parent can check on them without waiting. After a `/resume` the in-memory
/// registry is empty, so it also scans the on-disk `subagents/<main-id>/`
/// snapshots (deduped against the live rows) — the enumeration `task_revive`
/// selects a resumable run from.
pub(crate) struct TaskListTool {
    /// The parent session's sub-agent snapshot dir cell (see
    /// [`AgentConfig::child_transcript_dir`]); resolved at call time so the
    /// disk scan survives a resume. `None` (or unresolved) → in-memory list only.
    pub(crate) transcript_dir: ChildDirCell,
}

#[async_trait::async_trait]
impl hrdr_tools::Tool for TaskListTool {
    fn name(&self) -> &'static str {
        "task_list"
    }
    fn description(&self) -> &'static str {
        "List your background sub-agents: each one's id, label, status (running / done / \
         cancelled) and — for a write-capable sub-agent — the isolated git worktree its changes \
         are in. Covers both the ones live in this session and, after a `/resume`, the ones \
         persisted on disk from earlier sessions (shown by their `NNN-slug` stem id, marked `done` \
         or `orphaned`) — pass a stem to `task_transcript` to read one back, or to `task_revive` \
         to re-engage it. A live task's result is delivered to you automatically; use this to \
         check progress, not to collect results."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    fn read_only(&self) -> bool {
        true
    }
    /// Checking on running sub-agents means asking the same question until the
    /// answer changes — no arguments to vary, so every check is byte-identical.
    fn repeatable(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        _args: serde_json::Value,
        ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        // The in-memory rows, plus the on-disk stems they already cover (a live
        // task's transcript path names its `<stem>.jsonl`), so the disk scan below
        // does not list a run twice.
        let (mut rows, live_stems): (Vec<String>, std::collections::HashSet<String>) = {
            let v = ctx
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let rows = v
                .iter()
                .map(|t| {
                    let mut row = format!("#{} [{}] {}", t.id, t.status().as_str(), t.label);
                    if !t.model.is_empty() {
                        row.push_str(&format!("  model: {}", t.model));
                    }
                    if let Some(started) = t.started {
                        row.push_str(&format!("  {}", fmt_elapsed(started.elapsed())));
                    }
                    if let Some(wt) = &t.worktree {
                        row.push_str(&format!("  worktree: {}", wt.display()));
                    }
                    row
                })
                .collect();
            let stems = v
                .iter()
                .filter_map(|t| {
                    t.transcript
                        .as_ref()
                        .and_then(|p| p.file_stem())
                        .and_then(|s| s.to_str())
                        .map(str::to_string)
                })
                .collect();
            (rows, stems)
        };
        // On-disk runs from earlier sessions (post-`/resume` the registry above is
        // empty), deduped against the live rows. A finished/orphaned run is invisible
        // in memory but recoverable here, and this is what `task_revive` selects from.
        if let Some(dir) = resolve_child_dir(&self.transcript_dir) {
            let disk: Vec<String> = scan_subagent_runs(&dir)
                .into_iter()
                .filter(|r| !live_stems.contains(&r.stem))
                .map(|r| {
                    let state = if r.done { "done" } else { "orphaned" };
                    let label = if r.label.trim().is_empty() {
                        "sub-task"
                    } else {
                        r.label.trim()
                    };
                    let mut row = format!("{} [{state}] {label}", r.stem);
                    if let Some(cwd) = r.cwd.as_deref().filter(|c| is_hrdr_worktree(c.as_ref())) {
                        row.push_str(&format!("  worktree: {cwd}"));
                    }
                    row
                })
                .collect();
            if !disk.is_empty() {
                rows.push("On disk (from earlier sessions — revive by stem id):".to_string());
                rows.extend(disk);
            }
        }
        if rows.is_empty() {
            return Ok("No background tasks.".to_string());
        }
        Ok(hrdr_tools::truncate(&rows.join("\n"), ctx.max_output))
    }
}

/// `task_output`: peek a RUNNING sub-agent's live progress without waiting.
///
/// Deliberately live-only. Reading a finished run back is `task_transcript`'s job,
/// and this tool used to do a lossy version of it too (a `NNN-slug` stem branch
/// rendering the on-disk transcript through [`crate::transcript_to_text`], which
/// drops each tool call's arguments and result). Two tools answering the same
/// question at different fidelities is how a model ends up with the worse answer,
/// so the overlap was cut rather than kept: peek here, read back there.
pub(crate) struct TaskOutputTool {
    pub(crate) live: AgentRegistry,
}

#[async_trait::async_trait]
impl hrdr_tools::Tool for TaskOutputTool {
    fn name(&self) -> &'static str {
        "task_output"
    }
    fn description(&self) -> &'static str {
        "Peek what a RUNNING background sub-agent has produced so far, by its integer `id` (from \
         `task_list`), without blocking — for when the user asks how a task is going. Shows the \
         newest output; the middle is dropped if it is long. The final result of a live task is \
         delivered to you automatically when it finishes, so you never need to poll. To read a \
         run BACK — a finished one, one from an earlier session, or any run's reasoning and tool \
         calls in full — use `task_transcript` instead; this tool only sees live tasks."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "integer",
                    "description": "The live task's integer id (see `task_list`). For an on-disk run from an earlier session, use `task_transcript` with its `NNN-slug` stem."
                }
            },
            "required": ["id"]
        })
    }
    fn read_only(&self) -> bool {
        true
    }
    /// Polling one running task's progress repeats the same `id` by definition —
    /// the tool is asked again precisely because the output is expected to grow.
    fn repeatable(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let id_val = args
            .get("id")
            .ok_or_else(|| anyhow::anyhow!("task_output needs an `id` (see `task_list`)"))?;
        // Live tasks only, addressed by integer id (an all-digit string counts —
        // the same id, differently typed). A `NNN-slug` stem is an on-disk run,
        // which belongs to `task_transcript`: it renders reasoning and each tool
        // call's arguments and result, where this tool's renderer would drop them.
        let Some(id) = id_val
            .as_u64()
            .or_else(|| id_val.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
        else {
            // One message for every non-integer id. Splitting on whether the string
            // looks like a stem would be guesswork — `valid_run_stem` only rejects
            // path traversal, not shape — and both cases have the same answer.
            let given = id_val.as_str().map(str::trim).unwrap_or_default();
            anyhow::bail!(
                "task_output takes a LIVE task's integer id (see `task_list`), and got \
                 `{given}`. To read a run back — a finished one, or one from an earlier session \
                 addressed by its `NNN-slug` stem — use `task_transcript`, which also shows its \
                 reasoning and every tool call's arguments and result."
            );
        };
        // Prefer the live event log; fall back to the registry entry's stored
        // result if the task already finished and its live entry was pruned.
        let peek = self.live.with(|v| {
            v.iter().find(|e| e.bg_id == Some(id)).map(|e| {
                let events = e
                    .events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .since(0)
                    .0;
                peek_events(&events)
            })
        });
        if let Some(text) = peek.filter(|t| !t.is_empty()) {
            // The TAIL, in the same rendering `task_transcript` produces: on a
            // still-running task the newest output is its current progress, and
            // keeping the head would hand back stale narration from the start.
            // That framing is the only difference between the two tools.
            return Ok(tail_lines(&text, ctx.max_output));
        }
        let done = {
            let v = ctx
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            v.iter().find(|t| t.id == id).map(|t| {
                let body = t
                    .result
                    .clone()
                    .unwrap_or_else(|| format!("(task #{id} is {})", t.status().as_str()));
                (body, t.transcript.clone())
            })
        };
        match done {
            Some((text, transcript)) => {
                // Same reasoning as the live-peek branch above: keep the tail.
                let mut out = hrdr_tools::truncate_middle(&text, ctx.max_output);
                // Point at the durable transcript for the full run — richer than
                // the stored summary, and it outlives the live event log.
                if let Some(p) = transcript {
                    out.push_str(&format!(
                        "\n\n(for the whole run — its reasoning and every tool call — use \
                         `task_transcript`; don't `read` the raw file at {}, which is one JSON record \
                         per streamed token)",
                        p.display()
                    ));
                }
                Ok(out)
            }
            None => anyhow::bail!("no background task #{id} (see `task_list`)"),
        }
    }
}

/// `task_revive`: re-engage a finished, orphaned, or crashed sub-agent with a
/// follow-up — the counterpart to `task_steer`, which only reaches a *running*
/// turn. Resolution is live-first, disk-fallback:
///
/// * **Live** — the sub-agent is still retained in [`AgentRegistry`] (finished but
///   not yet pruned): reuse its in-memory conversation directly (the freshest
///   copy) and its recorded worktree.
/// * **Disk** — otherwise hydrate from the persisted `<stem>.json` snapshot under
///   `subagents/<main-id>/` ([`revive_target_from_disk`]), which carries the real
///   model-facing `messages`, and reuse the worktree still on disk.
///
/// Either way it builds a FRESH agent from that state, points it at the existing
/// worktree/branch so the follow-up's commits stack there, and spawns it as a
/// background run — so the result is delivered exactly like a `task`'s. Building a
/// fresh agent (rather than resuming the retained object) keeps the two paths one
/// codepath and is lossless: the persisted `messages` are the conversation.
pub(crate) struct TaskReviveTool {
    /// Base policy for the revived sub-agent (endpoint/model overlaid live, then
    /// moved onto the run's own identity). Same base a `task` spawn uses.
    base: AgentConfig,
    runtime: SharedDelegationRuntime,
    pub(crate) bg_handles: BgHandles,
    /// The SAME concurrency slots `task` uses, so a revive counts against the
    /// caps rather than opening an uncounted extra sub-agent.
    pub(crate) slots: Arc<SubagentSlots>,
    /// Both caps, `(read-only, write-capable)` — a revived read-only run belongs
    /// on the read-only pool, exactly as a fresh one does.
    max_readonly: usize,
    max_write: usize,
    cost_total: Arc<std::sync::Mutex<f64>>,
    cost_partial: Arc<std::sync::atomic::AtomicBool>,
    lsp: Option<Arc<hrdr_tools::LspRegistry>>,
    transcript_dir: ChildDirCell,
    live: AgentRegistry,
}

impl TaskReviveTool {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        base: AgentConfig,
        runtime: SharedDelegationRuntime,
        bg_handles: BgHandles,
        slots: Arc<SubagentSlots>,
        cost_total: Arc<std::sync::Mutex<f64>>,
        cost_partial: Arc<std::sync::atomic::AtomicBool>,
        lsp: Option<Arc<hrdr_tools::LspRegistry>>,
        transcript_dir: ChildDirCell,
        live: AgentRegistry,
    ) -> Self {
        let max_readonly = base.max_readonly_subagents;
        let max_write = base.max_write_subagents;
        Self {
            base,
            runtime,
            bg_handles,
            slots,
            max_readonly,
            max_write,
            cost_total,
            cost_partial,
            lsp,
            transcript_dir,
            live,
        }
    }

    /// Live-first resolution: the in-memory state of a still-retained sub-agent,
    /// or `None` if no live entry has that background id. Refuses a still-running
    /// one — that is `task_steer`'s job, not revive's.
    async fn revive_from_live(
        &self,
        bg: u64,
        ctx: &hrdr_tools::ToolContext,
    ) -> Result<Option<RevivedState>> {
        let found = self.live.with(|v| {
            v.iter()
                .find(|e| e.bg_id == Some(bg))
                .map(|e| (e.key, e.running, Arc::clone(&e.agent), e.label.clone()))
        });
        let Some((key, running, agent, label)) = found else {
            return Ok(None);
        };
        if running {
            bail!(
                "background task #{bg} is still running — use `task_steer` to add to its current \
                 turn, not `task_revive`."
            );
        }
        // Its freshest conversation + identity + cwd come from the retained agent;
        // its worktree/branch from the registry entry that recorded them.
        let (messages, reference, cwd, read_only) = {
            let a = agent.lock().await;
            (
                a.messages_owned(),
                a.model_ref().clone(),
                a.cwd(),
                a.read_only(),
            )
        };
        let usage = self.live.usage(key).unwrap_or_default();
        let worktree = {
            let v = ctx
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            v.iter()
                .find(|t| t.id == bg)
                .and_then(|t| t.worktree.clone().zip(t.branch.clone()))
        };
        Ok(Some(RevivedState {
            session_cost: usage.cost_usd,
            usage,
            messages,
            reference,
            cwd,
            read_only,
            worktree,
            label,
        }))
    }

    /// Build a fresh agent from `st` on its existing worktree/branch and spawn it
    /// as a background run — so the follow-up's result is delivered exactly like a
    /// `task`'s. Synchronous, like [`spawn_background`] it wraps.
    fn spawn(
        &self,
        ctx: &hrdr_tools::ToolContext,
        prompt: String,
        st: RevivedState,
    ) -> Result<String> {
        let mut cfg = revive_base_config(&self.base, st.read_only);
        // The parent's LIVE resolved endpoint (identity + key), whole — exactly as
        // `SubagentTool::execute` overlays it, so the revived run inherits an
        // endpoint that agrees with itself.
        let runtime = self
            .runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let live_ep = runtime.endpoint.resolved;
        cfg.base_url = live_ep.base_url().to_string();
        cfg.api_key = live_ep.api_key().map(str::to_string);
        cfg.api_version = live_ep.api_version().map(str::to_string);
        cfg.headers = live_ep.headers().to_vec();
        cfg.model = live_ep.reference().clone();
        cfg.effort = runtime.endpoint.effort;
        // Move onto the run's OWN identity, re-deriving its endpoint/key (it may be
        // a different provider than the parent is on now) and inheriting the
        // parent's key across the same endpoint.
        let parent_ctx = AuthContext {
            api_key: live_ep.api_key(),
            base_url: live_ep.base_url(),
        };
        apply_model_ref(&mut cfg, st.reference.clone(), Some(&parent_ctx))
            .map_err(|e| anyhow::anyhow!("task_revive: {e:#}"))?;
        cfg.memory_roots = ctx.memory_project.clone().zip(ctx.memory_global.clone());
        // Run inside the reused worktree (a write run) or the recorded/main dir.
        cfg.cwd = match &st.worktree {
            Some((path, _)) => path.clone(),
            None if st.cwd.exists() => st.cwd.clone(),
            None => ctx.cwd.clone(),
        };
        cfg.context_window = child_context_window(
            cfg.context_window,
            Some(cfg.model.provider().as_str()),
            &cfg.base_url,
            cfg.model.model(),
        );
        // Slot on the pool that matches what the revived run may DO, with the same
        // caps a fresh `task` uses: a read-only follow-up needs none of the write
        // concurrency (it changes nothing and shares the dir), a writer continuing
        // in its worktree takes the write cap, and a writer with no worktree shares
        // the main dir — so, like a fresh one, only ever one at a time.
        let write_capable = !cfg.read_only;
        let (cap, kind) = match (write_capable, st.worktree.is_some()) {
            (false, _) => (self.max_readonly, "read-only"),
            (true, true) => (self.max_write, "write"),
            (true, false) => (1, "write"),
        };
        let Some(slot) = self.slots.acquire(write_capable, cap) else {
            bail!(
                "too many {kind} sub-agents already running (limit {cap}). Wait for one to \
                 finish — you are notified automatically — then revive."
            );
        };
        let label = if st.label.trim().is_empty() {
            "revived-task".to_string()
        } else {
            st.label.clone()
        };
        let worktree = match st.worktree {
            Some((path, branch)) => SpawnWorktree::Existing {
                repo: ctx.cwd.clone(),
                path,
                branch,
            },
            None => SpawnWorktree::None,
        };
        let restore = RestoredContext {
            messages: st.messages,
            session_cost: st.session_cost,
            usage: st.usage,
        };
        spawn_background(
            cfg,
            prompt,
            label,
            ctx.call_id.clone(),
            slot,
            &ctx.background_tasks,
            &self.bg_handles,
            Arc::clone(&self.cost_total),
            Arc::clone(&self.cost_partial),
            self.lsp.clone(),
            self.transcript_dir.clone(),
            self.live.clone(),
            worktree,
            Some(restore),
        )
    }
}

#[async_trait::async_trait]
impl hrdr_tools::Tool for TaskReviveTool {
    fn name(&self) -> &'static str {
        "task_revive"
    }
    fn description(&self) -> &'static str {
        "Re-engage a finished, orphaned, or crashed sub-agent with a follow-up `prompt`, instead \
         of re-delegating from scratch. It reuses the sub-agent's full context and — if it was \
         write-capable — its existing git worktree/branch, so follow-up changes stack on the same \
         branch. Use it to hand review fixes back to the SAME sub-agent that did the work, or to \
         continue a run left unfinished when the session was closed. Pass the `id` from \
         `task_list`: a live task's integer id, or an on-disk run's `NNN-slug` stem. Runs in the \
         background like `task` — its result is delivered to you automatically."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": ["integer", "string"],
                    "description": "The sub-agent to revive: a live task's integer id, or an on-disk run's `NNN-slug` stem (see `task_list`)."
                },
                "prompt": {
                    "type": "string",
                    "description": "The follow-up for the sub-agent — the next thing for it to do, with any context it needs."
                }
            },
            "required": ["id", "prompt"]
        })
    }
    fn read_only(&self) -> bool {
        false
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let id_val = args
            .get("id")
            .ok_or_else(|| anyhow::anyhow!("task_revive needs an `id` (see `task_list`)"))?;
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| anyhow::anyhow!("task_revive needs a non-empty `prompt`"))?
            .to_string();

        // Live-first: an integer (or all-digit) id names a retained in-memory run.
        let as_int = id_val
            .as_u64()
            .or_else(|| id_val.as_str().and_then(|s| s.trim().parse::<u64>().ok()));
        if let Some(bg) = as_int {
            if let Some(st) = self.revive_from_live(bg, ctx).await? {
                return self.spawn(ctx, prompt, st);
            }
            bail!(
                "no live background task #{bg} to revive — if it is from an earlier session, pass \
                 the `NNN-slug` stem id shown by `task_list`."
            );
        }
        // Disk fallback: a `NNN-slug` stem from an earlier session.
        let stem = id_val
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("task_revive needs an integer id or a stem id (see `task_list`)")
            })?;
        let dir = resolve_child_dir(&self.transcript_dir).ok_or_else(|| {
            anyhow::anyhow!("no session directory yet — cannot revive `{stem}` from disk")
        })?;
        let st = revive_target_from_disk(&dir, stem).await?;
        self.spawn(ctx, prompt, st)
    }
}

/// `task_steer`: add instructions to a background sub-agent's in-flight turn.
pub(crate) struct SteerTool {
    pub(crate) live: AgentRegistry,
}

#[async_trait::async_trait]
impl hrdr_tools::Tool for SteerTool {
    fn name(&self) -> &'static str {
        "task_steer"
    }
    fn description(&self) -> &'static str {
        "Give additional instructions to a running background sub-agent. The message is queued \
         on the sub-agent's active turn and reaches it before its next model request; if its current \
         response finishes first, the retained sub-agent starts a follow-up turn with the message. \
         Use the task id from `task` / `task_list`; finished or unknown tasks cannot be steered."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "The running task id (see `task_list`)." },
                "prompt": { "type": "string", "description": "Additional instructions for the sub-agent." }
            },
            "required": ["id", "prompt"]
        })
    }
    fn read_only(&self) -> bool {
        false
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let id = args
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow::anyhow!("task_steer needs an integer `id` (see `task_list`)"))?;
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .filter(|p| !p.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("task_steer needs a non-empty `prompt`"))?;
        let queued = self.live.with(|entries| {
            let entry = entries.iter().find(|e| e.bg_id == Some(id))?;
            if !entry.running {
                return Some(false);
            }
            entry
                .steering
                .lock()
                .ok()
                .map(|mut queue| queue.push_back(Steer::plain(prompt)))?;
            Some(true)
        });
        match queued {
            Some(true) => Ok(format!("Steered background task #{id}.")),
            Some(false) => anyhow::bail!("background task #{id} is no longer running"),
            None => anyhow::bail!("no running background task #{id} (see `task_list`)"),
        }
    }
}

/// `task_cancel`: abort one background sub-agent and discard its (unreviewed)
/// worktree.
pub(crate) struct TaskCancelTool {
    pub(crate) bg_handles: BgHandles,
    pub(crate) live: AgentRegistry,
}

#[async_trait::async_trait]
impl hrdr_tools::Tool for TaskCancelTool {
    fn name(&self) -> &'static str {
        "task_cancel"
    }
    fn description(&self) -> &'static str {
        "Cancel a running background sub-agent by its `id` (from `task_list`). Its work is \
         abandoned. If it was write-capable and its isolated worktree has no changes, the worktree \
         is removed; if it has changes — uncommitted files OR commits the sub-agent made on its \
         branch — they are NOT thrown away: the worktree is kept and this call tells you where it \
         is so you can review and merge or discard it. Use when the user asks to stop a task or it \
         is no longer needed."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "The task id (see `task_list`)." }
            },
            "required": ["id"]
        })
    }
    fn read_only(&self) -> bool {
        false
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let id = args.get("id").and_then(|v| v.as_u64()).ok_or_else(|| {
            anyhow::anyhow!("task_cancel needs an integer `id` (see `task_list`)")
        })?;
        // Abort the worker if it is still running, and AWAIT the aborted task so
        // its future is fully dropped before we touch the worktree — otherwise the
        // worker could still be mid-write when we assess and remove it. Bounded so
        // a wedged task can't hang the cancel; abort resolves promptly for the
        // I/O-bound sub-agent in the common case.
        let handle = {
            let mut handles = self
                .bg_handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            handles
                .iter()
                .position(|(hid, _)| *hid == id)
                .map(|pos| handles.remove(pos).1)
        };
        let aborted = handle.is_some();
        if let Some(h) = handle {
            h.abort();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), h).await;
        }
        // Mark the registry entry cancelled and take its worktree for discard.
        let worktree = {
            let mut v = ctx
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match v.iter_mut().find(|t| t.id == id) {
                Some(t) => {
                    t.cancelled = true;
                    t.done = true;
                    t.worktree.clone().zip(t.branch.clone())
                }
                None if !aborted => anyhow::bail!("no background task #{id} (see `task_list`)"),
                None => None,
            }
        };
        // A worktree with uncommitted work is NOT thrown away: keep it and tell
        // the caller where it is, so the (possibly partial) changes aren't lost.
        // Only a clean worktree is removed.
        let kept_note = match worktree {
            Some((path, branch)) => {
                if worktree_has_changes(&ctx.cwd, &path, &branch).await {
                    // The task id stays addressable precisely so this is
                    // resolvable with tools — see the retain rule in
                    // `TurnState::drain_background`. Name them: told only to go
                    // look at the worktree, a model reaches for `rm -rf` to
                    // finish the job, which the prompt forbids and which throws
                    // the work away.
                    Some(format!(
                        " Its worktree has changes (uncommitted files or commits on its branch), \
                         so it was kept rather than thrown away, and task #{id} stays addressable \
                         for it: `task_diff {id}` to review what it did, `task_consume {id}` to \
                         bring uncommitted work into your working dir, `git merge --ff-only \
                         {branch}` to take its commits, then `task_cleanup {id}` (add \
                         `force: true` to discard the worktree and whatever is left in it). Never \
                         `rm -rf` it — that leaves a stale git worktree registration \
                         behind.\n  worktree: {}\n  branch:   {branch}",
                        path.display(),
                    ))
                } else {
                    remove_worktree(&ctx.cwd, &path, &branch);
                    // The worktree is gone, so nothing is left to address by id:
                    // drop the fields that keep the entry alive, letting the next
                    // drain prune it.
                    let mut v = ctx
                        .background_tasks
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(t) = v.iter_mut().find(|t| t.id == id) {
                        t.worktree = None;
                        t.branch = None;
                    }
                    None
                }
            }
            None => None,
        };
        // Clear its live panel entry.
        self.live.with(|v| {
            for e in v.iter_mut().filter(|e| e.bg_id == Some(id)) {
                e.running = false;
                e.done = true;
                e.delivered = true;
            }
        });
        Ok(format!(
            "Cancelled background task #{id}.{}",
            kept_note.unwrap_or_default()
        ))
    }
}

/// Resolve a task id to its `(worktree path, branch)` from the shared
/// background-task registry — the one lookup `task_diff` / `task_consume` /
/// `task_cleanup` all do. The entry persists after delivery exactly so this is
/// addressable by id. `Ok(None)` means the task has no worktree (read-only, or a
/// shared-dir writer in a non-git project); each caller words that case for
/// itself, since what it implies differs per tool.
fn task_worktree(
    ctx: &hrdr_tools::ToolContext,
    id: u64,
) -> anyhow::Result<Option<(PathBuf, String)>> {
    let v = ctx
        .background_tasks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match v.iter().find(|t| t.id == id) {
        Some(t) => Ok(t.worktree.clone().zip(t.branch.clone())),
        None => anyhow::bail!("no background task #{id} (see `task_list`)"),
    }
}

/// The worktree's uncommitted paths (modified, staged, and untracked), from
/// `git status --porcelain`. `None` when the status could not be read at all —
/// callers treat that as "assume dirty", never as clean.
async fn worktree_dirty_files(path: &std::path::Path) -> Option<Vec<String>> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())?;
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.get(3..).map(str::trim))
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// `a, b, c … and 4 more` — a file list short enough to sit inside one sentence.
fn short_file_list(files: &[String], max: usize) -> String {
    let shown: Vec<&str> = files.iter().take(max).map(String::as_str).collect();
    let rest = files.len().saturating_sub(shown.len());
    let mut s = shown.join(", ");
    if rest > 0 {
        s.push_str(&format!(" … and {rest} more"));
    }
    s
}

/// Run `git apply --3way` in `repo` with `patch` fed on stdin, returning
/// `(succeeded, git's own message)`. `patch` is bytes, not text: a `--binary`
/// diff of a file that isn't valid UTF-8 must survive the round trip untouched.
async fn git_apply_3way(
    repo: &std::path::Path,
    extra: &[&str],
    patch: &[u8],
) -> anyhow::Result<(bool, String)> {
    use tokio::io::AsyncWriteExt;
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .args(["apply", "--3way"])
        .args(extra)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().context("spawning `git apply`")?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(patch).await;
        let _ = stdin.shutdown().await;
    }
    let out = child
        .wait_with_output()
        .await
        .context("running `git apply`")?;
    let mut msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.trim().is_empty() {
        if !msg.is_empty() {
            msg.push('\n');
        }
        msg.push_str(stdout.trim());
    }
    Ok((out.status.success(), msg))
}

/// The files `git apply --3way --check` said it could only apply *with
/// conflicts* — parsed out of its `Applied patch to 'x' with conflicts.` lines.
/// A dry run reports conflicts on stderr but still exits 0, so the exit code
/// alone cannot tell a clean 3-way apply from a conflicting one.
fn conflicted_paths(msg: &str) -> Vec<String> {
    msg.lines()
        .filter(|l| l.ends_with("with conflicts."))
        .filter_map(|l| {
            let start = l.find('\'')? + 1;
            let end = l.rfind('\'')?;
            (end > start).then(|| l[start..end].to_string())
        })
        .collect()
}

/// `task_cleanup`: after the parent has merged a finished write sub-agent's work
/// into its own working dir, remove that worktree and drop the task. This is the
/// explicit "merged, done with it" signal — it does NOT perform the merge.
pub(crate) struct TaskCleanupTool {
    pub(crate) live: AgentRegistry,
}

#[async_trait::async_trait]
impl hrdr_tools::Tool for TaskCleanupTool {
    fn name(&self) -> &'static str {
        "task_cleanup"
    }
    fn description(&self) -> &'static str {
        "Remove a finished write sub-agent's worktree once you have merged its work into your \
         own working dir. Use it as the 'I'm done with this one' signal AFTER you have brought \
         the changes over (`task_consume` — this tool does NOT merge \
         for you). Pass the task `id` (from `task_list`). It refuses if the worktree still has \
         uncommitted changes (bring them over with `task_consume` first), or if its branch has \
         commits not yet reachable from your HEAD (it looks unmerged) — so you can't delete work \
         by accident. `force: true` overrides BOTH refusals and really does remove it: use it \
         when you already brought the work over another way (cherry-pick / squash / \
         `task_consume`), or when you have decided the leftovers are debris — the result then names \
         what was discarded."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "The task id (see `task_list`)." },
                "force": {
                    "type": "boolean",
                    "description": "Remove even if the worktree has uncommitted changes or the branch has commits not reachable from HEAD — confirm you already brought the work over (`task_consume`) or accept losing it. Default false."
                }
            },
            "required": ["id"]
        })
    }
    fn read_only(&self) -> bool {
        false
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let id = args.get("id").and_then(|v| v.as_u64()).ok_or_else(|| {
            anyhow::anyhow!("task_cleanup needs an integer `id` (see `task_list`)")
        })?;
        // Look up the task's worktree. The entry persists after delivery exactly
        // so this is addressable by id.
        let Some((path, branch)) = task_worktree(ctx, id)? else {
            // No worktree (a read-only or shared-dir task) — nothing to remove;
            // just drop the entry.
            ctx.background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|t| t.id != id);
            return Ok(format!("Task #{id} has no worktree to clean up."));
        };
        let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
        // What is uncommitted in the worktree right now. `None` means the status
        // could not be read — treated as dirty, so a bad read never deletes work
        // silently. Read once: it decides the refusal below AND, under `force`,
        // the honest account of what got discarded.
        let dirty = worktree_dirty_files(&path).await;
        // Refuse while the working tree is dirty — uncommitted changes are
        // definitely NOT merged, so removing the worktree would lose them...
        // unless `force`, which is the caller saying exactly that. A `force` that
        // still refused just taught the model to `rm -rf` the worktree behind the
        // harness's back (which is what happened), losing the same work with no
        // record of it — so `force` now really forces, and reports the cost.
        if !force && dirty.as_ref().is_none_or(|f| !f.is_empty()) {
            anyhow::bail!(
                "worktree for task #{id} has uncommitted changes ({}) — bring them over with \
                 `task_consume {id}` (or commit them in the worktree) first, then clean up. Nothing \
                 was removed. If that work is debris you want thrown away, call task_cleanup \
                 again with force:true and it will be discarded.",
                path.display()
            );
        }
        // Guard against removing work that was never merged. Count the branch's
        // commits not reachable from HEAD; if any, the branch looks unmerged, so
        // refuse unless `force` (the parent brought them over via cherry-pick /
        // squash, which leaves the originals unreachable). Biased to refuse: a
        // failed count is treated as "unmerged" so nothing is deleted on a bad read.
        if !force {
            let unmerged = tokio::process::Command::new("git")
                .arg("-C")
                .arg(&ctx.cwd)
                .args(["rev-list", "--count", &format!("HEAD..{branch}")])
                .output()
                .await
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "?".to_string());
            if unmerged != "0" {
                anyhow::bail!(
                    "branch `{branch}` (task #{id}) has {unmerged} commit(s) not reachable from \
                     your HEAD — it looks unmerged. Merge it first, or if you already brought the \
                     work over (cherry-pick / squash) call task_cleanup again with force:true. \
                     Nothing was removed."
                );
            }
        }
        remove_worktree(&ctx.cwd, &path, &branch);
        ctx.background_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|t| t.id != id);
        self.live.with(|v| {
            for e in v.iter_mut().filter(|e| e.bg_id == Some(id)) {
                e.running = false;
                e.done = true;
                e.delivered = true;
            }
        });
        // A forced removal states what it cost — the files whose uncommitted
        // contents are now gone, named, so the loss is on the record instead of
        // being a silent "Cleaned up."
        let discarded = match &dirty {
            Some(files) if !files.is_empty() => format!(
                " Discarded uncommitted changes in {} file(s): {}.",
                files.len(),
                short_file_list(files, 10)
            ),
            None => " Its uncommitted state could not be read before removal, so anything \
                     uncommitted there is gone."
                .to_string(),
            Some(_) => String::new(),
        };
        Ok(format!(
            "Cleaned up the worktree for task #{id}.{discarded}"
        ))
    }
}

/// Run `git` in `dir` and return `(success, stdout, stderr)`, both trimmed.
async fn git_in(dir: &std::path::Path, args: &[&str]) -> anyhow::Result<(bool, String, String)> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running `git {}` in {}", args.join(" "), dir.display()))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}

/// Everything a finished write task left OUTSIDE its commits, captured from the
/// worktree before anything is moved.
///
/// Captured eagerly, bytes and all, because landing the commits first means
/// stashing this out of the way — and `git stash push -u` takes the untracked
/// files with it, so a later read of the worktree would find them gone.
struct Leftovers {
    /// `git diff HEAD --binary`: staged and unstaged changes to tracked files.
    patch: Vec<u8>,
    /// `status\tpath` pairs for the report.
    tracked: Vec<(String, String)>,
    /// Untracked (non-ignored) files with their contents.
    untracked: Vec<(String, Vec<u8>)>,
}

impl Leftovers {
    fn is_empty(&self) -> bool {
        self.patch.is_empty() && self.untracked.is_empty()
    }
    fn count(&self) -> usize {
        self.tracked.len() + self.untracked.len()
    }
}

/// Read a worktree's uncommitted work into memory. Ignored files stay where they
/// are (`--exclude-standard`): they are build output, not results.
async fn capture_leftovers(path: &std::path::Path) -> anyhow::Result<Leftovers> {
    // `diff HEAD` is the worktree-vs-HEAD delta — exactly the uncommitted work,
    // staged and unstaged together. `--binary` so a non-text change survives, and
    // bytes rather than a `String` so the patch reaches `git apply` unmodified.
    let diff_out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["diff", "HEAD", "--binary"])
        .output()
        .await
        .context("reading the task worktree's uncommitted diff")?;
    if !diff_out.status.success() {
        anyhow::bail!(
            "could not read the worktree's uncommitted changes (`git diff HEAD` in {}): {}",
            path.display(),
            String::from_utf8_lossy(&diff_out.stderr).trim()
        );
    }
    let (_, names, _) = git_in(path, &["diff", "HEAD", "--name-status"]).await?;
    let tracked: Vec<(String, String)> = names
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(st, p)| (st.trim().to_string(), p.trim().to_string()))
        .collect();

    let others_out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .await
        .context("listing the task worktree's untracked files")?;
    let mut untracked = Vec::new();
    for rel in String::from_utf8_lossy(&others_out.stdout)
        .split('\0')
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        let bytes = tokio::fs::read(path.join(rel))
            .await
            .with_context(|| format!("reading untracked file `{rel}` out of the worktree"))?;
        untracked.push((rel.to_string(), bytes));
    }
    Ok(Leftovers {
        patch: diff_out.stdout,
        tracked,
        untracked,
    })
}

/// Land captured leftovers on the parent checkout, staged and uncommitted.
///
/// All-or-nothing: both halves are pre-flighted before anything is written, so a
/// refusal never leaves a half-applied tree and then reports success.
async fn land_leftovers(
    ctx: &hrdr_tools::ToolContext,
    id: u64,
    path: &std::path::Path,
    lo: &Leftovers,
) -> anyhow::Result<String> {
    let mut blockers: Vec<String> = Vec::new();
    if !lo.patch.is_empty() {
        let (ok, msg) = git_apply_3way(&ctx.cwd, &["--check"], &lo.patch).await?;
        if !ok {
            anyhow::bail!(
                "task #{id}'s uncommitted changes do not apply to your working dir:\n{msg}"
            );
        }
        blockers.extend(conflicted_paths(&msg));
    }
    // A copy would clobber an existing file, and an existing file is either the
    // user's or the parent's own work — never overwrite one blind.
    blockers.extend(
        lo.untracked
            .iter()
            .filter(|(rel, _)| ctx.cwd.join(rel).exists())
            .map(|(rel, _)| format!("{rel} (already exists)")),
    );
    if !blockers.is_empty() {
        anyhow::bail!(
            "{} file(s) in task #{id}'s uncommitted work conflict with your working dir:\n  {}\n\
             Review them in the worktree ({}) and bring them over by hand.",
            blockers.len(),
            blockers.join("\n  "),
            path.display(),
        );
    }

    // The dry run passed, but it is not the same check: `--check` turns `--index`
    // off, while the real `--3way` stages, so it additionally requires the index
    // to match the working tree for the files it touches. `git apply` is atomic
    // (no `--reject`), so a failure here applies nothing.
    if !lo.patch.is_empty() {
        let (ok, msg) = git_apply_3way(&ctx.cwd, &[], &lo.patch).await?;
        if !ok {
            anyhow::bail!(
                "`git apply --3way` refused task #{id}'s uncommitted changes — it applies \
                 atomically, so nothing was applied:\n{msg}\nA `does not match index` here means \
                 YOUR working dir has unstaged edits to one of the same files (this applies \
                 through the index): commit or stash yours, then consume again."
            );
        }
    }
    let mut copied: Vec<String> = Vec::new();
    for (rel, bytes) in &lo.untracked {
        let dest = ctx.cwd.join(rel);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {} for a copied file", parent.display()))?;
        }
        tokio::fs::write(&dest, bytes)
            .await
            .with_context(|| format!("writing untracked file `{rel}` into your working dir"))?;
        copied.push(rel.clone());
    }
    if !copied.is_empty() {
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&ctx.cwd)
            .args(["add", "--"])
            .args(&copied)
            .output()
            .await;
    }

    let mut report = String::new();
    for (status, file) in &lo.tracked {
        report.push_str(&format!("  {status} {file}\n"));
    }
    for file in &copied {
        report.push_str(&format!(
            "  A {file} (untracked in the worktree — copied)\n"
        ));
    }
    Ok(report)
}

/// `task_consume`: bring everything a finished write task produced into the
/// parent checkout, in one call and in the order that works.
///
/// Replaces a hand-rolled sequence with a trap in it. By hand the commits are
/// `git -C <worktree> rebase <target>` then `git merge --ff-only <branch>`, and
/// the target is what goes wrong: `HEAD` inside the worktree is *the worktree's
/// own* HEAD, so `rebase HEAD` is a no-op that reports "Current branch is up to
/// date". The fast-forward then fails because the parent moved, and the fallback
/// — `cherry-pick` — leaves the branch unreachable from HEAD, so `task_cleanup`
/// refuses until it is forced. One correct sequence becomes three wrong ones, and
/// the recovery discards the guard that would have caught a real mistake. Here
/// the parent's HEAD is resolved to a SHA in the parent checkout and the rebase
/// is onto that: there is no name left to get wrong.
///
/// **Commits first, then leftovers**, which is the other half of why this is one
/// tool. A worktree can hold both, and the order is not symmetric: land the
/// uncommitted work first and you must commit it, which moves your HEAD, which
/// stops the branch fast-forwarding — manufacturing the rebase problem above.
/// Landing the commits first leaves the parent at the branch tip, which is
/// exactly what the leftover patch was computed against.
///
/// All-or-nothing at each step, and the worktree is restored on any failure: a
/// conflicting rebase is aborted, a stash taken to clear the way is popped back.
pub(crate) struct TaskConsumeTool;

#[async_trait::async_trait]
impl hrdr_tools::Tool for TaskConsumeTool {
    fn name(&self) -> &'static str {
        "task_consume"
    }
    fn description(&self) -> &'static str {
        "Bring everything a finished write sub-agent produced into your working dir, in one call. \
         Pass the task `id` (from `task_list`). Its COMMITS are rebased onto your current HEAD and \
         fast-forwarded in (history stays linear); anything it left UNCOMMITTED is then applied \
         and staged for you to review with `git diff --cached` and commit yourself. Review first \
         with `task_diff` — this merges, it does not check. All-or-nothing: on conflict nothing \
         lands, your tree and the worktree are left as they were, and the conflicting files are \
         named. Afterwards call `task_cleanup` to remove the worktree."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "The task id (see `task_list`)." }
            },
            "required": ["id"]
        })
    }
    fn read_only(&self) -> bool {
        false
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let id = args.get("id").and_then(|v| v.as_u64()).ok_or_else(|| {
            anyhow::anyhow!("task_consume needs an integer `id` (see `task_list`)")
        })?;
        let Some((path, branch)) = task_worktree(ctx, id)? else {
            anyhow::bail!(
                "task #{id} has no worktree to consume — it was read-only or shared your working \
                 dir, so anything it changed is already in your tree."
            );
        };

        let leftovers = capture_leftovers(&path).await?;
        let (_, ahead, _) = git_in(
            &ctx.cwd,
            &["rev-list", "--count", &format!("HEAD..{branch}")],
        )
        .await?;
        let commits: u64 = ahead.parse().unwrap_or(0);
        if commits == 0 && leftovers.is_empty() {
            return Ok(format!(
                "Nothing to consume from task #{id}: `{branch}` has no commits your HEAD is \
                 missing, and its worktree is clean. `task_cleanup` with id {id} removes it."
            ));
        }

        // A rebase refuses to run over a dirty tree, so stash the leftovers out of
        // the way. Their contents are already in memory, so this is only about
        // giving git a clean tree — and it is reversible, which `reset --hard`
        // would not be.
        let stashed = if commits > 0 && !leftovers.is_empty() {
            let (ok, _, err) = git_in(&path, &["stash", "push", "--include-untracked"]).await?;
            if !ok {
                anyhow::bail!(
                    "could not set task #{id}'s uncommitted work aside to rebase its commits — \
                     nothing was merged. git said: {err}"
                );
            }
            true
        } else {
            false
        };
        // Where the branch pointed before any of this. A rebase that SUCCEEDS and
        // is then followed by a failure is the case that needs it: `rebase
        // --abort` only unwinds a rebase still in progress, so without this the
        // branch stays moved after a refused fast-forward — and the tool would be
        // claiming it left everything as it was while having quietly rewritten
        // the sub-agent's history.
        let (_, orig_tip, _) = git_in(&path, &["rev-parse", "HEAD"]).await?;
        // Put the worktree back exactly as it was found, on any failure after this
        // point: branch tip first, then the stash. That order matters — the stash
        // was taken against the pre-rebase tree, so popping it onto a rebased one
        // can conflict and leave the worktree in a state nobody asked for.
        let restore = |stashed: bool, rewound: bool, path: std::path::PathBuf, tip: String| async move {
            if rewound && !tip.is_empty() {
                let _ = git_in(&path, &["reset", "--hard", &tip]).await;
            }
            if stashed {
                let _ = git_in(&path, &["stash", "pop"]).await;
            }
        };

        let mut report = String::new();
        if commits > 0 {
            // The fix for the whole trap: resolve the parent's HEAD to a SHA
            // *here*, in the parent checkout. `HEAD` would name the worktree's own
            // head once it crossed the `-C` boundary, and a branch name would have
            // to be the right one. A SHA is neither.
            let (ok, head_sha, err) = git_in(&ctx.cwd, &["rev-parse", "HEAD"]).await?;
            if !ok || head_sha.is_empty() {
                restore(stashed, false, path.clone(), orig_tip.clone()).await;
                anyhow::bail!("could not resolve your HEAD to rebase task #{id} onto: {err}");
            }

            let (rebased, _, rebase_err) = git_in(&path, &["rebase", &head_sha]).await?;
            if !rebased {
                // Name what collided before unwinding, then unwind.
                let (_, conflicts, _) =
                    git_in(&path, &["diff", "--name-only", "--diff-filter=U"]).await?;
                let _ = git_in(&path, &["rebase", "--abort"]).await;
                restore(stashed, false, path.clone(), orig_tip.clone()).await;
                let files = if conflicts.is_empty() {
                    String::new()
                } else {
                    format!(" Conflicting files: {}.", conflicts.replace('\n', ", "))
                };
                anyhow::bail!(
                    "rebasing `{branch}` (task #{id}) onto your HEAD hit a conflict — the rebase \
                     was aborted and NOTHING was merged; your tree and the worktree are as they \
                     were.{files} Resolve it in the worktree ({}) and consume again, or review \
                     and cherry-pick by hand. git said: {rebase_err}",
                    path.display()
                );
            }

            let (merged, _, merge_err) = git_in(&ctx.cwd, &["merge", "--ff-only", &branch]).await?;
            if !merged {
                // The rebase landed; only the merge did not. Rewind the branch so
                // the sub-agent's commits are where it left them.
                restore(stashed, true, path.clone(), orig_tip.clone()).await;
                anyhow::bail!(
                    "`{branch}` (task #{id}) rebased cleanly but would not fast-forward into your \
                     working dir — usually an uncommitted change of your own in a file it \
                     touches. Nothing was merged, and the branch was rewound to where the \
                     sub-agent left it. git said: {merge_err}"
                );
            }
            let size = task_size_summary(&ctx.cwd, &branch)
                .await
                .map(|s| format!("\n{s}"))
                .unwrap_or_default();
            report.push_str(&format!(
                "Merged {commits} commit(s) from `{branch}` (rebased onto your HEAD, \
                 fast-forwarded in).{size}\n"
            ));
        }

        if !leftovers.is_empty() {
            match land_leftovers(ctx, id, &path, &leftovers).await {
                Ok(files) => {
                    report.push_str(&format!(
                        "Applied {} uncommitted file(s), STAGED and not committed:\n{files}\
                         Review with `git diff --cached` — every hunk, like a PR — and commit \
                         them yourself with a proper message.\n",
                        leftovers.count()
                    ));
                    // Only now: the work is in the parent, so the stashed copy is
                    // a duplicate, and leaving it would have `task_cleanup` refuse
                    // a worktree whose contents are already safe.
                    if stashed {
                        let _ = git_in(&path, &["stash", "drop"]).await;
                    }
                }
                Err(e) => {
                    // The commits, if any, are already merged — rewinding the
                    // branch now would say the opposite of what happened.
                    restore(stashed, false, path.clone(), orig_tip.clone()).await;
                    let merged_note = if commits > 0 {
                        format!(
                            " NOTE: the {commits} commit(s) DID merge — only the uncommitted half \
                             failed, and it is back in the worktree."
                        )
                    } else {
                        String::new()
                    };
                    anyhow::bail!("{e}{merged_note}");
                }
            }
        }

        report.push_str(&format!(
            "The worktree is still on disk — call `task_cleanup` with id {id} to remove it.\n"
        ));
        Ok(hrdr_tools::truncate_saved(
            &report,
            ctx.max_output,
            ctx.max_output_lines,
            hrdr_tools::TruncateSide::Head,
            "task_consume",
        ))
    }
}

/// `task_diff`: review a finished write sub-agent's work in one call, instead of
/// the parent hand-rolling the 3-command git recipe (`status --porcelain`,
/// `log --oneline HEAD..<branch>`, `diff HEAD...<branch>`) every time.
/// `task_transcript`: read a sub-agent's run back as plain text.
///
/// Exists because the harness used to point at the `.jsonl` and say "`read` it",
/// and a session did exactly that: the reply came back as one JSON record per
/// streamed token, which is the same run at a multiple of the tokens with the
/// content buried in syntax. The records are already folded into entries by the
/// same reducer the panes use; this renders them.
pub(crate) struct TaskTranscriptTool {
    /// Where a finished/orphaned run's `<stem>.jsonl` lives, so a run from an
    /// earlier session (post-`/resume`) is still readable. `None` → live only.
    pub(crate) transcript_dir: ChildDirCell,
}

#[async_trait::async_trait]
impl hrdr_tools::Tool for TaskTranscriptTool {
    fn name(&self) -> &'static str {
        "task_transcript"
    }
    fn description(&self) -> &'static str {
        "DIAGNOSTIC: read a sub-agent's whole run back as plain text — what it was asked, what it \
         thought, every tool call with its arguments and result, and what it answered. Reach for \
         it when something is WRONG and the result alone doesn't explain it: `task_diff` shows a \
         change you didn't expect, a task reports success but its work says otherwise, it failed \
         or was cancelled, or it clearly misread the brief. Most tasks need none of this — the \
         result is delivered to you automatically, and a write task's work is reviewed with \
         `task_diff` (the change) not here (the conversation). A whole run is a lot of context, so \
         spend it when you have a question it answers. Pass a live/finished task's integer `id`, \
         or the `NNN-slug` stem of a run from an earlier session (see `task_list`); long runs page \
         with `offset`/`limit`, like `read`. Never `read` the raw `.jsonl` yourself — it is one \
         JSON record per streamed token and says the same thing at many times the size."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": ["integer", "string"],
                    "description": "The task id: a live task's integer id, or an on-disk run's `NNN-slug` stem (see `task_list`)."
                },
                "offset": {
                    "type": "integer",
                    "description": "1-based line to start at in the rendered transcript. Default 1."
                },
                "limit": {
                    "type": "integer",
                    "description": "How many lines to return. Default: as many as fit the output cap."
                }
            },
            "required": ["id"]
        })
    }
    fn read_only(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let id_val = args
            .get("id")
            .ok_or_else(|| anyhow::anyhow!("task_transcript needs an `id` (see `task_list`)"))?;
        let offset = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1) as usize;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);

        // Same dual addressing as `task_output`: an integer (or all-digit string)
        // names a live/recently-finished task, a `NNN-slug` stem names a run on
        // disk. Resolve either to the `.jsonl` the fold reads.
        let path = match id_val
            .as_u64()
            .or_else(|| id_val.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
        {
            Some(id) => {
                let from_registry = ctx
                    .background_tasks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .iter()
                    .find(|t| t.id == id)
                    .and_then(|t| t.transcript.clone());
                from_registry.ok_or_else(|| {
                    anyhow::anyhow!(
                        "no transcript for task #{id} — it may predate this session (try its \
                         `NNN-slug` stem from `task_list`), or the task may not exist"
                    )
                })?
            }
            None => {
                let stem = id_val
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "task_transcript needs an integer id or a stem id (see `task_list`)"
                        )
                    })?;
                if !valid_run_stem(stem) {
                    anyhow::bail!("`{stem}` is not a valid run id (see `task_list`)");
                }
                let dir = resolve_child_dir(&self.transcript_dir).ok_or_else(|| {
                    anyhow::anyhow!("no session directory yet — cannot read `{stem}` from disk")
                })?;
                dir.join(format!("{stem}.jsonl"))
            }
        };
        if !path.exists() {
            anyhow::bail!(
                "no transcript at {} — the run may have been pruned (see `task_list`)",
                path.display()
            );
        }
        let entries = transcript_log::read_transcript(&path);
        let text = crate::transcript_to_plain_text(&entries, crate::TRANSCRIPT_TOOL_BODY_MAX);
        if text.trim().is_empty() {
            return Ok(format!("The run recorded no output ({}).", path.display()));
        }
        Ok(window_lines(&text, offset, limit, ctx.max_output))
    }
}

/// Room left for a window's one-line header once the body has its budget.
const WINDOW_HEADER_BUDGET: usize = 200;

/// Lines `start..` of `lines`, at most `limit` of them and within `budget` bytes.
/// Returns the joined text and how many lines it took.
///
/// The one place the line budget is applied — `task_output`'s tail and
/// `task_transcript`'s page differ only in which window they ask for, and that is
/// the whole intended difference between the two tools.
fn take_lines(lines: &[&str], start: usize, limit: usize, budget: usize) -> (String, usize) {
    let mut out = String::new();
    let mut taken = 0usize;
    for line in lines.iter().skip(start).take(limit) {
        if out.len() + line.len() + 1 > budget {
            break;
        }
        out.push_str(line);
        out.push('\n');
        taken += 1;
    }
    (out.trim_end().to_string(), taken)
}

/// `limit` lines of `text` from 1-based `offset`, with a header naming the total
/// and the offset to continue from.
///
/// Paged rather than truncated: reading a run back starts at the beginning, and
/// the reader needs to know there IS more and how to ask for it.
fn window_lines(text: &str, offset: usize, limit: Option<usize>, max_output: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = offset.saturating_sub(1).min(total);
    let budget = max_output.saturating_sub(WINDOW_HEADER_BUDGET);
    let (body, taken) = take_lines(&lines, start, limit.unwrap_or(usize::MAX), budget);
    let last = start + taken;
    let mut header = format!("Transcript lines {}-{last} of {total}", start + 1);
    if last < total {
        header.push_str(&format!(
            " — {} more; continue with offset: {}",
            total - last,
            last + 1
        ));
    }
    format!("{header}\n\n{body}")
}

/// The LAST lines of `text` that fit in `max_output`, headed with what was kept
/// and where the rest is.
///
/// The peek's half of the split: same rendering as a page, opposite end. It says
/// how many earlier lines it dropped, because a peek that silently starts
/// mid-run reads like the whole run.
fn tail_lines(text: &str, max_output: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let budget = max_output.saturating_sub(WINDOW_HEADER_BUDGET);
    // Walk back from the end while the lines still fit, then take that window
    // forward through the shared helper.
    let mut start = total;
    let mut used = 0usize;
    while start > 0 {
        let len = lines[start - 1].len() + 1;
        if used + len > budget {
            break;
        }
        used += len;
        start -= 1;
    }
    let (body, taken) = take_lines(&lines, start, usize::MAX, budget);
    let mut header = if taken >= total {
        format!("Progress so far — all {total} lines")
    } else {
        format!(
            "Progress so far — last {taken} of {total} lines ({} earlier omitted; \
             `task_transcript` reads the run from the start)",
            total - taken
        )
    };
    header.push_str(":\n\n");
    format!("{header}{body}")
}

pub(crate) struct TaskDiffTool;

#[async_trait::async_trait]
impl hrdr_tools::Tool for TaskDiffTool {
    fn name(&self) -> &'static str {
        "task_diff"
    }
    fn description(&self) -> &'static str {
        "Review a finished write sub-agent's work: any uncommitted/untracked leftovers in its \
         worktree, its commits, and either the full merge-base diff (`git diff \
         HEAD...<branch>`) or — pass `commit` — just one commit's diff (`git show`), for \
         reviewing a large result commit-by-commit instead of one unreviewable blob. Use it when \
         a write task reports back, before you merge its work into your own working dir. Pass \
         the task `id` (from `task_list` or the delivery message)."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "The task id (see `task_list`)." },
                "commit": {
                    "type": "string",
                    "description": "Optional: review just one commit instead of the full diff. \
                        Either a 1-based index into the commit list this tool prints (newest \
                        first — so you can read the list, then pass e.g. \"2\"), or a git commit \
                        hash. Must be one of this task's own commits (HEAD..<branch>); a rev \
                        outside the task is refused."
                }
            },
            "required": ["id"]
        })
    }
    fn read_only(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &hrdr_tools::ToolContext,
    ) -> anyhow::Result<String> {
        let id = args
            .get("id")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow::anyhow!("task_diff needs an integer `id` (see `task_list`)"))?;
        // Same lookup as `task_cleanup`: the entry persists after delivery, so
        // this is addressable by id whether or not the task has been reviewed yet.
        let Some((path, branch)) = task_worktree(ctx, id)? else {
            anyhow::bail!(
                "task #{id} has no changes to diff — it was read-only or shared your working \
                 dir, so nothing landed in an isolated worktree."
            );
        };

        // 1. Uncommitted/untracked leftovers in the worktree itself.
        let status_out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&path)
            .args(["status", "--porcelain"])
            .output()
            .await
            .context("running `git status` in the task's worktree")?;
        let dirty = String::from_utf8_lossy(&status_out.stdout)
            .trim()
            .to_string();

        // 2. The commits under review, run against the PARENT's cwd (the branch
        // is visible there too — worktrees share one object store). Also the
        // list `commit` indexes into below, so its numbering matches what the
        // model just read.
        let log_out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&ctx.cwd)
            .args(["log", "--oneline", &format!("HEAD..{branch}")])
            .output()
            .await
            .context("running `git log` for the task's branch")?;
        let commits = String::from_utf8_lossy(&log_out.stdout).trim().to_string();

        let mut report = String::new();
        if !dirty.is_empty() {
            report.push_str(&format!(
                "WARNING: the sub-agent left uncommitted/untracked changes in its worktree — \
                 these are NOT included in the diff below and must be handled (reviewed and \
                 committed, or discarded) before you merge or run `task_cleanup`. To bring them \
                 into your working dir in one call, use `task_consume {id}`:\n{dirty}\n\n"
            ));
        }
        if commits.is_empty() {
            // An empty `HEAD..branch` is ambiguous and reads as a benign
            // all-clear, so name both cases and the one check that tells them
            // apart — otherwise the model spends calls playing detective (it
            // can't be resolved by git topology alone: a branch that produced
            // nothing and one whose commit was already fast-forwarded into HEAD
            // both leave the branch tip at HEAD).
            report.push_str(&format!(
                "Commits: branch `{branch}` has no commits beyond your HEAD — nothing to \
                 merge. This means EITHER the sub-agent produced no commits, OR its work was \
                 already integrated into your HEAD (e.g. a fast-forward from branch timing). \
                 If you expected changes, run `git show --stat HEAD` to check whether its \
                 commit already landed before you rely on this or run `task_cleanup`.\n\n"
            ));
        } else {
            report.push_str(&format!("Commits:\n{commits}\n\n"));
        }

        let commit_arg = args
            .get("commit")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match commit_arg {
            // 3a. One commit only: resolve the selector against THIS task's
            // commits (never arbitrary repo history), then `git show` it — a
            // reviewable slice of a result too big for the full diff.
            Some(selector) => {
                let (hash, index, total) =
                    resolve_task_commit(ctx, id, &branch, &commits, selector).await?;
                let show_out = tokio::process::Command::new("git")
                    .arg("-C")
                    .arg(&ctx.cwd)
                    .args(["show", &hash])
                    .output()
                    .await
                    .context("running `git show` for the selected commit")?;
                let diff =
                    hrdr_tools::redact_secret_diffs(&String::from_utf8_lossy(&show_out.stdout));
                report.push_str(&format!(
                    "Showing commit {index}/{total} (`git show {hash}`):\n"
                ));
                report.push_str(if diff.trim().is_empty() {
                    "(no diff)\n"
                } else {
                    &diff
                });
            }
            // 3b. The full merge-base diff, also from the parent's cwd (three-dot:
            // everything since the merge-base, not just what's uncommitted in the
            // worktree — which is clean once the sub-agent has committed).
            None => {
                let diff_out = tokio::process::Command::new("git")
                    .arg("-C")
                    .arg(&ctx.cwd)
                    .args(["diff", &format!("HEAD...{branch}")])
                    .output()
                    .await
                    .context("running `git diff` against the task's branch")?;
                let diff =
                    hrdr_tools::redact_secret_diffs(&String::from_utf8_lossy(&diff_out.stdout));
                report.push_str(&format!("Diff (git diff HEAD...{branch}):\n"));
                report.push_str(if diff.trim().is_empty() {
                    "(no diff)\n"
                } else {
                    &diff
                });
            }
        }

        Ok(hrdr_tools::truncate_saved(
            &report,
            ctx.max_output,
            ctx.max_output_lines,
            hrdr_tools::TruncateSide::Head,
            "task_diff",
        ))
    }
}

/// Resolve a `task_diff` `commit` selector to `(hash, 1-based index, total)`
/// within task `id`'s own commits — never arbitrary repo history. `commits` is
/// the trimmed `git log --oneline HEAD..<branch>` output already computed by
/// the caller (newest first), so a numeric `selector` indexes into exactly the
/// list the tool just printed. Anything else is a git rev, verified to
/// actually be one of `HEAD..<branch>` via `git rev-list` before use — this
/// is what stops the model from pulling up a diff from outside the task.
///
/// A selector can be BOTH: an abbreviated git hash may be all digits
/// (`1234567`). Only a number that fits the printed list (`1..=total`) is an
/// index — anything larger falls through to rev resolution, so an all-digit
/// hash resolves as the hash it is instead of erroring as an out-of-range
/// index. (An all-digit hash abbreviation short enough to also be a valid
/// index reads as the index — same as what the model just saw printed.)
async fn resolve_task_commit(
    ctx: &hrdr_tools::ToolContext,
    id: u64,
    branch: &str,
    commits: &str,
    selector: &str,
) -> anyhow::Result<(String, usize, usize)> {
    let lines: Vec<&str> = commits.lines().collect();
    let total = lines.len();
    let as_index = selector.parse::<usize>().ok();
    if let Some(n) = as_index.filter(|n| (1..=total).contains(n)) {
        let hash = lines[n - 1]
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string();
        return Ok((hash, n, total));
    }
    // Not a usable index — a git rev. Resolve it to a full hash, then require
    // it actually be reachable as one of the task's own commits before showing
    // it.
    let rev_parse = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&ctx.cwd)
        .args(["rev-parse", "--verify", &format!("{selector}^{{commit}}")])
        .output()
        .await
        .context("resolving `commit` as a git rev")?;
    if !rev_parse.status.success() {
        // A short number that isn't a rev either was almost certainly meant as
        // an index into the printed list — say so instead of "not a rev".
        if as_index.is_some() && selector.len() < 4 {
            anyhow::bail!(
                "task #{id} commit index {selector} is out of range — its commit list has \
                 {total} commit(s) (see the `Commits:` list `task_diff` just printed; it's \
                 1-based, newest first)"
            );
        }
        anyhow::bail!("`{selector}` is not a valid git commit rev");
    }
    let hash = String::from_utf8_lossy(&rev_parse.stdout)
        .trim()
        .to_string();
    let rev_list_out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&ctx.cwd)
        .args(["rev-list", &format!("HEAD..{branch}")])
        .output()
        .await
        .context("listing task's commits to verify `commit` belongs to it")?;
    let rev_list = String::from_utf8_lossy(&rev_list_out.stdout);
    let Some(pos) = rev_list.lines().position(|l| l == hash) else {
        anyhow::bail!(
            "`{selector}` is not one of task #{id}'s commits (`git rev-list HEAD..{branch}`) — \
             refusing to show history outside this task"
        );
    };
    Ok((hash, pos + 1, total))
}

/// The full agent-profile set for `config`, layered by precedence — each source
/// overriding a same-named agent from the one before it:
/// built-ins < discovered files (`.claude`/`.opencode`/`.hrdr`) < `[[subagent]]`
/// config. Used both to populate the `task` tool and to resolve `--agent`.
///
/// Discovered profiles are **untrusted, repo-local** content — arbitrary
/// `.claude`/`.opencode`/`.hrdr` Markdown files that ship inside a cloned repo,
/// as opposed to `[[subagent]]` config, which is the user's own trusted config
/// file. Two trust-boundary rules apply only to discovered profiles:
/// - a discovered profile can never overlay a built-in's name (`explore`,
///   `review`, `plan`, `general`) — the built-in always wins, so a malicious
///   repo can't silently swap out `explore`'s instructions. The collision is
///   logged (to stderr; profile resolution runs before this agent has an event
///   channel to post an [`AgentEvent::Notice`] on) and the file is otherwise
///   ignored;
/// - a discovered profile can never set `proactive` (which nudges the main
///   agent to delegate to it **unprompted**) — it's forced to `false` even for
///   a non-colliding name, since prompting the model to reach for
///   attacker-controlled instructions without being asked is itself the risk.
pub fn resolve_agent_profiles(config: &AgentConfig) -> Result<Vec<SubagentProfile>> {
    // Field-level merge: when `incoming` names an existing profile, each field it
    // leaves unset (`None`) inherits the one already in the slot, so pinning e.g.
    // just `model` on a built-in doesn't blow away its prompt/read_only/description.
    // A non-matching name is pushed whole, as a brand-new profile. `name` keeps the
    // existing slot's casing.
    fn overlay(profiles: &mut Vec<SubagentProfile>, incoming: SubagentProfile) {
        match profiles
            .iter_mut()
            .find(|p| p.name.eq_ignore_ascii_case(&incoming.name))
        {
            Some(slot) => {
                let SubagentProfile {
                    name: _,
                    model,
                    description,
                    prompt,
                    read_only,
                    tools,
                    temperature,
                    effort,
                    max_steps,
                    proactive,
                } = incoming;
                if model.is_some() {
                    slot.model = model;
                }
                if description.is_some() {
                    slot.description = description;
                }
                if prompt.is_some() {
                    slot.prompt = prompt;
                }
                if read_only.is_some() {
                    slot.read_only = read_only;
                }
                if tools.is_some() {
                    slot.tools = tools;
                }
                if temperature.is_some() {
                    slot.temperature = temperature;
                }
                if effort.is_some() {
                    slot.effort = effort;
                }
                if max_steps.is_some() {
                    slot.max_steps = max_steps;
                }
                if proactive.is_some() {
                    slot.proactive = proactive;
                }
            }
            None => profiles.push(incoming),
        }
    }
    let mut profiles = builtin_subagent_profiles();
    let builtin_names: Vec<String> = profiles.iter().map(|p| p.name.clone()).collect();
    for mut p in discover_agent_profiles(&config.cwd)? {
        if builtin_names
            .iter()
            .any(|n| n.eq_ignore_ascii_case(&p.name))
        {
            eprintln!(
                "hrdr: ignoring repo-local agent profile '{}' from {:?} — it collides with a \
                 built-in agent name; built-ins cannot be overridden by discovered files",
                p.name, config.cwd
            );
            continue;
        }
        p.proactive = Some(false);
        overlay(&mut profiles, p);
    }
    for up in config.subagent_profiles.clone() {
        overlay(&mut profiles, up);
    }
    Ok(profiles)
}

/// The always-available built-in sub-agents: read-only `explore` and `review`
/// personas. Merged with the user's `[[subagent]]` profiles in [`Agent::new`]
/// (a user profile of the same name overrides the built-in).
pub fn builtin_subagent_profiles() -> Vec<SubagentProfile> {
    vec![
        SubagentProfile {
            name: "explore".to_string(),
            model: None,
            description: Some(
                "Read-only codebase investigator — trace files, types, and call \
                 paths and report back. Use proactively when a question needs \
                 broad exploration, to keep the main context lean."
                    .to_string(),
            ),
            prompt: Some(EXPLORE_PROMPT.to_string()),
            read_only: Some(true),
            tools: None,
            temperature: None,
            effort: None,
            max_steps: None,
            proactive: Some(true),
        },
        SubagentProfile {
            name: "review".to_string(),
            model: None,
            description: Some(
                "Read-only code reviewer — audit code or a change for bugs, edge \
                 cases, and security issues. Use proactively after writing or \
                 changing non-trivial code, before finalizing."
                    .to_string(),
            ),
            prompt: Some(REVIEW_PROMPT.to_string()),
            read_only: Some(true),
            tools: None,
            temperature: None,
            // A careful reviewer default: think harder before flagging.
            effort: Some("high".to_string()),
            max_steps: None,
            proactive: Some(true),
        },
        SubagentProfile {
            name: "plan".to_string(),
            model: None,
            description: Some(
                "Planner — investigates read-only and returns a concrete, \
                 step-by-step implementation plan in its report. Changes nothing; \
                 use it to design the work before delegating the change."
                    .to_string(),
            ),
            prompt: Some(PLAN_PROMPT.to_string()),
            read_only: Some(true),
            tools: None,
            temperature: None,
            effort: None,
            max_steps: None,
            proactive: Some(false),
        },
        SubagentProfile {
            name: "coder".to_string(),
            model: None,
            description: Some(
                "Write-capable implementer — hand it a precise, self-contained \
                 spec (exact files, symbols, before→after) and it implements \
                 exactly that, verifies, and commits. Use proactively for \
                 well-scoped implementation and mechanical changes; scope the \
                 work first."
                    .to_string(),
            ),
            prompt: Some(CODER_PROMPT.to_string()),
            read_only: Some(false),
            tools: None,
            temperature: None,
            effort: None,
            max_steps: None,
            proactive: Some(true),
        },
        SubagentProfile {
            name: "general".to_string(),
            model: None,
            description: Some(
                "General-purpose agent — full tool access for open-ended, \
                 multi-step tasks (explore and modify). Same as `task` with no \
                 `agent`."
                    .to_string(),
            ),
            prompt: None,
            read_only: Some(false),
            tools: None,
            temperature: None,
            effort: None,
            max_steps: None,
            proactive: Some(false),
        },
    ]
}

const EXPLORE_PROMPT: &str = "\
You are an EXPLORE sub-agent: a read-only code investigator. You have read and \
search tools only — you cannot modify files or run mutating commands. Investigate \
the area described and report back so the parent agent can act on your findings.

- Search from more than one angle — by symbol, by string/error text, and by the \
  project's file/directory conventions — so you don't miss a second definition or \
  an alternate code path.
- Trace the relevant files, types, and call paths; quote key code with `path:line`.
- Answer the question directly. Lead with the conclusion, then the evidence.
- Don't speculate past what the code shows; if something is missing or you could \
  not find it, say so explicitly rather than guessing.
- Return a tight, structured summary — not a narrative of your search. Lead with \
  a 1-3 line answer, then findings as `path:line` bullets; keep it short unless \
  the task genuinely needs more.";

pub(crate) const REVIEW_PROMPT: &str = "\
You are a REVIEW sub-agent: a read-only code reviewer. You have read and search \
tools only — you cannot modify files. Review the code or change described and \
report your findings.

- Check, in order: correctness and logic errors; edge cases and error handling; \
  concurrency, races, and resource leaks; security (injection, secrets, SSRF, \
  auth, unvalidated input); API/contract misuse; and missing or wrong tests. \
  Weigh real bugs over style nits.
- Verify every finding against the actual code — read the lines you cite. Never \
  invent a bug that isn't there or a line you didn't read; a false positive costs \
  the caller more than a missed nit.
- For each finding give: severity, `path:line`, what's wrong (a concrete failing \
  input or scenario), and a concrete fix.
- Lead with the most serious issues, grouped by severity. Skip pure style.
- End with a one-line verdict: safe to ship as-is, or what must change first. If \
  it's clean, say so plainly.";

const PLAN_PROMPT: &str = "\
You are a PLAN sub-agent: a read-only planner. Investigate the task with your \
read and search tools, then return a concrete implementation plan in your report. \
You cannot modify files or run mutating commands. Plan the work; do NOT implement \
it.

- First understand the task: trace the relevant code with your read/search tools, \
  and note how the project already does similar things so the plan fits in.
- Build the plan with: the goal in one line; the approach and why; the exact \
  files/functions/types to change; ordered steps, each sized as an independently \
  implementable — and independently reviewable — chunk: a step names the \
  files/functions it changes, its constraints, and a done-criterion, so the \
  caller can hand any single step to a coder sub-agent as a self-contained \
  brief; edge cases and risks; and how to verify (build/test/lint). Be concrete \
  enough that another agent can execute it without re-investigating — name real \
  paths and symbols, not placeholders.
- Return the full plan in your report — that report is your entire hand-off, and \
  the caller acts on it directly. Do not depend on writing anything to disk.";

const CODER_PROMPT: &str = "\
You are a CODER sub-agent: implement the task you were given, exactly and \
narrowly. The spec is your contract: build what it says, all of it, nothing \
beyond it.

- No drive-by refactors, renames, or reformatting beyond the task; no new \
  files/docs/helpers the task didn't call for; don't over-engineer (no \
  flexibility nothing uses).
- Follow the codebase's existing patterns — find how it already does this kind \
  of thing and match it.
- Verify before reporting: build/test/lint scoped to what you touched; fix what \
  your change broke. Never weaken a test to get green.
- You cannot ask questions. If part of the spec is ambiguous or turns out wrong \
  against the real code, do the unambiguous part, and report exactly what you \
  skipped or adapted and why — an honest partial beats an improvised whole.
- If faithful implementation balloons far past what the spec implies — many more \
  files or far more churn than the brief names — stop rather than deliver a \
  monster: implement the coherent core, commit it, and report the remainder as \
  proposed follow-up chunks. A reviewable partial beats an unreviewable whole.
- Commit each coherent unit as you go (Conventional Commits) and leave a clean \
  tree; your commits and report are the entire hand-off.";

/// List the model ids available for `config`'s provider.
///
/// The trusted ChatGPT OAuth provider does not expose the OpenAI-compatible
/// `/v1/models` endpoint (a plain `GET` there returns `401 Unauthorized`), so it
/// is discovered through the account model catalog behind a coordinated —
/// refreshing — OAuth access token, the same source the agent's `models`
/// tool uses. Every other provider falls back to the OpenAI-compatible
/// `/v1/models` listing.
pub async fn list_provider_models(config: &AgentConfig) -> Result<Vec<String>> {
    // The identity resolved against this config, with the auth-derived switch
    // applied (`oauth_derived` reads the OAuth store) so a keyless built-in
    // `openai` with a stored OAuth credential reports the Codex endpoint here —
    // otherwise this would list `/v1/models` off `api.openai.com` (401, no key)
    // instead of the account catalog.
    let resolved = crate::oauth_derived(ResolvedModel::from_config(config));
    if resolved.is_codex_oauth() {
        let access = coordinated_oauth_access(resolved.kind(), resolved.base_url()).await?;
        let catalog = chatgpt_model_catalog(&access, false).await;
        let mut ids: Vec<String> = catalog.models.into_iter().map(|m| m.slug).collect();
        ids.sort();
        return Ok(ids);
    }
    let client = Client::new(
        config.base_url.clone(),
        config.api_key.clone(),
        config.model.model().to_string(),
    );
    client.list_models().await
}

/// Whether `cwd` (or an ancestor) is inside a git repo. `.git` may be a
/// directory (normal) or a file (worktrees/submodules).
pub fn in_git_repo(cwd: &std::path::Path) -> bool {
    cwd.ancestors().any(|d| d.join(".git").exists())
}

/// A throwaway git worktree for an isolated sub-agent (every write-capable
/// sub-agent, when the project is a git repo). Created on a scratch branch off
/// the current `HEAD`; [`finish`](Self::finish)
/// removes it if the sub-agent made no changes, else leaves it with a pointer.
///
/// Implements [`Drop`] for best-effort cleanup when the owning future is
/// cancelled before [`finish`](Self::finish) is called.
pub(crate) struct Worktree {
    /// The repo the worktree belongs to (the sub-agent's original cwd).
    repo: PathBuf,
    /// The worktree checkout (the sub-agent's cwd while it runs).
    pub(crate) path: PathBuf,
    /// The scratch branch the worktree is on.
    branch: String,
    /// Set to `true` by `finish()` so `Drop` knows cleanup already happened
    /// and should not run again.
    cleaned: bool,
}

impl Drop for Worktree {
    fn drop(&mut self) {
        if self.cleaned {
            return; // already handled by keep() or a previous drop
        }
        // Best-effort synchronous cleanup for a worktree abandoned before it was
        // kept (a cancelled future). `remove_worktree` double-forces, so it clears
        // the lock this process placed at creation.
        remove_worktree(&self.repo, &self.path, &self.branch);
    }
}

impl Worktree {
    /// Create a worktree off `repo`'s current HEAD. Errors if `repo` isn't a git
    /// repository (or git isn't available).
    pub(crate) async fn create(repo: &std::path::Path) -> Result<Self> {
        if !in_git_repo(repo) {
            bail!("worktree isolation requires a git repository");
        }
        // Best-effort prune of any stale worktrees from previously aborted runs.
        prune_stale_worktrees(repo).await;
        // A unique name per worktree: the timestamp alone collides when two are
        // created within the clock's resolution (macOS `SystemTime` is only
        // ~microsecond-grained), so a same-instant pair — parallel `task` calls,
        // or parallel tests — both tried `git worktree add hrdr/task-<same>` and
        // one failed. The process id plus a monotonic counter make it
        // collision-free within and across processes.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let uniq = format!(
            "{stamp}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let branch = format!("hrdr/task-{uniq}");
        // Worktrees live under `<repo>/.hrdr/worktrees/` — inside the working tree
        // (so the parent agent can review them with its cwd-confined file tools and
        // on the same filesystem, no cross-device rename), but ignored via the
        // repo's local `info/exclude` so a fresh checkout never shows in
        // `git status` or gets staged by `git add -A`. `.hrdr/` is git-ignored, so
        // it is also skipped by `grep`/`tree` and won't pollute searches.
        let top = git_toplevel(repo).await;
        let worktrees_dir = top.join(".hrdr").join("worktrees");
        ensure_worktree_ignored(repo, &worktrees_dir).await;
        let path = worktrees_dir.join(format!("wt-{uniq}"));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .arg("worktree")
            .arg("add")
            .arg("-b")
            .arg(&branch)
            .arg(&path)
            .arg("HEAD")
            .output()
            .await
            .context("running `git worktree add`")?;
        if !out.status.success() {
            bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        // Lock the worktree, tagging the lock with this process's pid. A lock stops
        // any sweep (`git worktree prune`, this or another hrdr instance's startup
        // GC) from removing it while it is live; the pid lets a later sweep tell a
        // still-running owner (skip it) from a crashed one (stale lock → reclaim).
        // Best-effort — a failed lock just means less protection.
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "lock", "--reason"])
            .arg(format!("hrdr:pid={}", std::process::id()))
            .arg(&path)
            .output()
            .await;
        Ok(Self {
            repo: repo.to_path_buf(),
            path,
            branch,
            cleaned: false,
        })
    }

    /// Detach automatic cleanup and hand back the worktree's location. Used when
    /// a background write sub-agent's worktree must **outlive** its run — the
    /// changes stay for the parent to review and merge, and are removed only by
    /// an explicit `task_cancel` or a session reset (see [`remove_worktree`]).
    pub(crate) fn keep(mut self) -> KeptWorktree {
        self.cleaned = true; // Drop becomes a no-op
        KeptWorktree {
            path: self.path.clone(),
            branch: self.branch.clone(),
        }
    }
}

/// The surviving location of a [`Worktree`] whose cleanup was detached via
/// [`Worktree::keep`]. Removal is later done explicitly with [`remove_worktree`].
pub(crate) struct KeptWorktree {
    pub(crate) path: PathBuf,
    pub(crate) branch: String,
}

/// Whether a sub-agent's worktree holds work that cancelling its task must NOT
/// silently throw away: either uncommitted files in the checkout, OR commits the
/// sub-agent made on its scratch `branch` that are not yet on the main line
/// (`HEAD..branch`). Checking only `git status` misses the latter — a sub-agent
/// that ran `git commit` leaves a clean working tree, and removing the branch
/// with `git branch -D` would then delete the commits. Best-effort and biased
/// toward keeping: any git failure reports "has changes" so cleanup can't destroy
/// unreviewed work on a bad read.
async fn worktree_has_changes(
    repo: &std::path::Path,
    path: &std::path::Path,
    branch: &str,
) -> bool {
    // Uncommitted work in the checkout (modified/staged/untracked).
    let dirty = tokio::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()
        .await
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(true); // a failed read → assume changes, never delete blindly
    if dirty {
        return true;
    }
    // Commits on the scratch branch that aren't reachable from the main HEAD.
    let range = format!("HEAD..{branch}");
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "--count", &range])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() != "0")
        // A failed count (e.g. branch/HEAD unresolvable) → assume commits exist.
        .unwrap_or(true)
}

/// Blocking twin of [`worktree_has_changes`] for the synchronous reset path
/// ([`Agent::abort_background_tasks`]), which cannot `.await`. Same semantics:
/// true if the checkout is dirty OR the branch has commits not on the main line,
/// biased toward "has changes" on any git failure so a reset never deletes
/// unreviewed work.
pub(crate) fn worktree_has_changes_sync(
    repo: &std::path::Path,
    path: &std::path::Path,
    branch: &str,
) -> bool {
    let dirty = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(true);
    if dirty {
        return true;
    }
    let range = format!("HEAD..{branch}");
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "--count", &range])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() != "0")
        .unwrap_or(true)
}

/// Force-remove a background sub-agent's worktree and its scratch branch. Run
/// against the main repo (`repo`), best-effort — used by `task_cancel` and by
/// [`Agent::abort_background_tasks`] for a worktree with no unreviewed work.
/// Synchronous so it can run from non-async cleanup paths.
pub(crate) fn remove_worktree(repo: &std::path::Path, path: &std::path::Path, branch: &str) {
    // `--force --force`: the first overrides the "has modifications" guard, the
    // second overrides the lock (this is an explicit, owner-driven removal, so the
    // lock has served its purpose). A single `--force` refuses a locked worktree.
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force", "--force"])
        .arg(path)
        .output();
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["branch", "-D", branch])
        .output();
}

/// Run `git worktree prune` in `repo` to clean up leftover worktree entries
/// from previously aborted agents. This is the safest possible prune — git
/// only removes entries whose checkout directory no longer exists. Branch
/// cleanup is intentionally skipped here: task branches may contain committed
/// work that a user hasn't reviewed yet.
async fn prune_stale_worktrees(repo: &std::path::Path) {
    let _ = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "prune"])
        .output()
        .await;
}

/// Whether process `pid` is still alive, so a lock tagged with it can be told
/// apart from a crashed owner's stale lock. `kill -0` on unix (0 = alive);
/// conservative `true` elsewhere and on any error, so a live worktree is never
/// reclaimed by mistake — the cost is only that a crash orphan lingers.
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(true)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Sweep leftover sub-agent worktrees under `.hrdr/worktrees/`: remove the CLEAN
/// ones — a finished/merged task, or an orphan left when a previous run exited —
/// and KEEP any with unreviewed changes (uncommitted files or unmerged commits),
/// same rule as `task_cancel`/reset. Only touches worktrees on the
/// `.hrdr/worktrees` path; other worktrees the user made are left alone.
///
/// A LIVE worktree is protected by its `git worktree lock`: if the lock's
/// `hrdr:pid=N` owner is still running, the entry is skipped, so a second hrdr
/// instance (or this one, later, if ever run in-session) never deletes a
/// worktree out from under a running sub-agent. Only a stale lock (dead owner) or
/// an unlocked entry is eligible. Runs `git worktree prune` and reaps orphaned
/// `hrdr/task-*` branches too. Synchronous + best-effort.
pub(crate) fn gc_worktrees(repo: &std::path::Path) {
    let Ok(out) = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    let mut lock: Option<Option<u32>> = None; // Some(pid?) once a `locked` line is seen
    // `--porcelain` emits one block per worktree, blocks separated by a blank
    // line: `worktree <path>` / `HEAD <sha>` / `branch refs/heads/<name>` (or
    // `detached`) / optional `locked [<reason>]`. Flush on each blank line, plus a
    // final sentinel for the last block.
    for line in text.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let (Some(p), Some(b)) = (path.take(), branch.take()) {
                let ours = p.components().any(|c| c.as_os_str() == ".hrdr")
                    && p.components().any(|c| c.as_os_str() == "worktrees");
                // A lock whose owning pid is still alive means the worktree is
                // live in some hrdr process — never touch it.
                let live_lock = matches!(lock, Some(Some(pid)) if pid_alive(pid));
                if ours && p.exists() && !live_lock && !worktree_has_changes_sync(repo, &p, &b) {
                    remove_worktree(repo, &p, &b);
                } else if ours && !p.exists() && !live_lock && lock.is_some() {
                    // The checkout is GONE but its registration survives, locked.
                    // `git worktree prune` below refuses locked entries, so this
                    // would sit in the user's `git worktree list` forever. Unlock
                    // it so the prune can clear it. Nothing can be lost: there is
                    // no directory left to hold work, and the branch is judged
                    // separately by `git branch -d`, which refuses an unmerged one.
                    //
                    // Real sessions get here by `rm -rf`-ing a worktree — which the
                    // prompt forbids, but which was the only exit from a cancelled
                    // task whose entry had been pruned (see
                    // `TurnState::drain_background`). That hole is closed, but the
                    // debris outlives it, in repos that already have some.
                    let _ = std::process::Command::new("git")
                        .arg("-C")
                        .arg(repo)
                        .args(["worktree", "unlock"])
                        .arg(&p)
                        .output();
                }
            }
            path = None;
            branch = None;
            lock = None;
        } else if let Some(rest) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = Some(rest.trim_start_matches("refs/heads/").to_string());
        } else if let Some(rest) = line.strip_prefix("locked") {
            // `locked hrdr:pid=N` (or bare `locked`). Extract the pid if present.
            let pid = rest
                .trim()
                .strip_prefix("hrdr:pid=")
                .and_then(|s| s.trim().parse::<u32>().ok());
            lock = Some(pid);
        }
    }
    // Drop worktree metadata for checkouts whose directory is gone, then reap
    // orphaned scratch branches. `git branch -d` (lowercase) only deletes a fully
    // MERGED branch and refuses one with unreviewed commits, and it can't touch a
    // branch still checked out by a kept worktree — so this never loses work.
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "prune"])
        .output();
    reap_orphan_task_branches(repo);
}

/// `git branch -d` every `hrdr/task-*` branch: git deletes only the fully-merged,
/// not-checked-out ones and safely refuses the rest, so crash-orphaned scratch
/// branches don't pile up. Best-effort.
pub(crate) fn reap_orphan_task_branches(repo: &std::path::Path) {
    let Ok(out) = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/hrdr/task-*",
        ])
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    for b in String::from_utf8_lossy(&out.stdout).lines() {
        let b = b.trim();
        if b.is_empty() {
            continue;
        }
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["branch", "-d", b])
            .output();
    }
}

/// The working-tree root of `repo` (`git rev-parse --show-toplevel`), so a
/// worktree checkout sits at the repo root even when `repo` is a subdirectory.
/// Falls back to `repo` itself if git can't answer.
async fn git_toplevel(repo: &std::path::Path) -> std::path::PathBuf {
    tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| std::path::PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| repo.to_path_buf())
}

/// The shared git dir (`--git-common-dir`, made absolute). Its `info/exclude`
/// holds local, per-clone ignore rules that are never committed. Resolves
/// correctly for a normal repo, a linked worktree, and a submodule.
async fn git_common_dir(repo: &std::path::Path) -> std::path::PathBuf {
    let raw = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| std::path::PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| repo.join(".git"));
    if raw.is_absolute() {
        raw
    } else {
        repo.join(raw)
    }
}

/// Make sure the sub-agent worktree dir is git-ignored, WITHOUT touching the
/// repo's tracked `.gitignore`.
///
/// If `worktrees_dir` is already ignored — by the repo's `.gitignore`, a global
/// gitignore, or a previous `info/exclude` entry (`git check-ignore` sees all of
/// these) — nothing is done. Otherwise a `/.hrdr/worktrees/` rule is appended to
/// the clone-local `<git-common-dir>/info/exclude`, which is never committed, so
/// the worktree checkouts stay out of `git status` and `git add -A` without
/// touching a tracked file — and without ignoring the rest of `.hrdr/` (e.g. a
/// tracked `.hrdr/skills/`). Idempotent and best-effort.
async fn ensure_worktree_ignored(repo: &std::path::Path, worktrees_dir: &std::path::Path) {
    // Already ignored by any mechanism → leave it alone.
    let already_ignored = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "-q", "--"])
        .arg(worktrees_dir)
        .output()
        .await
        .map(|o| o.status.success()) // exit 0 = the path is ignored
        .unwrap_or(false);
    if already_ignored {
        return;
    }
    // Not ignored: add a clone-local exclude rule (not the tracked `.gitignore`).
    // Scoped to `.hrdr/worktrees/` specifically — NOT all of `.hrdr/`, which also
    // holds user-authored `.hrdr/skills/` that the repo may legitimately track.
    let exclude = git_common_dir(repo).await.join("info").join("exclude");
    const ENTRY: &str = "/.hrdr/worktrees/";
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|l| l.trim() == ENTRY) {
        return;
    }
    if let Some(parent) = exclude.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude)
    {
        let lead = if current.is_empty() || current.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let _ = write!(
            f,
            "{lead}# hrdr sub-agent worktrees (local, not committed)\n{ENTRY}\n"
        );
    }
}

impl Agent {
    /// Abort all running background sub-agent tasks and remove every background
    /// registry/live entry. Finished-but-undelivered tasks are discarded too.
    ///
    /// Clean worktrees are removed, but a worktree with **changes** (uncommitted
    /// files or commits on its branch) is **kept** — a `/new` or `/clear` must not
    /// silently throw away a sub-agent's unreviewed work. It stays on disk under
    /// `.hrdr/worktrees/` (recoverable via `git worktree list`).
    pub fn abort_background_tasks(&mut self) {
        let mut handles = self
            .bg_handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, handle) in handles.drain(..) {
            handle.abort();
        }
        drop(handles);

        // Workers publish only by finding their pre-existing registry/live entry.
        // Clearing both stores while holding their locks means a worker either
        // publishes before this cleanup (and is then removed) or finds no entry
        // afterward; no stale result can be recreated. Collect the worktrees
        // first so we can tear down the clean ones after releasing the lock.
        let worktrees: Vec<(std::path::PathBuf, String)> = {
            let mut reg = self
                .ctx
                .background_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let wts = reg
                .iter()
                .filter_map(|t| t.worktree.clone().zip(t.branch.clone()))
                .collect();
            reg.clear();
            wts
        };
        let repo = self.ctx.cwd.clone();
        for (path, branch) in worktrees {
            // Keep a worktree that holds unreviewed work — a reset must not throw
            // it away. Only remove the clean ones.
            if !worktree_has_changes_sync(&repo, &path, &branch) {
                remove_worktree(&repo, &path, &branch);
            }
        }
        self.registry.with(|v| {
            v.retain(|e| e.key == 0 || e.bg_id.is_none());
        });
    }

    /// Number of background sub-agent tasks currently tracked (running or
    /// recently finished but not yet reaped). Finished handles are reaped
    /// lazily here and in [`spawn_background`], so the count reflects live
    /// tasks after the reap.
    pub fn bg_handle_count(&self) -> usize {
        if let Ok(mut v) = self.bg_handles.lock() {
            // Best-effort reaping (see spawn_background).
            v.retain(|(_, h)| !h.is_finished());
            v.len()
        } else {
            0
        }
    }
}

#[cfg(test)]
mod revive_tests {
    use super::*;
    use crate::transcript_log::{EndStatus, Record, TranscriptLog};
    use hrdr_tools::Tool;

    /// A resolved dir cell pointing at `dir`, as the real one resolves post-save.
    fn cell(dir: &std::path::Path) -> ChildDirCell {
        Some(std::sync::Arc::new(std::sync::Mutex::new(Some(
            dir.to_path_buf(),
        ))))
    }

    /// Persist one run's transcript (`Start` [+ `Text`] [+ `End`]) at
    /// `dir/<stem>.jsonl`, as a real sub-agent run writes it.
    fn write_run(
        dir: &std::path::Path,
        stem: &str,
        label: &str,
        text: Option<&str>,
        complete: bool,
    ) {
        let mut t = TranscriptLog::create(dir, stem).unwrap();
        t.write(&Record::Start {
            model: "m".into(),
            label: label.into(),
            prompt: "do it".into(),
        });
        if let Some(x) = text {
            t.write(&Record::Text { chunk: x.into() });
        }
        if complete {
            t.write(&Record::End {
                status: EndStatus::Ok,
                bytes: 0,
            });
        }
    }

    /// `task_transcript` renders a run as plain text — reasoning and every tool
    /// call with its arguments and result — instead of the raw records.
    ///
    /// The tool exists because a real session followed a "`read` it for the
    /// complete run" pointer to a `.jsonl` and got one JSON record per streamed
    /// token back. `transcript_to_text` was no substitute: it prints `[tool: edit]`
    /// and drops the arguments and the result, which is the part you read a run
    /// back FOR.
    #[tokio::test]
    async fn task_transcript_renders_a_run_without_the_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = TranscriptLog::create(dir.path(), "003-planner").unwrap();
        t.write(&Record::Start {
            model: "m".into(),
            label: "Plan the protocol".into(),
            prompt: "plan it".into(),
        });
        // Streamed one token per record, exactly as a live run writes it.
        for chunk in ["I ", "should ", "read ", "the ", "codec."] {
            t.write(&Record::Reasoning { text: chunk.into() });
        }
        t.write(&Record::ToolStart {
            id: "c1".into(),
            name: "read".into(),
            args: r#"{"path":"src/codec.rs"}"#.into(),
        });
        t.write(&Record::ToolEnd {
            id: "c1".into(),
            name: "read".into(),
            result: "pub fn decode() {}".into(),
            ok: true,
        });
        t.write(&Record::Text {
            chunk: "Plan: three phases.".into(),
        });
        t.write(&Record::End {
            status: EndStatus::Ok,
            bytes: 0,
        });
        drop(t);

        let ctx = hrdr_tools::ToolContext::new(dir.path().to_path_buf());
        let out = TaskTranscriptTool {
            transcript_dir: cell(dir.path()),
        }
        .execute(serde_json::json!({"id": "003-planner"}), &ctx)
        .await
        .unwrap();

        // No JSON: not a record key in sight.
        assert!(
            !out.contains("{\"t\":") && !out.contains("\"chunk\""),
            "rendered text must carry no record syntax: {out}"
        );
        // The streamed deltas are joined back into readable prose.
        assert!(
            out.contains("I should read the codec."),
            "reasoning is reassembled: {out}"
        );
        // The part `transcript_to_text` throws away: the call's args AND result.
        assert!(out.contains("## Tool: read"), "{out}");
        assert!(
            out.contains(r#"{"path":"src/codec.rs"}"#),
            "args kept: {out}"
        );
        assert!(out.contains("pub fn decode() {}"), "result kept: {out}");
        assert!(out.contains("Plan: three phases."), "{out}");
    }

    /// One rendering, two windows: a peek and a page describe a run in the same
    /// vocabulary, and differ only in which end they keep.
    ///
    /// `task_output` used to render through `transcript_to_text` (tool name only,
    /// no args, no result) while `task_transcript` showed everything — so the same
    /// run looked like two different runs depending on which tool asked, and the
    /// one a model reaches for first was the poorer of the two.
    #[test]
    fn a_peek_and_a_page_render_the_same_way() {
        use crate::{EntryKind, transcript_to_plain_text};
        let entries = vec![
            crate::Entry::now(EntryKind::Tool {
                id: "c1".into(),
                name: "read".into(),
                args: r#"{"path":"src/codec.rs"}"#.into(),
                result: "pub fn decode() {}".into(),
                ok: true,
                done: true,
                expanded: false,
            }),
            crate::Entry::now(EntryKind::Assistant("done".into())),
        ];
        let rendered = transcript_to_plain_text(&entries, crate::TRANSCRIPT_TOOL_BODY_MAX);

        // Both windows carry the args and the result — the detail the peek's old
        // renderer dropped — and both are drawn from this one rendering.
        let peek = tail_lines(&rendered, 10_000);
        let page = window_lines(&rendered, 1, None, 10_000);
        for view in [&peek, &page] {
            assert!(view.contains("## Tool: read"), "{view}");
            assert!(view.contains(r#"{"path":"src/codec.rs"}"#), "{view}");
            assert!(view.contains("pub fn decode() {}"), "{view}");
        }
        // The difference is the framing, and each says which window it gave you.
        assert!(peek.starts_with("Progress so far"), "{peek}");
        assert!(page.starts_with("Transcript lines 1-"), "{page}");
    }

    /// A peek that must drop lines keeps the NEWEST ones and says how many it
    /// dropped — a peek silently starting mid-run reads like the whole run.
    #[test]
    fn a_peek_keeps_the_newest_lines_and_admits_the_cut() {
        let text: String = (1..=200).map(|i| format!("line {i}\n")).collect();
        // A budget that fits only a handful of lines once the header is reserved.
        let out = tail_lines(&text, 300);
        assert!(out.contains("line 200"), "keeps the newest: {out}");
        assert!(!out.contains("line 1\n"), "drops the oldest: {out}");
        assert!(
            out.contains("earlier omitted") && out.contains("task_transcript"),
            "and names what was dropped, plus where to read it: {out}"
        );
    }

    /// A long run pages like `read`, and says how much is left and how to ask for
    /// it — a transcript is read from the start, so silently keeping the tail
    /// (what a *peek* does) would hide the beginning with no sign it was cut.
    #[tokio::test]
    async fn task_transcript_pages_a_long_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = TranscriptLog::create(dir.path(), "004-long").unwrap();
        t.write(&Record::Start {
            model: "m".into(),
            label: "long".into(),
            prompt: "go".into(),
        });
        for i in 0..40 {
            t.write(&Record::Notice {
                msg: format!("step {i}"),
            });
        }
        drop(t);

        let ctx = hrdr_tools::ToolContext::new(dir.path().to_path_buf());
        let tool = TaskTranscriptTool {
            transcript_dir: cell(dir.path()),
        };
        let page = tool
            .execute(
                serde_json::json!({"id": "004-long", "offset": 1, "limit": 5}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(page.starts_with("Transcript lines 1-5 of "), "{page}");
        assert!(page.contains("continue with offset: 6"), "{page}");
        assert!(
            page.contains("step 0") && !page.contains("step 30"),
            "{page}"
        );

        // The next window starts where the last one ended.
        let next = tool
            .execute(
                serde_json::json!({"id": "004-long", "offset": 6, "limit": 5}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(next.starts_with("Transcript lines 6-10 of "), "{next}");
        assert!(!next.contains("step 0\n"), "no overlap with page 1: {next}");
    }

    /// An unknown run says so instead of returning an empty transcript, and a
    /// live-task id with no transcript points at the stem form.
    #[tokio::test]
    async fn task_transcript_refuses_what_it_cannot_read() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = hrdr_tools::ToolContext::new(dir.path().to_path_buf());
        let tool = TaskTranscriptTool {
            transcript_dir: cell(dir.path()),
        };
        let err = tool
            .execute(serde_json::json!({"id": "009-nope"}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no transcript at"), "{err}");
        // Not a valid stem at all.
        let err = tool
            .execute(serde_json::json!({"id": "../escape"}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a valid run id"), "{err}");
        // An integer id with nothing in the registry.
        let err = tool
            .execute(serde_json::json!({"id": 42}), &ctx)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no transcript for task #42"), "{err}");
    }

    /// Persist the sibling `<stem>.json` snapshot the revive path hydrates from.
    fn write_snapshot(
        dir: &std::path::Path,
        stem: &str,
        name: &str,
        cwd: &str,
        messages: Vec<ChatMessage>,
        read_only: bool,
    ) {
        let state = crate::SessionState {
            name: name.to_string(),
            cwd: cwd.to_string(),
            messages,
            read_only,
            ..Default::default()
        };
        crate::Session::new(state.persisted())
            .save_to_path(&dir.join(format!("{stem}.json")))
            .unwrap();
    }

    /// `task_list` merges the on-disk snapshots with the in-memory registry and
    /// does NOT list a run that is both live and on disk twice (the live entry's
    /// transcript path stem identifies its on-disk pair).
    #[tokio::test]
    async fn task_list_merges_disk_runs_and_dedupes_live() {
        let dir = tempfile::tempdir().unwrap();
        // A completed run that is ALSO live this session (a registry entry whose
        // transcript names `000-audit.jsonl`) → shown once, as the live row.
        write_run(dir.path(), "000-audit", "audit task", Some("done"), true);
        // An orphan with no snapshot → labelled from its `Start` record.
        write_run(dir.path(), "001-explore", "explore auth", None, false);

        let ctx = hrdr_tools::ToolContext::new(std::env::temp_dir());
        ctx.background_tasks
            .lock()
            .unwrap()
            .push(hrdr_tools::BackgroundTask {
                id: 5,
                label: "audit task".to_string(),
                done: true,
                result: Some("ok".to_string()),
                transcript: Some(dir.path().join("000-audit.jsonl")),
                ..Default::default()
            });

        let out = TaskListTool {
            transcript_dir: cell(dir.path()),
        }
        .execute(serde_json::json!({}), &ctx)
        .await
        .unwrap();

        assert!(out.contains("#5"), "the live row is present: {out}");
        assert!(out.contains("On disk"), "the disk section header: {out}");
        assert!(
            out.contains("001-explore [orphaned] explore auth"),
            "the orphan is listed, labelled from its Start record: {out}"
        );
        assert!(
            !out.contains("000-audit"),
            "the live-and-on-disk run is not duplicated in the disk section: {out}"
        );
    }

    /// A model-supplied stem that tries to escape the snapshot dir (path
    /// separators, `..`) is rejected before it is joined onto a path.
    #[test]
    fn run_stem_rejects_path_traversal() {
        assert!(valid_run_stem("003-fix"));
        assert!(valid_run_stem("000-audit"));
        assert!(!valid_run_stem(""));
        assert!(!valid_run_stem("../secrets"));
        assert!(!valid_run_stem("a/b"));
        assert!(!valid_run_stem("a\\b"));
        assert!(!valid_run_stem(".."));
        assert!(!valid_run_stem("/etc/passwd"));
    }

    /// An on-disk run belongs to `task_transcript`; `task_output` is live-only and
    /// hands a stem over instead of serving a lossier copy of the same answer.
    ///
    /// `task_output` used to read stems itself, rendering through
    /// `transcript_to_text` — which prints `[tool: read]` and drops the arguments
    /// and the result. Two tools answering one question at different fidelities
    /// means whichever the model reaches for first decides how much it learns, so
    /// the overlap was removed rather than left to chance.
    #[tokio::test]
    async fn task_output_is_live_only_and_hands_a_stem_to_task_transcript() {
        let dir = tempfile::tempdir().unwrap();
        write_run(
            dir.path(),
            "003-fix",
            "fix the bug",
            Some("HELLO-FROM-DISK"),
            true,
        );
        let ctx = hrdr_tools::ToolContext::new(std::env::temp_dir());

        // Any non-integer id is refused, naming what it got and where it IS served.
        for given in ["003-fix", "not a stem"] {
            let err = TaskOutputTool {
                live: AgentRegistry::new(),
            }
            .execute(serde_json::json!({"id": given}), &ctx)
            .await
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("integer id") && err.contains("task_transcript"),
                "the refusal names the tool that serves it, for `{given}`: {err}"
            );
            assert!(err.contains(given), "and echoes what it got: {err}");
        }
        // An all-digit string is still that live id, differently typed.
        assert!(
            TaskOutputTool {
                live: AgentRegistry::new(),
            }
            .execute(serde_json::json!({"id": "7"}), &ctx)
            .await
            .unwrap_err()
            .to_string()
            .contains("no background task #7"),
            "an all-digit string resolves as an integer id, not a stem"
        );

        // And the capability did not vanish with the branch: the same run reads
        // back through `task_transcript`, with more than it had before.
        let out = TaskTranscriptTool {
            transcript_dir: cell(dir.path()),
        }
        .execute(serde_json::json!({"id": "003-fix"}), &ctx)
        .await
        .unwrap();
        assert!(
            out.contains("HELLO-FROM-DISK"),
            "the persisted output still reads back: {out}"
        );
    }

    /// The dirty-tree nudge fires once per distinct working-dir state, so a
    /// parallel fan-out gets one warning rather than one per task — but a tree
    /// that changed since (groundwork committed, something new dirtied) is news
    /// again.
    #[test]
    fn the_dirty_tree_nudge_repeats_only_when_the_tree_changed() {
        let warned = std::sync::Mutex::new(None);
        let scaffold = vec!["src/new_module.rs".to_string(), "src/lib.rs".to_string()];

        assert!(
            dirty_state_is_news(&warned, &scaffold),
            "the first spawn from a dirty tree must warn"
        );
        assert!(
            !dirty_state_is_news(&warned, &scaffold),
            "three more parallel spawns off the same tree must not repeat it"
        );
        assert!(!dirty_state_is_news(&warned, &scaffold));

        // The model committed the scaffold and only scratch is left: a different
        // state, and one worth saying out loud again.
        let after_commit = vec!["notes.txt".to_string()];
        assert!(dirty_state_is_news(&warned, &after_commit));
        assert!(!dirty_state_is_news(&warned, &after_commit));

        // A clean tree is never warned about at the call site, but it is still a
        // state change — so dirtying the tree again later warns.
        assert!(dirty_state_is_news(&warned, &[]));
        assert!(dirty_state_is_news(&warned, &after_commit));
    }

    /// The nudge's input: uncommitted work in the parent tree is what a worktree
    /// spawn cannot see, and it must include untracked files (a brand-new
    /// scaffold module is untracked, and is exactly what gets forgotten).
    #[tokio::test]
    async fn uncommitted_groundwork_is_visible_to_the_spawn_check() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "hi").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        if !repo.join(".git").exists() {
            return; // git unavailable — skip
        }
        assert_eq!(
            worktree_dirty_files(repo).await.as_deref(),
            Some(&[][..]),
            "a committed tree is clean, so a spawn from it says nothing"
        );

        // Groundwork a delegating agent typically leaves behind: a new untracked
        // module, plus an edit to a tracked file that the chunks build on.
        std::fs::write(repo.join("scaffold.rs"), "pub trait New {}").unwrap();
        std::fs::write(repo.join("f.txt"), "changed").unwrap();
        let dirty = worktree_dirty_files(repo).await.unwrap();
        assert!(
            dirty.contains(&"scaffold.rs".to_string()),
            "an untracked scaffold must be reported: {dirty:?}"
        );
        assert!(
            dirty.contains(&"f.txt".to_string()),
            "a modified tracked file must be reported: {dirty:?}"
        );
        assert!(
            dirty_state_is_news(&std::sync::Mutex::new(None), &dirty),
            "and that state warns"
        );
    }

    /// `task_revive`'s disk fallback hydrates from the `<stem>.json` snapshot
    /// (real persisted messages) and reuses the run's still-on-disk worktree +
    /// branch. A worktree removed since is refused, not continued elsewhere.
    #[tokio::test]
    async fn task_revive_disk_fallback_hydrates_and_reuses_worktree() {
        // A real repo, so `Worktree::create` works and `current_branch` resolves.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "hi").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        if !repo.join(".git").exists() {
            return; // git unavailable — skip
        }
        let wt = Worktree::create(repo).await.unwrap().keep();
        let wt_path = wt.path.clone();
        let branch = wt.branch.clone();

        // Persist a sub-agent snapshot whose cwd is that worktree, with real
        // messages the revive must carry forward.
        let subdir = tempfile::tempdir().unwrap();
        write_run(
            subdir.path(),
            "002-coder",
            "add feature",
            Some("did it"),
            true,
        );
        write_snapshot(
            subdir.path(),
            "002-coder",
            "add feature",
            &wt_path.display().to_string(),
            vec![
                ChatMessage::user("the original brief"),
                ChatMessage::assistant("done"),
            ],
            false,
        );

        let st = revive_target_from_disk(subdir.path(), "002-coder")
            .await
            .unwrap();
        assert_eq!(st.label, "add feature");
        assert_eq!(
            st.worktree,
            Some((wt_path.clone(), branch.clone())),
            "reuses the existing worktree + branch"
        );
        assert!(
            st.messages
                .iter()
                .any(|m| m.content.as_deref() == Some("the original brief")),
            "hydrates the persisted messages (with their signed thinking blocks)"
        );

        // The worktree removed since → refused, not continued somewhere else (and
        // no crash).
        remove_worktree(repo, &wt_path, &branch);
        let gone = revive_target_from_disk(subdir.path(), "002-coder").await;
        assert!(
            gone.is_err(),
            "a removed worktree is refused: {:?}",
            gone.err()
        );
    }

    /// A revived sub-agent comes back with the capability it RAN with: the
    /// snapshot records `read_only`, and the config the revive rebuilds it on
    /// prunes the registry the same way its profile did.
    ///
    /// Pins the regression where capability was not persisted at all, so a
    /// revived `explore`/`review`/`plan` silently gained the writers and the shell
    /// its profile withheld — in the recorded (shared) working dir.
    #[tokio::test]
    async fn a_revived_read_only_run_gets_no_writers() {
        let dir = tempfile::tempdir().unwrap();
        // A read-only run's cwd is the shared dir, never a worktree — it has none.
        let cwd = dir.path().display().to_string();
        write_run(
            dir.path(),
            "004-explore",
            "explore auth",
            Some("looked"),
            true,
        );
        write_snapshot(
            dir.path(),
            "004-explore",
            "explore auth",
            &cwd,
            vec![ChatMessage::user("look around")],
            true,
        );
        write_run(dir.path(), "005-coder", "add feature", Some("did it"), true);
        write_snapshot(
            dir.path(),
            "005-coder",
            "add feature",
            &cwd,
            vec![ChatMessage::user("build it")],
            false,
        );

        let base = subagent_base_config(&AgentConfig {
            model: "local://m".parse().unwrap(),
            ..Default::default()
        });
        // The tool set the revive path actually builds — asserted on the names, the
        // registry being pruned before the sub-agent runs.
        let tools = |st: &RevivedState| -> Vec<String> {
            let agent = Agent::new(revive_base_config(&base, st.read_only)).unwrap();
            let mut names: Vec<String> = agent.tools().into_iter().map(|(n, _)| n).collect();
            names.sort();
            names
        };

        let ro = revive_target_from_disk(dir.path(), "004-explore")
            .await
            .unwrap();
        assert!(ro.read_only, "the snapshot records the read-only scope");
        let ro_tools = tools(&ro);
        assert!(
            ro_tools.contains(&"read".to_string()) && ro_tools.contains(&"grep".to_string()),
            "it is still an agent — the readers stay: {ro_tools:?}"
        );
        for w in ["write", "edit", "move", "delete", "copy"] {
            assert!(
                !ro_tools.contains(&w.to_string()),
                "a revived read-only run must not get `{w}`: {ro_tools:?}"
            );
        }
        // A shell it DOES get: read-only is the sandbox's job
        // (`effective_sandbox` → `SandboxMode::Read`), and a revived run is
        // scoped by the same constructor as a fresh one, so it lands here too.
        assert!(
            ro_tools.contains(&"shell".to_string()),
            "a revived read-only run keeps its shell: {ro_tools:?}"
        );

        let rw = revive_target_from_disk(dir.path(), "005-coder")
            .await
            .unwrap();
        assert!(!rw.read_only, "a write run is recorded as write-capable");
        let rw_tools = tools(&rw);
        for w in ["write", "edit", "shell"] {
            assert!(
                rw_tools.contains(&w.to_string()),
                "a revived write run keeps `{w}`: {rw_tools:?}"
            );
        }
    }
}
