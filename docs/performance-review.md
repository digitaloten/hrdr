# Performance review — 2026-08-04

Scope: full codebase (working tree clean, all fixes from the 2026-08-04 code
review pushed). Report only — no code changed. Findings ranked by impact; each
was verified by tracing the caller that establishes frequency and the input
size.

## Findings

### 1. Per-round full-history snapshot: clone ×3 + serialize + atomic write, on the UI thread

`crates/hrdr-agent/src/turn_loop.rs:759`
(`on_event(AgentEvent::History(self.messages.clone()))`),
`crates/hrdr-agent/src/registry.rs:415` (`log.push(ev.clone())`),
`crates/hrdr-tui/src/app.rs:2690-2691` →
`crates/hrdr-tui/src/app/session.rs:154-164` (`persist_mid_turn` →
`save_session`, synchronous on the UI event loop — `on_turn_msg` at app.rs:2445
→ `apply_event` at :2456), `crates/hrdr-agent/src/session.rs:341` (`persisted()`
full-state clone), `:778-786` (`snap` clone + `to_string` + `write_atomic`)

Once per tool round (`for step in 0..self.max_steps`, turn_loop.rs:512), the
whole `Vec<ChatMessage>` — every tool result, multi-MB at the end of a long turn
— is cloned three times (turn loop → registry event log → frontend
`persist_mid_turn`), then the session is cloned twice more and fully
re-serialized and atomically rewritten. A 20-round turn costs O(N) clones +
serializes + disk writes per round, i.e. O(N²) over the turn, all stalling the
UI event loop. The registry-log copy's payload is effectively dead: the
transcript reducer ignores `History` entirely (transcript.rs:678-681), the
panes' `events_since` replay (pane.rs:397) goes through that reducer, and the
only other reader, the headless runner (apps/hrdr/src/main.rs:1070), uses just
`msgs.len()`.

Fix, in increasing order of effort:

- (a) the registry log stores a lightweight marker for `History` (the event's
  existence advances the cursor; carry only the message count for main.rs:1070)
  — removes clone #2;
- (b) `apply_event` takes the `History` payload by value and hands it to
  `persist_mid_turn` (app.rs:2691 `messages.clone()` → move) — removes clone #3;
- (c) `Session::save` avoids the double state clone: build the `created`
  timestamp before serializing and serialize the original (`persisted()` +
  `snap` are two full copies per save, session.rs:341/:778);
- (d) throttle mid-turn snapshots (a 1–2 s debounce, or move the write off the
  UI thread): the append-only `<id>.jsonl` transcript already covers crash
  recovery between snapshots, so the full rewrite per round is redundant
  durability.

### 2. Request body: history cloned, deep-copied into a Value tree, then serialized — per request

`crates/hrdr-llm/src/client.rs:999` (`messages: messages.to_vec()`),
`:1062-1063` (`serde_json::to_value(body)`), `:1016-1018` (`post` →
`.json(body)`)

Every request (one per round, from `chat_stream` at :1191-1203) copies the whole
history into the `ChatRequest`, deep-copies it into a `serde_json::Value` tree,
and serializes from that tree. The `Value` intermediate exists only for the
cache-breakpoint / `prompt_cache_key` / DeepSeek grafts (:1064-1118); when none
apply — `cache != Ephemeral`, no consumed `prompt_cache_key`, not a DeepSeek
host — which is every local-server default, it is pure waste.

Fix: when no graft applies, serialize `ChatRequest` straight to bytes
(`serde_json::to_vec(&request)`) and send `.body(bytes)`; keep the Value path
for the graft cases. Removes a full deep copy and all per-string `Value::String`
allocations per round.

### 3. `list_sessions()` re-scans the sessions dir on every frame and every keystroke while a `/resume` popup is live

`crates/hrdr-app/src/completion.rs:201-204`
(`"resume" => crate::list_sessions()`), called per frame from
`crates/hrdr-tui/src/ui.rs:148` (`draw` → `active_completions()`, on the ~120 ms
repaint ticker even when nothing changed) and per keypress from
`crates/hrdr-tui/src/app.rs:994`. `list_sessions` (session.rs:1193-1209) does
`read_dir` of the sessions dir and every cwd subdir, a `metadata()` stat per
session file (plus a cached-meta clone), and a sort — every call. The
mtime-keyed `meta_cache` (session.rs:1121) only avoids the JSON parse; the scan
itself is paid ~8×/s plus per keystroke.

Fix: memoize `active_completions()` keyed by editor content (recompute on key
press/paste, reuse in `draw`), and/or cache the `list_sessions()` result for the
popup's lifetime keyed by sessions-dir mtime.

### 4. `@file` completion re-ranks the whole 20k-file index per frame and per keystroke

`crates/hrdr-tui/src/app/completion.rs:160-166` (`file_completion_items` →
`rank_file_matches(&self.file_index, query)`),
`crates/hrdr-app/src/completion.rs:105-130` (`rank_file_matches`:
`p.to_ascii_lowercase()` per path, a scored Vec of every match, a sort), index
size `WALK_MAX_FILES = 20_000` (`crates/hrdr-app/src/util.rs:455`)

