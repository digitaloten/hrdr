# DRY Audit

Codebase duplication analysis for hrdr. Analysis date: 2026-07-23. Each section
names a concern, where it lives, and whether it's _dry_ (one source of truth),
_damp_ (intentional duplication with a good reason), or _wet_ (accidental
duplication to fix). Line references are point-in-time and may drift.

---

## 1. Secret/credential file detection — DRY ✅

**One canonical list in `crates/hrdr-tools/src/lib.rs:559` —
`secret_file_reason`.**

Every content-reading path routes through it:

- `read` tool → `guard_secret_read` → `secret_file_reason`
- `grep` tool → `grep_line_is_secret` → `secret_file_reason`
- `validate_attach_path` (used by `@file` mentions + `/add`) →
  `secret_file_reason`
- `redact_secret_diffs` (git diff output) → its own redaction, but calls the
  same pattern-matching logic

Test coverage for each path exists in its own crate. Adding a new secret-file
pattern requires editing exactly one match block.

## 2. Path helpers — DRY ✅

**`resolve_under`, `floor_char_boundary`, `canonicalize_nearest` — defined once
in `crates/hrdr-tools/src/lib.rs`, re-exported where needed.**

`hrdr-app/src/util.rs:170-174` just re-exports (`pub use hrdr_tools::…`). No
crate reimplements these.

## 3. Slash-command dispatch — DAMP ✅

**`hrdr-tui/src/app/commands.rs:11` has `handle_slash` that mirrors
`hrdr-app/src/commands/dispatch.rs:12`'s `dispatch`.**

This is intentional: the TUI handler intercepts TUI-only commands (`edit`,
`reload`, `goto`, `find`/`search`, `next`/`prev`) then falls through to the
shared dispatcher for everything else. The shared `CommandHost` trait lets both
frontends drive the same command logic. The comment at line 18-21 explains the
split explicitly.

## 4. `AgentConfig` / test construction — DRY ✅

**~60+ call sites construct `AgentConfig { … }` inline, mostly in tests.**

- `crates/hrdr-agent/src/lib.rs` — ~60 inline constructions in test modules
- `crates/hrdr-tui/src/app/e2e.rs` — ~7 inline constructions
- `crates/hrdr-agent/src/validate.rs` — has `cfg()` and `cfg_with()` helpers
- `crates/hrdr-agent/src/resolve.rs` — has its own `cfg()` and `cfg_with()`
  helpers
- `crates/hrdr-app/src/commands/model.rs` — 3 inline constructions
- `crates/hrdr-app/src/commands/dispatch.rs` — 1 inline
- `crates/hrdr-app/src/login.rs` — 2 inline

`cfg()`/`cfg_with()` are duplicated across `validate.rs` and `resolve.rs` (both
in `hrdr-agent`). `lib.rs` tests have no equivalent at all.

### 4a. `fn r(s: &str) -> ModelRef` — 4 identical copies → DRY ✅

