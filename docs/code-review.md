# Code Review — 2026-08-01

Full-codebase review on a clean `main` (`7dd00c5`). This pass deliberately
targets the coverage GAP the 2026-07-31 review declared — `hrdr-web`,
`hrdr-app`, `hrdr-tui`, `hrdr-editor`, `hrdr-protocol`, and the half of
`hrdr-tools` that was never opened (`gate.rs`, `hooks.rs`, `web.rs`,
`replace.rs`, `mcp/client.rs`) — and re-verifies the findings the previous
report left open.

## Findings

### 1. HIGH — Basic auth mode is unusable from a browser: no `WWW-Authenticate` challenge

**`crates/hrdr-web/src/server.rs:332-335`**

```rust
if !authed {
    auth::rate_limit_record(&state.auth, client_ip);
    return Err((StatusCode::UNAUTHORIZED, "unauthorized").into_response());
}
```

A 401 with no `WWW-Authenticate` header is not an auth challenge — per RFC 9110
§11.6.1 the header is mandatory on a 401, and browsers use it to decide whether
to show the credential prompt. Without it, a browser pointed at an
`--auth basic` server renders the bare `unauthorized` body and never offers a
way to supply credentials. There is no `WWW-Authenticate` anywhere in the tree
(`grep -rn "WWW-Authenticate" crates apps` → no matches).

This matters most in exactly the deployment Basic mode exists for: `serve()`
(`server.rs:68-76`) _forces_ Basic or Users for any non-loopback bind, so the
remote-access story runs through the one mode a browser cannot start.

```
Repro: hrdr web --auth basic (with basic_user/basic_password_hash set), open / in a browser
Expect: the browser's Basic-auth credential prompt
Actual: a plain "unauthorized" page, no prompt, no way in
```

Fix: on the `AuthMode::Basic` path, return the 401 with
`WWW-Authenticate: Basic realm="hrdr", charset="UTF-8"`.

### 2. HIGH — `users` auth mode has no browser entry point at all

**`crates/hrdr-web/src/server.rs:96-102, 170-183`**

Four routes exist: `/healthz`, `/` (auth-gated), `/ws` (auth-gated),
`POST /login`, `POST /logout`. In `AuthMode::Users`, `check_auth`
(`server.rs:327-329`) authenticates only via the `hrdr_session` cookie — which
is minted _by_ `POST /login`. A browser that has never logged in therefore gets
401 from `/`, and there is no unauthenticated route that serves a login form.

Nothing else closes the loop:

- `spa_index_html()` (`lib.rs:26-35`) is behind the `ui` feature, whose default
  is `[]` (`hrdr-web/Cargo.toml:10-12`) and which `apps/hrdr` does not enable —
  so `/` would serve `INDEX_HTML`, a static "connect a websocket client" page,
  even if it were reachable.
- `crates/hrdr-ui/src/` (`lib.rs`, `main.rs`, `state.rs`) contains no login
  code: `grep -rln login crates/hrdr-ui/src/` → no matches.

So `--auth users`, the mode `serve()` recommends for remote access, is reachable
only by a client that already knows to `POST /login` and carry the cookie by
hand. The mode is wired end-to-end on the server (SQLite store, argon2, cookie
HMAC, CSRF-safe logout, `--add-user`/`--remove-user` CLI) with no client half.

```
Repro: hrdr web --auth users --add-user alice; open / in a browser
Expect: a login form
Actual: 401 "unauthorized"; the only way in is a hand-rolled POST /login
```

Fix: serve a login page unauthenticated in `users` mode (either an `/login` GET
route or letting `/` fall through to a login form when the cookie is absent).
Until then, the mode is a documentation hazard — it looks supported.

### 3. MEDIUM — Hook timeout message reports seconds as milliseconds

**`crates/hrdr-tools/src/hooks.rs:144-147` and `335-337`**

```rust
Err(_) => notes.push(format!(
    "[hook `{}` timed out after {}ms; killed]",
    hook.run, hook.timeout_secs
)),
```

The field is `timeout_secs`, the timer is
`Duration::from_secs(hook.timeout_secs)` (lines 106 and 269). A hook on the
default 30-second timeout reports `timed out after 30ms`, three orders of
magnitude off. Both hook families — file hooks and lifecycle hooks — carry the
identical defect; the unit rename (`timeout_ms` → `timeout_secs`) is called out
in the test at line 426 but the two format strings were never updated with it.

