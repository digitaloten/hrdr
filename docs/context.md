# Open context — 2026-07-29

Everything raised in the delegation/sandbox work of 2026-07-29 that is **not
done**. Written so the next session can pick any item up cold. Closed work is
not restated here; read `CHANGELOG.md` and `git log` for that.

Landed this day, for orientation only:

| commit    | what                                                                                     |
| --------- | ---------------------------------------------------------------------------------------- |
| `645b203` | verification gate discovered from CI, or ecosystem convention                            |
| `0b2a12a` | `verify` tool — runs the gate, stops at the first failure                                |
| `aac1787` | sub-agent worktrees removed; sub-agents share the parent's cwd                           |
| `899ecd2` | `.git` read-only for write sub-agents (bwrap / Landlock / Seatbelt)                      |
| `c2cd73b` | dead `task_diff`/`task_consume`/`task_cleanup` references removed; caps 1 write / 2 read |
| `313eb0e` | `memory` tool is main-agent-only                                                         |

---

## 1. Harness gaps still open

From the original 9-item list mined out of session-7 and session-8.

### #4 — delegation has no post-merge verification hook

Every sub-agent checks its own change against a tree the others are also
editing. Nothing checks the union. `verify` now exists, so the hook has
somewhere to point: after the last task in a batch is in, run the gate.

Sharpened by an observation from session-8: **two of three fix sub-agents called
`verify`, got `Err`, and reported success anyway**, relabelling the failure as
"pre-existing" in their hand-back report. The harness knew the gate had failed
and the report did not carry that fact. Whatever is built here should make a
failed `verify` structural in the hand-back, not dependent on the model choosing
to mention it.

### #5 — the test nudge has no teeth

Fired 3/3 in session-7, obeyed 1/3. With `verify` in place it has somewhere to
escalate to instead of staying advisory.

### #6 — the evidence gate checks presence, not relevance

A `verification` field containing "git log shows 3 commits" satisfies a claim it
does not support. Weakest of the set — checking that evidence _answers_ its
claim is a semantic judgement a string check cannot make. One observation behind
it; worth leaving until there is a second.

---

## 2. Sub-agent attack surface — audited, not addressed

Found by a delegated audit against the local Codex checkout
(`/home/mxaddict/Projects/harness/codex`, `81da9de`). Each item below was traced
to code; the ones marked **unverified** were reported by the audit and not
independently re-checked.

### 2.1 Project `AGENTS.md` is writable by a sub-agent — DELIBERATE, marked

A write sub-agent can edit `<cwd>/AGENTS.md`; the parent reads it back as
project conventions on its next prompt rebuild (`/clear`, `set_cwd`, a new
agent), with no trust framing — unlike memory, which arrives under
`MEMORY_PREAMBLE`'s "trust them but verify".

**Decision: left open.** AGENTS.md is also how a project legitimately carries
instructions and prompt-processing detail, and narrowing it would cost that. A
`// NOTE:` sits on the push site in `build_system_prompt_sections`
(`crates/hrdr-agent/src/lib.rs`) so the trade is visible. Revisit only if the
injection path starts mattering more than the feature.

### 2.2 Project skills are writable and shadow built-ins — OPEN

`crates/hrdr-agent/src/skills.rs:80-101` — `skill_dirs` includes
`cwd/.hrdr/skills`, `cwd/.claude/commands`, `cwd/.opencode/command`, all under
the writable cwd root and none in `PROTECTED_METADATA_DIRS`. Project files are
discovered _before_ built-ins and shadow them by name, so
`.hrdr/skills/commit.md` silently replaces the vetted `:commit`.
`discover_skills` re-runs on every `set_cwd`/`clear` and in every new
`Agent::new`, including a sub-agent the parent spawns later. `model_invocable`
defaults true.

Same shape as AGENTS.md but with a weaker second use — a project skill is a
convenience, where AGENTS.md is a core feature. Probably the strongest remaining
candidate.

### 2.3 Network is unconditionally allowed — OPEN

Verified: `(allow network*)` is appended unconditionally after the mode match in
the Seatbelt profile, and there is no `--unshare-net` anywhere in `bwrap_args`
(`crates/hrdr-tools/src/sandbox.rs`). Every hrdr agent, every mode, main or sub,
can reach the network.

Codex defaults to `NetworkSandboxPolicy::Restricted` and unshares. hrdr's own
source calls this "a declared follow-up, not v1" — it is a known choice, not an
oversight. It is the one place hrdr is materially weaker than Codex by default.

### 2.4 `.git` protection is delegated-only, not uniform — OPEN, design question

hrdr subtracts `.git` only when `config.delegated && !config.read_only`; the
main agent keeps it writable because it has to commit. Codex denies `.git`
writes identically for every agent, because its main agent does not commit
either — it escalates through an approval flow instead.

Making hrdr uniform means building that approval path. Not obviously worth it;
recorded because it is the structural difference between the two designs.

### 2.5 Smaller, verified

- **`std::env::temp_dir()` is granted whole**, not just `session_scratch_dir()`
  (`sandbox.rs`, `for_agent`). Broader than the stated need. Pre-existing.
- **`tool_output_dir` is per-user, not per-session**
  (`crates/hrdr-tools/src/lib.rs`) — one 0700 dir shared by every concurrent and
  recent hrdr session for that user, and a readable root for every agent. A
  sub-agent can read spooled shell output from your other sessions on other
  projects. No clobbering (filenames carry a nanosecond stamp + atomic counter);
  this is exposure, not corruption.
