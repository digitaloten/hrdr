# Security & Correctness Audit

**Date:** 2026-07-22 · **Remediated & re-reviewed:** 2026-07-23 · **Depth:**
High · **Scope:** Full codebase — all crates (`hrdr-tools`, `hrdr-llm`,
`hrdr-agent`, `hrdr-app`, `hrdr-editor`, `hrdr-tui`, `hrdr` binary) and all
source files.

## Methodology

The attack surface was mapped by identifying entry points: HTTP handlers
(`fetch`, `search`, MCP HTTP/SSE transports), CLI args (`clap` in `main.rs`),
file parsers (read/write/edit/replace/grep tools), IPC (MCP stdio/HTTP, LSP),
and environment reads (`HRDR_*` env vars, `HOME`/`XDG` paths). Each class of
vulnerability was checked systematically against every source file: injection,
memory/resource, crypto, AuthZ/AuthN, data integrity, error handling, and
concurrency.

Findings were verified by re-reading surrounding code, tracing callers, and
constructing concrete trigger scenarios. The original pass found 16 issues; the
remediation and this re-review track them below.

---

## Open findings

One LOW residual remains from the 2026-07-23 remediation re-review. The resolved
findings (2 HIGH, 4 MEDIUM, 12 LOW) have been pruned — see `git log` for the
commits that closed them.

---

### O3 — LOW: `read` TOCTOU dev/ino re-check is Unix-only (M1 residual)

**`crates/hrdr-tools/src/tools/read.rs`** — the `#[cfg(unix)]` dev/ino block

The M1 fix opens the file first and reads through the handle, then re-checks the
opened descriptor's `dev`/`ino` against the canonical path — but only under
`#[cfg(unix)]`. On Windows there is no such re-check, so the narrow
open-secret-then-swap-to-non-secret race (open resolves to a secret, the path is
then repointed at a non-secret before `guard_secret_read` canonicalizes it) is
not caught, and the tool reads the pre-swap handle (the secret). Low: swapping a
file that is already open is much harder on Windows, and the audit's concrete
scenario was Unix symlinks.

**Fix:** add a Windows identity re-check (e.g. `BY_HANDLE_FILE_INFORMATION`
volume-serial + file-index via `GetFileInformationByHandle`), or document the
platform limitation.

---

## Summary

| Severity  | Open  | Resolved |
| --------- | ----- | -------- |
| Critical  | 0     | 0        |
| High      | 0     | 2        |
| Medium    | 0     | 4        |
| Low       | 1     | 12       |
| **Total** | **1** | **16**   |

**Overall risk: Low.** The security-critical paths are well-built: `fetch`/SSRF
guard uses a TOCTOU-free DNS resolver; `SseDecoder` is memory-bounded; the
credential store uses atomic write + `0600` + cross-process locking; PKCE uses a
CSPRNG-backed verifier with SHA-256 S256; the untrusted content envelope uses a
verified-absent nonce; secret-denylist coverage is broad (`read`, `grep`, `git`,
`replace`, `fileops`, `lsp_nav`, `write`/`edit`); `canonicalize_nearest`
prevents `..` path escapes. No critical pathologies: no MD5/SHA1, no hardcoded
secrets, no panics on untrusted SSE input, no buffer overflows, no data races,
no unbounded allocation in hot paths.

Everything except O3 is fixed. O3 is a Windows-only gap in the `read` TOCTOU
identity check; hrdr targets UNIX workflows, so the practical exposure is small.
