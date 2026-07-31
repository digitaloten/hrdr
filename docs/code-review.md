# Code Review — 2026-07-31

Full-codebase correctness review (clean working tree on `main`).

## Findings

### 1. HIGH — SseDecoder::finish() returns Ok with truncated events after internal overflow

**`crates/hrdr-llm/src/sse.rs:223-241`**

`finish()` checks `self.overflowed` at line 224 before its own work, but not
after. `flush_line()` at line 229 can set `overflowed = true` (lines 172 or 187)
— and `finish()` never rechecks. The truncated event is pushed to `ready` at
line 234 and returned as `Ok(events)`.

All three backends (`client.rs:939`, `anthropic.rs:422`, `codex.rs:268`) match
on `finish()::Ok` and trust it to mean "events are intact." A truncated event
passes into normal JSON parsing and fails with a misleading parse error instead
of the clean `SseOverflow`.

```
Repro: Push a data: line that fills cur_data to MAX_BUFFER_BYTES - 15 bytes,
       then push a second data: line WITH NO NEWLINE that overflows. Call finish().
Expect: Err(SseOverflow)
Actual: Ok([SseEvent { data: <truncated to 14 bytes> }])
```

Fix: after `flush_line()` at line 229, recheck `self.overflowed` and return
`Err(SseOverflow)` if set, discarding `ready`.

### 2. MEDIUM — Codex "error" event drops outer transient code when nested error object takes precedence

**`crates/hrdr-llm/src/codex.rs:478`**

```rust
let err_obj = ev.get("error").filter(|e| e.is_object()).unwrap_or(ev);
let code = err_obj.get("code").and_then(Value::as_str);
```

When `ev` has both a top-level `"code"` (e.g. `"server_error"`) and a nested
`"error"` object without its own `"code"`, `err_obj` becomes the nested object,
`code` becomes `None`, and `classify_codex_error(None)` → `Other` (terminal) at
line 541. A transient error is misclassified as terminal, killing the turn.

No known server emits this hybrid shape today; fragility against future API
changes or proxy wrapping.

```
Repro: {"type":"error","code":"server_error","error":{"message":"try later"}}
Expect: ChatErrorKind::Transient
Actual: ChatErrorKind::Other
```

Fix: also check outer `ev` for `code` when inner object lacks one.

### 3. LOW — Dangling tool entries survive a live panic in the transcript

**`crates/hrdr-agent/src/transcript.rs:636-641` and `registry.rs:779-798`**

`ToolStart` creates an entry with `ok: true, done: false` (line 636-637). If the
tool panics, the `catch_unwind` at `registry.rs:783` catches the panic and emits
`Notice` + `TurnDone` — but never emits `ToolEnd`. The transcript permanently
shows the tool as running (spinner). `settle_restored_tools` (line 196) corrects
this only during session _restore_.

Cosmetic only: no data corruption, no state inconsistency beyond display.

```
Repro: Agent calls a tool that panics mid-execution
Expect: Transcript shows tool as failed (ok=false, done=true)
Actual: Transcript shows tool as ok=true, done=false indefinitely
```

Fix: the panic handler at `registry.rs:794-798` could walk the event log and
emit `ToolEnd { ok: false }` for any open tool call before emitting `TurnDone`.

## Cleared

### hrdr-llm

- **retry.rs backoff**: jitter uses 1,000 evenly-spaced slots, atomic counter —
  no bias, no lockstep.
- **client.rs auth header filtering** (`apply_extra_headers`:574-601):
  `is_auth_header_name` case-insensitive, `apply_extra_headers` runs BEFORE
  `auth()` adds the real key — a forged `Authorization` in `extra_headers` never
  reaches the wire.
- **client.rs wire_log_over_cap**:
  `current >= cap || line_len > cap.saturating_sub(current)` — saturating sub
  prevents underflow.
- **capped_read.rs exactly-at-cap**: one more chunk is peeked (lines 79-83) to
  distinguish exactly-at-cap from oversized.
- **anthropic.rs thinking_budget**: small `max_tokens` (< 2048) correctly
  returns `None`.
- **codex.rs response.incomplete unrecognized reason**: falls through to
  `_ if is_incomplete => "length"` at line 571.
- **catalog.rs write_cache**: `unique_sibling_path` + atomic rename — no TOCTOU.

### hrdr-tools

- **sandbox.rs `canonicalize_nearest` + `normalize_path`**
  (`lib.rs:686-705, 849-882`): resolves symlinks and lexical `..` — a
  `cwd/nonexistent/../../etc/passwd` path is caught by the component-aware
  `starts_with` check.
- **sandbox.rs Landlock `install_landlock_rules`** (line 1125): `BestEffort`
  compatibility, enforced via `RulesetStatus::NotEnforced` check — a kernel that
  enforces nothing fails the spawn.
