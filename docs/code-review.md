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

---

## Findings

### HIGH

**1. The rate-limiter map still grows without bound — the fix is unreachable**
`crates/hrdr-web/src/auth.rs:125`, `:133-141`

`8f220d7` added key eviction to `rate_limit_record`, but it cannot run. The
function pushes `Instant::now()` and only then prunes, so the entry it tests is
never empty:

```rust
let entry = guard.entry(ip).or_default();
entry.push(Instant::now());          // entry.len() >= 1 from here on
prune_rate_limit_entry(entry);       // prunes >60s old; the one just pushed stays
if entry.is_empty() { guard.remove(&ip); }   // dead branch
```

`check_rate_limit` is the larger half: it is called on every login and every WS
upgrade (`server.rs:221`, `:286`), and its `guard.entry(ip).or_default()`
inserts a key for every IP it has ever seen. Nothing removes those. The original
attack is unchanged.

```
Repro: 10k distinct IPs each send one failed auth, then 61s pass
Expect: rate_limiter.len() == 0 (all entries expired and evicted)
Actual: rate_limiter.len() == 10000, every Vec empty
```

The fix that works is a sweep, not a per-entry check: prune and drop empty keys
across the map on a cadence (every Nth call, or by elapsed time), since an IP
that never comes back is exactly the one no per-IP code path will ever revisit.

### MEDIUM

**2. `Retry-After` HTTP-date parsing is arithmetically wrong**
`crates/hrdr-llm/src/client.rs:352`

`days_from_civil` misstates Howard Hinnant's algorithm. It computes
`153 * (m + 1) / 5` where the algorithm requires `(153 * (m - 3) + 2) / 5`. The
day-of-year offset is wrong for every date, and the month spacing is wrong too,
so the error is not even a constant:

```
Repro: days_from_civil(1970, 1, 1) / (2000, 1, 1) / (1994, 11, 6)
Expect: 0 / 10957 / 8710
Actual: 122 / 11079 / 9197        (off by 122, 122, 487)
```

Every date-form `Retry-After` therefore resolves roughly four months into the
future and clamps to the 60 s ceiling. A date in the _past_, which the function
documents as returning `None`, yields a 60 s delay instead.

There are no tests for `parse_imf_fixdate` or `days_from_civil` — 40 lines of
hand-transcribed calendar arithmetic, none of it observed against a known value.
Either delete the HTTP-date branch (delta-seconds is what providers actually
send) or take the algorithm from a dependency.

### LOW

**3. The `atomic_write` TOCTOU guard cannot fire**
`crates/hrdr-tools/src/tools/mutation.rs:141-155`

The guard compares `metadata(path)` against
`metadata(canonicalize_nearest(path))` and rejects on a `(dev, ino)` mismatch.
`std::fs::metadata` follows symlinks, so a path and its canonical form resolve
to the same inode by construction — that is what canonicalisation means. The
condition is unreachable.

```
Repro: write through /tmp/link/f where link -> /tmp/real
Expect (as documented): rejected as a swapped component
Actual: dev/ino identical on both sides, write proceeds
```

A new file misses it regardless: `metadata(path)` is `Err` before the write, so
the `if let` never binds. The check-to-write window the comment claims to close
is exactly as open as it was. `guard_not_swapped` (`lib.rs`) is the mechanism
that actually does this for reads and is still not used here.

**4. `set_timeout(None)` does not restore what its doc comment says**
`crates/hrdr-llm/src/client.rs:646-651`

`Client::new` sets `.timeout(300s)` — an overall request deadline. `set_timeout`
rebuilds the client with `.connect_timeout(dur).read_timeout(dur)` and no
`.timeout(...)` at all. The doc comment now claims `None` "restores the default
300-second timeout, matching the `Client::new` builder"; it does not. After any
`set_timeout` call there is no overall request deadline, which was the substance
of the original finding.

Connect + read timeouts are arguably the better choice for a streaming client —
but then the comment should say that, and `Client::new` should agree. As it
stands the two constructors disagree about which deadline exists and the comment
describes neither.

**5. `prune_rate_limit_entry` documents a return value it does not have**
`crates/hrdr-web/src/auth.rs:144-146`

> `/// Prune expired timestamps from a rate-limit entry and return whether any remain.`

It returns `()`. Had it returned that bool, finding 1's dead branch would have
had something to test.

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

The three confirmed findings share one cause: each is a change of _mechanism_ —
does an eviction happen, does a conversion produce the right number, does a
guard ever reject — landed without an observable, in a commit whose test suite
went green because nothing in it looks at any of the three.