Wrong by a factor of 1000 in a diagnostic the model reads and acts on: a
formatter that legitimately needed 40s reads as one that died in 30ms, which
points at "the hook is broken" instead of "raise the timeout".

```
Repro: a file hook `sleep 5` with timeout_secs = 1
Expect: "timed out after 1s; killed"
Actual: "timed out after 1ms; killed"
```

Fix: `{}s` in both format strings (or `{}`+`s`), same edit twice.

### 4. MEDIUM — `SseDecoder::finish()` still returns `Ok` with truncated events

**`crates/hrdr-llm/src/sse.rs:223-241`** — carried from the 2026-07-31 review,
re-verified as **still open** on `7dd00c5`.

`finish()` checks `self.overflowed` at line 224 and never rechecks after the
`flush_line()` at line 229 — which can set it at line 172 (`remaining == 0`) or
line 187 (over-long `data:` value truncated at a char boundary). The truncated
event is pushed to `ready` and handed back as `Ok(events)`.

All three backends (`client.rs`, `anthropic.rs`, `codex.rs`) treat `Ok` from
`finish()` as "intact", so the truncated payload reaches JSON parsing and fails
with a misleading parse error instead of the clean `SseOverflow`.

Fix: recheck `self.overflowed` after the `flush_line()` and return
`Err(SseOverflow)`, discarding `ready`.

### 5. MEDIUM — The wire round-trip test proves nothing, and hides three phantom message types

**`crates/hrdr-protocol/src/lib.rs:326-379`**

The test's doc comment claims "Every JSON example from §4 of the web-ui-plan
must parse and re-serialize to the same value." What it actually does:

```rust
let v: serde_json::Value = serde_json::from_str(json)...;
let round = serde_json::to_string(&v).unwrap();
let v2: serde_json::Value = serde_json::from_str(&round).unwrap();
assert_eq!(v, v2, "{name}: round-trip mismatch");
```

`Value → String → Value` is the identity for any syntactically valid JSON. The
assertion cannot fail, and `ServerFrame` / `ClientMsg` — the types the test
exists to pin — are never named.

The proof that this is not academic is in the fixture list itself: lines 358-368
carry `approval_requested`, `approval_closed` and `answer_approval` examples,
and `ServerMsg` (lines 227-272) and `ClientMsg` (lines 290-316) have no such
variants. `grep -rn approval crates/hrdr-protocol crates/hrdr-web` finds them
only in those three string literals and one unrelated doc comment on
`session.rs:466`. Three wire messages the protocol claims to speak and cannot
parse, sitting green in CI.

Fix: deserialize each example into `ServerFrame`/`ClientMsg`, re-serialize, and
compare `Value`s — then either add the approval variants or delete the fixtures.

### 6. LOW — Web WS handler panics on a serialization failure, four times

**`crates/hrdr-web/src/server.rs:391, 404, 420, 431`**

```rust
let snap_json = serde_json::to_string(&snapshot).unwrap();
```

Four `unwrap()`s on `serde_json::to_string` inside the socket task. The wire
types are all serde-derived structs so a failure is not reachable _today_, but
the panic lands in a detached `tokio::spawn` where it kills the connection
silently — the failure mode is the worst-diagnosable one available. `tick()`
(`session.rs:362, 372`) already uses `unwrap_or_default()` for the same call;
the WS path should match.

### 7. LOW — `env_override` leaks a `String` per call, and its comments are unedited thinking

**`crates/hrdr-web/src/config.rs:187-203`**

```rust
Ok(v) => {
    // Return a leaked string — fine for startup-only config.
    // Actually just use the default env var String.
    let s = v;
    // We need to return a reference that lives long enough.
    // This is a startup-only config load; leak is acceptable.
    Box::leak(s.into_boxed_str())
}
```

Three comments, two of which contradict the third, and one
(`Actually just use the default env var String`) is a note-to-self that
describes a change that was never made. The lifetime gymnastics exist only
because the function returns `&'a str`; `env_override_opt` right below it
returns `String` and needs no leak. The two callers (`bind_str`, `auth_str`,
lines 102/104) both `.to_string()` the result at lines 115/117 anyway — so every
leak is immediately copied and discarded.

