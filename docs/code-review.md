# Code Review — 2026-07-29

Scope: entire codebase (working tree clean on `main`).

Reviewed: all crates — `hrdr-web`, `hrdr-llm`, `hrdr-tools`, `hrdr-agent`,
`hrdr-app`, `hrdr-tui`, `hrdr-editor`, `hrdr-protocol`, `hrdr-test-support`.

## Findings

### MEDIUM

**1. `std::sync::Mutex` held across argon2 in async handler blocks tokio
workers** `crates/hrdr-web/src/server.rs:235`, `crates/hrdr-web/src/users.rs:65`

`login_handler` acquires `users_db` (`Arc<Mutex<Option<Connection>>>` — a
`std::sync::Mutex`) at server.rs:235 and holds it through
`users::verify(conn, …)` at line 237, which runs argon2id password verification
(100–200 ms) inside the critical section. `std::sync::Mutex::lock()` blocks the
OS thread when contested; combined with CPU-intensive argon2 work, concurrent
login attempts starve other tokio tasks on the same worker threads.

```
Repro: POST /login with valid credentials, 8 concurrent requests while also
       GET /healthz.
Expect: /healthz responds in < 100 ms.
Actual: /healthz stalls until all login argon2 work completes (up to seconds
        of unresponsiveness).
```

Trace:

- `login_handler` at `server.rs:235`: acquires `users_db` lock
- `server.rs:237`: calls `users::verify(conn, …)` while lock is held
- `users.rs:66`: `get_password_hash` (fast SQL query)
- `users.rs:71`: `auth::verify_basic_password` (argon2id, 100–200 ms)
- `server.rs:240`: `drop(db)` — lock released only after argon2

The DB query should be scoped under the lock, the argon2 run outside it.

### LOW

**2. Rate-limiter HashMap grows without bound for successful-auth traffic**
`crates/hrdr-web/src/auth.rs:131`, `crates/hrdr-web/src/server.rs:315`

`check_rate_limit` inserts an empty `Vec<Instant>` via `.entry(ip).or_default()`
for every IP on first access (auth.rs:131), but never removes it on the success
path. Only `rate_limit_record` removes empty entries (auth.rs:145–147) and runs
the periodic full-map sweep (auth.rs:149–155). Since successful authentication
calls `check_rate_limit` but NOT `rate_limit_record` (server.rs:315 returns
`Ok(())` without recording), empty entries accumulate. The sweep runs only every
64th call to `rate_limit_record`, so it may never fire on a deployment with only
successful auths.

```
Repro: Authenticate from 1000 distinct IPs (proxy/container pool), every auth
       succeeds, no auth ever fails.
Expect: Rate-limiter HashMap stays bounded, expired entries evicted.
Actual: Empty `Vec<Instant>` entries accumulate; eviction only on sweep
        (every 64th failed auth).
```

**3. `logout_handler` has no authentication check (CSRF)**
`crates/hrdr-web/src/server.rs:263`

`POST /logout` sets a session-clearing cookie without verifying the caller holds
a valid session. A cross-origin POST from an attacker's page clears the victim's
`hrdr_session` cookie.

```
Repro: POST /logout from https://evil.com via form submission (no cookie).
Expect: 403 (requires authentication).
Actual: 200 OK with `Set-Cookie: hrdr_session=; Max-Age=0`.
```

The `SameSite=Strict` on the response restricts sending the cookie later, but
does not prevent setting it from a cross-site response.

---

## Cleared

- **Argon2 constant-time vs user enumeration** (`users.rs:66–69`): nonexistent
  user burns argon2 on `DUMMY_HASH`, matching the timing of a real verify. No
  user-oracle via timing. ✓
- **`X-Forwarded-For` spoofing** (`auth.rs:174–186`): only honored when the peer
  is loopback; remote peers' spoofed headers are ignored. ✓
- **WS origin check** (`auth.rs:199–224`): correctly rejects foreign hosts,
  cross-loopback spellings, null origins, and loopback-without-port. ✓
