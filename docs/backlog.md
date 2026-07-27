# hrdr backlog

**One file.** Merged 2026-07-27 from `deferred-improvements.md`, `compare.md`
(the four-harness comparison) and `security-audit.md`, which are deleted — read
`git log` for what they said before this.

**Every claim below was re-verified against the tree at `8c76cdb`** before it
was carried over. What did not survive verification is either corrected in place
or listed under
[Corrections made during the merge](#corrections-made-during-the-merge). Items
that had shipped are in [Record](#record-closed-efforts), not here.

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

Ranked by value per unit of work, all verified open.

1. **Protect `.git` inside a writable root.** The sandbox's path guard asks only
   "is this under a writable root", and a worktree's `.git` **is** under the
   worktree — so a write sub-agent can still install `.git/hooks/pre-commit`
   that runs on the parent's next commit. That is a worse version of the
   incident the whole sandbox was built for. Codex keeps `.git`, `.agents`,
   `.codex` non-writable inside a writable root (`protocol/src/permissions.rs`
   `PROTECTED_METADATA_PATH_NAMES`, `forbidden_agent_metadata_write`), rationale
   in-tree: folders whose contents _"could be modified to escalate the
   privileges of the agent (e.g. `.codex`, `.git`, notably `.git/hooks`)"_.
   Hermes has no equivalent, so codex's finding stands unweakened. _Caveat:_
   hrdr's own `task_*` plumbing legitimately writes git metadata (worktrees,
   commits, cherry-picks), so the rule is "the **model's** file tools may not;
   hrdr's plumbing may" — a `ToolContext` distinction hrdr does not draw today.
   And `git commit` through `shell` writes `.git/index`, so the shell half is
   only enforceable at the OS layer.
2. **`doom_loop` — repeated-identical-tool-call detection.** Verified absent
   (`grep -rn "doom\|loop_detect\|repeated_call" crates/` → 0). The only
   backstop is a _count_ cap: `max_steps` rounds, a wrap-up nudge three out, a
   final tool-less round, plus `RepeatGuard`. A model stuck re-running the same
   failing `cargo test` burns the whole round budget **and the whole cost cap**
   before anything notices. Opencode raises a `doom_loop` ask when the last 3
   tool parts are the same tool with byte-identical `JSON.stringify(input)`
   (`session/processor.ts`). For hrdr the action should be an injected `Notice`
   ("you have called X with identical arguments 3 times — change approach"), not
   an approval prompt. ~half a day. _Caveat:_ three identical calls are
   legitimate for `watch` polling and `task_output` on a running sub-agent —
   needs a per-tool opt-out list.
3. **The unconditional preamble names tools some agents lack.** Verified:
   `templates/base.md` still says _"Find the relevant code with
   grep/find/ls/tree/read before changing anything"_ and _"For multi-step work,
   plan with todo…"_, both above the first capability gate; and `TodoTool` still
   has **no `read_only()` override** (`grep read_only tools/todo.rs` → 0), so
   `todo` is excluded from `read_only_names()` and dropped by `retain_only`. The
   `explore`, `review` and `plan` profiles are told to plan with a tool they do
   not have; a custom agent file with a `tools:` allow-list is told to use
   `find`/`ls`/`tree`/`todo` it lacks. Fix: reword those two lines to name no
   specific tools, plus a test that every tool named in the unconditional block
   is in `read_only_names()`. (The same class of defect the `skill` listing
   avoided by gating its section on the tool's presence.)
4. **Re-evaluate the `git` tool now that Read mode exists — with the corrected
   premise.** The old entry claimed `git -C /elsewhere log` succeeds in `read`
   mode. **It does not:** a leading flag in the subcommand slot (`-C`, `-c`,
   `--git-dir`) is refused with a dedicated test
   (`refuses_a_leading_flag_in_the_subcommand_slot`), and `FORBIDDEN_ANY` blocks
   `-c`, `--config-env`, `--ext-diff`, `--textconv`, `--exec`, `--output`,
   `--upload-pack`, `--receive-pack`. The real residual is narrower and still
   real: **only `shell` and `watch` go through `sandboxed_shell_command`**, so
   `GitTool`'s `Command::new("git")` runs with no OS confinement at all — it is
   outside the boundary, bounded only by its own allow-list and git's
   repo-relative semantics. The open question is unchanged: with the sandbox
   shipped, read-only agents could get a confined `shell` (codex-style) and the
   9-verb allow-list could shrink or go.
5. **`AGENTS.md`: notice on load, and stop dropping big files silently.**
   Verified: `gather_agent_docs` concatenates whatever it finds walking cwd
   upward, and `MAX_AGENTS_FILE_BYTES` (64 KiB) skips any single file **entirely
   and silently** — hermes' own `AGENTS.md` is 73.4 KB, a real file hrdr would
   ignore without a word. Two defects in one place: the silent drop, and that an
   untrusted clone's `AGENTS.md` becomes system-prompt-level instruction with
   zero inspection (hermes scans context files first, blocking with a
   `[BLOCKED: …]` placeholder because _"the file would otherwise enter the
   system prompt verbatim and the user has no chance to intervene"_). _Caveat:_
   a regex scanner over project docs false-positives on exactly the repos a
   coding agent gets pointed at. **The cheap 80%:** notice naming path and byte
   count when a new/changed `AGENTS.md` loads, and reframe the block header to
   distinguish project file from user instruction. Full scanning waits for
   evidence.

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
| Repeated-call / loop detection       | —          | —                 | ✅         | ✗   | **✗** (top of list #2)  |
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
- **Don't execute tool calls from a reply that hit the output cap.** Verified
  unchanged: `acc.truncated()` raises a user-facing `AgentEvent::Notice` and
  execution proceeds — **the model is never told**. pi drops the whole batch and
  hands each call a synthetic error telling the model to re-issue. Smaller for
  hrdr (strict `serde_json`, no salvage parser, so truncated args usually fail
  to parse anyway); the residual is that earlier complete calls execute and the
  model resumes with no signal it lost the calls it intended. One-line fix:
  append the warning to the last tool result, the shape the round-budget wrap-up
  already uses.
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
- **Grep backend coverage is a test-harness problem.** The three backends
  (`grep_ripgrep`, `grep_posix`, `grep_builtin`, plus `grep_builtin_multiline`)
  are selected by host detection, so the POSIX path only runs where ripgrep is
  absent — in practice CI only. **Force each backend in tests** instead of
  letting detection decide. (This is the narrow true finding left after
  rejecting pi's rg/fd auto-download, whose downloader does no integrity
  checking at all. Note hermes verifies SHA-256 always and cosign when
  available, so the objection was to pi's _implementation_ — "auto-download is
  inherently unacceptable" is not the lesson. hrdr's single-static-binary
  posture is the reason it still says no.)

### Permissions, isolation, and state

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
- **Per-call permission escalation.** Codex's shell schemas carry
  `sandbox_permissions`, `justification`, `prefix_rule` and
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
- **Gate LSP tool registration on the project having a matching server.**
  Verified still true: `definition`/`references`/`rename` are registered
  whenever `config.lsp || config.lsp_shared`, and
  `project_lsp_extensions(&config.cwd)` is computed **a few lines later** purely
  to decide pre-warming. In a Ruby, PHP, Java or docs-only tree hrdr ships three
  tool schemas whose only possible outcome is a failed call, with the
  information to suppress them already in hand. This is hermes' `check_fn`
  pattern (a tool absent from the schema unless a predicate passes, TTL-cached
  30 s) applied where hrdr has the same shape of problem. _Caveat:_ it makes the
  tool set — and the prompt — vary with cwd contents, one more axis of prefix
  divergence, and the manifest probe would wrongly hide the tools in a monorepo
  whose `Cargo.toml` is one directory down. **Gate on the union of _configured_
  server extensions, not the pre-warm heuristic.**
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

Declared when the sandbox was specced and deliberately left out of v1, plus what
bring-up turned up. All eight verified still open. The rules that govern this
work are under [Standing constraints](#standing-constraints).

- **No network axis.** Verified: no `--unshare-net` anywhere, and the Seatbelt
  profile emits `(allow network*)`. The declared route was seccomp on Linux;
  note Codex has since moved past that to a MITM proxy with netns routing
  (`codex-rs/network-proxy/`: `proxy.rs` 80 KB, `runtime.rs` 71 KB, `socks5.rs`
  42 KB, `certs.rs` 35 KB) — and that its network approval is
  `Stage::Experimental, default_enabled: false`, so the default codex experience
  is _network denied by the sandbox, no prompt_. **Revisit the mechanism before
  building the old plan.**
- **Bundled `bwrap`.** Verified: `which::which("bwrap")`, no bundled copy. Hosts
  without bubblewrap degrade to Landlock (weaker: no read axis). Codex ships its
  own (`linux-sandbox/src/bundled_bwrap.rs`); doing the same removes the most
  common degradation. Note `linux-sandbox/src/bwrap.rs` is **102 KB** — not a
  thin wrapper.
- **Curated read allow-list for `write` mode.** Reads are unrestricted there by
  decision, so a shell command can read `~/.ssh`: verified that
  `guard_secret_read` is called only from the file tools (`read`, `grep`,
  `lsp_nav`) and has **no shell-side equivalent**. A read allow-list, or a
  secret-path deny-list applied at the mount level, closes it.
- **Windows has no OS layer.** Software path-guard only, permanently for v1;
  AppContainer or a restricted token is the eventual answer (Codex has
  `windows-sandbox-rs/`). Until then every Windows session gets the
  `NO_OS_SANDBOX_NOTICE`.
- **No shell-command pre-flight.** A write outside the roots reaches the model
  as the kernel's `Read-only file system`, not as an explanation. A heuristic
  parser (codex's `parse_command.rs`) could say "this would write outside your
  roots" first — in front of the sandbox, never instead of it. Shares its
  dependency with the shell-parsing finding above.
- **The `git` tool is outside the boundary.** See top-of-list #4 — only `shell`
  and `watch` are wrapped by `sandboxed_shell_command`.
- **macOS Seatbelt has never run.** The profile is pure-tested only; the e2e
  test is `cfg(target_os = "macos")` and no Mac was available. It is also a
  coarsening of codex's `seatbelt_base_policy.sbpl` — no `pseudo-tty`, no
  `/dev/null` write, no `iokit-open`/`user-preference-read` — so the first real
  run should read a denial as **"profile too tight"** before "sandbox broken";
  pty and `/dev/null` writes are the likely first additions.
- **The degradation-notice cell is process-global.** Verified:
  `static SANDBOX_NOTICE: OnceLock<Mutex<(HashSet<String>, VecDeque<String>)>>`,
  one queue for the whole process, so with several agents in flight one agent's
  turn loop can drain a notice another produced. Also the cause of the known
  `sandbox_notice_reaches_the_event_stream` flake. A per-agent channel owned
  beside the policy fixes both.

**Where codex's sandbox has moved past hrdr's** (context for any of the above,
not separate items): its policy is no longer "writable roots" but a
precedence-ordered entry list of `{path, access, missing_path_behavior}` with
`Read|Write|Deny`, most-specific-wins and **deny beats write beats read**, paths
as globs or symbolic tokens
(`Root, Minimal, ProjectRoots{subpath}, Tmpdir, SlashTmp, Unknown` — `Unknown`
retained so newer config degrades to warn-and-ignore); named permission profiles
with `extends` inheritance versus hrdr's flat three-value `SandboxMode`; and a
fourth mode hrdr has no slot for, `PermissionProfile::External { network }` —
"filesystem isolation is enforced by an external caller", which hrdr's
`SandboxMode::None` conflates with "unsandboxed", losing the ability to keep the
network axis while disabling the FS layer.

---

## Tooling / agent capability

- **Memory drift detection.** A periodic prune/verify pass over the `memory`
  store — check each `<slug>.md` still has a `MEMORY.md` pointer (and vice
  versa) and flag/prune stale or contradicted memories. Cheap because
  `rebuild_index` regenerates the pointer index on every mutation (verified), so
  files and index cannot drift **structurally**; what is missing is semantic
  staleness, plus the external-drift guard and usage-tracking halves recorded
  above.
- **LSP diagnostics dedup.** The same diagnostic can surface more than once
  (overlapping ranges / re-published sets); dedupe before showing the model.
- **A revived sub-agent always runs write-capable.** Verified: `SessionState`
  carries no `read_only` field, and `TaskReviveTool` takes the same
  `SubagentSlots` a `task` spawn does — so a revived former read-only explorer
  runs write-capable in the recorded/main dir. Not central to the shipped use
  cases (both target write sub-agents), but a real gap: persist the flag and
  honour it on revive.
- **Skills follow-ups** (feature shipped 2026-07-27 — `hrdr-agent/src/skills.rs`
  plus `prompt::skills_section`). Left out on purpose: no `skill` usage signal
  (nothing records whether the model ever loads one, so there is no evidence for
  or against the listing's wording); no categories, unlike hermes'
  category→skills grouping, which only pays off past a few dozen skills; and a
  body still arrives as one tool result, so a procedure over
  `SKILL_OUTPUT_MAX_BYTES` (24 KiB) spills to a file the model must read.

---

## Consistency / robustness

- **Guardrail rules live in two places.** The rule set is encoded both in
  `hrdr-tools/src/guardrails.rs` (mechanical enforcement) and in the prompt
  fragments under `hrdr-agent/src/templates/*.md` (guidance telling the model
  not to attempt them). Adding a rule means editing both, or they drift. Not
  worth auto-deriving — the prompt phrasing is deliberately more nuanced than
  the terse guardrail messages — but a checklist/test that the two sets agree
  would catch drift. (Runtime-composed descriptions, above, would remove the
  duplication by construction; pi does exactly that.)
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
- **Cookie-attempt rate-limiting** — verified: `check_auth`'s `AuthMode::Users`
  branch returns 401 on an invalid `hrdr_session` cookie through an early
  `return Err(...)` that **skips** `rate_limit_record`. The cookie is
  HMAC-signed so this is not brute-forceable; counting attempts is still
  cleaner.
- **WebHost chrome posters** — verified: `WebHost`'s `CommandHost` impl
  overrides **neither** `identity_poster` nor `context_window_poster`, so an
  async `/model` switch updates chrome only via the agent's republish; and a
  failed autosave is silent (no web equivalent of the TUI's
  `record_session_save` notice).
- **WS origin check allows any localhost port** — verified: `check_ws_origin`
  returns `Ok` when the origin host is `localhost`, `127.0.0.1` or `[::1]`,
  **whatever the port**, so a malicious page served by another local app could
  open a WS with the victim's cookie. Tighten the localhost allowance to the
  served port.

---

## Test coverage gaps

- **TUI history up/down fix** (`6ff0172`, `suppress_completions`) shipped
  without a regression test — verified: `suppress_completions` appears in
  `app.rs` and **nowhere in the e2e suite**. Wanted: Up/Down after a
  slash-command history entry navigates history rather than the completion
  popup.
- **Wire log on the native backends.** `error_response` and `sse` records are
  emitted by `anthropic.rs`/`codex.rs` but untested — backend selection keys on
  the host, so a mock server on `127.0.0.1` cannot reach those paths. Verified:
  `wire_log_native_backends.rs` filters on `kind == "request"` only.

---

## Known behaviour to revisit

Not bugs; things whose surprise is worth having written down.

- **Building a sandbox policy touches the parent repo's `.git`.**
  `git_metadata_roots` `create_dir_all`s `refs/heads/hrdr` and
  `logs/refs/heads/hrdr` so they exist to be canonicalized and bind-mounted — so
  `Agent::new` with a linked-worktree cwd creates those two dirs in the
  **parent** `.git` at construction time. Harmless (git ignores empty ref dirs),
  but constructing an agent is not read-only with respect to the repo.
- **A worktree commit can print a `packed-refs.lock` EROFS line.** Ref
  maintenance triggered by a commit inside a sandboxed worktree may fail to
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
  not want; scheduled work for a coding agent lives in CI, and `watch` covers
  the intra-session case. Note the second-order cost hermes paid: cron sessions
  poisoned session-search ranking badly enough to need a demotion tier. The one
  detail worth stealing if hrdr ever ships anything scheduled is the _posture_ —
  cron runs get `skip_memory=True` unconditionally because _"cron system prompts
  would corrupt user representations"_, and approvals fail **closed** there.
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

- **Sub-agent filesystem isolation — all four peers lack it.** codex has two
  generations of sub-agent tooling and no worktrees; hermes' children share the
  parent's cwd while its own tool description claims _"separate working
  directory"_ (its `delegate_tool.py` has zero `worktree|mkdtemp|os.chdir|cwd=`
  matches) and default `max_concurrent_children` is 3; opencode's child session
  runs in the same directory (its `worktree/index.ts` exists but is not
  referenced from `task.ts`); pi's exists only as a 1015-line example spawning
  `pi --json` subprocesses with no isolation. **hrdr's git-worktree isolation is
  unique across all four, and `task_cleanup`'s merge verification has no peer.**
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
  `task_diff` is the mechanical answer); and skill loading that fails **open**
  (opencode logs and skips a YAML error, silently drops a file that fails its
  shape check).

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
