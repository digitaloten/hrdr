# Security & Correctness Audit

**Audited:** 2026-07-22 · **Re-reviewed:** 2026-07-23 · **Last finding closed:**
2026-07-26 · **Depth:** High · **Scope:** Full codebase — all crates
(`hrdr-tools`, `hrdr-llm`, `hrdr-agent`, `hrdr-app`, `hrdr-editor`, `hrdr-tui`,
`hrdr` binary) and all source files.

Remediation ran past the re-review: O4, O5 and O3 were closed on 2026-07-25/26,
after the re-review date. **Nothing from this audit is open.** The file is kept
for its methodology and its record of what the security-critical paths get right
— not as a live worklist.

## Methodology

The attack surface was mapped by identifying entry points: HTTP handlers
(`fetch`, `search`, MCP HTTP/SSE transports), CLI args (`clap` in `main.rs`),
file parsers (read/write/edit/replace/grep tools), IPC (MCP stdio/HTTP, LSP),
and environment reads (`HRDR_*` env vars, `HOME`/`XDG` paths). Each class of
vulnerability was checked systematically against every source file: injection,
memory/resource, crypto, AuthZ/AuthN, data integrity, error handling, and
concurrency.

Findings were verified by re-reading surrounding code, tracing callers, and
constructing concrete trigger scenarios. The original pass found 16 issues, all
since fixed.

---

## Open findings

**None.** All 16 findings are closed; O3, the last one, was fixed in `1794c5a`.
The resolved findings have been pruned — `git log` has the commits.

The one platform caveat left on record: `hrdr-llm`'s wire-debug log
(`HRDR_LOG_REQUESTS`) sets `0600` + `O_NOFOLLOW` only on unix and no explicit
ACL on Windows. Disclosed in its own doc comment, gated behind an opt-in env
var, and not a finding — pointing the log at a world-readable directory leaks on
any platform.

---

## Summary

| Severity  | Open  | Resolved |
| --------- | ----- | -------- |
| Critical  | 0     | 0        |
| High      | 0     | 2        |
| Medium    | 0     | 4        |
| Low       | 0     | 13       |
| **Total** | **0** | **16**   |

**Overall risk: Low.** The security-critical paths are well-built: `fetch`/SSRF
guard uses a TOCTOU-free DNS resolver; `SseDecoder` is memory-bounded; the
credential store uses atomic write + `0600` + cross-process locking; PKCE uses a
CSPRNG-backed verifier with SHA-256 S256; the untrusted content envelope uses a
verified-absent nonce; secret-denylist coverage is broad (`read`, `grep`, `git`,
`replace`, `fileops`, `lsp_nav`, `write`/`edit`); `canonicalize_nearest`
prevents `..` path escapes. No critical pathologies: no MD5/SHA1, no hardcoded
secrets, no panics on untrusted SSE input, no buffer overflows, no data races,
no unbounded allocation in hot paths.

Everything found by this audit is fixed. The last to close, O3, was the `read`
TOCTOU identity check running only on unix; it is now enforced on both platforms
through one helper (`guard_not_swapped`), so the guard cannot silently regress
on one of them again (`1794c5a`).

A deeper Windows-drift pass over the ~40 other `cfg(unix)`-only blocks was never
run — O3 was found by hand, not by that sweep. It is tracked in
`deferred-improvements.md`, with `store_lock.rs`, `auth.rs` and `auth_store.rs`
named as the next places to look.
