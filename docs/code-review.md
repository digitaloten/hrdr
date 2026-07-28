# Code Review — 2026-07-28

Scope: full codebase (clean working tree, `v0.8.4..HEAD` baseline).
Reviewed: `hrdr-web`, `hrdr-llm`, `hrdr-tools`, `hrdr-agent`. Skipped: `hrdr-app` CLI, `hrdr-tui`, `hrdr-editor`, `hrdr-ui` (no recent changes in the diff
range to those crates, and scope limited by available review time).

---

## Findings

### HIGH

**1. Rate limiter `HashMap<IpAddr, Vec<Instant>>` grows unbounded — memory DoS**
`crates/hrdr-web/src/auth.rs:30`, `:123-129`, `:131-138`

The `HashMap` entries (IP address keys) are **never evicted**. Each check prunes the `Vec<Instant>` values (`retain` within the window), but the map key itself
persists forever. An attacker sending one request each from many unique IPs (spoofed or real) permanently balloons the map.

```
Failure: Attacker sends 1 request each from 10M unique IPs → 10M permanent
Vec entries (most empty or one-element) → hundreds of MB → OOM.
```

### MEDIUM

**2. `logout_handler` omits `Secure` on TLS deployments — session cookie not cleared**
`crates/hrdr-web/src/server.rs:261-271`

`login_handler` at line 250 sets `; Secure` when `state.tls_enabled` is true. `logout_handler` takes **no `State`** (line 261) and always emits
`hrdr_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0` — without `Secure`. Some browsers refuse to clear a `Secure` cookie via a response that omits
`Secure`.

```
Failure: User logs out on TLS deployment → browser ignores the clear-Set-Cookie
→ session cookie survives → attacker with physical access reuses the session.
```

**3. WebSocket accepts unbounded frames — memory exhaustion**
`crates/hrdr-web/src/server.rs:348`

`handle_socket` receives an axum `WebSocket` with default configuration — no `max_frame_size` or `max_message_size`. An attacker can send arbitrarily large
WebSocket text frames, consuming server memory proportional to message size.

```
Failure: Attacker connects to /ws → sends 1 GB Text frame → server allocates
1 GB → repeated across connections → OOM.
```

**4. Usernames containing `:` deserialize incorrectly from session cookies**
`crates/hrdr-web/src/auth.rs:284-285`, `:303-309`

`mint_session_cookie` formats `{username}:{expiry}` and `verify_session_cookie` splits with `split_once(':')`. A username like `admin:backup` becomes
indistinguishable from `username=admin, expiry=backup`. The HMAC prevents forgery, but the cookie **authenticates as the wrong user** (`admin` instead of
`admin:backup`).

```
Failure: Create user "admin:backup" → login → cookie authenticates as "admin"
(truncated at the first colon) → privilege escalation within Users mode.
```

**5. `tail_window` panics on single-message input via `clamp`**
`crates/hrdr-agent/src/compaction.rs:442`

```rust
let keep = (msgs.len() / div.max(1)).clamp(2, msgs.len());
```

`clamp(min, max)` panics if `min > max`. When `msgs.len() == 1` and `div ≥ 2`: `(1 / 2).clamp(2, 1)` → integer division gives `0`, `0.clamp(2, 1)` → panic because
`2 > 1`.

```
Reachability: A conversation with [system, huge_user_message] where compaction
fires. The summarizer overflows at stages 0 (full) and 1 (elide_tool_results).
Stage 2 calls tail_window(&elide_tool_results(&full), 2) where `full =
messages[1..tail_start]`. If tail_start=2, that's one message. When div=2,
the calculation panics.
```

### LOW

**6. SSE `cur_data_started` set true when data buffer is full**
`crates/hrdr-llm/src/sse.rs:165-190`

When `remaining == 0` (the `cur_data` buffer is at `MAX_BUFFER_BYTES`), no data is appended — but `cur_data_started` is unconditionally set to `true` at line 190.
A subsequent blank line emits an event with stale (or partial prior-event) `cur_data` content. The `overflowed` flag causes the caller to discard these events,
so the impact is limited to the wasted event construction.

**7. `read_capped_json` unreachable overflow branch and incorrect comment**
`crates/hrdr-llm/src/capped_read.rs:119-127`

The `buf.len() > cap` bail at line 119 is unreachable: the `remaining` check at line 109 guarantees `chunk.len() > remaining` bails before any write that would
push past `cap`. The comment (lines 120-122) claims a zero-length chunk at exactly `cap` could land here — but `0 > 0` is false, so `extend_from_slice(&[])` is a
no-op. Dead code with a misleading comment.

**8. Sandbox TOCTOU between path check and disk write**
`crates/hrdr-tools/src/lib.rs:326-331`, `crates/hrdr-tools/src/tools/mutation.rs:96-114`

`resolve_write` canonicalizes the path, calls `check_write`, then returns the **uncanonicalized** path. The mutating tools then pass this path to `atomic_write`
without re-validating. A concurrent process (not the model) could replace a directory component with a symlink between check and write. `guard_not_swapped`
(lib.rs:710-725) exists for the read path but is not used by write/edit. Limited impact: requires an external concurrent attacker.

