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

## Resolved

Detailed entries pruned — each original finding is recorded here with the commit
that fixed it. Items with a residual left after the fix are tracked in **Open**.

| ID  | Finding                                                       | Fixed in  | Residual |
| --- | ------------------------------------------------------------- | --------- | -------- |
| H1  | MCP SSE `endpoint` SSRF — host validated against the base     | `ab2f1b7` | —        |
| H2  | LSP `check_confined` `..` escape — canonicalized fallback     | `98a86b3` | —        |
| M1  | `read` secret-denylist TOCTOU — open → dev/ino verify → read  | `e314853` | O3       |
| M2  | Guardrail depth cap — replaced with a 64 KiB cumulative bound | `e314853` | —        |
| L1  | `write`/`edit` didn't reject secret _targets_                 | `e314853` | —        |
| L2  | OAuth expiry overflow — `saturating_add`/`saturating_mul`     | `65a425d` | —        |
| L3  | Catalog fetch unbounded — `read_capped_json`                  | `910ccee` | —        |
| L4  | `extra_headers` auth precedence — applied before auth header  | `910ccee` | O4       |
| L5  | LLM client had no default timeout — 300 s fallback            | `910ccee` | —        |
| L6  | JWT claims unverified — documented as a routing hint only     | `65a425d` | —        |
| L7  | OAuth `state` non-constant-time — `constant_time_eq`          | `65a425d` | —        |
| L8  | Catalog cache not `0600` — `OpenOptionsExt::mode(0o600)`      | `910ccee` | —        |
| L9  | Hooks docs misleading — noted they bypass the guardrails      | `e314853` | —        |
| L10 | Windows hook path quotes unescaped — `"` → `""`               | `e314853` | O5       |
| O1  | Force-push guardrail bypass via `'"--force` mid-command quote | `5a2f644` | —        |
| O2  | `AuthEntry` derived `Debug` over live tokens (M4 residual)    | `c135d05` | —        |
| O4  | `extra_headers` could duplicate the auth header (L4 residual) | `483fa42` | —        |
| O5  | Windows hooks ran `cmd /C` — now bash/sh like everything else | `d009d80` | —        |

---

## Open findings (from the 2026-07-23 remediation re-review, most-severe first)

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

## Summary

| Severity  | Open  | Resolved |
| --------- | ----- | -------- |
| Critical  | 0     | 0        |
| High      | 0     | 2        |
| Medium    | 0     | 4        |
| Low       | 1     | 12       |
| **Total** | **1** | **16**   |

**Overall risk: Low.** The security-critical paths remain well-built:
`fetch`/SSRF guard uses a TOCTOU-free DNS resolver; `SseDecoder` is properly
memory-bounded; the credential store uses atomic write + `0600` + cross-process
locking; PKCE uses a CSPRNG-backed verifier with SHA-256 S256; the untrusted
content envelope uses a verified-absent nonce; secret-denylist coverage is broad
(`read`, `grep`, `git`, `replace`, `fileops`, `lsp_nav`, and now
`write`/`edit`); `canonicalize_nearest` prevents `..` path escapes. No critical
pathologies: no MD5/SHA1, no hardcoded secrets, no panics on untrusted SSE
input, no buffer overflows, no data races, no unbounded allocation in hot paths.

Both HIGH findings and the entire Medium set are fixed — including O1 (the
force-push guardrail quote-bypass, `5a2f644`), O2 (M4 — `AuthEntry` no longer
derives `Debug`, `c135d05`), O4 (`extra_headers` can no longer carry an auth
header at all, `483fa42`) and O5 (hooks run through bash/sh on every platform,
so there is no `cmd.exe` quoting to get wrong, `d009d80`). What remains is one
LOW residual, O3 — a Windows-only gap in the `read` TOCTOU identity check.