**Fixed** (`56b76ab`): one `pub(crate) fn r` in `model_ref.rs` (#[cfg(test)]),
removed 4 copies from `models.rs`, `resolve.rs`, `validate.rs`, `lib.rs`.

### 4b. `fn spec(s: &str) -> ModelSpec` — 4 copies → DRY ✅

**Fixed** (`56b76ab`): one `pub(crate) fn spec` in `model_ref.rs`
(#[cfg(test)]), removed 3 copies from `lib.rs` and 1 from `agents_dir.rs`.

### 4c. `ProviderConfig` — no `Default` impl → DRY ✅

**Fixed** (`56b76ab`): `ProviderConfig` now derives `Default`.

### 4d. `SubagentProfile` — no `Default` impl → DRY ✅

**Fixed** (`56b76ab`): `SubagentProfile` now derives `Default`.

### 4e. `cfg()`/`cfg_with()` duplicated across `validate.rs`/`resolve.rs` → DRY ✅

**Fixed**: one `cfg()`, `cfg_with(name, ProviderConfig)` and
`provider_config(base_url)` in `config.rs` behind `#[cfg(test)]` (the
`model_ref.rs` `r`/`spec` precedent); both private copies deleted.
`validate.rs`'s base-url flavour of `cfg_with` folds into
`cfg_with(name, provider_config(url))`, and `provider_config` now leans on
`ProviderConfig::default()` instead of spelling out all 8 fields (the #4c
leftover).

### 4f. ~60 inline `AgentConfig { … }` test constructions → premise was stale

Re-checked every `AgentConfig { … }` literal in the named files (69 `lib.rs`, 8
`e2e.rs`, 3 `model.rs`, 1 `dispatch.rs`, 2 `login.rs`): **all of them already
use `..Default::default()`** and pin only the 1–3 fields their test cares about.
`AgentConfig` has had a real production `Default` impl all along. The
consolidation this item imagined had effectively already happened; routing the
survivors through a `cfg_*` helper would replace self-documenting named fields
with positional args (the largest copy-paste cluster — 6 sites pinning
`base_url`+`api_key`+`model` — carries different values at every site), so it
was deliberately left alone.

The one genuine win taken: 23 field-less `AgentConfig { ..Default::default() }`
literals collapsed to `AgentConfig::default()` (16 in `lib.rs`, 7 in `e2e.rs`).
`hrdr-app`'s 6 sites stay untouched — the helpers are `#[cfg(test)] pub(crate)`
in a private module, and reaching them from another crate would mean exporting
test scaffolding publicly.

## 5. Session management layering — DRY ✅

**`hrdr-agent/src/session.rs` — persistence (read/write/lock).**
**`hrdr-app/src/sessions.rs` — UI-thread-safe wrappers.**

Clean split:

- `hrdr_agent::session::Session` / `SessionState` — the data model + file I/O
- `hrdr_app::save_agent_session` — locks the agent mutex, syncs state, saves
- `hrdr_app::latest_session_for_cwd` / `open_latest_session_for_cwd` — startup
  auto-resume logic

No overlap; the `hrdr-app` layer calls the `hrdr-agent` layer, never
reimplements it.

## 6. Skill/sub-agent discovery — DAMP ✅

**`crates/hrdr-app/src/skills.rs:64` — `skill_dirs` walks from cwd up to `/` +
XDG dirs.** **`crates/hrdr-agent/src/prompt.rs:201` — `gather_agent_docs` walks
from cwd up to `/` for AGENTS.md files.**

Same walk pattern (cwd → root + XDG fallback), but different payloads (skills vs
agent docs). A shared "walk project dirs" iterator could DRY the directory
traversal, but the logic is simple (~15 lines each) and the divergence in what
they collect makes a shared abstraction borderline over-engineering at current
scale.

## 7. `CommandHost` trait impls — DAMP ✅

**Three `impl CommandHost` blocks:**

- `crates/hrdr-tui/src/app/commands.rs` — real TUI host
- `crates/hrdr-app/src/commands/dispatch.rs` — `TestHost` (used in dispatch
  tests)
- `crates/hrdr-app/src/login.rs` — `TestLoginHost`

The trait itself is the DRY mechanism; each impl is a different kind of host.
`TestHost` and `TestLoginHost` share some trivial method bodies (no-ops for
`autosave`, `set_session_label`, etc.) but the login host has login-specific
state the dispatch test host doesn't need. A shared `TestHost` base could
eliminate the duplicate no-op bodies, but the overhead is tiny.

## 8. TUI selector draw functions — DRY ✅

**Fixed** (`aa09f5b`): the shared scaffolding is now two helpers in
`crates/hrdr-tui/src/ui.rs` — `modal_frame` (centered frame + block + small-area
early-return, returning the inner `Rect`) and `draw_pick_body` (the two-column
search/hint/list body). All six draw functions call `modal_frame`; the five
two-column pickers (model, skill, login-providers, effort, theme) share
`draw_pick_body`; `draw_session_selector` keeps its bespoke four-column body on
top of the shared frame. ~408 lines of duplicated scaffolding removed, one place
to change selector chrome. The historical duplication is described below.

**Six nearly identical modal-drawing functions in `crates/hrdr-tui/src/ui.rs`,
each ~100-150 lines:**

| Function                | Line | Selector Type           | Width Clamp |
| ----------------------- | ---- | ----------------------- | ----------- |
| `draw_model_selector`   | 229  | `ModelSelector`         | 92          |
| `draw_skill_selector`   | 334  | `SkillSelector`         | 92          |
| `draw_login_modal`      | 424  | `LoginProviderSelector` | 76          |
| `draw_effort_selector`  | 600  | `EffortSelector`        | 64          |
| `draw_theme_selector`   | 686  | `ThemeSelector`         | 92          |
| `draw_session_selector` | 771  | `SessionSelector`       | 110         |

All six share identical scaffolding (~40 lines each, 240 lines total):

1. Calculate centered `Rect` from area with width/height clamping
2. `f.render_widget(Clear, rect)`
3. Create `Block` with identical style/padding (`BLOCK_PAD_X`, `theme.user_bg`)
4. Early return on `inner.height < 3 || inner.width < 6`
5. Search line (`"Search  "` + filter + cursor `▌`)
6. Hint line (`"N items · ↑↓ select · Enter … · Esc cancel"`)
7. List height math (`inner.height.saturating_sub(3)`)
8. Scroll offset calculation
9. Row iteration with selected/unselected `Line` rendering

The only per-selector variations are the **row type**, **width clamp**, **hint
text**, and **row rendering**. The selector state machine (`Selector<T>` in
`crates/hrdr-tui/src/app/selector.rs`) is already perfectly DRY; the draw
functions are the duplication.

**Actionable**: Extract a generic `draw_selector_modal<T>` that takes closures
for row rendering and a config struct for width/hint/label. Estimated savings:
~500 lines removed, one place to change selector chrome.

## 9. File-attach flows — DRY ✅

**Both `@file` mentions (`util.rs:109 expand_mentions`) and `/add`
(`dispatch.rs:290`) use the same function: `hrdr_tools::read_attach_file`.**

Both also share `crate::MAX_ATTACH_BYTES` from `util.rs:107`. The only
difference is `/add` rejects overlarge files while `@file` truncates — a
deliberate UX choice documented at `dispatch.rs:306-309`.

## 10. Post-edit `FileChange` notes formatting — DRY ✅

**`tools/write.rs:94-97`** and **`tools/edit.rs:216-219`** — identical 4-line
block:

```rust
let mut warn = fc.notes.join("\n");
if !warn.is_empty() {
    warn.insert(0, '\n');
}
```

Both tools call `apply_file_change`, receive `FileChange { notes, .. }`, and
format the notes identically.

**Fixed**: `FileChange::formatted_notes()` in `mutation.rs`; both copies now
call it.

## 11. `create_dir_all` + `with_context` — DRY ✅

**`tools/write.rs:86-89`**, **`tools/fileops.rs:99-102`** (MoveTool),
**`tools/fileops.rs:410-413`** (CopyTool) — three identical instances:

```rust
if let Some(parent) = to.parent() {
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating {}", parent.display()))?;
}
```

**Fixed**: `ensure_parent_dir(path)` in `tools/mod.rs`, same context message;
all three sites call it.

## 12. Secret-file write/edit guards — DAMP ✅

**`tools/write.rs:45-51`** and **`tools/edit.rs:105-111`** share identical
`secret_file_reason(canonicalize_nearest(path))` → `bail!(…)` structure, but
each message is tailored ("refusing to write…" vs "refusing to edit…") and
meaningful to the model. `fileops.rs:384-388` has a different message again
("copying it would place its contents…"). The read-side `guard_secret_read` is
already a shared helper; the write-side variation is legitimately DAMP.

## 13. `tokio::fs::try_exists(…).await.unwrap_or(false)` — DRY ✅

8 occurrences across `tools/write.rs` and `tools/fileops.rs`.

**Fixed**: `path_exists(path)` in `tools/mod.rs`; all 8 sites converted.

## 14. `"(no matches)"` string literal — DRY ✅

5 occurrences in `tools/find.rs` and `tools/grep.rs`.

**Fixed**: `const NO_MATCHES` in `tools/mod.rs`; all 5 production sites use it.
Test assertions keep the bare literal — a test pinning the exact output string
is legitimate.

## 15. `ignore::WalkBuilder` patterns — DAMP ✅

4 files build `ignore::WalkBuilder`: `find.rs`, `grep.rs`, `tree.rs`,
`replace.rs`. `grep.rs` already extracted its own `ignore_walker` helper;
`find.rs` has an inline copy differing only in `max_depth` and `parents`.
`tree.rs` and `replace.rs` use intentionally different configurations.

## 16. `strip_prefix(&ctx.cwd).unwrap_or(&path).display()` — DRY ✅

`tools/replace.rs` (×3, one of them producing an owned `String`) and
`tools/lsp_nav.rs` — the same relative-path display pattern.

**Fixed**: `rel_display(path, cwd) -> std::path::Display<'_>` in `tools/mod.rs`;
all four sites converted. `strip_prefix` uses that yield a `&Path` rather than a
`Display` are a different pattern and stay as they are.

---

## Summary

| #   | Concern                            | Verdict |
| --- | ---------------------------------- | ------- |
| 1   | Secret-file detection              | DRY ✅  |
| 2   | Path helpers                       | DRY ✅  |
| 3   | Slash-command dispatch             | DAMP ✅ |
| 4   | AgentConfig test construction      | DRY ✅  |
| 4a  | `fn r()` ModelRef parser (×4)      | DRY ✅  |
| 4b  | `fn spec()` ModelSpec parser (×3)  | DRY ✅  |
| 4c  | `ProviderConfig` no Default (×25)  | DRY ✅  |
| 4d  | `SubagentProfile` no Default (×11) | DRY ✅  |
| 4e  | `cfg()`/`cfg_with()` (×2 copies)   | DRY ✅  |
| 4f  | Inline `AgentConfig { … }` (×83)   | DRY ✅  |
| 5   | Session layering                   | DRY ✅  |
| 6   | Project-dir walk                   | DAMP ✅ |
| 7   | CommandHost impls                  | DAMP ✅ |
| 8   | TUI selector draw functions        | DRY ✅  |
| 9   | File-attach flows                  | DRY ✅  |
| 10  | Post-edit notes formatting         | DRY ✅  |
| 11  | `create_dir_all` + context         | DRY ✅  |
| 12  | Secret-file write/edit guards      | DAMP ✅ |
| 13  | `try_exists` scattered (×8)        | DRY ✅  |
| 14  | `"(no matches)"` literal (×5)      | DRY ✅  |
| 15  | `ignore::WalkBuilder`              | DAMP ✅ |
| 16  | `strip_prefix` display (×3)        | DRY ✅  |

**All actionable items are closed.** #4a–#4d landed in `56b76ab`; #4e/#4f, #10,
#11, #13, #14 and #16 landed after that. What remains in this document is the
DAMP verdicts and the rationale for what was deliberately left duplicated.

The biggest item — the six TUI selector draw functions (#8) — is done
(`aa09f5b`).

---

Verdict: **The codebase is well-factored.** Most duplication is intentional
(DAMP) with documented rationale. The WET spots were small and mechanical, and
are now fixed — no architectural rot.
