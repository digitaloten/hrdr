# Threading plan — dedicated execution contexts for TUI, sub-agents, and tool calls

Status: plan v2 (2026-08-04), incorporating the review of v1. Target: 1 thread
for the main TUI, 1 thread per sub-agent, 1 thread per tool call — so that the
UI stays snappy while agents work. This document maps the current model
(measured, not guessed), maps the target onto the codebase's concurrency
primitives, and lists the concrete work slices.

## Current state (measured)

One multi-threaded tokio runtime runs the whole process (`#[tokio::main]`,
default flavor, workers = CPUs — `apps/hrdr/src/main.rs:481`). Every unit of
concurrency is a tokio task on that pool; there are **no dedicated OS threads**
in production code anywhere in the workspace (every `std::thread::spawn` is in
`#[cfg(test)]`, or the config-watch debounce thread at
`crates/hrdr-app/src/util.rs:309`).

- **The TUI** is the runtime's root future, driven on the main OS thread
  (`hrdr_tui::run` → `run_loop`, `crates/hrdr-tui/src/tui.rs:45-156`): a draw +
  `tokio::select!` over crossterm events, the `TurnMsg` channel, and a 120 ms
  repaint ticker. Already the "1 TUI thread".
- **A turn** is a `tokio::spawn`'d task (`AgentRegistry::start_turn_on`,
  `crates/hrdr-agent/src/registry.rs:865`) that takes the per-agent
  `tokio::sync::Mutex<Agent>` and holds it for the **entire** `Agent::run`
  (`registry.rs:890-891`). The UI thread only ever `try_lock`s it.
- **Sub-agents** are already one task each, each with its own
  `Arc<Mutex<Agent>>` (`crates/hrdr-agent/src/delegation.rs:225,324,353`), so a
  main turn and a sub-agent already run concurrently; concurrency is bounded by
  `SubagentSlots` (`delegation.rs:25-74`). Sub-agent _construction_
  (`Agent::new`, config read + skills scan) runs synchronously on the parent's
  turn task (`delegation.rs:220`) — deliberately, so `task_steer` can address
  the id as soon as `task` returns (the code documents this at
  `delegation.rs:218-219`).
- **Tool calls** run **inline in the turn task**: `run_tool_batch`
  (`crates/hrdr-agent/src/turn_loop.rs:992-1169`) boxes each call as a future
  (`turn_loop.rs:1053`) and polls them with `join_all` inside the task
  (`turn_loop.rs:1152-1161`). Blocking filesystem work runs inline on whatever
  tokio worker polls that task — `std::fs` whole-file reads in
  `read.rs:105-134`, per-file reads in `grep.rs:230,325`, blocking `ignore`
  walks in `find.rs:66-82` and `tree.rs:119-132`, the
  `workspace_map`/`workspace_members` walks inside the `task` tool
  (`delegation.rs:1429,1476-1598`). The only `spawn_blocking` calls in the
  workspace are `web.rs:56,245`, the retention sweep
  (`crates/hrdr-tui/src/lib.rs:176`), and the `@file` index walk
  (`crates/hrdr-app/src/util.rs:362`). (`memory::recall` at
  `turn_loop.rs:912-922` is _deliberately_ synchronous — the comment says small
  files, best-effort — and stays that way; it is not in Slice 1.)
- **UI-thread blocking**: the whole session is `serde_json::to_string`'d and
  atomically written on the UI thread after **every committed tool round**
  (`persist_mid_turn`, `crates/hrdr-tui/src/app/session.rs:154-177`, driven by
  `apply_event` at `app.rs:2690-2692`) and at every turn end (`autosave`,
  `session.rs:237-266`); `@file`/`@dir` attach reads run synchronously on the UI
  thread on every submit (`prepare_outgoing_tracked`,
  `crates/hrdr-app/src/util.rs:184-213`); `sync_panes` clones registry entries
  every frame (`crates/hrdr-agent/src/pane.rs:302-325`).
