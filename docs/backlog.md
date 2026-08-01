# hrdr backlog

**One file.** Merged 2026-07-27 from `deferred-improvements.md`, `compare.md`
(the four-harness comparison) and `security-audit.md`, which are deleted — read
`git log` for what they said before this.

**Every claim below was re-verified against the tree at `8c76cdb`** before it
was carried over. What did not survive verification is either corrected in place
or listed under
[Corrections made during the merge](#corrections-made-during-the-merge). Items
that had shipped are in [Record](#record-closed-efforts), not here.

**Pruned 2026-07-27**: a fifteen-commit pass (`0fae706`..`36a7f2b`) cleared
everything that was actionable without a decision, and those entries are
**deleted, not annotated** — `git log` is the history. What the pass taught, and
the two decisions it surfaced, are under
[Cleared in the 2026-07-27 pass](#cleared-in-the-2026-07-27-pass).

**Pruned again 2026-07-30**, by the sandbox redesign (`5c9f675`..`c114a6a`). It
closed most of [Sandbox follow-ups](#sandbox-follow-ups) and the first two
top-of-list items, mostly by deleting the mechanism rather than finishing it —
see
[Cleared in the 2026-07-30 sandbox redesign](#cleared-in-the-2026-07-30-sandbox-redesign).
The same pass folded `docs/context.md` (a dated open-items file from 2026-07-29)
into this one and deleted it, so this is again the only backlog. Entries whose
subject no longer exists are annotated where the reasoning still teaches
something and deleted where it does not.

**Pruned again 2026-08-01**, by the full-codebase review pass
(`4e66a1c`..`2e3be29`, released v0.10.0). It closed all sixteen of its own
findings, so `docs/code-review.md` is deleted per the convention below; what it
left open is under [Review coverage still owed](#review-coverage-still-owed) and
[Known behaviour to revisit](#known-behaviour-to-revisit), and what it taught is
under
[Cleared in the 2026-08-01 review pass](#cleared-in-the-2026-08-01-review-pass).

**Pruned again 2026-08-02**, by the backend pass (`7e80605`..`9c3d012`). macOS
Seatbelt turned out to have been running on CI all along and is closed; Windows
gained an OS backend for `read` mode and is half closed, with the remaining half
recorded under [Sandbox follow-ups](#sandbox-follow-ups) as a decision rather
than an implementation. See
[Cleared in the 2026-08-02 backend pass](#cleared-in-the-2026-08-02-backend-pass).

Conventions:

- **Symbol names, not line numbers.** Line numbers rot — the old docs cited
  `hrdr-tools/src/lib.rs:965` for `Tool::description`, now at `:1155`, and half
  of `compare.md` cited a `system.j2` that no longer exists. Peer-harness
  citations keep their paths (clones are still at
  `~/Projects/harness/{codex,hermes-agent,opencode,pi}`).
- **Docs for finished work get deleted.** What survives a completed effort is
  only what still binds future work: those live in
  [Standing constraints](#standing-constraints).
- **Peer claims were not re-run.** The comparison was verified twice when it was
  written (2026-07-26, one sub-agent per harness, each given the same
  preliminary claims to confirm or refute). This merge re-verified the **hrdr
  side** of every finding — the half that decides whether an item is still open.

---

## Top of the list

The five that were here are all shipped — see
[Cleared in the 2026-07-27 pass](#cleared-in-the-2026-07-27-pass). Items 1 and 2
closed on 2026-07-30 with the sandbox redesign, both by **deletion**: the
file-tool metadata guard is gone (it refused the honest path while `shell`
walked round it), so there is no list to extend; and there is no `git` tool, so
nothing runs outside `sandboxed_shell_command`. What is left needs a decision,
not work.

1. **Full `AGENTS.md` injection scanning, if wanted.** The cheap 80% shipped
   (`2bee4bf`): a skipped file surfaces as a notice naming path and size, and
   the block header states the files come from the project tree and cannot
   override the cardinal rules or the user. A jailed agent now skips the
   project's `AGENTS.md` and skills entirely, which covers the untrusted-repo
   case. What is still open is hermes' `_scan_context_content` — blocking a file
   with a `[BLOCKED: …]` placeholder on an injection heuristic. Deliberately
   deferred: a regex scanner over project docs false-positives on exactly the
   repos a coding agent gets pointed at (security tooling, shell-hardening
   guides, this file), and hermes needed three scopes to make it tolerable.
   **Wants evidence of a real attempt first.**
2. **Delegation has no post-merge verification hook.** Every sub-agent checks
   its own change against a tree the others are also editing; nothing checks the
   union. `verify` exists, so the hook has somewhere to point: after the last
   task in a batch, run the gate. Sharpened by a real observation — two of three
   fix sub-agents called `verify`, got `Err`, and reported success anyway — and
   now _forced_, because `task_transcript` is gone: the fix has to be structural
   in the hand-back rather than "the parent can go read what happened".
3. **The test nudge has no teeth.** Fired 3/3 in one session, obeyed 1/3. With
   `verify` in place it has somewhere to escalate to instead of staying
   advisory.
4. **The evidence gate checks presence, not relevance.** A `verification` field
   containing "git log shows 3 commits" satisfies a claim it does not support.
   Weakest of the set — whether evidence _answers_ its claim is a semantic
   judgement a string check cannot make. One observation behind it; worth
   leaving until there is a second.
5. **`git restore <path>` / `git checkout <path>` is unguarded, and the
   don't-discard-others'-work rule is sub-agent-only.** The guardrails block the
   whole-tree forms (`git checkout .`, `git restore .`) but not the single-path
   form — the one that discards someone else's uncommitted work file by file.
   And `templates/subagent_write.md` forbids `git checkout`/`restore`/`stash`
   outright while the main agent's copy — `write_main.md` since the 2026-08-01
   split, not `write.md` — only tells it to look first, though the main agent
   has more authority and the same need. Both surfaced by a real incident: a
   concurrent hrdr session was editing a file, an unexpected `M` appeared in
   `git status`, and it was restored away on the assumption that the only other
   writer was a sub-agent. Recovered because the other session had committed it.

---

## Peer-comparison findings still open

Grouped by theme. Cross-harness agreement is noted because two harnesses
reaching the same conclusion independently is the strongest signal in the
comparison.

### Where hrdr is the outlier

| Thing                                | codex      | hermes            | opencode   | pi  | hrdr                    |
| ------------------------------------ | ---------- | ----------------- | ---------- | --- | ----------------------- |
| Per-model prompt/behaviour variation | ✅ catalog | ✅ substring list | ✅ 9 files | ✗   | **✗** (verified: none)  |
| Runtime-composed tool descriptions   | ✅         | ✅                | ✅         | ✅  | **✗** (`&'static str`)  |
| Shell commands parsed, not regexed   | ✅         | —                 | ✅         | ✗   | **✗** (15 regexes)      |
| Ask-the-user affordance              | ✅         | ✅                | ✅         | ✗   | **✗** (verified absent) |
| Repeated-call / loop detection       | —          | —                 | ✅         | ✗   | ✅ shipped 2026-07-27   |
| Deferred tool loading                | ✅         | ✅                | —          | ✗   | **✗** (~31 defs sent)   |
| Model-invocable skills               | ✅         | ✅                | ✅         | ✅  | ✅ shipped 2026-07-27   |

Rows 1 and 2 are three- and four-of-four against us — the ones where being the
outlier is most likely to be a mistake rather than a stance.

- **Parse shell commands instead of regex-matching them.** Codex:
  `shell-command/src/parse_command.rs` (82 KB) plus a Starlark `execpolicy` DSL.
  Opencode: tree-sitter (bash + PowerShell) walking command nodes, deriving
  out-of-project path arguments and an **arity-truncated** "always" prefix
  (`permission/arity.ts`) so `git checkout main -b foo` generalises to
  `git checkout *`. hrdr matches 15 hand-written regexes against the raw command
  line, with the cost admitted in-tree (_"the regex crate has no lookaround —
  `--force` must not also match `--force-with-lease`"_, plus `[^&|;]*` on every
  rule to avoid crossing a separator: a hand-rolled tokeniser spelled in regex).
  _Cost:_ a week. _Counter-argument that keeps this off the top of the list:_
  hrdr's guardrails are deliberately a **small deny list on an autonomous
  agent**, not an approval system — a parse buys precision, not a new
  capability. **Worth it only if hrdr adopts a path-scoped restriction** (`.git`
  protection's shell half, or out-of-project asks).
- **Runtime-composed tool descriptions.** Verified: `Tool::description()`
  returns `&'static str`. Three live consequences: `task`'s description cannot
  list the actually-configured models; the guardrail-message duplication (below)
  could be eliminated by construction; and the sandbox's writable roots cannot
  appear in the descriptions of the tools they constrain, which is why the
  positive declaration had to go into `SECTION_SANDBOX` instead. All four peers
  build descriptions at runtime. _Caveat:_ `&'static str` buys testability and
  cache stability — interpolating runtime values changes the schema between
  turns and invalidates the tools cache block. If the only real case is "list
  configured models in `task`", `parameters()` is already a runtime
  `serde_json::Value`: cheaper and cache-equivalent.
- **Per-model behaviour as data, not code.** Codex ships a remote catalog with
  per-model `base_instructions`, `instructions_template`, personality variables
  and `ModelMessages{approvals, auto_review, permissions}`; hermes ships an
  editable substring list plus per-family blocks carrying a dated provenance
  trail (_"Observed on DeepSeek v4-flash… returned fabricated listings"_,
  _"adapted from OpenCode's gemini.txt"_, _"Ported from cline/cline#11514"_);
  opencode selects one of nine prompt files by model-id substring. **hrdr is the
  only one of four with zero per-model variation** (verified: no model-string
  branching in `prompt.rs`). Both vendors that post-train also send **less**
  prompt to their own models — codex's guidance shrinks from gpt-5.2 to gpt-5.6,
  hermes pointedly omits Claude from its enforcement list. If hrdr ever ships
  per-model prompts: **wire them with a test that every file in the directory is
  reachable** — codex has 5 dead prompt files of 6, opencode 1 of 9 plus two
  more.
- **Deferred tool loading behind a search bridge.** Codex:
  `ToolExposure::{Direct, Deferred, DirectModelOnly, Hidden}` + BM25 search over
  withheld metadata, MCP and V1 sub-agents default to `Deferred`. Hermes:
  `tool_search`/`tool_describe`/`tool_call` bridges, **core tools never
  deferred** (_"Always-load means always-load. No exceptions."_), and the gate
  is a **no-op unless deferrable tools would exceed ~10% of the context
  window**. Both exempt core tools. hrdr sends every def every request (~31 for
  a fully-featured main agent: 17 from `ToolRegistry::with_defaults`, the rest
  from `Agent::new`). _Decisive caveat:_ that is ~4-6k tokens, usually cached.
  **Do it if MCP tool counts get large, not now.**
- **Ask-the-user affordance.** Verified absent (`grep -rn "AskUser|ask_user"` →
  0). Three of four peers have one; hrdr's autonomy posture (headless runs,
  NDJSON, cost caps) is the reason it does not, which makes this a stance — but
  an unrecorded one until now.

### Editing and tool ergonomics

- **Fuzzy `old_string` matching that preserves unchanged lines.** hrdr already
  _detects_ the class and writes a good message (_"a near-match differing only
  in whitespace/indentation exists"_) but still **fails the call**; it has a
  CRLF retry (`is_crlf_dominant`) and no trailing-whitespace or quote retry,
  while `read` clips at `MAX_LINE`, so the model's view can differ from disk in
  exactly these ways. pi retries in a normalized space (NFKC, per-line
  `trimEnd`, smart quotes → ASCII, dash/space variants → plain) and — the clever
  part — `applyReplacementsPreservingUnchangedLines` widens each replacement to
  the lines it touches, rewrites only those, and copies every other line back
  byte-for-byte, with a duplicate-line alignment guard and a line-count
  assertion. _Caveat:_ fuzzy matching in an edit tool normalizes Unicode as a
  side effect of an unrelated change. **Cheapest useful subset:** trailing
  whitespace + quotes/dashes/spaces, no NFKC, no new dependency — and **report**
  when a fuzzy match was used (pi tracks `usedFuzzyMatch` and doesn't surface
  it).
- **Per-model argument tolerance.** pi repairs tool arguments per model —
  `prepareEditArguments` re-parses `edits` when it arrives as a JSON string,
  commented _"Some models (Opus 4.6, GLM-5.1) send edits as a JSON string
  instead of an array"_. hrdr's tolerance is `serde(alias)` on **path fields
  only** (`read.rs`, `edit.rs`, `fileops.rs`); the payload fields that cost most
  when they fail — `old_string`/`new_string`/`content`/`pattern` — have none.
- **Expose session/model metadata to shell commands.** Verified:
  `Shell::command` configures program + args only, nothing else. pi injects
  `PI_SESSION_ID`, `PI_SESSION_FILE`, `PI_PROVIDER`, `PI_MODEL`,
  `PI_REASONING_LEVEL` (deleting inherited copies first) and tells the model
  they exist. _Caveat:_ widens what leaks into every subprocess and its logs —
  pi's `exposeSessionEnvironment` toggle is the right shape, defaulting **off**.
- **Truncation caps are 10×/40× tighter than opencode's.** Verified
  `DEFAULT_MAX_OUTPUT = 5_120` / `DEFAULT_MAX_OUTPUT_LINES = 50` against
  opencode's 50 KB / 2000 lines. Both spill to a re-readable file so nothing is
  lost, but 50 lines means a `cargo test` failure or a 60-line diff costs a
  second round trip opencode wouldn't pay. **Not measured in hrdr's traces —
  worth one experiment**, not a change.

### Permissions, isolation, and state

**Instruction surfaces a repo can write, outside `jail`.** From the 2026-07-29
sub-agent attack-surface audit (traced against codex `81da9de`). `jail` closed
both for the untrusted-repo case — it loads neither — but every other mode still
reads them, and that is a deliberate trade rather than an oversight:

- **Project `AGENTS.md` is writable by a write sub-agent** and read back as
  project conventions on the parent's next prompt rebuild (`/clear`, `set_cwd`,
  a new agent), **with no trust framing** — unlike memory, which arrives under
  `MEMORY_PREAMBLE`'s "trust them but verify". Left open on purpose: `AGENTS.md`
  is also how a project legitimately carries instructions, and narrowing it
  costs that. A `// NOTE:` sits on the push site in
  `build_system_prompt_sections`. The cheap half is a trust frame comparable to
  memory's — that is the actual open item, not the write.
- **Project skills shadow built-ins by name.** `skill_dirs` includes
  `cwd/.hrdr/skills`, `cwd/.claude/commands`, `cwd/.opencode/command`, all under
  the writable cwd; project files are discovered _before_ built-ins and win,
  with `model_invocable` defaulting true, so `.hrdr/skills/commit.md` silently
  replaces the vetted `:commit`. Re-runs on every `set_cwd`/`clear` and in every
  new `Agent::new`. Same shape as `AGENTS.md` but with a **weaker second use** —
  a project skill is a convenience where `AGENTS.md` is a core feature — which
  makes it the stronger candidate of the two if either is closed.

**Two smaller confinement gaps, verified and unchanged:**

- **`std::env::temp_dir()` is granted whole** in `write` mode, not just
  `session_scratch_dir()`. Broader than the stated need, and pre-existing.
- **`shell` runs unconfined where no OS backend exists** — Windows always, Linux
  without Landlock. The file tools stay guarded and `NO_OS_SANDBOX_NOTICE`
  fires, so it is admitted rather than silent.

- **One permission evaluator instead of four unrelated mechanisms.** Opencode
  has a single primitive — an ordered `{permission, pattern, action}` list
  evaluated `findLast`-wins with globbing on both fields — and gets plan mode,
  sub-agent restriction, read-only agents, out-of-project confinement, `.env`
  gating, loop detection and headless mode out of it as **data**. hrdr now has
  **four** mechanisms that don't compose: `guardrails` (shell only, terminal
  `bail!`), `read_only` (registry name filter), per-tool secret-file `bail!`s
  (deliberately not shared), and — since 2026-07-27 — the sandbox path guard.
  Adding a fifth restriction means writing a fifth mechanism. _Caveat:_
  opencode's three actions are `allow|ask|deny`; hrdr's autonomy posture
  collapses that to `allow|deny`, and a two-valued evaluator over globs is worth
  much less. **Honest MVP:** keep the mechanisms, express _what they check_ as
  one rule list. A refactor, not a feature — wait until hrdr wants a second
  path-scoped restriction (`.git` protection is exactly that trigger).
- **Out-of-project access as an observable event, not a removed capability.**
  Opencode's `external-directory.ts` raises an `external_directory` permission
  keyed on the containing **directory glob**, with an allow-list pre-seeded from
  the overflow dir, temp, skill dirs and reference dirs, called from `read`,
  `edit`, `write`, `glob`, `grep`, `lsp`, `shell` and `apply_patch`. hrdr
  removed cwd confinement outright (`f0d903a`) and the sandbox has since closed
  the **write** half — reads stay broad by decision, so this is now specifically
  about _read_ visibility. _Caveat:_ hrdr's write sub-agents legitimately read
  the parent repo (shared `Cargo.lock`, `~/.cargo`, `/usr/lib`), so the
  allow-list may need to be large enough that the signal isn't worth it.
- **Per-call permission escalation.** Still open, and note that hrdr **built and
  then deleted** an approval gate on 2026-07-30 — the mechanism existed for
  widening the _sandbox_, whose motivating failure (bwrap's user namespace
  breaking ssh) was removed rather than routed around. This item is the
  different, narrower question of overriding a **guardrail**, and it survives
  that deletion. Anyone rebuilding a gate should read why the last one went: it
  only ever helped when a human was present to answer, and a human who is
  present can run the command themselves (`!command`). Codex's shell schemas
  carry `sandbox_permissions`, `justification`, `prefix_rule` and
  `additional_permissions`, with outcomes `ApprovedForSession` and
  `ApprovedExecpolicyAmendment` (the latter appending a durable rule to
  `$CODEX_HOME/rules/default.rules`). hrdr's guardrails are terminal `bail!`
  with no approval path, so when a guardrail is wrong — `git add -A` in a repo
  the user genuinely wants fully staged — the agent can only give up or work
  around the regex. _Caveat:_ interactive approval cuts against hrdr's posture.
  Smaller defensible version: keep guardrails terminal, allow an explicit
  `override_guardrail: "<rule>"` argument that logs loudly and is refused unless
  config opted that rule into overridable.
- **A channel to tell the model that something changed.** hrdr has no way to say
  "a tool appeared, the cwd moved, memory was written" — the only path is a
  prompt rewrite, and `refresh_system` fires on MCP connect, `clear()` and
  `set_cwd()` only. Codex decomposes mutable state into ten typed sections and
  emits a **developer-role fragment containing only the delta** per sampling
  step, byte-budgeted, in stable XML markers, advanced by RFC 7386 merge
  patches. Hermes goes the opposite way (freeze, rebuild at compaction) and
  **given hrdr's small volatile set, hermes' posture is the cheaper correct
  answer** — the memory half of it already shipped. If the honest list is
  "memory changed, AGENTS.md changed", one appended `# Context update` developer
  message gets most of the value. Do the cheap version first.
- **Memory: usage tracking and an external-drift guard.** Two halves, both open.
  Codex tracks whether memories are _used_ (citation blocks, plus parsing the
  model's shell commands for reads of memory paths) and feeds
  `usage_count`/`last_usage` into pruning — hrdr's `read`/`grep` calls under the
  memory dir are directly observable with no parsing needed. _Caveat:_ usage
  count is a bad proxy for memories whose value is preventing a mistake — hrdr's
  own `no-migration-pre-1.0` earns its keep by being _injected_, never read, so
  counting reads would prune the most valuable first. Separately, hermes'
  `_detect_external_drift` refuses a full-file rewrite when on-disk content
  wouldn't round-trip through the tool's own parser (manual edit, sibling
  session), backing up to `.bak.<ts>` instead of clobbering; hrdr has no
  equivalent (verified: no drift/round-trip/`.bak` logic in `memory.rs`). Both
  fold into the memory-drift item below.
- ~~**Gate LSP tool registration on the project having a matching server.**~~
  **Moot 2026-07-30:** `definition`/`references`/`rename` are deleted (2 calls
  in 9,350 — available and ignored), so there are no LSP tool schemas to gate.
  The diagnostics path, which is the valuable half, was never a tool and is
  unchanged.
- **Per-provider tool-JSON-schema rewriting — file, don't build.** Opencode
  rewrites every tool schema per model before the wire, with three quirks and a
  reason each: OpenAI/Azure sanitisation; Moonshot/Kimi strip every sibling key
  of a `$ref` (_"Moonshot expands `$ref` before validation and rejects sibling
  keywords"_) and collapse tuple-style `items`; Gemini converts integer enums to
  string enums. hrdr ships one schema shape to every provider (verified: no
  `sanitize`/`$ref`/`additionalProperties` handling in `hrdr-llm`) and targets a
  **wider** provider spread. _Caveat, strongest in this section:_ **there is no
  evidence hrdr is broken on any provider.** This is a known-good design for
  when a provider rejects a schema, not work.

### Observability

- **Session search.** Verified: sessions live at
  `sessions/<cwd-slug>/<name-slug>.json`, zstd-compressed once idle, with
  `list_sessions()` but no index — so cross-project recall means walking every
  slug directory and decompressing every archive. "What did we decide about the
  delegation retry backoff three weeks ago?" is unanswerable. Hermes: FTS5,
  three modes inferred from args, **zero LLM calls**. Two specifics worth
  copying if built: **exclude sub-agent sessions from results** (hermes hides
  `("subagent","tool")` sources; hrdr's on-disk sub-agent runs are the exact
  analog and would flood every query), and **demote rather than exclude**
  automated sources, because repetitive vocabulary dominates bare BM25 and
  starves interactive sessions. _Honest smaller version:_ grep the current
  project's slug directory, decompressing lazily. No FTS engine, most of the
  value.
- **Prompt introspection — leverage on every size claim in this file.** Verified
  absent (`grep -rn "prompt-size|context_breakdown"` → 0). Both the codex and
  hermes passes closed with the same admission: neither binary was instrumented,
  so **every size comparison in the old comparison doc was structural, not
  measured**, and hrdr's prompt had to be reconstructed in Python to be counted.
  Hermes ships both halves: a live per-category budget (system prompt, tool
  definitions, rules, skills index, **MCP separately from builtin schemas**,
  sub-agent definitions, memory, conversation) preferring the provider's
  measured `last_prompt_tokens` over its own estimate; and `hermes prompt-size`,
  which builds a real offline agent with dummy credentials so the numbers match
  the wire. hrdr has the estimators and a context gauge but no category
  attribution and no way to dump the assembled prompt. _Caveat:_ char/4 invites
  false precision — report bytes and labelled estimates, resist a
  percentage-of-window pie chart.

---

## Sandbox follow-ups

**Most of this list closed on 2026-07-30 with the sandbox redesign** (nine
slices, `5c9f675`..`c114a6a`; `docs/sandbox-redesign.md` is the decision
record). What closed and how:

- **No network axis** — closed by _deletion_, not by building it. No mode
  confines the network in any direction now. In the mode that mattered it was
  never a boundary (a delegated agent reports to a parent that has a network, so
  injected text propagates through the report), and it was dead weight in
  `jail`, whose tool set holds nothing that can open a socket. Codex's
  MITM-proxy route is still the reference if it ever returns — as a designed
  feature with a threat model, not a vestigial field.
- **Bundled `bwrap`** — moot: bwrap is deleted. Linux confines with Landlock,
  macOS with Seatbelt, and there is nothing to install. The kernel floor moved
  to 5.13 in exchange.
- **Curated read allow-list for `write` mode** — still the honest gap, but the
  shape changed. A `shell` command in `write` mode can still
  `cat ~/.ssh/id_rsa`. What did land is narrower and real: `shell` output now
  drops lines naming a credential file AND redacts the hunk body of a diff
  touching one (`DiffRedactor`), so the _accidental_ leak — a broad `rg`, a
  `git diff` of `.env` — no longer reaches the transcript. Deliberate
  exfiltration is untouched and admits it.
- **No shell-command pre-flight** — still open, and cheaper than it was: the
  EROFS note now names the remedy (`sandbox_writable_roots`,
  `--sandbox-writable-root`, `!command`), which was most of what a pre-flight
  would have said.
- **The `git` tool is outside the boundary** — moot: there is no `git` tool, and
  `shell` is wrapped.
- **macOS Seatbelt has never run** — CLOSED 2026-08-02, see
  [Cleared in the 2026-08-02 backend pass](#cleared-in-the-2026-08-02-backend-pass).
  It had been running on every macOS CI job all along; the tests could not say
  so.
- **Windows has no OS layer** — HALF CLOSED 2026-08-02. `read` mode is confined
  by Mandatory Integrity Control; `write` mode is not, and still takes
  `NO_OS_SANDBOX_NOTICE`. The remaining half is the next item.
- **Windows `write` mode has no OS confinement**, and closing it costs something
  the other two backends do not. A Low-integrity child can only write to objects
  _labelled_ Low, so each writable root would have to be relabelled
  (`icacls <root> /setintegritylevel Low`) and reverted after. Two consequences,
  both real: the label **persists** if hrdr dies between spawn and revert, and
  while it is set **any other Low-integrity process can write there** — a
  sandboxed browser renderer, say. Landlock and Seatbelt leave no trace at all.
  Needs a decision before it touches a user's repository, not just an
  implementation.

Opened by the redesign:

- **The sub-agent `<stem>.json` snapshot is written and never read.** It existed
  for `task_revive`, which is gone. Its only reader now is its own test
  (`background_subagent_persists_its_own_session_state`). Deleting it removes a
  per-turn-boundary write; keeping it keeps a crash-durable record of what a
  sub-agent actually said. Decide, rather than leaving a file nobody loads — and
  note that the panes read the sibling `.jsonl`, not this.
- **`--sandbox jail` cannot apply to a write-capable session**, so it floors at
  `write` and emits a notice pointing at the `prisoner` agent. That is honest
  but it is not what the word means. A jailed _main_ agent is coherent (five
  tools, no shell — an audit session), and the write floor exists for sub-agents
  that must write. Worth revisiting as "session jail means jail, and a
  write-capable sub-agent under it is the thing that floors".
- **`jail` loses `git log` on the audited repo** — real provenance value, and
  the accepted cost of having no subprocess. Argues for a narrow read-only git
  capability later, not a general shell.
- **Package caches are writable and shared across projects.** Content-addressed
  and integrity-checked, which is what makes it acceptable, but poisoning
  `~/.cargo/registry` affects builds the user later runs by hand. Revisit if a
  per-project cache overlay ever gets cheap.

**Where codex's sandbox has moved past hrdr's** (context, not separate items):
its policy is a precedence-ordered entry list of
`{path, access, missing_path_behavior}` with `Read|Write|Deny`,
most-specific-wins and **deny beats write beats read**, paths as globs or
symbolic tokens
(`Root, Minimal, ProjectRoots{subpath}, Tmpdir, SlashTmp, Unknown` — `Unknown`
retained so newer config degrades to warn-and-ignore); named permission profiles
with `extends` inheritance versus hrdr's flat four-value `SandboxMode`; and a
mode hrdr has no slot for, `PermissionProfile::External { network }` —
"filesystem isolation is enforced by an external caller", which hrdr's
`SandboxMode::None` conflates with "unsandboxed".

---

## Tooling / agent capability

- **Memory drift detection.** A periodic prune/verify pass over the `memory`
  store — check each `<slug>.md` still has a `MEMORY.md` pointer (and vice
  versa) and flag/prune stale or contradicted memories. Cheap because
  `rebuild_index` regenerates the pointer index on every mutation (verified), so
  files and index cannot drift **structurally**; what is missing is semantic
  staleness, plus the external-drift guard and usage-tracking halves recorded
  above.
- **Profile-faithful revive** — the residual the above left. Only _capability_
  is persisted, not the profile: a revived run does not get its original persona
  (`agent_prompt`) or explicit `tools:` allow-list back, because neither was
  ever persisted. Restoring them means persisting the profile name and
  re-resolving it at revive time, which is a larger change than the capability
  fix and a question about what "revive" means — the same run, or the same
  _agent_. **Needs a call.**
- **Skills follow-ups** (feature shipped 2026-07-27 — `hrdr-agent/src/skills.rs`
  plus `prompt::skills_section`). Left out on purpose: no `skill` usage signal
  (nothing records whether the model ever loads one, so there is no evidence for
  or against the listing's wording); no categories, unlike hermes'
  category→skills grouping, which only pays off past a few dozen skills; and a
  body still arrives as one tool result, so a procedure over
  `SKILL_OUTPUT_MAX_BYTES` (24 KiB) spills to a file the model must read.

---

## Consistency / robustness

- **Guardrail rules live in two places** — and now a test says so when they
  drift. The rule set is still encoded both in `hrdr-tools/src/guardrails.rs`
  (mechanical enforcement) and in the prompt fragments (guidance telling the
  model not to attempt them), which is deliberate: the prompt phrasing is more
  nuanced than the terse guardrail message, so it is written, not derived. What
  shipped 2026-07-27 (`37edba4`) is the drift check —
  `every_guardrail_is_explained_in_the_prompt` pairs each rule with the token(s)
  the rendered write+shell prompt must contain, positionally, so a 16th
  guardrail fails the test until whoever added it writes the guidance too (or
  records that the rule needs none). Two notes from building it: the guidance is
  spread across `base.md` (pipe-to-shell) and `shell.md` (interactive git), not
  only `write.md`; and the two pipe rules share one identical message string, so
  the table is keyed positionally rather than by message. Eliminating the
  duplication itself still wants runtime-composed descriptions.
- **The pipe-to-shell guardrail's recovery text assumes POSIX.** Verified
  nuance: there are **two** pipe rules — a POSIX `curl|wget … | sh` one and a
  case-insensitive PowerShell-shaped `iwr|iex` one — so coverage is not
  POSIX-only. What is POSIX-only is the recovery example
  (`curl -fsSL <url> -o <tmp>/script.sh`), built in `guardrails.rs` outside the
  `Shell` seam because `default_guardrails()` has no shell in scope. Correct
  today (the shell is always bash/sh); it is the one place a new dialect would
  have to be threaded through by hand.
- **hrdr sets no Windows ACL on any file it writes.** The residual of the
  Windows-drift audit pass, which **ran** and landed three fixes (`8e5bc9d`):
  the credential `sync_all` gated on unix though portable, `atomic_write`'s
  symlink guard likewise, and owner-only file creation re-decided at four sites.
  All ~130 `cfg` gates were classified; ~25 are `#[cfg(unix)]` on _tests_
  (needing bash, python3 or symlinks) and are not findings, and `proc.rs`, the
  pid-liveness probes and `prompt.rs`'s package-manager names are deliberate and
  documented. The guarantee left on Windows is the containing per-user
  directory's inherited default, stated once on `hrdr_llm::owner_only_options`.
  Per-user ACLs need a new dependency in `hrdr-llm` and are a deliberate
  non-goal until someone runs hrdr on Windows in anger.
- **`O_NOFOLLOW` covers only the final path component.** A symlinked _parent_
  directory is still traversed on the wire-log open, and there is no Windows
  equivalent at all, so callers relying on it keep their own preflight check.
  Recorded on `owner_only_options_no_follow`; closing it properly means
  resolving the whole path under a directory handle.

---

## Web UI follow-ups

Post-parity: the implemented spec's deferred list plus review residuals. All
verified still open.

- **Session-browser UI** — list + open other sessions from the client; the
  server gains a `list_sessions()`-backed message pair.
- **Syntax highlighting in code blocks** (syntect-wasm or highlight.js interop)
  — verified: no highlighting in `hrdr-ui`.
- **Modal pickers** (model/effort/theme/session) as bottom sheets over the
  `begin_*_selector` hooks.
- **v2: attach to a live TUI session** — blocked on making event-log compaction
  min-cursor-aware across readers: `PaneSet::sync` calls
  `live.compact(s.key, next)` after folding, so the log is effectively
  single-reader today.
- **Native desktop/mobile shell** — webview over embedded `hrdr-web`.
- **Read-only/observer auth mode.**
- **WebHost chrome posters** — verified: `WebHost`'s `CommandHost` impl
  overrides **neither** `identity_poster` nor `context_window_poster`, so an
  async `/model` switch updates chrome only via the agent's republish; and a
  failed autosave is silent (no web equivalent of the TUI's
  `record_session_save` notice).

---

## Test coverage gaps

- **Wire log on the native backends — unblocked, still unwritten.**
  `error_response` and `sse` records are emitted by `anthropic.rs`/`codex.rs`
  and nothing asserts them; `wire_log_native_backends.rs` filters on
  `kind == "request"` only. The reason it was impossible is gone —
  `Client::set_backend_for_test` (`c5a6019`) makes both native paths reachable
  from a `127.0.0.1` mock, and seven tests now drive them. Writing this one is
  ordinary work now, not a blocked item.

### Provider-divergence gaps — closed 2026-08-02

The audit's ten findings were closed across five slices (`c5a6019`, `49feedd`,
`5d87db5`, `2363e6b`, and the commit adding this line). `hrdr-llm` went from 215
to 241 tests. What was learned, kept because it shapes future work:

- **Host-keyed backend detection is why the native paths were untestable.**
  `detect_backend` reads the host, so every mock on `127.0.0.1` is
  `Backend::OpenAi`. A `#[cfg(test)] Client::set_backend_for_test` fixes it for
  one client instance with no statics to race through; reach for it rather than
  re-deriving the problem. It unblocks the wire-log entry above — which is now
  merely unwritten, not impossible.
- **Two gaps stay open by decision, not oversight.** (a) 408/522/524 are
  retryable _only_ because `classify_status` says so — `is_transient`'s text
  fallback has no needle for them, unlike the other six transient statuses. A
  test now derives the unprotected set from real behaviour, so both deleting an
  arm and quietly adding a needle fail loudly. Giving them needles is a
  behaviour change nobody has asked for. (b) All three mid-stream error paths
  hardcode `retry_after: None` (`client.rs`, `anthropic.rs`, `codex.rs`), and
  `retry_after_hint` reads a typed error's field directly — so a rate limit
  delivered _mid-stream_ never has its requested delay honoured on any backend.
  Only the HTTP-status path (`error_from_response`) does. Asserted and
  commented, not fixed.
- **`UNNAMED_MODEL` reaches the wire literally on both native backends**,
  because `wire_model` runs after their early returns. Pinned as a known
  limitation. Erroring early is worth doing, but at provider-selection time in
  hrdr-agent — not in `chat_stream`, where it would fire once per turn and where
  a wrong error kind would make the retry loop spin on a permanent config error.
- **The warm models.dev catalog path is deliberately uncovered.**
  `catalog::load_cached` reads process-global state (`HRDR_MODELS_PATH` / the
  XDG cache dir), so warming it in a test leaks into every other test in the
  binary. The cold path and the `ANTHROPIC_MAX_TOKENS` 8192 fallback are pinned;
  the warm resolution rules are covered separately in `catalog`'s own tests.
- **Still not covered, stated as a gap:** hrdr-agent's `MockResp` cannot set a
  response header, so no _end-to-end_ test exercises the agent's retry loop
  consuming a real `Retry-After`. `retry_after_from_headers` is covered directly
  instead.
- **Not a coverage gap, do not re-derive:** `Delta` deserializes only
  `reasoning_content`, so providers streaming `delta.reasoning` (several
  OpenAI-compatible gateways) have their reasoning silently dropped. Missing
  feature, not a missing test.
- **Never audited at all:** `sse.rs`, `capped_read.rs`, `fs.rs`, most of
  `catalog.rs`; hrdr-tui / hrdr-web / hrdr-tools / hrdr-app entirely.

Raised while doing the five slices, out of their scope, each re-verified against
the tree before being written here:

- **The `UNNAMED_MODEL` docstring tells half the story.** It states that putting
  the sentinel on the wire "cannot succeed anywhere it is actually read", then
  says the _OpenAI-shaped_ builder omits the field. Correctly scoped as far as
  it goes, but it never says the two native builders emit it verbatim — and this
  docstring is where a reader would go to check the invariant. Add the caveat,
  or fold it into whatever lands for the early-error decision above.
- **`parse_imf_fixdate` ignores the weekday.** It splits on `", "` and discards
  the prefix, so `Xyz, 06 Nov 2999 …` parses fine. Laxer than RFC 7231 and
  harmless — the weekday is redundant with the date — but worth knowing before
  someone "fixes" a test that relies on it.
- **A `thinking_delta` for an index with no open block is silently dropped.**
  `map_event`'s `thinking_slot.get_mut` no-ops, which looks right; the point is
  that the neighbouring `input_json_delta` path carries an explicit note about
  why it must not default to slot 0, and the thinking path's equivalent choice
  is unexplained. A comment, not a fix.
- **`serve_once` takes `&'static str`**, so every mock SSE body must be a
  literal. Fine today; it makes a table-driven stream test awkward. Relaxing it
  to `impl Into<String>` is a one-line change when someone needs it.

---

## Review coverage still owed

The 2026-08-01 pass (see
[Cleared in the 2026-08-01 review pass](#cleared-in-the-2026-08-01-review-pass))
closed every finding it raised, but it did not read everything. What it never
opened, so nobody records it as reviewed:

- **`hrdr-app/src/commands/dispatch.rs`** — 946 lines, the slash-command
  dispatcher both frontends route through. Untouched by any review pass.
- **`hrdr-tui/src/ui.rs` block rendering** and **`app/commands.rs`**. Only the
  mouse/selection path (`e1b3023`) and the scroll/highlight math were read.
- **Twelve `hrdr-tools` files**: `find`, `ls`, `secret_diff`, `mutation`,
  `todo`, `tree`, `verify`, `memory`, `verification`, `ansi`, `test_nudge`,
  `lsp`. The 2026-07-31 pass listed them as a gap and the 2026-08-01 pass
  covered `gate`, `hooks`, `web`, `replace` and `mcp/client` instead.

## Known behaviour to revisit

Not bugs; things whose surprise is worth having written down.

- **A profile can allow-list itself into having no search tool.** `read_only`
  keeps a shell on purpose, and `jail` keeps `grep`/`find`/`ls`/`tree`; but an
  `allowed_tools` list naming neither leaves an agent that can only `read` by
  exact path. Nothing validates that at load, and no prompt section can name a
  search tool it does not hold — `base.md` says "whichever search tool you hold"
  precisely because there may be none. Worth deciding whether such a profile
  should be refused at load rather than shipped blind.
- **The prompt tests pin literal prose spans**, so reflowing a paragraph breaks
  them without changing a rule. Six of the ~10 breaks in the 2026-08-01 voice
  pass were a phrase moving across a newline, not a phrase being cut. Fixing the
  prose to satisfy the assertion is the right direction — rewriting assertions
  to match new prose makes them tautological — but the coupling costs a test
  round-trip per reflow. Matching on normalized whitespace would remove it.
- **`write.md` is still 28 KB resident** after the git/release split, and
  carries ~10 shouted ALL-CAPS rule headers. Emphasis that dense stops
  selecting. Neither is a defect; both are what is left of "the always-on prompt
  is too long".

- **A mid-stream retry can double the text the user already saw.**
  `drain_stream` forwards `AgentEvent::Text` to the frontend as it arrives, so a
  stream that fails after emitting some output and is then retried — by
  `RetryBudget` on a transient error, or now by `recover_context_overflow` on a
  drain-time `context_length_exceeded` (PR #24) — re-streams the model's reply
  from the start, and the frontend has no signal to discard the first partial.
  Not new and not observed in practice: Codex reports overflow as a
  `response.failed` event before any content. Noted because the overflow path
  widened the set of errors that reach the retry, and because the fix (a
  "discard what I streamed for this round" event) is a protocol change, not a
  local one.
- **Building a sandbox policy touches the parent repo's `.git`.**
  `git_metadata_roots` `create_dir_all`s `refs/heads/hrdr` and
  `logs/refs/heads/hrdr` so they exist to be canonicalized and bind-mounted — so
  `Agent::new` with a linked-worktree cwd creates those two dirs in the
  **parent** `.git` at construction time. Harmless (git ignores empty ref dirs),
  but constructing an agent is not read-only with respect to the repo.
- **A commit from a linked worktree can print a `packed-refs.lock` EROFS line.**
  Ref maintenance triggered by a commit inside a confined worktree may fail to
  create `<parent>/.git/packed-refs.lock` while the commit itself lands and
  exits 0. Observed during bring-up, asserted by no test — treat it as possible,
  not guaranteed, and **do not widen the roots to silence it**.
- **Input-path unification UX.** Since "every user message is a queued `Steer`",
  a submitted message renders when its `Steered` event is pumped (a beat after
  submit) rather than synchronously, matching sub-agent behaviour. Intended and
  imperceptible with a fast pump; if it ever reads as laggy, pump the opener
  synchronously.
- **tok/s excludes tool time.** The generating marker divides streamed tokens by
  _model working time_: `infer_elapsed()` pauses while `tools_running > 0`, and
  the loader is hidden entirely during a tool call. By design — it reports model
  speed, not wall-clock throughput. Showing "running tool…" instead of hiding
  the loader, or tracking wall-clock throughput separately, is a feature, not a
  fix.

---

## Considered and declined

Recorded so the next audit does not re-litigate them — if you disagree, argue
with the reason, don't re-file the finding.

- **Batched `edits[]` on the `edit` tool — declined 2026-07-26.** The design was
  worked through (flat `edits: [{path, old_string, new_string}]`, anchors
  resolved against as-read content, two-phase all-or-nothing) and rejected on
  cost/benefit: single edits are what models handle best; with prompt caching
  the marginal cost of a second edit call is its own args plus the trimmed
  result, so the batch's real saving is round-trip latency — not worth the
  validation/overlap/error-reporting machinery, which is its own bug surface.
  The failure-retry cost that motivated it was fixed at the root instead
  (formatter-aware staleness + apply-anyway, `da714e1`). Two constraints on any
  revival: tool args must be object-rooted, so a bare array can never be the
  schema; and pi's version is worth reading first (matching against the
  **original** content, overlaps rejected naming both indices, applied in
  reverse) because hrdr would feel it more — every mutating call is a
  serialization barrier, and hrdr serializes **all** mutation globally where pi
  queues per realpath.
- **A CLI-shaped tool surface — rejected 2026-07-27.** Asked whether every tool
  should be an args-array like `git`. No: model CLI fluency is with _existing_
  CLIs (available through `shell`), an invented CLI grammar is less familiar
  than JSON function-calling, args-arrays give zero field-level schema guidance,
  and the whole cross-cutting layer (read-guard, staleness culprit, secret
  guard, LSP-on-edit, spool nudges) keys on knowing which field is the path.
  `git`'s shape is right **because it wraps a known CLI behind an allowlist**,
  not because args-arrays are good.
- **Merging the three LSP tools into one `lsp` tool with an operation enum.**
  Opencode collapsed nine operations into one tool and had to make
  `line`/`character` **required** even for `workspaceSymbol`, which ignores
  them. The enum only pays past ~6 operations. **Add ops as new tools if
  wanted.**
- **Moving hrdr's tool descriptions into `.txt` files.** Opencode does, but it
  composes at runtime on top of them — `include_str!` would be a lateral move.
  The change that matters is the return type (`&'static str` → `String`), filed
  above.
- **Slash-command dispatch is mirrored** between `hrdr-tui/src/app/commands.rs`
  and `hrdr-app/src/commands/dispatch.rs`. Intentional: the TUI handler
  intercepts TUI-only commands (`edit`, `reload`, `goto`, `find`, `next`/`prev`)
  then falls through to the shared dispatcher. `CommandHost` is the DRY
  mechanism; the split is explained at the call site.
- **Two project-dir walks** — `skills.rs::skill_dirs` and
  `prompt.rs::gather_agent_docs` both walk cwd → `/` plus XDG dirs. **Now both
  in `hrdr-agent`** (they were split across crates when this was first judged),
  so a shared iterator is cheaper than it was; still ~15 lines each with
  diverging payloads (skill dirs vs `AGENTS.md`), so still judged borderline
  over-engineering. Re-examine only if a third walk appears.
- **Four `CommandHost` impls** — `TuiHost`, `WebHost`, and the test hosts
  `TestHost` and `RouteTestHost`. The trait is the shared mechanism; the test
  hosts share some trivial no-op bodies, but the login host carries
  login-specific state. A shared test base would remove a few no-ops for very
  little gain.
- **Secret-file write/edit guards are tailored, not shared.** `write.rs`,
  `edit.rs` and `fileops.rs` each `bail!` with their own message ("refusing to
  write…", "refusing to edit…", "copying it would place its contents…"). The
  structure repeats; the wording is deliberately specific and meaningful to the
  model. The read side (`guard_secret_read`) is already shared.
- **`tree.rs` and `replace.rs` build their own walkers.** Genuinely different
  configuration — variable `max_depth` and no ignore toggles in `tree.rs`;
  `hidden(false)` with no `.gitignore` handling in `replace.rs` — so they stayed
  out of the shared `ignore_walker` that `find` and `grep` use.
- **The three grep backends keep separate bodies.** Divergent flag sets
  (ripgrep's `--hidden`/`--glob`, POSIX grep's documented `--exclude-dir` trap,
  the built-in `ignore::Walk`). `GrepBackend` already dispatches by exhaustive
  match; shared methods would wrap nothing.
- **Two "is this the ChatGPT/Codex endpoint" checks, on purpose.**
  `hrdr_llm::detect_backend` uses a permissive host+substring test to pick a
  wire protocol (a mirror or gateway still needs the Responses-API body shape);
  `config::is_codex_oauth` uses strict equality against one constant to gate
  OAuth credential injection. Unifying them would weaken a security boundary
  documented at its call site.
- **`AgentEvent` is matched in two places** — `transcript.rs`'s shared
  `apply_event` fold and `subagent_transcript.rs`'s `Record` projection — but
  they build different artifacts (live TUI transcript vs serializable record).
  Not a fork.
- **`lsp.rs` and `mcp/client.rs` spawn without `proc::spawn_group`.** They hold
  `Option<ProcessGroup>` in long-lived fields, rely on the guard's `Drop` with
  documented field ordering, and never kill explicitly — the `GroupKill` handle
  would be dead weight.
- **Evals.** pi's `packages/evals` is `private: true`, holds **one** case
  (assert the model answers "Paris"), is scored by plain `vitest` exact-match
  with no judge and no dataset, **is not run in CI**, and its harness disables
  everything pi does (`noTools: "all"`, `noExtensions`, `noSkills`). Against
  hrdr's ~1,450 in-repo tests that is an aspiration, not an advantage. **If hrdr
  builds evals, build them because we want them** — not to close a gap.
- **Cron / scheduled runs.** Hermes' `cron/scheduler.py` is 194 KB around an
  in-process 60-second poll thread inside a long-lived gateway daemon under
  launchd/systemd. It presupposes a resident daemon hrdr does not have and does
  not want; scheduled work for a coding agent lives in CI, and the intra-session
  case is "say what you are waiting on and end the turn" (there is no polling
  tool: `watch` was deleted 2026-07-30). Note the second-order cost hermes paid:
  cron sessions poisoned session-search ranking badly enough to need a demotion
  tier. The one detail worth stealing if hrdr ever ships anything scheduled is
  the _posture_ — cron runs get `skip_memory=True` unconditionally because
  _"cron system prompts would corrupt user representations"_, and approvals fail
  **closed** there.
- **Auto-downloading `rg`/`fd`.** See the grep-backend item above: rejected on
  distribution grounds (single static binary) and because the degradation ladder
  is a feature on locked-down machines — **not** because auto-download is
  inherently unacceptable (hermes does it with SHA-256 + cosign).
- **Code mode / V8 tool execution.** Codex ships it and makes it mandatory on
  its newest models (`gpt-5.6-{sol,terra,luna}` declare
  `"tool_mode": "code_mode_only"`, hiding every nested tool behind `exec` in a
  fresh V8 isolate); opencode's `CodeModeTool` is **not** in its builtin
  registry and exposes MCP/CodeMode tools only, explicitly not top-level tools —
  the opposite idea under the same name. **Not the same feature twice; ignore
  both.**

Seams already done right, worth copying rather than reinventing: `Shell`
(`tools/shell.rs`), `EditorEngine` (`hrdr-editor`, trait + 2 impls with zero
call-site branching), `Transport` (`mcp/types.rs`), `GrepBackend`, `ModelRef`,
`ChatErrorKind`, `proc::ProcessGroup`.

---

## Leads worth not regressing

From the comparison, and only the ones a future change could plausibly trade
away. Not work; guardrails on work.

**Checked and correct as-is — do not "fix" these:**

- `git_metadata_roots` (`sandbox.rs`) serves hrdr **itself** being launched
  inside a user's linked worktree, where the agent must still commit. Its
  sibling `enclosing_git_dir` does the same for an agent scoped _below_ a repo
  root (a `task` with a narrow `cwd`). Both are pinned by tests; neither is dead
  code left over from the removed sub-agent worktrees.
- The `git worktree remove --force` and `git branch -D` guardrails protect a
  _user's_ own worktrees and branches generically — they were never about
  sub-agent worktrees.
- The `git rebase HEAD` guardrail is a generic `-C <dir>` footgun rule, not
  task-specific.
- **`memory.rs` writing outside the sandbox roots is correct.** That is where
  memory lives, and routing it through `check_write` would break the feature.
  The audit framed this as a bypass to plug; it is not one. What was separable
  was _authority_, and that is handled (`313eb0e`: `memory` is main-agent-only).
- Session and transcript persistence carry no removed fields and neither uses
  `deny_unknown_fields`, so an old file still loads. Relevant every time a
  record type loses a variant — as `Record::EscalationDecided` just did.

- **Sub-agent filesystem isolation — all four peers lack it.** codex has two
  generations of sub-agent tooling and no worktrees; hermes' children share the
  parent's cwd while its own tool description claims _"separate working
  directory"_ (its `delegate_tool.py` has zero `worktree|mkdtemp|os.chdir|cwd=`
  matches) and default `max_concurrent_children` is 3; opencode's child session
  runs in the same directory (its `worktree/index.ts` exists but is not
  referenced from `task.ts`); pi's exists only as a 1015-line example spawning
  `pi --json` subprocesses with no isolation. **hrdr matched codex and opencode
  here deliberately in 2026-07-29: sub-agents share the working directory, and
  the isolation this paragraph praised was removed. The read-only `.git` that
  briefly replaced it went too (2026-07-30) — it refused the file tools a write
  `shell` walked round. What a sub-agent's scope is now: its `cwd`, which `task`
  can narrow, enforced by the kernel.**
- **Read-before-write that refuses.** hrdr blocks all three non-`Fresh`
  `ReadState`s. Hermes detects staleness and its own docstring says _"Does not
  block — the write still proceeds"_; pi's `write` overwrites unconditionally. A
  lead over two peers independently — do not soften it into a warning.
- **Semantic `rename`.** No LSP at all in codex or hermes; opencode's `lsp` tool
  is experimental and has **no rename op**. The single capability no other
  harness in this comparison has.
- **Guardrails with no off switch.** hermes has `HERMES_YOLO_MODE`, a default
  `"smart"` mode where an auxiliary LLM auto-approves, and a headless path that
  auto-approves **without running the scanners** (plus a CVE for a contextvar
  race onto that path). hrdr's are compiled in, read no env var, have no LLM in
  the loop, and apply to sub-agents. **hrdr's autonomy posture is coherent
  precisely because there is no headless carve-out.**
- **USD cost budgeting, and session retention/compression.** Absent in all four
  peers, both of them.
- **ROI-gated mid-history pruning with recoverable pointers.** Codex has only
  truncate-at-capture and full auto-compact with no rung between; opencode's
  prune is off by default and replaces content with
  `"[Old tool result content cleared]"` and **no re-read path**. hrdr spills to
  `tool_output_dir()` and substitutes a pointer with recovery instructions.
- **Concurrency that cannot corrupt the tree.** `concurrent()` defaults to
  `read_only()`, so mutating tools are a strict barrier. Codex's `shell_command`
  and `exec_command` both declare parallel-safe with no path-level locking.
- **Skill shadowing beats skill syncing.** Built-ins embedded, project/user
  files shadow by name, first-source-wins, tested. Hermes copies bundled skills
  to `~/.hermes/skills/` and needs an MD5 origin-hash manifest, a v1→v2
  migration and a `.no-bundled-skills` opt-out to work out whether the user
  customised a copy. hrdr has no such state to get wrong.
- **Post-edit LSP diagnostics folded into the edit result.** `apply_file_change`
  returns `lsp.diagnostics_note` with the success message — the model learns it
  broke the build in the same tool result. Opencode does the same on mutation
  (parity on the mechanism that matters); codex and hermes have no LSP.
- **Bounded output everywhere, not just for the shell.** 11 file/search tools
  plus `git`, all through `truncate_saved`. Codex truncates only shell output,
  so `cat` of a 5 MB file costs a round trip to discover.
- **What every peer got wrong and hrdr should not copy:** dead prompt files that
  look live (codex 5-of-6, opencode 1-of-9 plus two more); read-before-write
  that warns instead of blocking (hermes, pi); sub-agent self-reports treated as
  facts (hermes pops `files_written` before the model sees it and answers with a
  prompt saying summaries _"are SELF-REPORTS, not verified facts"_ — hrdr's
  answer is that a sub-agent's edits are already in the tree, so `git diff` is
  the mechanical check); and skill loading that fails **open** (opencode logs
  and skips a YAML error, silently drops a file that fails its shape check).

---

## Standing constraints

Decisions from completed work that still govern new work. These are not backlog
items — they are rules.

- **hrdr-agent owns ALL agent logic; hrdr-app is only agent↔TUI glue.** Every
  agent, main or sub, runs the same codepath — no special-casing, no parity
  forks. Do not ask "how should sub-agents behave"; they behave exactly like the
  main agent because it is the same code.
- **Only the `AgentEvent` fold persists in a transcript.** User=`Steered`,
  Assistant=`Text`, `Reasoning` text, tool args+results, agent `Notice`→System.
  Frontend-pushed _chrome_ is not persisted — slash-command System output,
  `/diff` output, per-turn Stats, Header, `Reasoning.took_ms` — it is
  display-only, not context, and not needed to resume.
- **No migration or back-compat fallback before 1.0.** Clean breaks; delete
  old-format fallback code when you find it.
- **`hrdr-llm` has three streaming paths** (`client.rs`, `anthropic.rs`,
  `codex.rs`), and an invariant added to one does not reach the other two. This
  produced security finding O4 (duplicate auth header) and a wire log that
  silently covered only one backend. Anything cross-cutting added to
  `client.rs`'s request path must be checked against the other two.
- **The sandbox is a boundary, not a hint** (the code is
  `crates/hrdr-tools/src/sandbox.rs`).
  - _Never confine the hrdr process itself._ No Landlock `restrict_self`, no
    prctl, outside a child `pre_exec`. hrdr does its own session/config/memory
    I/O in-process; confining it breaks the app.
  - _Never silently pretend to sandbox._ Any path that runs a command with less
    confinement than the mode asks for must set its notice first (each notice at
    most once per process). `read` degrading to write-confinement under Landlock
    is decided and allowed — being quiet about it is not.
  - _The writable set is all of it:_ cwd + `env::temp_dir()` + session scratch +
    tool-output + the four linked-worktree git metadata roots (worktree gitdir,
    `objects`, `refs/heads/hrdr`, `logs/refs/heads/hrdr`) + configured extras.
    Drop temp and compilers die; drop scratch/tool-output and overflow spill
    breaks; drop the git roots and every write sub-agent's commit fails.
    Equally: never widen to the whole parent `.git` — that re-opens the escape
    the sandbox exists to close.
  - _bwrap argv order is semantics_ (later mounts shadow earlier ones):
    `--ro-bind / /` before the rw `--bind`s; `--tmpfs /tmp` before the
    cwd/scratch/tool-output binds (the scratch dir lives under `/tmp`); and
    `/bin`, `/sbin`, `/lib`, `/lib64` emitted as `--symlink <read_link(p)> <p>`
    on usr-merged distros, never `--ro-bind` and never a guessed
    `usr/<basename>`.
  - _`ToolContext::new` stays unconfined._ Only `Agent::new` installs a real
    policy; hundreds of tool tests build a bare context against tempdirs.
  - _Guard model-supplied paths only._ Memory storage, overflow-spill writes and
    hook/LSP/MCP subprocesses are app infrastructure and bypass
    `resolve_read`/`resolve_write` by design. The one deliberate widening is
    `rename`, which also guards the server-returned workspace-edit targets — the
    guard's contract is _where writes land_, not _who typed the path_.
  - _`SECTION_SANDBOX` stays after `SECTION_ENVIRONMENT`_ (its roots name the
    per-agent worktree, so it belongs in the volatile tail), and the
    prompt-cache split anchor stays `SECTION_ENVIRONMENT`.
  - _Broad reads in `write` mode and full env passthrough in bwrap_ (no
    `--clearenv`) are decided v1 tradeoffs, not oversights. Narrowing either is
    follow-up work, not a bug fix.
- **A skill the model can load is still the user's procedure**
  (`hrdr-agent/src/skills.rs`, `prompt::skills_section`).
  - _The listing is a menu, never the content._ Name + one-line description
    only; bodies come from the `skill` tool when one applies. Under the byte
    budget descriptions are dropped tail-first and **names always survive** — a
    name the model cannot see is a skill it can never load.
  - _No source paths in the listing._ They name the per-agent worktree, so they
    would differ between sibling sub-agents and push per-agent bytes into the
    shared cache prefix. The tool's own result names the source, where it costs
    nothing shared.
  - _A skill body is instruction, and it is project-authored._ It reaches the
    model as tool output — which the base prompt otherwise calls data, never a
    command — so the result frames it explicitly as the user's/project's
    instructions and names the source. Same trust class as `AGENTS.md`, and the
    same open exposure (an untrusted clone's `.hrdr/skills`).
  - _`model_invocable: false` is a boundary._ Such a skill is unlisted **and**
    refused by the tool, with an error telling the model to ask the user to run
    `:name`. Only a literal `false` opts out (a typo fails open, visibly, rather
    than silently hiding a skill). Built-in `:release` carries it because its
    last step pushes a tag.
  - _The prompt section is gated on the tool._ A profile whose `tools:`
    allow-list drops `skill` gets no listing: naming a tool an agent lacks is
    the defect the pi comparison found, not a pattern to repeat.
- **A new tool picks its interface shape by rule, not by taste** (taxonomy from
  the 2026-07-27 survey of all 31 tools). The shape is load-bearing: the
  cross-cutting layer (read-guard, staleness culprit naming, secret guard,
  LSP-on-edit, spool nudges) keys on JSON-schema'd fields, so a tool the harness
  cannot introspect is a tool it cannot protect.
  - _Default_ — one noun-tool, flat args object: one capability, one required
    primary arg, the rest optional flags (`read`, `edit`, `grep`).
  - _`action` enum_ — several **mutating** verbs over one resource sharing one
    field vocabulary (`memory`: view/write/edit/delete/search over
    name/description/body/scope).
  - _`mode` enum_ — **read-only** views of one dataset (`models`:
    current/providers/models).
  - _Separate prefix-family tools_ — verbs with distinct schemas or distinct
    read-only gating (`task_*`: spawn takes description/prompt/model, diff takes
    commit, cleanup takes force). One mega-schema would leave most fields
    meaningless per action — that is the real hallucination trap.
  - _CLI args-array passthrough_ — reserved for wrapping an **existing,
    well-known** CLI behind an allowlist (`git`). Never the shape for a bespoke
    tool. A model that wants raw CLI already has `shell`.
  - _Time is seconds, always_ — `timeout_secs`, `interval_secs`; never `_ms` in
    a model-facing schema (`shell.timeout_ms` renamed 2026-07-27, old name
    poisoned). Removed or renamed params must be **poisoned** with an
    instructive error: `tool_args` ignores unknown fields workspace-wide, so a
    silent drop is the default failure mode.
  - _Shared vocabulary across tools_ — one concept keeps one field name and one
    default polarity everywhere: `pattern` + `literal: true` opt-out is the
    matching shape for both `grep` and `replace` (aligned 2026-07-27; their
    previously inverted regex defaults were a silent trap).

---

## Corrections made during the merge

What the three source docs got wrong, found by re-verifying every claim. Kept
because a backlog that quietly fixes its own errors teaches nothing.

**These are a dated record (2026-07-27), not current facts.** Several describe
code that has since been deleted: there is no `git` tool (#1, #2), the tool
count in #6 predates the 2026-07-30 cut of ten tools, and the guardrail count in
#3 has moved. Read them as "what was corrected then", and check the code for
now.

1. **`git -C /elsewhere log` does not succeed.** The sandbox follow-up asserted
   it did, in `read` mode. Leading flags in the subcommand slot (`-C`, `-c`,
   `--git-dir`) are refused with a dedicated test, and `FORBIDDEN_ANY` blocks
   seven more. The true residual — the git tool spawns an **unconfined**
   subprocess because only `shell` and `watch` are wrapped — is now what the
   item says.
2. **The `git` tool has 9 read-only subcommands, not 14.** `ALLOWED` is
   `status, diff, log, show, blame, branch, describe, remote, shortlog`.
   `compare.md`'s opencode section said 14.
3. **Guardrails: 15 rules, and one of them is PowerShell-shaped.** `compare.md`
   said "14 hand-written regexes"; the real count is 13 destructive-command
   patterns plus 2 pipe-to-shell rules (POSIX and `iwr|iex`). So
   `deferred-improvements.md`'s "the pipe-to-shell guardrail assumes POSIX" was
   true only of its **recovery text**, not its coverage.
4. **There are four `CommandHost` impls, not three** — `WebHost` arrived with
   the web UI and was never added to the count — and the login test host is
   named `RouteTestHost`, not `TestLoginHost`.
5. **The two project-dir walks are now in one crate.** `skill_dirs` moved to
   `hrdr-agent` with the skills work, so the "different crates" half of the
   reason to leave them alone is gone; the verdict survives on size and
   diverging payloads.
6. **Tool count: ~31, not ~30** — 17 from `ToolRegistry::with_defaults`, the
   rest registered by `Agent::new`. The taxonomy's "all 31 tools" was right.
7. **`security-audit.md`'s closing line was stale.** It said the Windows-drift
   pass "was never run"; it ran and landed three fixes (`8e5bc9d`).
8. **Every `system.j2` citation in `compare.md` was dead.** The template engine
   was removed in `5f6e386` — the prompt is `include_str!` markdown fragments
   assembled as an ordered section list. Those line numbers pointed at a file
   that no longer exists, which is why this file cites symbols instead.
9. **Line numbers rot generally.** `Tool::description` moved from `lib.rs:965`
   to `:1155`; `gather_agent_docs` from `:210` to `prompt.rs:567`. Same policy.
10. **The audit's summary table did not add up.** Severity rows summed to 19
    (2 + 4 + 13) against a stated total of 16 findings. Which number is wrong is
    not recoverable from the doc, and every finding is closed either way — so
    the record above states the total and the discrepancy rather than picking
    one.
11. **The sandbox's path guard covers 16 call sites across 9 tool files** (`ls`,
    `fileops`, `grep`, `edit`, `lsp_nav`, `read`, `replace`, `write`, `tree`),
    not the "14 sites" the shipping notes recorded — `replace` and `rename`'s
    server-returned targets were added after that count was written.

---

## Cleared in the 2026-07-27 pass

Fifteen commits, `0fae706`..`36a7f2b`, cleared every item that was actionable
without a decision: the five that were top of this list, plus the LSP dedup,
revive capability, the notice channel, both web auth holes, the grep-backend and
guardrail-drift and TUI-history test gaps. Read `git log` for what each did —
the entries are gone from the sections above, per this file's own convention.

What survives is the part that would otherwise be relearned:

- **`AgentEvent::Notice` never reaches the model.** The `doom_loop` entry
  prescribed injecting one; it would have fixed nothing. The channel that
  reaches the model is a note appended to the round's last tool result — the
  shape the round-budget warning, the repeat nudge and the truncation warning
  all now share.
- **`read_only()` means "does not mutate the working tree", not "touches no
  state".** `todo` was classified as mutating and pruned from read-only agents
  while the prompt told every agent to plan with it. Anything holding state only
  in the agent's own `ToolContext` belongs in the read-only set — and if its
  calls are order-sensitive, it opts out of `concurrent()` separately, which
  defaults to `read_only()`.
- **An item's proposed fix is a hypothesis.** Three of these described their own
  fix wrongly (the two above, plus `git -C /elsewhere log`, which was already
  refused). Verify the claim before implementing the remedy.
- **A guard's blast radius is the thing to check first.** The metadata guard
  initially covered `.hrdr`/`.claude`/`.opencode` too, which would have refused
  every write a sub-agent makes in `<repo>/.hrdr/worktrees/wt-N` had the check
  been whole-path rather than root-relative, and did block "add a project skill"
  even when it was correct. Narrowed to `.git`; the rest is a decision at the
  top of this file.

## Cleared in the 2026-07-30 sandbox redesign

Nine slices, `5c9f675`..`c114a6a`, ≈9,000 lines net removed.
`docs/sandbox-redesign.md` is the decision record and stays; the code is the
truth. `docs/context.md` was folded into this file and deleted, per this file's
own convention.

Closed **by deletion**, which is the shape of most of it: the `.git` lock, all
of escalation, the network axis, bwrap, `DenialKind`, ten tools, and `grep`'s
two subprocess backends. Also shipped: `jail` mode with a fixed five-tool set,
the `prisoner` agent, per-profile `sandbox:`, per-session `tool_output_dir`,
package-manager cache roots, `--sandbox-writable-root`, unified
untrusted-content wrapping, and `task`'s `cwd`.

What survives that would otherwise be relearned:

- **Deleting a mechanism can be the fix.** The escalation ladder existed for one
  failure — bwrap's user namespace making ssh refuse `/etc/ssh/ssh_config` — and
  removing bwrap removed the failure. Two large features (a consent gate with an
  audit trail, a widening ladder) were answering a problem the redesign deleted.
  Ask what a mechanism is _for_ before improving it.
- **A guard the front door bypasses stops only the honest path.** The `.git`
  file-tool lock refused `write`/`edit` a path `shell` reached in one step,
  while refusing legitimate `.git/info/exclude` edits and user-requested hooks.
  Same reasoning killed the network denial: it bought one hop of latency, not
  containment, because the sub-agent reports to a parent that has a network.
- **"Available and ignored" is the only usage figure worth acting on.** That a
  tool the model was handed gets called measures availability. `references` 2
  calls in 9,350, `definition` 0, `rename` 0, `watch` 4 — that measures need.
- **Removing a tool means auditing what it was the only home for.** `grep`
  filtered credential files out of its own output; deleting it from every
  non-jail mode would have left `shell` — the actual search path — with no
  secret handling at all. The filter moved to `shell` and grew a diff-aware
  half.
- **Confinement that a mode's tool set makes unreachable is not confinement.**
  `web_fetch`/`web_search`/MCP run in the hrdr parent, outside the sandbox, so a
  "confined" agent holding them had a working network egress. Jail's boundary is
  its tool set as much as its roots.
- **A floor that silently inverts a request needs a notice.** `--sandbox jail`
  on a write-capable session floors at `write`; without saying so, somebody who
  typed the word meaning "contain me" gets full project write and never learns.
- **`cargo` here runs through a wrapper that indents `error:` lines**, so
  `grep -E "^error"` reports a false pass. Read its summary line. This cost a
  follow-up commit.

## Cleared in the 2026-08-01 review pass

`docs/code-review.md` (2026-07-31, refreshed 2026-08-01) is **deleted** per this
file's own convention — every finding in it shipped. Nine commits,
`4e66a1c`..`2e3be29`, released as **v0.10.0**; `git log` is the history.

Sixteen findings closed: the web server's missing `WWW-Authenticate` challenge
and the `users` mode that had no browser entry point at all;
`SseDecoder::finish` returning `Ok` with a truncated event; a Codex error whose
code sat at one level and its message at another; a wire round-trip test that
compared `Value` to itself and so green-lit three message types the protocol
cannot parse; a hook timeout reporting seconds as `ms`; a crashed turn leaving
its tool call spinning forever. Plus the SSRF blocklist's missing `100.64/10`,
`attr_value` matching an attribute suffix, a DDG snippet reading past its block,
a CI-file cap that hid every single-file config, four `unwrap()`s in a detached
socket task, a leaked `String` per env var, a dead duplicate of the login
handler, and a stale `allow(dead_code)`.

What survives that would otherwise be relearned:

- **A skill the model cannot see is not a copy that can be relied on.**
  `write.md`'s Releasing section and the `:release` skill were the same
  procedure twice, and only the skill said to watch the tag's CI run.
  `model_invocable: false` keeps a skill out of the listing entirely, so
  plain-English "cut a release" only ever reached the copy missing that step.
  Duplication drifts; the reachable copy is the one that must be complete.
- **A red tag run SKIPS its publish jobs rather than failing them.** The push
  succeeds, the tag exists, nothing is published — how v0.4.3 and v0.5.0 were
  tagged with nothing behind them, and it happened again on `de1b12b` in this
  same pass. Enumerate the run's jobs and confirm the artifact landed; "tagged
  and pushed" is not "released".
- **Gate the prompt on what the tool set IS, not on what built it.**
  `ToolRegistry::with_defaults` registers `grep`/`find`/`ls`/`tree` and
  `Agent::new` strips them for every non-jail mode, so `has("grep")` alone
  marked a full write agent as jailed. The jail is the whole shape: those tools,
  no write tool, and no shell.
- **A test that models an impossible agent proves nothing about the real one.**
  The read-only prompt test built `retain_only(read_only_names())` — shell-less
  — while `config.read_only` deliberately keeps a shell. It had never covered
  the agent it was named for. Prefer building a live `Agent` over
  hand-assembling a registry.
- **Compressing prose has a ceiling the tests set.** 224 pinned literal spans
  across the corpus, ~110 in `write.md` alone: careful rewording of four
  always-on files yielded ~900 bytes, while moving git/release guidance behind
  `!delegated` yielded 9.4 KB. Structure beats wording by an order of magnitude
  when the wording is already frozen.
- **Guidance with no trigger phrase has to stay resident.** Deleting and
  Dependencies did not move to main-only with Git and Releasing: a sub-agent
  deletes files and reads dependency APIs like anyone else, and nothing is said
  before `rm -rf` that a gate could match on.

## Cleared in the 2026-08-02 backend pass

Six commits, `7e80605`..`9c3d012`. Two of the three OS backends changed status,
neither by writing much code.

**macOS Seatbelt was never untested — the tests just could not say so.** Its
end-to-end test opened with two silent `return`s (no `/usr/bin/sandbox-exec`, no
shell), so a run that exercised nothing was indistinguishable from one that
passed, and this file recorded it as never having run while CI ran it on every
macOS job. Both conditions now assert on a runner and skip only locally, and
`ci_runs_a_real_os_backend` fails if a runner detects the `None` fallback.

**Windows `read` mode is now confined**, by Mandatory Integrity Control: a
Low-integrity process cannot write to any object labelled Medium or higher,
which is everything the user owns, while reads are untouched. Applied the way
Landlock is — by the child to itself — because `CreateProcessAsUserW` cannot be
reached through a `tokio::process::Command`, so hrdr re-execs itself as
`hrdr __sandbox-exec -- <shell> -c <cmd>` and lowers its own token first.

What survives that would otherwise be relearned:

- **A skip that cannot fail is not a skip.** The Seatbelt tests were the shape
  `write.md` already warns about, one level up: not a check that could not fail,
  but a check that could decline to run and report the same green. Any test
  gated on a prerequisite needs to assert where that prerequisite is guaranteed.
- **`current_exe()` is the test binary inside a unit test.** The first Windows
  end-to-end test lived in `hrdr-tools`, whose test binary is what the backend
  then re-executed — it handed `__sandbox-exec -- cmd …` to libtest as filter
  arguments and wedged the Windows job for 37 minutes. Anything exercising the
  real wrapper belongs in `apps/hrdr/tests/`, where `CARGO_BIN_EXE_hrdr` names
  it.
- **Blind FFI fails on names, not logic.** Three CI round trips, every one a
  constant or trait import that had moved between `windows-sys` releases
  (`SE_GROUP_INTEGRITY`, `anyhow::Context`) — never the token or SID logic,
  which was written once and never changed. Spell a fixed ABI value out locally
  instead of importing it and the class disappears.
- **A red run skips its publish jobs rather than failing them.** Seen again on
  `de1b12b`: rustfmt went red, and the six publish jobs reported `skipped`. The
  release was not cut, which is the system working — but only because the tag
  had not been pushed yet.

## Record: closed efforts

No worklist here — read `git log`. Kept only so nobody re-opens a closed
question.

**Security & correctness audit** (2026-07-22, re-reviewed 2026-07-23, last
finding closed 2026-07-26; full-codebase, high depth). Attack surface was mapped
by entry point — HTTP handlers (`fetch`, `search`, MCP HTTP/SSE), CLI args, file
parsers, IPC (MCP stdio/HTTP, LSP), environment reads — and each vulnerability
class was checked against every source file: injection, memory/resource, crypto,
AuthZ/AuthN, data integrity, error handling, concurrency. **16 findings, all
fixed, 0 open** — the resolved detail is in `git log`. (Its summary table listed
2 High / 4 Medium / 13 Low against a total of 16; see correction 10.) O3, the
last to close, was the `read` TOCTOU identity check running only on unix, now
enforced on both platforms through one helper (`guard_not_swapped`, `1794c5a`).
Overall risk was assessed **Low**, and what the security-critical paths get
right is worth keeping that way: the `fetch`/SSRF guard uses a TOCTOU-free DNS
resolver; `SseDecoder` is memory-bounded; the credential store uses atomic
write + `0600` + cross-process locking; PKCE uses a CSPRNG verifier with SHA-256
S256; the untrusted-content envelope uses a verified-absent nonce; the secret
denylist covers `read`, `grep`, `git`, `replace`, `fileops`, `lsp_nav`,
`write`/`edit`; `canonicalize_nearest` prevents `..` escapes. No MD5/SHA1, no
hardcoded secrets, no panics on untrusted SSE input, no unbounded allocation in
hot paths. Two platform residuals were **not** findings and are tracked above:
no Windows ACL, and `O_NOFOLLOW` covering only the final component.

**Prompt architecture** (`c5e5ced`, `5f6e386`, `6274c80`, `b1a698f`, plus
`5adc9ff`, `e02cb5f`). The hermes pass's top finding — a cache breakpoint at
hrdr's own stable/volatile boundary — plus the frozen-memory defect it found
while verifying it. `system.j2` and minijinja are gone; the prompt is ten
`include_str!` markdown fragments assembled as an ordered named-section list
(`base → global_agents_md → global_memory → project_agents_md → project_memory → capability group → skills → persona → environment → sandbox`),
memory re-gathers at the compaction boundary (the one moment the prefix cache is
dead anyway), persona sits above the environment tail, and
`SystemPrompt::prefix_len_before(SECTION_ENVIRONMENT)` — a fold over section
lengths, not a substring search — is carried to the client as
`system_cache_split`. All four Anthropic breakpoints are now spent: tools,
stable prefix, system tail, rolling last message. A resumed/revived session
rebuilds the prompt so the split matches the installed text, and the
OpenAI-shape path emits the system message as two marked parts at the same
boundary.

**Model-invocable skills** (`3ffc406`, 2026-07-27). Closed the
pi/hermes/opencode finding and the defect behind it. Discovery, parsing and
expansion moved to `hrdr-agent/src/skills.rs`; `prompt::skills_section` renders
a name + one-line-description menu as `SECTION_SKILLS` (956 bytes for the nine
listed built-ins, in the cached prefix, no bodies and no source paths); a
read-only `skill` tool returns the expanded body through the same `expand_body`
a `:` invocation uses. Took pi's opt-out shape as `model_invocable: false`; did
not take hermes' "err on the side of loading" framing or its operator-side
disable list.

**OS sandbox** (issue #13, `df01afb`..`bf0ac01`, 2026-07-27). Nine slices from a
twice-verified spec, now deleted — the design lives in
`crates/hrdr-tools/src/sandbox.rs`'s doc comments and tests.
`SandboxMode {none,write,read}`, default `write`; software path-guard on 16 call
sites across nine file-tool modules; bwrap primary on Linux, Landlock fallback,
Seatbelt on macOS, software-only on Windows; degradation notices byte-pinned.
What survives as rules is under Standing constraints; what was left out is under
Sandbox follow-ups.

**Web UI** (2026-07-26). `hrdr serve` — axum HTTP+WS, an optional embedded
Dioxus SPA, three auth modes (token/basic/users), TLS-gated remote access. The
plan doc is deleted; leftovers are under Web UI follow-ups.

**Also closed and deleted along the way:** the transcript unification
(hrdr-agent owns the `Entry` model, `apply_event` builder and renderer;
frontends render only), the agent-logic migration (main and sub-agents on one
codepath), session retention/compression, the memory tool's design, the DRY and
seam audits (their survivors are under Considered and declined), and the
tool-robustness audit (13 items: 11 shipped, 2 dropped in re-triage).

**Tracked elsewhere:** the Codex catalog compatibility pin is GitHub issue #2.
Issue #13 (sandbox) is shipped and should be closed.