Fix: make `env_override` return `String` like its sibling, drop the leak and all
three comments.

### 8. LOW — DRY: `users::verify` duplicates the login handler and is dead

**`crates/hrdr-web/src/users.rs:70-78` vs
`crates/hrdr-web/src/server.rs:236-250`**

`users::verify` is exactly the login handler's logic — fetch the hash, fall back
to burning `DUMMY_HASH` when the user is absent, compare — and it is called from
nowhere but its own unit test (`grep -rn "users::verify" --include=*.rs` → no
production hits). The handler open-codes the same three steps because it needs
to drop the DB mutex before the argon2 work, which `verify` cannot express.

The risk is drift in the half that matters: a fix to the enumeration-oracle
behaviour applied to `verify` would look done and change nothing.

Fix: either give `verify` the shape the handler needs (take a hash rather than a
`&Connection`, so both paths share the comparison) or delete it and fold its
test into the handler's.

### 9. LOW — SSRF blocklist misses the shared-address space

**`crates/hrdr-tools/src/web.rs:611-616`**

`is_blocked_ipv4` covers loopback / RFC1918 / link-local / unspecified, but not
`100.64.0.0/10` (RFC 6598 carrier-grade NAT, `Ipv4Addr::is_shared`), which is
routinely used for internal service meshes and for the metadata/ingress ranges
of several hosting providers. Not a hole in the authoritative guard's _design_ —
`SsrfGuardResolver` filters whatever `is_blocked_ip` says is blocked, so
widening the predicate widens both layers at once — just a gap in its table.

Fix: add the range alongside the existing arms. Note that `Ipv4Addr::is_shared`
is **not** usable — it is still behind the unstable `ip` feature on 1.97, so it
has to be spelled out as an octet match.

### 10. LOW — `attr_value` matches an attribute suffix

**`crates/hrdr-tools/src/web.rs:743-755`**

`tag.find("href=")` matches inside `data-href=`, `xlink:href=` or any attribute
ending in the requested name, so the wrong value is extracted. Only reachable
through DDG result scraping, where a wrong href yields a wrong result URL rather
than anything unsafe.

Fix: require a preceding space (or `<`) before the key.

### 11. LOW — DDG snippet scan reads past the result block it bounded

**`crates/hrdr-tools/src/web.rs:453-465`**

`block_end` is computed to stop a snippet-less result stealing the next result's
snippet — but only the `find("result__snippet")` is bounded by it. The
subsequent `html[s..].find('>')` and `html[sgt..].find("</a>")` search to the
end of the document, so a `result__snippet` near `block_end` can still pull text
out of the following result.

### 12. LOW — More than 16 workflow files hides every single-file CI config

**`crates/hrdr-tools/src/gate.rs:329-356`**

`ci_files` extends `out` with the directory scan (`.github/workflows` and
friends) _first_, appends the `NAMED` single-file configs after, and only then
`out.truncate(MAX_CI_FILES)`. A monorepo with 16+ workflow files therefore drops
`.gitlab-ci.yml`, `Jenkinsfile`, etc. entirely — the truncation is documented as
"reading the first handful in sorted order gets the same answer", which holds
within one source but not across them.

Fix: truncate the directory scan before appending `NAMED`, so the named configs
are never the ones cut.

### 13. LOW — `#[allow(dead_code)]` on a live function

**`crates/hrdr-web/src/session.rs:644-648`**

`next_seq` is called from `build_snapshot`, `tick`, `submit_sync`, `cancel` and
`next_seq_internal`. The attribute is stale and suppresses a warning that would
now be correct if the function ever _did_ go unused.

### 14. LOW — Turn-task panics are swallowed by the web tick loop

**`crates/hrdr-web/src/session.rs:66-71`**

```rust
let handle = s.main_turn_handle.take().unwrap();
// join to avoid leaking
drop(handle);
```

The comment says "join"; `drop` on a finished `JoinHandle` detaches it without
observing its result, so a panic inside the turn task never surfaces anywhere —
the session simply goes idle. (The registry's own `catch_unwind` covers the tool
layer; this is the layer above it.)