- **Synchronization**: one bounded `mpsc::channel::<TurnMsg>(1024)` from all
  tasks to the UI loop, with an `EventSender` coalescing sink
  (`app.rs:270-365`); `std::sync::Mutex`-backed registry, steering queues, todos
  and background-task lists.

## Target model — mapped to the codebase

The codebase's concurrency unit is the tokio task, not the OS thread. The three
"threads" map as follows; each is delivered with the primitives the code already
uses rather than raw `std::thread` (see Non-goals for why raw threads would be
wrong):

| Target                      | Delivery                                                                                                                                                                                 | Today                                                        |
| --------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------ |
| 1 thread for the main TUI   | the root task on the main thread, **plus no blocking work on it**                                                                                                                        | root task exists; save/send paths block it                   |
| 1 thread per sub-agent loop | one spawned task per sub-agent (already true), **plus construction off a tokio worker**                                                                                                  | one task each; construction inline on the parent's turn task |
| 1 thread per tool call      | each tool call's blocking work on the blocking pool (a dedicated thread per call), and each call in a batch dispatched as its own task so CPU-bound calls run in parallel across workers | inline in the turn task, blocking fs on a worker             |

The "snappy" wins are (a) removing the per-round multi-MB save from the UI
thread, (b) removing blocking fs from tokio workers, (c) sub-agent construction
off the parent's turn task. (a) and (b) are the ones users feel.

## Work slices

Ordered so each slice compiles and passes the gate on its own, and so nothing
touches the same files as a sibling (disjoint write sets).

### Slice 1 — blocking tool internals → `spawn_blocking` (the "1 thread per tool call" core)

Files: `crates/hrdr-tools/src/tools/read.rs`, `grep.rs`, `find.rs`, `tree.rs`,
`crates/hrdr-agent/src/delegation.rs` (workspace walks).

Move the blocking filesystem work inside each tool's async `execute` onto
`tokio::task::spawn_blocking`. Each blocking section must be a self-contained
`Send` closure taking owned values (the resolved `PathBuf`, cloned
`cwd`/`max_output`/`max_output_lines`, the parsed regex) — no borrows into
`&ToolContext` across the boundary — and the async wrapper stays. The extraction
is per _section_, not per line:

- `read.rs:105-134` — the whole open + guard + `read_to_string` sequence in one
  closure (it is contiguous sync code).
- `grep.rs` — **the whole walk loop** (`grep_builtin`/`grep_builtin_multiline`,
  walker at `grep.rs:213,310` + per-file reads at `230,325`) in one closure; not
  one `spawn_blocking` per file.
- `find.rs:66-82` — the whole `ignore` walk in one closure.
- `tree.rs:119-132` — the `collect_entries` walk in one closure (`render_tree`
  at 113 is CPU-only and stays).
- `delegation.rs:1429,1476-1598` — `workspace_map`/`workspace_members` walks.

`memory::recall` (`turn_loop.rs:912-922`) is explicitly **not** in this slice:
the code documents it as deliberately synchronous (small files, best-effort),
and moving it would contradict a stated decision for the cheapest of the five
sites.

### Slice 2 — per-call dispatch in the tool batch (parallel CPU-bound tool calls)

File: `crates/hrdr-agent/src/turn_loop.rs` (`run_tool_batch`, 992-1169).

Today `join_all` polls every call's future on the turn task, so CPU-bound tool
code serializes. Change the batch to `tokio::spawn` each call (the futures are
already `Box<dyn Future<Output = TimedResult> + Send>` at `turn_loop.rs:1053`,
with `'static` captures — `ToolContext` clone, `String`s, `Arc<ToolRegistry>`,
`Arc<Vec<Hook>>`) and `join_all` the `JoinHandle`s, preserving join-order ==
call-order (the `zip` at 1166 still pairs call↔result) and the stream-forwarder
interleave (`tokio::select!` at 1154-1161). The per-call timeout stays inside
the spawned future (`crates/hrdr-tools/src/lib.rs:1756`), so it is unaffected.

Two mechanisms the naive change silently breaks, both mandatory:

1. **Panic containment.** Today a panicking tool future unwinds the turn task
   and `catch_unwind` at `registry.rs:893` turns it into
   `TurnOutcome { panicked: true }` + the `open_tool_ends` cleanup
   (`registry.rs:904-916`). With `tokio::spawn`, the panic is captured into the
   `JoinHandle` and `await` yields `JoinError` **without unwinding** — the
   `catch_unwind` never fires. The batch must explicitly
   `std::panic::resume_unwind(payload)` for a panicked handle _inside_ the
   existing `AssertUnwindSafe` scope (or convert it to the same turn-error +
   abort-siblings outcome) so the observable behavior is unchanged.
2. **Cancel/abort.** Esc-Esc cancels by aborting the turn task
   (`registry.rs`/`app.rs` cancel path), which today drops the `join_all` and
   each tool future — a `bash` tool's `kill_on_drop` then kills the child.
   Dropping a `JoinHandle` merely _detaches_ the task, leaving every in-flight
   tool running to its own timeout. The turn task needs a Drop guard that aborts
   all in-flight handles (which propagates to the children via the existing
   `kill_on_drop`), so a cancelled turn still stops its tools.

### Slice 3 — session saves off the UI thread

Files: `crates/hrdr-tui/src/app/session.rs` (`persist_mid_turn`, `autosave`,
`reserve_session_id`).

**The split is the whole point, and it is not "move `save_session` to a task".**
`save_session` (`crates/hrdr-agent/src/session.rs:1362-1390`) does two things:
(a) the **mint** — `unique_session_id` (fs uniqueness check + reservation lock),
`acquire_session_lock`, and returns `SaveOutcome { id, first_save, open_lock }`
which the UI applies as `state.id`, the held open-lock, and
`refresh_subagent_dir` — and (b) the **write** —
`Session::new(state.persisted()).save(&id)` (serialize + atomic write). The mint
MUST stay synchronous on the UI thread: the id and transcript dir must exist
before the turn starts (the sub-agent transcript dir is derived from it,
`session.rs:179-186`), and a background mint would let a second save/reserve
mint a _second_ id + open-lock + file while the first is in flight. So:

- Keep `reserve_session_id` fully synchronous (it already is).
- `persist_mid_turn` / `autosave`: capture the snapshot **on the UI thread**
  (the state mutation stays there), then spawn the serialize + write with a
  **latest-wins coalescer**: at most one save task in flight; a pending flag
  holds a newer snapshot; the newest pending snapshot is always written next. A
  later save always supersedes an in-flight one — the snapshot is captured at
  enqueue time, so `/rename` and `/clear` (which mutate `state.name`/start a new
  session) cannot interleave with a stale in-flight write.
- **The turn-end flush needs an await point.** `on_turn_msg` is a synchronous
  `&mut self` method (`app.rs:2445`) and its `Done` arm calls `autosave()`
  synchronously (`app.rs:2507`) — a coalescer flush that must await the
  in-flight task cannot run there. Add the await in the `run_loop` select branch
  that drains `TurnMsg` (`tui.rs:143-151`) or make the `Done` handling
  asynchronous; the quit path (`tui.rs:105-119`) is already async and awaits.
- The `created_cache` (`crates/hrdr-agent/src/session.rs:746-749`) is a
  thread-safe `std::sync::Mutex<HashMap<PathBuf, u64>>` and needs no change —
  the ordering hazard is the mint, not the cache.