**9. `Retry-After` header only handles delta-seconds, not HTTP-date**
`crates/hrdr-llm/src/client.rs:300-301`

`retry_after_from_headers` parses `Retry-After` as `u64` seconds only. RFC 7231 §7.1.3 also allows `Retry-After: <HTTP-date>`. A server sending a date format
gets `None` (silently ignored), so the retry loop won't delay.

**10. `set_timeout(None)` removes the original 300s request timeout**
`crates/hrdr-llm/src/client.rs:575-583`

`Client::new()` sets a 300-second request timeout (line 513). `set_timeout(None)` rebuilds `reqwest::Client` with NO timeout at all — `reqwest`'s own default,
not the `Client`'s original 300s. A caller that does `set_timeout(Some(x))` then `set_timeout(None)` has no per-request deadline.

---

## Cleared

- **Argon2 usage**: `Argon2::default()` (argon2id, m=19 MiB, t=2, p=1) meets OWASP minimums. `verify_basic` runs argon2 even on username mismatch
  (`DUMMY_HASH` anti-enumeration). ✓
- **SQL injection**: All queries in `users.rs` use `rusqlite::params![]`. ✓
- **Token/secret generation**: `rand::rng().random::<[u8; 32]>()` uses CSPRNG (ChaCha12). ✓
- **Constant-time comparison**: `verify_token` and `verify_basic` use `subtle::ConstantTimeEq`. ✓
- **Session cookie integrity**: HMAC-SHA256 over `username:expiry` with 32-byte CSPRNG key; expiry enforced server-side. ✓
- **CSRF hardening**: Session cookie has `SameSite=Strict` + `HttpOnly`. WS upgrade validates `Origin` against `Host` header with port-aware loopback
  enforcement (`auth.rs:174-199`). ✓
- **X-Forwarded-For spoofing**: `extract_client_ip` only honors `X-Forwarded-For` when TCP peer is loopback (reverse-proxy scenario). ✓
- **Path traversal in memory tool**: `safe_stem` rejects `/` and `\\`; `resolve` rejects `..` components. Both slugify + defense-in-depth. ✓
- **Sandbox `.git` protection**: `protected_metadata_dir` checks all canonical path components for `.git`; symlink escape caught via canonicalization
  (tested at sandbox.rs:1396-1403). ✓
- **URI encoding**: `file_uri` correctly percent-encodes non-ASCII per RFC 3986; `uri_to_path` decodes via byte buffer to avoid Latin-1 mojibake. ✓
- **ANSI stripping**: Correctly handles CSI sequences (all parameter/intermediate bytes), OSC (BEL and ST termination), CR progress-bar redraw, and
  truncated sequences at end-of-input. ✓
- **Process group killing**: `unix_group_kill` guards `pid > 1` to prevent `kill(-0)`/`kill(-1)`. `ProcessGroup::Drop` group-kills on cancel/abort. ✓
- **`atomic_write` symlink/hardlink handling**: Detects symlinks (`is_symlink()`) and hardlinks (link count > 1), falling back to in-place `write` through
  the link rather than replacing it with a regular file. ✓
- **Guardrail `strip_unbalanced_quotes`**: Falls back toward false positive (blocks safe commands) rather than false negative (allowing dangerous ones).
  The doc comment at `guardrails.rs:11-19` explicitly declares guardrails are a seatbelt, not a lock. ✓
- **Guardrail nested `sh -c` extraction**: Bounded by `MAX_NESTED_PAYLOAD_BYTES` (64 KiB cumulative), not depth. ✓
- **`canonicalize_nearest`**: Correctly resolves `..` in non-existent suffixes and canonicalizes the existing prefix. ✓
- **`file_uri` ↔ `uri_to_path` round-trip**: Tested with non-ASCII filenames, spaces, and raw-UTF-8-from-server edge case. ✓
- **`atomic_write` permissions preservation**: Unix: carries over existing mode bits (executable stays executable). Windows: leaves ACL inheritance to
  containing directory (permissions are not a bitmask there). ✓

---

## Hardening

- **Rate limiter global mutex** (`auth.rs:124`, `:133`): Every auth attempt acquires the same `Mutex<HashMap<…>>`. Under heavy legitimate load, all auth
  checks serialize through this lock. A per-IP sharded approach would distribute contention.
- **Rate limiter check→record TOCTOU** (`auth.rs:122-127`, `:131-138`): Concurrent requests at the same boundary (e.g., both read `len() == 9`) can both
  pass, overshooting the 10-attempts-per-minute budget by 2-3× under concurrency.
- **Delegation path rewriting** (`delegation.rs:1422-1440`): Only rewrites exact `"<cwd>/"` prefix occurrences. Absolute paths without trailing slash and
  parent-directory paths escape the rewrite. Acknowledged in the comment at lines 1411-1419.
- **SSE Content-Type not validated** (`client.rs:784-809`): The SSE decoder is fed response bytes without checking that `Content-Type` is
  `text/event-stream`. A status-200 HTML/JSON error page produces garbage SSE events; the caller treats these as a transient error and retries until the
  retry budget exhausts.