The index build is off-thread and cached (good); the ranking is not — while an
`@…` token is active it runs per frame (ui.rs:148) and per keypress
(app.rs:994), allocating one lowercase String per path per call.

Fix: the content-keyed memo from finding 3 removes the per-frame half; for the
per-keystroke half, precompute a lowercase path table once when the index lands
and rank against that.

### 5. Full-history token re-estimate every round on endpoints that report no usage

`crates/hrdr-agent/src/budget.rs:122-123`
(`estimate_tokens_in_messages(&self.messages)` inside `account_usage`, which
runs every round — turn_loop.rs:562)

On any server that reports no usage (common for local llama.cpp/vLLM), this is a
full O(history) token pass per round to feed the context gauge and the
compaction trigger.

Fix: keep a running `messages_tokens` counter on the agent, incremented per
pushed/edited message and reset on compaction/clear — O(1) per round. History is
append-mostly, so the running sum stays exact except across compaction, which
rewrites everything anyway.

### 6. One realpath chain per file / per output line in the secret filters

`crates/hrdr-tools/src/tools/grep.rs:219`
(`secret_file_reason(&crate::canonicalize_nearest(path))` inside the `'walk`
loop, per file visited) and `crates/hrdr-tools/src/lib.rs:1233`
(`grep_line_is_secret` → `canonicalize_nearest(&cwd.join(tok))`, per ingested
line that matches the `path:NN:` shape, from `ingest_line!`)

The `ignore` walker already excludes symlinked entries (`file_type().is_file()`
at grep.rs:215-216), so every walked path is a real file and the
canonicalization (a `canonicalize()` syscall chain per file) resolves nothing —
the name check only needs the basename. `rg -n` output repeats the same path
token once per match line, so 200 hits of one file = 200 identical realpath
chains.

Fix: drop the per-file canonicalize in the grep walk (the walker's paths are
already real); memoize the secret verdict per path token for one command run in
the shell path.

### 7. Compaction tail-window selection re-sums overlapping suffixes

`crates/hrdr-agent/src/compaction.rs:451-460` —
`estimate_tokens_in_messages(&msgs[start..tail_start])` per candidate,
re-summing a growing suffix from scratch; plus `first_viable_compact_stage`'s
per-stage history clone and re-estimate (compaction.rs:948-953). O(tail_turns ×
history) on each compaction — and compaction runs exactly when the history is at
its largest.

Fix: one newest→oldest pass accumulating per-message token counts into a running
total; reuse a single elided copy across the ladder sizing.

### 8. `/resume` picker rebuilds every row and column width per frame

`crates/hrdr-tui/src/ui.rs:600-663` (`draw_session_selector`, per frame on the
repaint ticker): per session, a `chrono::DateTime::from_timestamp` + clock read

- `relative_time` formatting and a `display_dir`, then three full passes over
  all rows for column widths — none of which changes between frames unless the
  filter or selection moved.

Fix: cache the rendered rows and derived widths on the `SessionSelector`,
recompute only on filter/selection/list change.

### 9. Picker refilter allocates per candidate per keystroke

`crates/hrdr-tui/src/app/selector.rs:43-46` (`refilter` on every `push_char`/
`backspace`), `crates/hrdr-agent/src/models.rs:786-791` (`filter_model_choices`
builds `format!("{} {} {}://{}", …).to_lowercase()` per candidate) — same shape
in `filter_themes`, `filter_sessions`, `filter_skills`.

Fine for the small pickers; for the model picker over a large catalog this is
O(n) `format!`+`to_lowercase` allocations per keystroke, each followed by a
subsequence scan.

Fix: precompute a lowercase haystack per choice once when the `Selector` is
constructed and match against that.

### 10. fstat syscall per transcript record

`crates/hrdr-agent/src/transcript_log.rs:434` (`self.file.metadata()` per
`append_line`, i.e. per coalesced record — every ≤512 bytes of streamed text
plus every tool event). The value is used only to roll back a partial write.

Fix: track the appended length in the struct (increment on success, `set_len` on
rollback) and skip the fstat on the happy path.

## Coverage

Traced: the round loop and event pipeline (turn_loop, registry, panes,
transcript), session save/load, request building and SSE decoding (hrdr-llm),
shell/grep/find/ls output paths, the compaction and budget paths, transcript
logging, the TUI event loop and draw path (completion, pickers, selectors,
session selector), hrdr-app completion/history/status, hrdr-editor host. Not
measured with a profiler — the frequency claims rest on traced callers (the 120
ms repaint ticker, the per-round loop, per-keystroke handlers), and the multi-MB
history sizes on the session-save comments and the compaction token counts seen
in transcripts. Not reviewed: `apps/hrdr/src/main.rs` (headless runner; read
only where it consumed the History event), `crates/hrdr-test-support`, test
harness files, and the markdown/theme rendering crates (hjkl-\*) — the latter
are third-party dependencies.
