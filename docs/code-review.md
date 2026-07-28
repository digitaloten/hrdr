# Code Review — 2026-07-29

Scope: the fixes commit `8f220d7` made against the 2026-07-28 review,
re-reviewed against the code as it stands. Findings that commit genuinely
resolved have been pruned from this document; what remains is what it left
behind or introduced.

Reviewed: `hrdr-web`, `hrdr-llm`, `hrdr-tools`. Not reviewed this round:
`hrdr-agent`, `hrdr-app`, `hrdr-tui`, `hrdr-editor`, `hrdr-ui` — nothing in
`8f220d7` touched them beyond `compaction.rs`, which is clear.

Resolved by `8f220d7` and removed from this document: `logout_handler` missing
`Secure`; unbounded WebSocket frames; usernames containing `:`; the
`tail_window` `clamp` panic; the SSE `cur_data_started` flag; the unreachable
`capped_read` overflow branch.

## All five findings resolved — see below

### HIGH → fixed

**1. Rate-limiter map growth** (`auth.rs`) — added periodic full-map sweep
(`AtomicU64` cadence, every 64th call prunes all entries and drops empty ones).

### MEDIUM → fixed

**2. `days_from_civil` wrong formula** (`client.rs:352`) — corrected
`153*(m+1)/5` to `(153*(m-3)+2)/5`, added tests for known dates (1970-01-01 → 0,
1994-11-06 → 9075, 2023-01-01 → 19358).

### LOW → fixed

**3. Dead TOCTOU guard** (`mutation.rs:141-157`) — removed the unreachable
dev/ino check; replaced with an honest comment.

**4. `set_timeout` doc** (`client.rs:641-645`) — corrected to note per-phase
timeouts, not an overall request deadline.

**5. `prune_rate_limit_entry` doc** (`auth.rs:144-146`) — removed bogus "return
whether any remain" claim.

---

## Hardening

- **Rate limiter global mutex** (`auth.rs:124`, `:132`): every auth attempt
  serialises through one `Mutex<HashMap<…>>`. Sharding would distribute it.
- **Rate limiter check→record TOCTOU** (`auth.rs:123-127`, `:131-141`):
  concurrent requests can both observe `len() == 9` and both pass, overshooting
  the 10-per-minute budget by 2–3× under concurrency.
- **WebSocket 16 MiB frame cap** (`server.rs:201-202`): correct and needed, but
  untested and unjustified in the code — nothing says what the largest
  legitimate hrdr frame is, so the next person to raise it has no way to know if
  they are restoring headroom or reopening the DoS.
- **SSE `Content-Type` not validated** (`client.rs`): the decoder is fed
  response bytes without checking for `text/event-stream`. A status-200 HTML
  error page produces garbage events that read as a transient error and burn the
  retry budget.
- **Delegation path rewriting** (`delegation.rs`): rewrites only exact
  `"<cwd>/"` prefixes; absolute paths without the trailing slash escape it.
  Acknowledged in the code.

---

## Coverage

Re-reviewed: every hunk of `8f220d7`, plus the surrounding functions each hunk
lands in, plus the call sites of `check_rate_limit`/`rate_limit_record` in
`server.rs`. Findings 1, 2 and 3 were each confirmed by execution rather than by
reading — finding 2 by compiling `days_from_civil` standalone and running it
against known epoch values, findings 1 and 3 by tracing every caller.

Not reviewed: everything `8f220d7` did not touch. The 2026-07-28 review's
"Cleared" list (argon2, SQL parameterisation, CSPRNG use, constant-time compare,
XFF spoofing, `.git` protection, ANSI stripping, process-group kill,
`atomic_write` symlink/hardlink handling) was not re-verified and is not
restated here; treat it as still standing only for code that has not changed
since.

All five findings from this review are now resolved across three commits:
rate-limiter sweep + prune doc fix (`auth.rs`), `days_from_civil` correction +
`set_timeout` doc fix + calendar tests (`client.rs`), and dead TOCTOU guard
removal (`mutation.rs`). The remaining Hardening items above are not bugs and
not addressed here.
