# Code review — 2026-08-04

Scope: full codebase review (working tree clean on `main`), low depth —
high-confidence findings only. Six findings survived end-to-end verification;
each was traced from a concrete input to a wrong observable result through every
guard in between.

## Findings (most severe first)

### 1. Write sandbox escaped via a dangling symlink

`crates/hrdr-tools/src/tools/mutation.rs:168`,
`crates/hrdr-tools/src/lib.rs:686` (`canonicalize_nearest`),
`crates/hrdr-tools/src/lib.rs:337` (`resolve_write`),
`crates/hrdr-tools/src/sandbox.rs:273` (`check_write`)

`canonicalize_nearest` cannot resolve a symlink whose target does not exist, so
the write guard checks the **lexical** path inside the writable root, and
`atomic_write`'s symlink branch then follows the link and creates the file at
the target — outside the writable roots. The OS backends (Landlock/Seatbelt)
confine only shell children, never the in-process file tools, so this defeats
the only guard on `write` in the default `SandboxMode::Write`.

```
Repro: SandboxMode::Write, writable root /home/u/proj; pre-existing
       symlink("/home/u/other/evil", "/home/u/proj/link") where
       /home/u/other exists and /home/u/other/evil does not.
       write(path="link", content="pwned")
Expect: refusal ("sandbox: refusing to write ... outside this agent's
        writable roots"), no file created
Actual: /home/u/other/evil is created with the content
```

Trace: `resolve_write` → `canonicalize_nearest("/home/u/proj/link")` —
`canonicalize()` fails on the dangling link, the loop peels `link`,
canonicalizes `/home/u/proj`, and re-joins the unresolved name → `check_write`
passes. `path_exists` (`try_exists`, a stat) reports `false` for the dangling
link, so the read-before-write gate is skipped. `atomic_write`'s
`symlink_metadata` (lstat) sees a symlink and calls `tokio::fs::write(path, …)`,
which follows the link and creates the target. An existing symlink pointing
outside the roots is refused correctly — the hole is exactly the dangling case,
which is the case `canonicalize_nearest` was built for but does not handle.

### 2. Open-lock id diverges for compressed sessions — two instances can own one session

`crates/hrdr-agent/src/session.rs:947-954` (with `:582-584`, `:658-661`, `:914`)

`Session::open_path` derives the open-lock id from `path.file_stem()`, which
leaves the `.json` on a `.json.zst` path (`session_id_from_path` exists
precisely because `file_stem` alone is wrong for the compressed form). The
compressed open therefore locks under `.{foo-json}.open.lock` while every other
actor — the plaintext open, `save_session`, the retention sweep — keys on `foo`.
After the resumed session autosaves to plaintext, the two names can never meet
and the lock stops protecting anything.

```
Repro:
1. Session "foo" idle past compress_after → sweep_sessions compresses
   foo.json → foo.json.zst (plaintext removed).
2. Instance A auto-resumes: open_latest_session_for_cwd
   (crates/hrdr-app/src/sessions.rs:60) passes the listing path
   foo.json.zst into Session::open_path → lock id = file_stem = "foo.json"
   → sanitize_name → holds .{foo-json}.open.lock. state.id = "foo".
3. A runs a turn; save_session writes foo.json, deletes foo.json.zst.
   A still holds .{foo-json}.open.lock.
4. Instance B starts (same cwd): listing shows foo.json → open_path →
   lock id "foo" → .{foo}.open.lock is free → B acquires → succeeds.
Expect: OpenError::Busy (session owned by A)
Actual: B resumes the same session; both autosave foo.json; the later
        write silently replaces the other instance's turns
```

### 3. `max_tokens`/`max_completion_tokens` routing keyed on the configured model, not the wire model

`crates/hrdr-llm/src/client.rs:988-989` (with `:1191-1192`, `:522-533`)

`request()` routes the output-cap field with
`uses_max_completion_tokens(&self.model)`, but the serialized body carries the
**resolved** model from `wire_model()` (`self.model` is never written back). On
the sentinel path the two disagree and a reasoning model gets the wrong field
name.

```
Repro: base_url = single-model vLLM serving o3-mini (the exact case
       wire_model was written for); model = "default" (sentinel);
       params.max_tokens = Some(4096)
Expect: body carries "model": "o3-mini" with "max_completion_tokens": 4096
        (the same request with model = "o3-mini" configured directly routes
        to max_completion_tokens — see the test at client.rs:1904)
Actual: body carries "max_tokens": 4096 — reasoning endpoints that reject
        that field 400, and the agent's recovery then drops the user's cap
        for the whole session
```

Only the sentinel path is affected; with a named model the two agree. The
existing test covers only the named path.

### 4. Shell overflow spool is missing the line that crosses the byte cap

`crates/hrdr-tools/src/tools/shell.rs:586-637` (with `:513-515`, `:845-851`)

