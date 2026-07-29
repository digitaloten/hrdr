# Code Review — 2026-07-29

Scope: entire codebase (working tree clean on `main`).

Reviewed: all crates via 5 review sub-agents + manual review of remaining
modules. Sub-agents covered `hrdr-tools`, `hrdr-agent`, `hrdr-editor`,
`hrdr-llm`, `hrdr-app` (3 failed with 429; those crates were re-reviewed
manually). Manual review covered `hrdr-web`, `hrdr-protocol`,
`hrdr-test-support`, `apps/hrdr`, `hrdr-tui`, and all tool implementations.

## Findings

### MEDIUM

**1. `session_cost` in `RestoredContext` is dead code — revived sub-agent cost
seeding is a no-op** `crates/hrdr-agent/src/delegation.rs:220-224`

```rust
if let Some(r) = restore {
    sub.set_messages(r.messages);
    sub.set_session_cost(r.session_cost); // (1) writes to local Arc<Mutex<f64>>
}
sub.cost_total = cost_total;             // (2) replaces local Arc with shared one
```

`set_session_cost` writes `r.session_cost` into `sub.cost_total` (the agent's
own `Arc<Mutex<f64>>`). Immediately after, line 224 replaces `sub.cost_total`
with the shared `cost_total` passed in — the value from step (1) is dropped. The
field `RestoredContext::session_cost` and all the plumbing to carry it serves no
purpose. Cost tracking works correctly only because all agents share the same
`cost_total` and add to it.

```
Repro: Session spends $2.50. task_revive a sub-agent.
Expect: revived agent's cost tracking reflects $2.50 baseline.
Actual: set_session_cost writes to a counter immediately replaced; the shared
        counter already held the right total. Works by accident.
```

Trace:

- `Agent::new(cfg)` at line 213: `sub` gets `cost_total: Arc<Mutex<f64>>` (0.0)
- Line 222: `sub.set_session_cost(r.session_cost)` writes to that local counter
- Line 224: `sub.cost_total = cost_total` replaces the entire field with the
  shared parent Arc — the value from step 2 is discarded
- Set-up before line 222 is the fix: move `sub.cost_total = cost_total` first,
  then `set_session_cost` on the shared Arc to ADD (not SET) the previous spend

### LOW

**2. `budget_preflight` allows unbounded overshoot of the cost cap on any single
call** `crates/hrdr-agent/src/budget.rs:75-82`

```rust
let spent = *self.cost_total.lock().unwrap_or_else(|p| p.into_inner());
if spent >= cap {
    bail!("cost budget exhausted: est. ${spent:.2} ≥ cap ${cap:.2}");
}
```

The preflight checks `spent >= cap` BEFORE issuing a call but does not bound how
much any single call may add. A call more expensive than the remaining headroom
passes and pushes the total far past the cap. Only the NEXT call is blocked.

```
Repro: max_cost = $5.00. Spend $4.99 on small calls.
       Next call: 200k prompt + 10k completion on an expensive model → $8.00.
Expect: call is refused or cost-limited.
Actual: preflight passes (4.99 < 5.00), call proceeds, total becomes $12.99.
        Cap blocks the *subsequent* call.
```

Trace:

- Line 79: `spent = 4.99`, `cap = 5.00` → `spent >= cap` is false → pass
- `account_usage` (line 133): `*total += 8.00` → `*total = 12.99`
- Next round: `12.99 >= 5.00` → true → bail

**3. Task cancellation races the background worker's post-`catch_unwind`
registry writes** `crates/hrdr-agent/src/delegation.rs:474-508`

```rust
let result = match result {
    Ok(s) => s,
    Err(panic_err) => { /* … */ }
};
if let Ok(mut v) = reg_done.lock()
    && let Some(t) = v.iter_mut().find(|t| t.id == id)
{
    t.done = true;
    t.result = Some(final_result);
}
```

Between `catch_unwind` finishing (line 474) and the registry writes (lines
495-508), `task_cancel` can call `h.abort()` and mark the task cancelled. The
sub-agent's successful result is discarded.