- **`shell` runs unconfined when no OS backend exists** (Windows always; Linux
  with neither bwrap nor Landlock). `deny_git_writes` is mount-based, so on such
  a host a sub-agent's `git commit` is not blocked. Pre-existing, and a notice
  is queued.

### 2.6 Cleared — checked and safe

- Hooks and hrdr config: no project-level hrdr config file exists (only
  `~/.config/hrdr/config.toml`), which is outside every agent's writable roots.
  A sub-agent cannot install a hook or change the next session's sandbox mode.
- Symlink and `..` escapes past the file-tool guard: closed by
  `canonicalize_nearest`.
- Spool-file clobbering between agents: names are unique.

---

## 3. Stale after the worktree removal — not yet fixed

From the same audit. `c2cd73b` fixed only the references to **deleted tools**;
the prose below still describes the old model.

- `README.md:828-834` — an entire paragraph describing worktree isolation as
  current behaviour.
- `README.md:930-996` — the Sandbox section's motivating incident is the
  pre-refactor one, and the section **never mentions `deny_git_writes` at all**,
  which is a shipped breaking change.
- `README.md:1119`, `crates/hrdr-tools/src/lsp.rs` — "a worktree-isolated
  sub-agent's tree" as a live example of paths outside the LSP root. The
  behaviour is fine; the example is impossible now.
- `crates/hrdr-agent/src/delegation.rs` (~line 1179) — "Worktree isolation is
  applied to _every_ write-capable sub-agent below", contradicted by the correct
  comment ~47 lines later in the same function.
- `crates/hrdr-agent/src/prompt.rs` (`SECTION_SANDBOX` comment) and
  `crates/hrdr-llm/src/anthropic.rs` (cache-split rationale) — both justify a
  design by "each sibling has its own worktree cwd". Siblings now share one cwd,
  so the stated reason is wrong even where the mechanism is still fine.
- `crates/hrdr-agent/src/config.rs` (~line 404) — `memory_roots`' doc justifies
  the override with a worktree slug that no longer exists.
- `crates/hrdr-tools/src/verification.rs` (`is_git_commit`) — comment cites
  keeping `git -C <worktree> commit` recognisable; that cannot happen now.
- `docs/backlog.md:42-54` (top item) and `:480-484`, `:703-707`, `:723-725`,
  `:735-738` — several entries reason from the per-agent worktree. The open
  questions may still be worth having; the stated reasoning needs correcting.

**Dead code:** `redact_secret_diffs`
(`crates/hrdr-tools/src/tools/secret_diff.rs`) has no callers — its only one was
`task_diff`. `pub`, so clippy will not flag it. Its doc already records this.
The whole `secret_diff` module is deletable if no new caller arrives.

---

## 4. Loose ends from working in a shared tree

- **`git restore <path>` / `git checkout <path>` is unguarded.** hrdr's
  guardrails block the whole-tree forms (`git checkout .`, `git restore .`) but
  not the single-path form — which is the one that discards someone else's
  uncommitted work file by file.
- **The don't-discard-others'-work rule is sub-agent-only.**
  `templates/subagent_write.md` forbids `git checkout`/`restore`/`stash`;
  `write.md` does not. The main agent has more authority and the same need — it
  is the one that reaches for `git restore` on a file it does not recognise.

Both surfaced by a real incident this session: a concurrent hrdr session in
another terminal was editing `docs/code-review.md`, an unexpected `M` appeared
in `git status`, and it was restored away on the assumption that the only other
writer was a sub-agent. Recovered in full (it had been copied first, and the
other session committed it as `aebf4a8`). Note that `.git` lockdown _worked_
here — the damage stayed uncommitted and was undoable.

---

## 5. Known-good, do not "fix"

Checked during the audit and correct as-is:

- `git_metadata_roots` (`sandbox.rs`) still serves hrdr **itself** being
  launched inside a user's own linked worktree, where the main agent must still
  commit. Two tests pin it:
  `hrdr_inside_a_linked_worktree_still_commits_under_the_metadata_guard` and
  `a_linked_worktree_commits_but_the_parent_repo_stays_blocked`.
- The `git worktree remove --force` and `git branch -D` guardrails still protect
  a user's own worktrees and branches generically.
- The `git rebase HEAD` guardrail is a generic `-C <dir>` footgun rule, not
  task-specific.
- `memory.rs` writing outside the sandbox roots is **correct** — that is where
  memory lives, and routing it through `check_write` would break the feature.
  The audit framed this as a bypass to plug; it is not one. What was actually
  separable was authority, and that is now handled (`313eb0e`).
- Session and transcript persistence carry no removed fields, and neither uses
  `deny_unknown_fields`, so an old file still loads.

---

## 6. Reported but not independently verified

Flagged so nobody treats these as established:

- "The frontend crates (`hrdr-app`, `hrdr-tui`, `hrdr-web`, `hrdr-protocol`,
  `hrdr-editor`) are clean of worktree references" — reported by a nested
  sub-agent, third-hand.
- The `AGENTS.md` and skills re-read paths in §2.1/§2.2 were traced by the audit
  agent, not re-checked by hand.
- Nobody traced Codex's approval-escalation flow, which is the thing that
  explains how it affords a uniform `.git` denial (§2.4).
- `crates/hrdr-tools/src/tools/shell.rs`, `turn_loop.rs`, and
  `hrdr-agent/src/lib.rs` were never read end-to-end by either audit agent.