- **Session cookie parsing** (`server.rs:319–326`): uses `strip_prefix` on each
  `;`-delimited part — a cookie named `x` with value `hrdr_session=…` cannot
  inject. ✓
- **SQL injection** (`users.rs:35,44,52`): all queries use parameterized `?1`
  placeholders. ✓
- **Grep off-by-one** (`grep.rs:395`): `if matches > max_matches` uses `>` (not
  `>=`), so the 200th match is included and the 201st triggers the cap — exactly
  200 matches at default. Offset form at line 417 uses `>=` to cap at 50 hits
  before emission. Both correct. ✓
- **SSE bare `\r`** (`sse.rs:119`): the decoder splits only on `\n` and strips
  trailing `\r` inside `flush_line`. A bare `\r` (SSE spec §9.2) would merge two
  lines. Unreachable in practice — HTTP SSE transport from every known
  server/proxy emits `\n` or `\r\n`. ✓
- **SSE `Content-Type` not validated** (`client.rs`): a status-200 HTML error
  page produces garbage events read as transient errors. The decoder has a 32
  MiB buffer cap (DoS guard), and the real error is surfaced on the next retry.
  Not a correctness bug. ✓
- **`atomic_write` hardlink-count TOCTOU** (`mutation.rs:159–169`): the
  `symlink_metadata` and `hardlink_count` race with concurrent hardlink
  creation. Window is two syscalls; the file tools are the only writer under
  normal operation. ✓
- **`replace` tool no `no_ignore` flag** (`replace.rs:330`): `collect_files`
  honors `.gitignore` (via `ignore::WalkBuilder`) with no opt-out flag, unlike
  `grep` which has `no_ignore`/`hidden`. By design — `replace` mutates files and
  gitignored/vendored directories are not intended modification targets.
  Documentation gap, not a bug. ✓

---

## Hardening

- **Ephemeral cookie secret invalidates all sessions on restart**
  (`auth.rs:44–45`): `AuthState::from_config` generates a fresh `cookie_secret`
  on every startup, logging everyone out.
- **WebSocket connections have no idle timeout** (`server.rs:351–466`):
  `handle_socket` has frame/message size limits but no heartbeat, idle timeout,
  or ping/pong handler.
- **Rate limiter global mutex** (`auth.rs:130,138`): every auth check serialises
  through one `Mutex<HashMap<…>>`. Sharding would distribute it.
- **Rate limiter check→record TOCTOU** (`auth.rs:129–133,137–141`): concurrent
  requests can both observe `len() == 9` and both pass, overshooting the
  10-per-minute cap by 2–3×.
- **`SubagentSlots` zero-cap silent** (`delegation.rs:32`): when `max = 0`
  (blocked by config validation but reachable via programmatic construction),
  all sub-agent spawns fail silently with "too many sub-agents".

---

## Coverage

Reviewed every crate. Read in full or near-full: all of `hrdr-web` (server,
auth, session, users, config), `hrdr-llm` (sse, client, codex, anthropic,
capped_read, catalog, types, fs), `hrdr-tools` (sandbox, gate, verification,
web, guardrails, proc, memory, lsp, all tools/\*.rs), `hrdr-agent` (delegation,
turn, turn_loop, session, compaction, config, prompt, auth, auth_store,
store_lock, hooks, models, resolve, transcript_log), `hrdr-app` (dispatch,
config, sessions, format, highlight, util, lib, helpers), `hrdr-editor` (lib,
plain), `hrdr-tui` (tui, app, ui), `hrdr-protocol` (lib), `hrdr-test-support`
(lib).

Not reviewed: `hrdr-ui` (excluded from workspace), `hrdr-app/src/completion.rs`,
`hrdr-app/src/effort.rs`, `hrdr-app/src/subagents.rs`,
`hrdr-agent/src/oauth.rs`, `hrdr-agent/src/chatgpt_models.rs` — supporting
modules that feed into the core logic reviewed above. The TUI rendering code
(`ui.rs` past line 200) was sampled at entry point and key render paths.