- **shell.rs `read_line_capped`** (line 433): buffers never exceed `cap`;
  overflowing lines are drained through their newline so the next line arrives
  intact.
- **shell.rs `run_streamed_command`** overflow file (line 601-635): opened
  lazily on first overflow, seeded with complete head — no bytes lost.
- **guardrails.rs `tokenized_for_match`** (line 454): word-splits via
  `shell_words::split`, sentinel-replaces internal whitespace —
  `rg 'git add -A'` does not false-positive, `git push "--force"` is caught.
- **guardrails.rs nested `sh -c` extraction** (line 209-253): recursive re-scan
  bounded by cumulative payload bytes (64k), not depth.
- **guardrails.rs `shells_out_to_task_tool`** (line 311): splits on unquoted
  control operators only — `grep 'x&' task_output` does not false-positive.
- **proc.rs `unix_group_kill`** (line 218): guards `pid > 1` — `-0` would signal
  the caller's group, `-1` every process on the system.
- **proc.rs `ProcessGroup::Drop`** (line 197): the dropped-future path
  (Esc/cancel) also kills the group, not just the leader `kill_on_drop` reaps.
- **write.rs read-state guard** (line 68): blocks overwrite of
  unread/partially-read/stale files; `full: true` mode covers oversized-line
  files.
- **edit.rs `is_crlf_dominant`** (line 28): `crlf > 0 && crlf >= lf_only` —
  correctly detects CRLF files so `edit` can match model's LF-only `old_string`.

### hrdr-agent

- **resolve_api_key cross-provider key leak** (`config.rs:1257`):
  `same_endpoint` compares `trim_end_matches('/')` on both URLs.
- **try_reserve / acquire_open_lock** (`session.rs:502,604`): `O_EXCL` with
  stale-reap-and-retry — no infinite loop.
- **SubagentSlots overflow** (`delegation.rs:34`): `fetch_update` with CAS
  prevents load-then-store race.
- **TranscriptLog torn-line** (`transcript_log.rs:423`): partial writes rolled
  back via `set_len`; `torn` flag adds `\n` prefix on next write.
- **coordinated_access_core missed wakeup** (`oauth.rs:759`):
  `fut.as_mut().enable()` called while holding lock, before releasing.
- **RunGuard aborted-turn race** (`registry.rs:878`): generation check prevents
  aborted predecessor's guard from ending successor's turn.
- **plan_prune victim indices** (`compaction.rs:348`): modifies message content
  in-place, not Vec structure — indices remain valid.
- **resolve_subagent_cwd symlink escape** (`delegation.rs:968`):
  `canonicalize_nearest` resolves symlinks before `starts_with` containment
  check.

## Hardening

- **oauth.rs:776** — `coordinated_access_core` double-loads credentials (lines
  737, 776). If a concurrent `/login` replaces the store with an expired-access,
  no-refresh credential between loads, `refresh("")` is called with an empty
  token (line 784). The API error propagates cleanly, the guard clears the gate,
  and waiters retry — correct outcome, one wasted API call. A
  `cur.refresh.is_empty()` check after line 776 would skip the pointless call.
- **session.rs:1289** — `sweep_dir` purges a `.json` or `.zst` file but doesn't
  clean up its counterpart when the companion survived a prior
  `compress_session_file` crash. Eventually consistent (next load+save cleans it
  up).
- **session.rs:715** — `sanitize_name` truncates at 48 chars via `.take(48)`.
  Two long names sharing a 48-char prefix produce the same slug;
  `unique_session_id` suffix handles the collision.

## Coverage

- **hrdr-llm**: fully reviewed (sse, client, retry, anthropic, codex,
  capped_read, catalog, types, fs).
- **hrdr-tools**: critically reviewed (sandbox, shell, guardrails, proc, write,
  edit, read, lib.rs path resolution). Not deeply reviewed: replace.rs, find.rs,
  grep.rs, ls.rs, secret_diff.rs, mutation.rs, todo.rs, tree.rs, verify.rs,
  mcp/, memory.rs, hooks.rs, verification.rs, ansi.rs, test_nudge.rs, gate.rs,
  web.rs.
- **hrdr-agent**: deeply reviewed (session, turn_loop, turn, turn_state,
  delegation, compaction, budget, config partial, auth, auth_store, oauth,
  transcript, transcript_log, registry, resolve, provider_catalog, validate,
  store_lock, pane, hooks, skills, models, model_ref). Partially: prompt.rs,
  config.rs layering.
- **hrdr-app, hrdr-web, hrdr-tui, hrdr-editor, hrdr-protocol**: GAP — not
  reviewed. Sub-agent output was truncated and could not be verified. These
  crates are not covered by this report.