```
Repro: Sub-agent finishes successfully just as the user issues task_cancel.
       Timing falls between catch_unwind returning Ok and reg_done.lock().
Expect: successful result lands in registry OR task_cancel bounces (task not running).
Actual: task_cancel marks the task cancelled, the successful result is lost.
        Sub-agent's edits are already in the tree; only the registry entry is wrong.
```

**4. `can_delegate` requires both `task` AND `models` tools — agent with `task`
alone gets no delegation guidance** `crates/hrdr-agent/src/prompt.rs:161`

```rust
let can_delegate = has("task") && has("models");
```

A custom agent profile that includes `task` but excludes `models` can spawn
sub-agents but receives no prompt-level delegation guidance. The `task` tool
schema still documents the `model` parameter, and the Environment block names
concurrency caps, but the `delegate.md` section (how to pick models, size
batches, partition work) is omitted.

```
Repro: [[subagent]] profile with tools = ["task", "read", "write", "shell", …],
       omitting "models". Agent calls `task`. No delegation guidance in prompt.
Expect: delegation guidance is present when `task` tool is registered.
Actual: guidance gated on `models` too; agent delegates without guidance.
```

### VERY LOW

**5. Stale `VisualLine::width` on intermediate hard-wrapped lines**
`crates/hrdr-editor/src/lib.rs:240-258`

When a word is hard-broken across visual lines (word wider than the terminal
column), `lines[k].width` is updated only for the final line at line 258.
Intermediate lines' `width` stays 0. Latent: no current code reads
`lines[i].width` for non-final lines — only `lines[lines.len()-1]` is read. No
visible misbehavior.

```
Repro: compute_wrapped_layout("abcde", 3)
       lines[0].chars = ['a','b','c'] (correct), width = 0 (should be 3)
Actual: positions array and cursor placement are correct; width field is never read.
```

**6. Standalone `\r` silently dropped in `PlainEngine::paste`**
`crates/hrdr-editor/src/plain.rs:207`

```rust
fn paste(&mut self, text: &str) {
    for c in text.chars() {
        if c != '\r' { self.insert(c); }
    }
}
```

Dropping `\r` unconditionally handles `\r\n` → `\n` (desirable) but also loses
standalone `\r` characters (legacy Mac line endings). The `VimEngine` override
does the same.

```
Repro: paste("a\rb") — standalone \r between chars
Expect: "a\rb" or "a\nb"
Actual: "ab" (standalone \r silently removed)
```

**7. Tab characters have zero display width in wrapping layout**
`crates/hrdr-editor/src/lib.rs:122-124`, `char_width` returning 0 for `\t`

`unicode_width::UnicodeWidthChar` returns `None` for `\t`, so
`char_width('\t') = 0`. The wrapping layout counts 0 display columns per tab;
the terminal expands tabs to tab-stop-aligned spaces. Cursor placement is wrong
on lines containing tabs.

```
Repro: set_content("\txyz"), width = 10
       Layout: tab=0 cols, total=3 cols used
       Terminal: tab=8 cols, total=11 cols used
       Cursor position from layout disagrees with terminal rendering.
```

## Cleared

- **Rate-limiter HashMap unbounded growth** (`web/auth.rs:134,137`):
  `check_rate_limit` now removes empty entries when `count == 0` (lines
  137-138). `rate_limit_record` removes its own empty entries (line 152-153) and
  runs a periodic full-map sweep every 64th call (lines 156-161). ✓
- **`std::sync::Mutex` held across argon2** (`web/server.rs:235-241`):
  `login_handler` acquires the `users_db` lock, fetches the stored hash via
  `get_password_hash`, then `drop(db)` (line 241) BEFORE running argon2 at
  line 244. The lock is scoped to the DB query only. ✓
- **Logout CSRF (no authentication check)** (`web/server.rs:273-283`):
  `logout_handler` now validates the session cookie before clearing it. Mode
  `Users` requires a valid `hrdr_session` cookie (lines 277-283); other auth
  modes have no cookie to clear, so logout is harmless. ✓