- The `jsonl` transcript already covers crash recovery between snapshots, so a
  coalesced save is safe (perf-review finding #1d).

### Slice 4 — `@file` attach reads off the UI thread

File: `crates/hrdr-app/src/util.rs` (`expand_mentions_tracked` /
`read_attach_file` / `read_attach_dir`, 130-213).

The attach reads and `discover_skills` scans run synchronously on the UI thread
per submit (from `submit_input` `app.rs:1282`, `spawn_turn` `app.rs:2236`,
`send_to_subagent` `app.rs:1853`). Move the reads to `spawn_blocking` and await
them on the send path. `prepare_outgoing_tracked` is called from
`prepare_outgoing_via` (`crates/hrdr-app/src/commands/helpers.rs:231`) and the
callers above; the reads become a spawned section the caller awaits — the
display copy stays on the UI thread, only the fs reads go off it.
`mark_files_read` / `agent_names` / `agent_cwd` (helpers.rs:238-242) stay on the
caller side and are unaffected. Note the ripple: `submit_input` / `spawn_turn` /
`send_to_subagent` are synchronous methods today; making the expansion await
something touches each caller (including the headless runner).

### Slice 5 — sub-agent construction off the turn task's worker

File: `crates/hrdr-agent/src/delegation.rs` (`SubagentTool::execute`, 1248-1449;
`spawn_background`, 171-555).

`Agent::new` for a sub-agent (config read, profile resolution, skills discovery,
LSP setup — `lib.rs:1477-1626`) runs synchronously on the parent's turn task
(`delegation.rs:220`), and **must stay before the entry is registered**:
`task_steer` must be able to address the id as soon as `task` returns, and a
same-batch concurrent `task`+`task_steer` or an immediate user steer would be
silently dropped for a not-yet-registered key (the registry returns `None` for
absent keys). So the parent's turn task cannot fully avoid waiting for
construction; the slice is to stop that wait from occupying a tokio worker:

- Wrap the synchronous `Agent::new` (and the `workspace_map`/`workspace_members`
  walks from Slice 1) in `spawn_blocking(...).await` inside `spawn_background`,
  keeping registration synchronous at the same point it is today
  (`delegation.rs:238`). The parent's turn task still waits for the ack (it must
  — the `task` result is the ack), but a worker thread is freed and the stall is
  off the runtime's main pool.
- A fully-async construction (register a placeholder entry, build the agent in
  the spawned task, queue steering until the entry appears) is explicitly
  deferred: it requires `AgentRegistry` to tolerate not-ready keys and changes
  the documented `task`→`task_steer` ordering guarantee. Note also that the
  catalog warm-up named in earlier drafts does not run for sub-agents (it is
  gated on `!config.delegated`, `lib.rs:1562`).

## Non-goals and risks

- **No raw `std::thread` per sub-agent or per tool call.** The turn loop and
  tools are async and await tokio primitives (timers, channels,
  `tokio::process`); they need a runtime on their thread. A current_thread
  runtime per sub-agent thread would duplicate the whole runtime stack for no
  isolation the current per-task model lacks. `spawn_blocking` already provides
  a dedicated thread per blocking call, which is the isolation the user-facing
  symptom needs.
- **The whole-turn agent mutex stays.** Holding `tokio::sync::Mutex<Agent>`
  across `run()` is what makes mid-turn steering and the event fold coherent;
  the UI already works around it with `try_lock`. Where a turn's latency blocks
  the UI, the fix is Slice 3, not the mutex.
- **Slice 2 cancel/panic are the highest-risk mechanics** — an aborted turn
  leaving spawned tool tasks (including `bash` subprocesses) detached is a
  silent behavior regression; the handle-abort Drop guard and the
  `resume_unwind` are part of the slice, not optional.
- **Slice 3 ordering**: the mint must not move; the turn-end flush needs an
  await point that does not exist today; snapshot-at-enqueue is what keeps
  `/rename` and `/clear` ordered.
- **Slice 5**: registration must stay ahead of the `task` ack; anything that
  delays it (placeholder entries) is deferred, not done silently.
- `catch_unwind` and cancellation (Slice 2) must preserve the existing panic
  containment and abort-on-cancel semantics; the `JoinHandle`s are polled inside
  the existing `AssertUnwindSafe` scope.

## Verification

Each slice: full workspace gate (`cargo fmt --all --check`, clippy with
`-D warnings`, `cargo build --locked -p hrdr`,
`cargo nextest run --workspace --all-features --locked --no-fail-fast`) plus the
crate's tests for the touched modules. Slice 2 adds a test that a panicking tool
still ends the turn with the panic outcome, and one that a cancelled turn's
spawned shell child is killed. Slice 3 adds e2e assertions for save ordering
(save after turn end, single session id across coalesced saves, no torn session
file). Slices are implemented one at a time, each reviewed, committed and pushed
before the next starts.