Fix: the handle is already known finished — `handle.now_or_never()` / a
`try_join` and a `Notice` on `Err(JoinError)` costs nothing and turns a silent
stall into a message.

## Carried forward, still open

Re-verified against `7dd00c5`; both remain as the 2026-07-31 review described
them.

- **`crates/hrdr-llm/src/codex.rs:478`** — a hybrid error shape (top-level
  `code`, nested `error` object without one) misclassifies a transient error as
  terminal. No known server emits it; fragility, not a live bug.
- **`crates/hrdr-agent/src/transcript.rs:636` + `registry.rs:779-798`** — a tool
  that panics leaves its transcript entry at `ok: true, done: false` forever
  (the spinner never stops). `settle_restored_tools` only fixes this on session
  _restore_.

## Cleared this pass

### hrdr-web

- **`auth.rs` `check_ws_origin`** (lines 206-284) — loopback origins are matched
  on the exact authority including port, cross-spelling (`localhost` vs
  `127.0.0.1` vs `[::1]`) is refused, the opaque `null` origin is refused, and
  an absent `Host` fails closed. Fifteen tests pin the matrix.
- **`auth.rs` `extract_client_ip`** (line 181) — `X-Forwarded-For` is honoured
  only when the peer is loopback; a direct remote peer's header is ignored, so
  the rate limiter cannot be evaded by spoofing.
- **`auth.rs` `verify_basic`** (line 87) — argon2 runs even when the username
  mismatches, so there is no user-existence timing oracle; `login_handler`
  (`server.rs:245-249`) burns `DUMMY_HASH` for the same reason.
- **`auth.rs` session cookie** (lines 319-361) — username is base64'd before the
  `:` join, so `admin:backup` and `admin` cannot collide; the MAC is compared
  with `subtle::ConstantTimeEq`; expiry is checked after the MAC.
- **`auth.rs` rate limiter** (lines 129-163) — empty IP entries are removed on
  both the check and record paths, plus a periodic full sweep every 64th record;
  no unbounded growth from single-request IPs.
- **`server.rs` refuse-to-bind matrix** (lines 62-81) — non-loopback requires
  `--allow-remote`, rejects token mode, requires Basic credentials to be
  present, and requires TLS. Consistent with the doc comment.
- **`server.rs` `logout_handler`** (line 273) — requires a valid session cookie
  in `users` mode, closing the CSRF-logout hole.
- **`server.rs` `handle_client_msg` Resume** (line 538) — replayed frames keep
  their original seqs and the `Resumed` marker reuses the last one, so a
  client's seq stream stays monotonic; direct frames go down a per-connection
  channel and are never broadcast.
- **`session.rs` `replay_after`** (line 245) — the three-rule contiguity check
  is stated in terms of ordering rather than buffer membership, which is what
  makes a snapshot's seq a valid cursor.
- **`session.rs` `adopt_state` / `host.rs` `resume`** (lines 808, 171) — the
  agent lock is taken before anything is swapped and the new session's open-lock
  is acquired before the old one is released; the regression test at
  `session.rs:1169` pins it.
- **`session.rs` `tick` diffing** (line 303) — the `from` computation handles a
  transcript that got _shorter_ (a `/clear` or a resume into an earlier
  conversation) by emitting an empty-entries frame that truncates the client.

### hrdr-tools

- **`web.rs` `SsrfGuardResolver`** (line 49) — reqwest connects only to the
  addresses this resolver returns, and it is the same resolution used to
  validate them, so there is no DNS-rebinding TOCTOU on the initial request or
  any redirect hop. Fails closed when nothing public remains.
- **`web.rs` `read_capped` / `push_capped`** (lines 493, 515) — every body read
  (`fetch`, DDG, SearXNG) is bounded before parsing; the exactly-at-cap case
  signals stop.
- **`web.rs` SearXNG client** (line 160) — deliberately unguarded (operator-set
  env var, not attacker-reachable) but with redirects disabled, so a compromised
  instance cannot steer a POST at an internal host after the fact. Rationale is
  documented and the two loopback spellings are both tested.
- **`hooks.rs` `render_command`** (line 63) — `{path}` is shell-quoted, and the
  tests pin that a path containing `'; rm -rf /; '` stays one argument.