When the byte total lands **exactly** on `max_output` at a line boundary, the
line that trips the cap is routed to the in-memory tail ring (head is already
full at exactly `head_budget == max_output`), and the spool file — seeded from
`head` alone — never receives it. The result hint claims the file holds the
"full output" with accurate line/byte totals.

```
Repro: max_output = 100, command prints 52 lines of "x" (2 bytes each with
       newline), e.g. `yes x | head -52`
Expect: spool file contains all 52 lines; hint says "(52 lines, 104 bytes)"
Actual: spool file contains 51 lines (line 51 is missing) — a grep of the
        file the model is told is the full output cannot find that line,
        and the printed totals are off by one line
```

### 5. Rejected second `!command` leaves a phantom tool block in the transcript

`crates/hrdr-tui/src/app.rs:1331-1345`

`user_shell_command` records `ToolStart` (which pushes the block into the live
transcript and the durable jsonl) **before** the "a !command is already running"
guard. The rejected command's block has no owner and never receives a `ToolEnd`
— it spins in the live session and persists as a settled-failed block on resume.

```
Repro: run `!sleep 5`, then within the 5s submit `!echo hi`
Expect: the second command is refused with no transcript artifact
Actual: a second "shell" block appears that never finishes (done: false),
        is written to the session jsonl, and reappears on resume
```

### 6. `extract_agent_mention` splices the wrong occurrence of the matched token

`crates/hrdr-app/src/util.rs:38-58` (with `:192-196`)

The splice deletes `input.find(raw)` — the **first** occurrence of the matched
token's byte string — rather than the token the loop actually matched. When the
name appears earlier as a substring of a non-token, the wrong text is deleted,
the real mention survives as literal text in the body, and a required separator
space is dropped.

```
Repro: sub-agent named "explore"; input "contact foo@explore then
       @explore run audit" sent to the model
Expect: the actual @explore mention token is removed, text preserved
Actual: "foo@explore" is gutted to "foo", the real mention stays in the
        body, and the words jam ("contact foothen @explore run audit") —
        the corrupted string is wrapped in the delegation directive and
        sent to the model
```

## Cleared

- **SSE parsing** (hrdr-llm/src/sse.rs): chunk splits mid-codepoint and
  mid-`\r\n`, CRLF/LF, multi-line folding, unterminated trailing events, and
  both 32 MiB caps all behave correctly; overflow is latched on every path.
- **Retry budget arithmetic** (hrdr-llm/src/retry.rs): 10 attempts = 9 retries,
  backoff doubling/cap/jitter exact; `Retry-After` parsed and clamped on both
  paths.
- **Usage accounting** (hrdr-llm): `input + cache_read + cache_write` matches
  Anthropic's actual `input_tokens` semantics (verified against provider docs).
- **Compaction arithmetic** (hrdr-agent/src/compaction.rs): tail-window
  boundaries, newest-turn-always-kept guards, and the `shrank()` no-op exit all
  correct.
- **OAuth single-flight refresh** (hrdr-agent/src/oauth.rs): coordination atomic
  under the lock; refresh-failure cascades bounded per caller.
- **`..`/symlink escapes for _existing_ targets** (hrdr-tools): refused; the
  canonicalize-nearest path handles the symlink-then-`..` shape.
- **Secret-diff redactor** (hrdr-tools/src/tools/secret_diff.rs): drops both
  `-`/`+` sides and every repeated line.
- **LSP/MCP id correlation** (hrdr-tools/src/lsp.rs, mcp/): framing caps and
  request/response matching correct.
- **Timeout group-kill** (hrdr-tools/src/tools/shell.rs): descendants reaped.
- **UI layout/scroll math** (hrdr-tui/src/ui.rs) and editor wrapping: no
  off-by-one found; heavily tested.

## Hardening

- `crates/hrdr-tools/src/lib.rs:542` — `url_host` is duplicated in hrdr-agent's
  resolve helpers; the two copies can drift (comment already says so, both in
  sync today).
- `crates/hrdr-tools/src/lib.rs:349-370` — `mark_read`/`mark_read_partial` fall
  back to the uncanonicalized path when canonicalization fails (a deleted file);
  keys could collide for distinct paths in that window.

## Coverage

Scope: full codebase, working tree clean on `main` (no pending diff, no feature
branch). Reviewed in full: hrdr-llm (10 files), hrdr-agent (31 files),
hrdr-tools (33 files incl. mcp/), hrdr-tui (14 files), hrdr-app (21 files),
hrdr-editor (3 files). Not reviewed: `apps/hrdr/src/main.rs` and the
`apps/hrdr/tests/*` + `crates/*/tests/*` harness files (test-only; the shared
logic they exercise lives in the crates above). `crates/hrdr-test-support` not
reviewed (dev-only sandbox harness). `CHANGELOG.md` / `README.md` / `docs/*` not
reviewed (docs, not code).