- **SSE bare `\r` merging lines** (`llm/sse.rs:119`): decoder splits only on
  `\n` and strips trailing `\r`. A bare `\r` would merge two lines but is
  unreachable — every known HTTP SSE transport emits `\n` or `\r\n`. ✓
- **SSE buffer DoS** (`llm/sse.rs:19,117-123`): 32 MiB cap on `line_buf` and
  folded `cur_data`, with `overflowed` flag that persists across push/finish
  calls. ✓
- **Guardrail bypass via quoting a flag** (`tools/guardrails.rs:446-453`):
  `tokenized_for_match` uses `shell_words::split` to remove quotes before
  matching, so `git push "--force"` is caught. Fallback
  `strip_unbalanced_quotes` handles malformed input. ✓
- **Guardrail bypass via nested `sh -c`** (`tools/guardrails.rs:418-425`):
  `extract_shell_c_args` recursively extracts `-c` payloads and re-scans them,
  bounded by cumulative payload size (64 KiB). ✓
- **`task_*` tools shelled out as programs** (`tools/guardrails.rs:302-307`):
  `shells_out_to_task_tool` splits on shell operators with quote-awareness,
  extracts the program word past `sudo`/`env`/etc, and matches exactly against
  `TASK_TOOLS`. A quoted mention (`grep task_output`) does not match. ✓
- **SSE chunk-split across UTF-8 codepoint** (`llm/sse.rs:118-132`): the decoder
  buffers raw bytes per-line and splits only on `\n`. Since `0x0A` never appears
  inside a multi-byte UTF-8 sequence, every `line_buf` is a complete codepoint
  sequence. Tested at `sse.rs:366-380`. ✓
- **Edit tool OOM on `replace_all`** (`tools/edit.rs:248-262`): computes
  projected output size before calling `String::replace`. Bails when projection
  exceeds 64 MiB. ✓
- **Edit tool secret-file refusal** (`tools/edit.rs:138-143`): checks
  `secret_file_reason` on canonical path before editing. ✓
- **Edit tool CRLF recovery** (`tools/edit.rs:179-199`): when `read` strips
  `\r`, the model copies `\n`-separated `old_string`. Retries match against a
  CRLF-translated form before giving up. ✓
- **Sandbox symlink/`..` escapes** (`tools/sandbox.rs:1557-1576`): `check_write`
  resolves canonical paths via `canonicalize_nearest`, so `dir/../../etc/passwd`
  and a symlink to `/etc` are both caught. ✓
- **Sandbox `.git` metadata write refusal** (`tools/sandbox.rs:374-387`):
  `protected_metadata_dir` checks for `.git` in any path component, never just
  root-relative. ✓
- **MCP `PendingGuard` cleanup on cancel** (`tools/mcp/transport.rs:37-44`):
  removes `id` from pending map on drop unless disarmed, covering future
  cancellation. ✓
- **Compaction tail-start alignment** (`agent/compaction.rs:521-547`):
  `compaction_tail_start` returns a `role:"user"` boundary;
  `mega_turn_tail_start` aligns past leading `role:"tool"` messages.
  Tool-call/result pairs stay intact. ✓
- **Compaction overflow recovery** (`agent/compaction.rs:562-601`): retries with
  shrinking stages (elide tool results, tail window at 1/2, 1/4, 1/8) on context
  overflow; backoff on transient errors. ✓
- **LSP rename rollback on cancellation** (`tools/lsp_nav.rs:54-100`):
  `RenameRollback` guard restores files on drop (cancelled future); disarm on
  success. ✓
- **Provider-name alias folding** (`agent/model_ref.rs:70-83`):
  `opencode`→`zen`, `chatgpt`→`openai`, `anthropic`→`claude`, etc. All
  canonicalized on construction; `://` separator makes parse context-free. ✓