- **`hooks.rs` timeouts** (lines 112-123, 274-295) — the process _group_ is
  killed on timeout, not just the leader, so a formatter that shelled out goes
  with it. (The message it prints is finding 3.)
- **`gate.rs` `commands_in_yaml`** (line 381) — one recursive walk over the
  document covers nine CI providers; a malformed file is skipped without taking
  the rest of the gate with it, and an oversized one is a miss rather than a
  failure. Both pinned by tests.
- **`gate.rs` `matched`** (line 167) — token-subset matching, so extra flags
  still count and a dropped `--workspace` does not; narrowing a whole gate
  command downgrades the scope to `Partial`.
- **`replace.rs` two-phase apply** (lines 153-254) — every target is planned and
  permission-checked before any file is written, `MAX_FILES` is counted against
  _matching_ files rather than candidates, and output size is bounded exactly
  for literal mode and incrementally for regex mode (so a `$1$1$1…` template
  cannot OOM). The diff shown is the post-hook content actually on disk.
- **`mcp/client.rs` `read_stdio_line_capped`** (line 30) — an oversized line is
  drained through its newline so the next message stays parseable; the EOF arm
  returns `Ok(None)` only for the call that saw the oversized line, and the
  following call reports EOF, so the reader loop cannot spin.
- **`mcp/client.rs` `connect_sse`** (line 230) — the server-supplied `endpoint`
  URL must match the base URL's host, and the per-message byte cap resets on
  each drained event rather than accumulating over the stream's lifetime.

### hrdr-editor / hrdr-tui

- **`editor/lib.rs` `char_width` + `compute_wrapped_layout`** (lines 122, 180) —
  wrap math and cursor placement share one width function, so wide glyphs and
  zero-width marks agree between the two.
- **`tui/ui.rs` `highlight_line`** (line 1688) — byte-index slicing is sound
  because `to_ascii_lowercase` preserves both length and char boundaries, which
  the doc comment states.
- **`tui/ui.rs` `clamp_u16`** (line 817) — saturates rather than truncating; the
  test spells out what the `as u16` bug did.
- **`tui/app.rs` mouse selection** (lines 1565-1615) — a press is not committed
  as a click until the release, so a drag and a click are distinguished by
  `moved`; scrolling clears the selection because its coordinates are screen
  cells, not transcript offsets.

## Accepted, not findings

- **Token in the query string** (`auth.rs:80`) — `?token=` lands in proxy logs
  and `Referer`. Acceptable because `serve()` refuses token mode on any
  non-loopback bind, so there is no proxy in the path.
- **`cookie_secret` is per-process** (`auth.rs:44-45`) — every restart
  invalidates outstanding sessions. Correct default for a single-user tool;
  would need to be persisted only if multi-instance serving arrives.
- **`/healthz` is unauthenticated** (`server.rs:97`) — returns a constant, leaks
  nothing but liveness.

## Coverage

- **hrdr-web**: fully reviewed (`auth`, `config`, `server`, `session`, `host`,
  `users`, `lib`). `convert.rs` read for call shape only.
- **hrdr-protocol**: fully reviewed.
- **hrdr-tools**: this pass added `gate.rs`, `hooks.rs`, `web.rs`, `replace.rs`
  (execute + collect), `mcp/client.rs` (transports + framing). Combined with the
  2026-07-31 pass, still not deeply reviewed: `find.rs`, `ls.rs`,
  `secret_diff.rs`, `mutation.rs`, `todo.rs`, `tree.rs`, `verify.rs`,
  `memory.rs`, `verification.rs`, `ansi.rs`, `test_nudge.rs`, `lsp.rs`.
- **hrdr-editor**: reviewed (`lib.rs` seam + wrapping); `plain.rs` and `host.rs`
  read for shape.
- **hrdr-tui**: partially reviewed — the mouse/selection/clipboard path added by
  `e1b3023`, the scroll and highlight math in `ui.rs`. `app/commands.rs` and the
  bulk of `ui.rs`'s block rendering are **not** covered.
- **hrdr-app**: partially reviewed — `util.rs` mention expansion, `login.rs`
  route shape, `config.rs`. `commands/dispatch.rs` (946 lines) is **not**
  covered.
- **hrdr-llm, hrdr-agent**: not re-reviewed beyond confirming the two carried
  findings; see the 2026-07-31 report for their coverage.