- **`--base-url` flag absence** (`apps/hrdr/src/main.rs:1270-1278`): no
  endpoint-override flag exists — endpoint comes from the provider config only.
  Enforced by clap refusing unknown arguments and a unit test asserting it. ✓
- **Plain engine cursor bounds** (`editor/plain.rs:51,55,76-113`): all cursor
  ops use `saturating_sub`, `min()`, and early-return guards. No panics from OOB
  cursor. ✓

## Hardening

- **`SandboxNotices` poisoned mutex drops notices**
  (`tools/sandbox.rs:558,568`): `.lock().ok()` silently discards the mutex-guard
  on poison. A poisoned sandbox-notice mutex costs a degradation warning rather
  than a panic — which is deliberate (see comment at line 548-549), but means a
  poisoned mutex silently silences all future sandbox degradation notices for
  that agent.
- **Rate limiter check→record TOCTOU** (`web/auth.rs:129-133,143-147`):
  concurrent requests can both observe `len() == 9` and both pass, overshooting
  the 10-per-minute cap by 2–3×.
- **Ephemeral cookie secret** (`web/auth.rs:44-45`): `AuthState::from_config`
  generates a fresh `cookie_secret` on every startup, invalidating all session
  cookies.
- **WebSocket connections have no idle timeout** (`web/server.rs:351+`):
  `handle_socket` has frame/message size limits but no heartbeat or idle
  timeout.
- **`SubagentSlots` zero-cap silent** (`agent/delegation.rs:32-46`): when
  `max = 0`, all sub-agent spawns fail with "too many sub-agents". Blocked by
  config validation but reachable via programmatic construction.
- **Tab display width** (see finding #7): `char_width` returns 0 for `\t`, so
  layout and terminal rendering disagree on cursor placement. Expand tabs to
  spaces on ingest or account for tab-stop width.

## Coverage

Reviewed every crate. Five review sub-agents covered `hrdr-tools`, `hrdr-agent`,
`hrdr-editor`, `hrdr-llm`, `hrdr-app` (3 were rate-limited; those crates
re-reviewed manually). Manual review covered `hrdr-web` (server, auth, users,
session, config, convert), `hrdr-protocol`, `apps/hrdr` (main.rs + CLI tests),
`hrdr-test-support`, `hrdr-tui`.

In detail: sandbox (full 2817-line module, bwrap/Landlock/Seatbelt backends, GPU
passthrough, git metadata roots, denial notes, scratch dir), guardrails (full
903-line module, all default rules + task-tool blocking + nested shell-c
re-scanning + quote bypass prevention), SSE decoder (561 lines, chunk-split
safety, overflow protection, UTF-8 handling, finish()), edit tool (542 lines,
CRLF recovery, stale-read unique-match, OOM guard, whitespace near-match), shell
tool (1500 lines, output capping, process groups, sandbox integration), LSP nav
(698 lines, transactional rename with cancellation rollback), web auth (623
lines, token/basic/session cookie, rate limiter, WS origin check, IP
extraction), web users (134 lines, user enumeration prevention with dummy hash),
model resolution (resolve.rs 735 lines, OAuth-derived endpoint switch, key
inheritance), compaction (708 lines, compaction trigger, prune gating,
self-compact, overflow recovery stages), turn loop (1572 lines, repeat guard,
error classifiers, retry backoff, drain stream), budget (144 lines, preflight
check, cost tracking), session (2920 lines, persistence, locking, session state
migration), delegation (3330 lines — sampled: cost seeding at 220-224, cancel
race at 474-508, background spawn at 330-528), model_ref (785 lines), prompt
(3163 lines, capability sections).

Not exhaustively line-read: `delegation.rs` beyond lines ~550,
`hrdr-app/src/completion.rs`, `hrdr-app/src/effort.rs`,
`hrdr-agent/src/oauth.rs`, `hrdr-agent/src/chatgpt_models.rs`, `hrdr-tui`
rendering paths — these were spot-checked at key entry points and feed into the
logic inspected above.
