# hrdr vs other coding-agent harnesses

**Date:** 2026-07-26 · **Method:** one sub-agent per harness, each reading
_both_ hrdr and its target, each given the same set of preliminary claims to
verify or refute. Findings below are the hardened output, not the first pass.

Harnesses compared (cloned in `~/Projects/harness/`):

| Harness        | Repo                        | Language   | Size |
| -------------- | --------------------------- | ---------- | ---- |
| `codex`        | `openai/codex`              | Rust       | 83M  |
| `hermes-agent` | `NousResearch/hermes-agent` | Python     | 245M |
| `opencode`     | `sst/opencode`              | TypeScript | 165M |
| `pi`           | `earendil-works/pi`         | TypeScript | 29M  |

Codex is single-provider and post-trains its own models. Hermes is a
general-purpose assistant, not a coding agent. Pi is a toolkit/SDK with a
coding-agent CLI on top. Opencode is the closest peer in kind. Differences that
follow from those product shapes are recorded as **deliberate**, not as gaps —
mixing the two is how a comparison like this turns into a bad backlog.

---

## hrdr as of this comparison

The baseline everything below is measured against.

**System prompt** — one Jinja template,
`crates/hrdr-agent/src/templates/system.j2`, 705 lines, rendered once per
session. **No per-model specialisation**: every provider and model gets the same
bytes. Ten conditionals, all gating on _capability_ rather than on model or tool
identity:

- `can_write` (has mutating tools), `has_shell`, `shell_posix`, `can_delegate`,
  `is_subagent`, `instructions`

Nineteen named sections: Cardinal rules, Workflow, Reporting, Memory, Untrusted
content, Safety, Saving memory, Scope, Editing, Tests, Debugging, Git,
Releasing, Deleting, Verifying, Shell, Committing, Delegating with `task`,
Delegating to a model the user named.

The Environment block (tool names, OS, date, shell, cwd) is appended _last_ by
`prompt.rs::append_environment` so that the working directory — the one line
that differs between sibling write sub-agents — stays at the tail and leaves
every byte before it a shared cache prefix.

**Tools (~30).** File/search: `read`, `write`, `edit`, `replace`, `move`,
`copy`, `delete`, `ls`, `tree`, `find`, `grep`. Shell: `shell` (bash, then POSIX
`sh`; presence-gated). LSP navigation as separate tools: `definition`,
`references`, `rename`. Also `git`, `memory`, `todo`, `watch`, `fetch`,
`search`. Delegation family: `task`, `task_steer`, `task_cancel`, `task_list`,
`task_output`, `task_diff`, `task_cleanup`, `task_revive`, plus `models`.

**Features.** Provider-agnostic client with native Anthropic/Codex paths;
sub-agents with five built-in profiles (`explore`, `review`, `plan`, `coder`,
`general`) in git worktrees; skills (10 built-in) with YAML frontmatter; memory
tool (project/global); mechanical shell guardrails; post-edit hooks and
lifecycle hooks; MCP client (stdio/HTTP/SSE); sessions with resume, retention
and compression; Ratatui TUI with pickers, themes and an embedded editor
(`hjkl`-based); context compaction; prompt caching.

**Known gaps already tracked** (in `deferred-improvements.md`, not findings
here): no ask-the-user tool, no enforced plan mode, no network dimension in
guardrails, no OS sandbox (issue #13), no evals.

---

<!-- Findings are appended per harness as each comparison lands. -->

## codex

Comparison run 2026-07-26. Two of my five preliminary claims were **refuted** —
recorded below because the wrong version is the one that sounds plausible.

### Corrections to the shallow reading

**The six `codex-rs/core/*_prompt.md` files are dead code.** I had reported them
as "a separate full prompt per model, selected at runtime". Five have zero
references anywhere in the tree; the sixth
(`prompt_with_apply_patch_instructions.md`) is `include_str!`'d only at
`codex-rs/core/src/session/tests.rs:1306`, in a test whose every case sets
`expects_apply_patch_description: false`, so the assertion never fires. Verified
independently. Two of the six are byte-identical to each other.

**The real mechanism is a remote model catalog, and it is more interesting than
what I claimed.** `base_instructions` is a per-model `String` on `ModelInfo`
(`protocol/src/openai_models.rs`, consumed
`models-manager/src/model_info.rs:53`), fetched over HTTP with ETag caching and
three refresh strategies (`models-manager/src/manager.rs:54-58`, `:384-402`).
Fallback chain: bundled `models-manager/models.json` (325 KB, 8 models, 8
`base_instructions`) then a single generic `models-manager/prompt.md` for
unknown slugs. The checked-in `.md` files are stale snapshots — a diff of
`gpt_5_2_prompt.md` against the catalog's served `gpt-5.2` instructions shows
one substantive divergence (approval mode "never" vs "never or on-failure").

Beyond per-model prompt _text_, the catalog also carries per-model
`instructions_template` with a `{{ personality }}` placeholder plus
`personality_{default,friendly,pragmatic}` variables, and
`ModelMessages{approvals, auto_review, permissions}` — so even the
approval-policy prose and the Guardian reviewer's policy prompt are remotely
patchable per model slug (`protocol/src/openai_models.rs:506-533`).

The structural trend across catalog models is the real lesson: **tool guidance
leaves the system prompt entirely after gpt-5.2.** gpt-5.2 has a
`# Tool Guidelines` section (298 lines total); gpt-5.4 (107), gpt-5.5 (138) and
gpt-5.6-sol (167) have none — per-tool behaviour moved into runtime-composed
tool descriptions and post-training.

**`spec_plan.rs` is not task planning — REFUTED.** It means "plan the tool
**specs** for this turn" (`core/src/tools/spec_plan.rs:176`
`build_tool_specs_and_registry`, accumulator `struct PlannedTools`). Codex's
actual planning affordance is the `update_plan` tool
(`core/src/tools/handlers/plan.rs:50`), a direct analog of hrdr's `todo`. There
is no runtime-enforced plan mode anywhere in codex. **hrdr is at parity; nothing
to adopt.**

**Network approval is real but opt-in — do not write it up as a default.**
`Feature::NetworkProxy` is `Stage::Experimental, default_enabled: false`
(`features/src/lib.rs:1051-1059`), and the decider is wired only when managed
network requirements are configured. The default codex experience is _network
denied by the sandbox, with no prompt_. The mechanism, where enabled, is a
managed MITM HTTP/SOCKS proxy — `network_approval.rs` is the allowlist-miss
callback (`core/src/tools/network_approval.rs:517-533`), not a syscall
interceptor; the sandbox's job is to force traffic into the proxy
(`linux-sandbox/src/bwrap.rs:279-280`, `sandboxing/src/seatbelt.rs:300-322`,
which fails closed to an empty policy if no proxy endpoint is usable).

**Code mode is shipped and mandatory on codex's newest models.** The `code_mode`
feature flag is default-false, but that is not the gate: `effective_tool_mode`
(`core/src/tools/mod.rs:63-73`) reads `model_info.tool_mode` first, and
`gpt-5.6-{sol,terra,luna}` all declare `"tool_mode": "code_mode_only"` — hiding
every nested tool, leaving the model `exec` (raw JS in a fresh V8 isolate) and
`wait`. Raw `rusty_v8`, not `deno_core`.

### Findings hrdr should act on

**1. Protect `.git` inside a writable root.** Highest value per unit of work,
and it plugs a hole in hrdr's own plan. Codex keeps `.git`, `.agents`, `.codex`
non-writable _inside_ a writable root unless explicitly granted
(`protocol/src/permissions.rs:22-38` `PROTECTED_METADATA_PATH_NAMES`, `:41-70`
`forbidden_agent_metadata_write`), with the rationale in-tree at
`protocol/src/protocol.rs:1058-1062`: folders whose contents _"could be modified
to escalate the privileges of the agent (e.g. `.codex`, `.git`, notably
`.git/hooks`)"_. hrdr's `docs/sandbox-design.md` never mentions git metadata,
and its slice-2 path guard checks only "is this under a writable root" — a
worktree's `.git` _is_ under the worktree. So even after the full sandbox ships,
a write sub-agent could install `.git/hooks/pre-commit` that runs on the
parent's next commit: a worse version of the incident that motivated the design.
Lands independently of issue #13. Subsumes the tracked "sub-agent isolation
guard" item with something mechanical rather than telemetry. _Caveat:_ hrdr's
own `task_*` plumbing legitimately writes git metadata (worktrees, commits,
cherry-picks), so the rule must be "the **model's** file tools may not; hrdr's
git plumbing may" — a `ToolContext` distinction hrdr does not currently draw.
And `git commit` via `shell` writes `.git/index`, so the shell-side half is only
enforceable by the OS layer.

**2. World-state diff fragments instead of silently rewriting the prompt.**
Codex decomposes mutable model-visible state into typed sections
(`core/src/context/world_state/mod.rs:196-227`, ten of them) and emits, per
sampling step, a **developer-role fragment containing only the delta**, wrapped
in stable XML markers, byte-budgeted per section, with snapshots persisted into
the rollout and advanced by RFC 7386 merge patches. hrdr instead rebuilds the
whole system prompt and **replaces `messages[0]` in place** when memory or
project docs change (`crates/hrdr-agent/src/lib.rs:1478-1490`). Two costs: the
model gets different bytes with no signal anything changed (it cannot notice a
tool disappeared), and rewriting the prefix invalidates the prompt cache that
`prompt.rs:96-100` goes to real trouble to protect. _Caveat:_ hrdr's volatile
set is much smaller — no plugins, apps, environments or collaboration modes. If
the honest list is "memory changed, AGENTS.md changed", one appended
`# Context update` developer message gets most of the value for a tenth of the
machinery. Do the cheap version first.

**3. Runtime-built tool descriptions.** Codex builds `description` as a `String`
during spec planning: `spawn_agent` embeds the live model list
(`handlers/multi_agents_spec.rs:68-69`), `tool_search` interpolates the enabled
source list, `shell_command` branches on `cfg!(windows)`. hrdr's
`Tool::description()` returns `&'static str` (`hrdr-tools/src/lib.rs:965`),
which is why `ShellTool` can vary by dialect (a compile-time-enumerable set) but
cannot interpolate runtime values. Three live consequences: `task`'s description
can't list the actually-configured models; the guardrail-message duplication
between `guardrails.rs` and `system.j2` that hrdr already tracks as a drift risk
could be eliminated by construction; and if the sandbox ships, writable roots
can't appear in the descriptions of the tools they constrain — which is
precisely the "positive declaration" lesson `sandbox-design.md:212-220` says to
copy. _Caveat:_ `&'static str` buys testability and cache-stability;
interpolating runtime values means the schema changes between turns,
invalidating caches. If the only real case is "list configured models in
`task`", putting it in `parameters()` (already a runtime `serde_json::Value`) is
cheaper and cache-equivalent.

**4. Per-call permission escalation in the tool schema.** Codex's shell schemas
carry `sandbox_permissions` (`use_default` | `with_additional_permissions` |
`require_escalated`), `justification`, `prefix_rule`, and
`additional_permissions` (`handlers/shell_spec.rs:298-403`), with outcomes
`ApprovedForSession` and `ApprovedExecpolicyAmendment` — the latter appending a
durable rule to `$CODEX_HOME/rules/default.rules`. hrdr's guardrails are
terminal: `bail!("command blocked: …")` with no approval mechanism of any kind.
When a guardrail is wrong — `git add -A` in a repo the user genuinely wants
fully staged — the agent can only give up or work around the regex. _Caveat:_
this cuts against hrdr's autonomy posture (headless runs, NDJSON, cost caps
assume nobody is watching). A smaller defensible version: keep guardrails
terminal but allow an explicit `override_guardrail: "<rule>"` argument that logs
loudly and is refused unless config opted that rule into overridable — no
interactivity, same escape valve.

**5. Memory usage tracking.** Codex tracks whether memories are actually _used_,
via citation blocks (`memories/read/src/citations.rs:6-70`) and by parsing the
model's shell commands for reads of memory paths
(`memories/read/src/usage.rs:30-75`), feeding `usage_count`/`last_usage` into
pruning. hrdr's tracked "memory drift detection" item covers structural drift;
this is the _semantic_ staleness half it's missing, and hrdr is well-positioned
— its `read`/`grep` calls under the memory dir are directly observable with no
bash parsing needed. _Caveat:_ usage count is a bad proxy for memories whose
value is preventing a mistake. hrdr's own `no-migration-pre-1.0` and
`work-directly-on-main` entries earn their keep by being _injected_, never by
being read — counting reads would prune the most valuable ones first. Any
adoption must distinguish "injected, therefore always used" from "topic file
nobody opened".

**6. Deferred tool loading + `tool_search`** (ranked last deliberately). Codex
has `ToolExposure::{Direct, Deferred, DirectModelOnly, Hidden}` plus a BM25
search tool over withheld tool metadata; MCP tools and the V1 sub-agent family
default to `Deferred`. All 8 catalog models set `supports_search_tool: true`.
hrdr sends all ~30 defs every request. _Caveat, and it's decisive:_ 30 tools is
~4-6k tokens on a 272k-context model, and hrdr's caching means it's usually
cached anyway. Codex needs this because it has hundreds of MCP/plugin/connector
tools. **Do it if MCP tool counts get large, not now.**

### Where hrdr is ahead

1. **Semantic code navigation — codex has none.** A case-insensitive sweep of
   `codex-rs/` for `lsp`, `language.?server`, `textDocument/definition` returns
   zero substantive hits. Codex's model finds symbols with `grep` through a
   shell. hrdr ships `definition`/`references`/`rename`
   (`tools/lsp_nav.rs:200,247,362`) over a 74 KB LSP client. Workspace-wide
   semantic `rename` has no shell equivalent — the closest is a `sed` sweep that
   corrupts comments and strings. **hrdr's clearest lead, and it isn't close.**
2. **A structured file/search/git surface.** Codex's entire model-visible file
   surface is `exec_command` + `apply_patch`. hrdr has 11 file/search tools plus
   `git`, all with uniform bounded output through `truncate_saved` (5120 bytes /
   50 lines) spilling to a re-readable pointer file. Codex truncates only shell
   output, so `cat` of a 5 MB file costs a round trip to discover.
3. **USD cost budgeting.** Nothing comparable client-side in codex — grep for
   `max_cost|cost_limit|price_per` yields only backend DTOs. hrdr prices from
   models.dev, counts sub-agents, stops before the next call, and refuses an
   unpriced model under a cap unless explicitly allowed (reporting `"≥ $X"`).
   Codex bills against a ChatGPT plan and doesn't need it.
4. **Mid-history pruning with an ROI gate.** hrdr replaces old tool bodies with
   pointers, keeping a recent window verbatim, **only when the reclaim beats the
   cost** (`compaction.rs:55,68,338,398`). Codex has only the two endpoints —
   truncate at capture, full auto-compact at 90% — with no rung in between (grep
   for `prune` in `core/src/` returns only unified-exec eviction).
5. **Concurrency that cannot corrupt the tree.** hrdr's `concurrent()` defaults
   to `read_only()`, so mutating tools are a strict barrier. Codex's
   `shell_command` _and_ `exec_command` both declare
   `supports_parallel_tool_calls() -> true` with no path-level locking — two
   concurrent codex shell commands can race on the same file. Notably its
   dedicated `apply_patch` _is_ serialized, which is arguably the wrong way
   round.
6. **One codepath for main and sub agents.** hrdr's standing constraint is
   exactly the invariant codex does not hold: it ships **two** generations of
   sub-agent tooling side by side (`multi_agent_v1` and `multi_agents_v2/`),
   selected per model by a catalog field. `handlers/multi_agents_tests.rs` is
   **151 KB** — the cost of carrying both.
7. **Merge-aware cleanup.** `task_cleanup` refuses to remove a worktree with
   uncommitted changes or unreachable branch commits, requiring `force: true`
   (`delegation.rs:2352-2361`). Codex's `close_agent` has no such gate.
   **Correction to my earlier claim:** codex _does_ have `resume_agent`, so
   `task_revive` is **not** unique. hrdr's narrower distinction is reviving runs
   persisted on disk from _earlier sessions_ by `NNN-slug` stem.
8. **Three MCP transports vs two** — hrdr adds legacy HTTP+SSE; codex has only
   Stdio and StreamableHttp (`config/src/mcp_types.rs:433-463`).

### Deliberate differences, not gaps

- **Server-delivered model behaviour.** `ModelInfo` carries `base_instructions`,
  `model_messages`, `shell_type`, `apply_patch_tool_type`, `tool_mode`,
  `multi_agent_version`, `truncation_policy`, `supports_search_tool`, and more —
  OpenAI reshapes client behaviour per model without shipping a binary. hrdr's
  models.dev catalog carries context windows and pricing because no
  multi-provider catalog has those fields. Different supply chain. The
  transferable _pattern_ is that codex made per-model variation a matter of
  **data, not code**.
- **Prompt length as a post-training artifact.** gpt-5.6's 167-line prompt is
  short because OpenAI trained the behaviour in. hrdr's 705 lines must steer
  Claude, deepseek, grok and whatever a local llama.cpp serves. **The line-count
  delta is not a finding.**
- **Enterprise managed policy** — `config/src/config_requirements.rs` is 147 KB
  of MDM/enterprise layers with `can_set` checks forbidding users from
  _loosening_ permissions. hrdr has one user.
- **Guardian LLM approval reviewer** — a sub-agent on a separate model judging
  escalations against an embedded risk taxonomy, with timeouts and a rejection
  circuit breaker. Requires a cheap, trusted, fast second model: what a
  single-provider vendor has and a multi-provider tool does not.
- **Code mode / V8, MCP server mode, plugins, marketplace, connectors, cloud
  tasks, realtime, image gen.** Hosted-consumer-product surface.
- **Interactive approvals as a posture.** Codex's four-way `AskForApproval`
  exists because a human is usually watching.

### Stale in hrdr's own sandbox design doc

`docs/sandbox-design.md` describes codex accurately on backends (bwrap primary
with a bundled copy — note `linux-sandbox/src/bwrap.rs` is **102 KB**, not a
thin wrapper — Landlock, seccomp, seatbelt, Windows crate) and on "on by
default". It is stale in five ways:

1. **The policy is no longer "writable roots".** It is a precedence-ordered
   entry list (`FileSystemSandboxPolicy { kind, glob_scan_max_depth, entries }`,
   `protocol/src/permissions.rs:223-228`) of
   `{path, access, missing_path_behavior}` with `Read|Write|Deny`,
   most-specific-wins, and **deny beats write beats read** at equal specificity.
   Paths may be globs or symbolic tokens
   (`Root, Minimal, ProjectRoots{subpath}, Tmpdir, SlashTmp, Unknown`) —
   `Unknown` retained deliberately so newer config degrades to warn-and-ignore.
2. **Protected workspace metadata is missing entirely** — see finding 1, the
   item most relevant to hrdr's motivating incident.
3. **Network is a MITM proxy, not a seccomp follow-up**
   (`codex-rs/network-proxy/`: `proxy.rs` 80 KB, `runtime.rs` 71 KB, `socks5.rs`
   42 KB, `certs.rs` 35 KB, plus netns routing). The doc frames it as seccomp at
   `:155-157`.
4. **Named permission profiles with inheritance** (`[permissions.<id>]` with
   `extends`, built-ins `:read-only` / `:workspace` / `:danger-full-access`)
   versus the doc's flat three-value enum.
5. **A fourth mode the doc has no slot for:**
   `PermissionProfile::External { network }` — "filesystem isolation is enforced
   by an external caller". hrdr's proposed `SandboxMode::None` conflates
   "unsandboxed" with "sandboxed by my container", losing the ability to keep
   the network axis while disabling the FS layer.

Also understated: codex's _software_ layer is a real parser, not regexes —
`shell-command/src/parse_command.rs` (82 KB) producing
`ParsedCommand::{Read, Search, ListFiles, Unknown}` — and `execpolicy/` is a
Starlark rule DSL with load-time unit tests on each rule.

### What could not be verified

Live catalog values (all per-model claims rest on the bundled fallback
`models.json`; the catalog is fetched at runtime and was not fetched). Whether
the six dead `.md` files serve some out-of-band purpose in OpenAI's internal
monorepo. Whether the SOCKS5 and MITM paths route through the same approval
decider (only `http_proxy.rs` confirmed). Any cap on simultaneous tool
executions outside `parallel.rs`. **Real assembled per-turn context size for
either tool** — neither binary was instrumented, so any "codex sends less" claim
is unproven; its short base prompt is offset by ~6.4 KB of permission templates
plus world-state fragments, skills and plugin instructions.

## pi

Comparison run 2026-07-26. All five claims confirmed, but two were
**understated** — and this pass found **two real defects in hrdr**, both
verified independently.

### Defects in hrdr found by this comparison

**1. The unconditional preamble names tools some agents don't have.**
`system.j2:24` says _"Find the relevant code with grep/find/ls/tree/read before
changing anything"_ and `:27` says _"For multi-step work, plan with todo…"_.
Both sit **above the first gate** (`can_write` opens at `:101`), so every agent
gets them. But:

- `TodoTool` has no `read_only()` override, and the trait default is `false`
  (`hrdr-tools/src/lib.rs:972-974`), so `todo` is excluded from
  `read_only_names()` (`:1093-1098`) and dropped by `retain_only` for read-only
  sub-agents. **Our `explore`, `review` and `plan` profiles are told to plan
  with a tool they do not have.**
- A custom agent file with a `tools:` allow-list (`agents_dir.rs:255`) — e.g.
  `tools: Read, Grep, Bash` — is told to use `find`/`ls`/`tree`/`todo` it lacks.

Cheapest fix: reword those two lines to not name specific tools, plus a test
that every tool named in the unconditional block is in `read_only_names()`.

**2. hrdr's skills are invisible to the model.** `grep -ci skill` over both
`system.j2` and `prompt.rs` returns **0**. Skills are parsed
(`hrdr-app/src/skills.rs:330-352`) and fed to the slash-command popup; on
invocation the body is sent as a user message. So all 10 built-in skills and
every user skill are user-invocable only — **the model can never decide "this is
a release, load the release checklist."** See finding 3.

### Corrections to the shallow reading

**The prompt-size gap is ~16x, not the 4.3x my file line counts implied.**
Measured end to end (pi reconstructed from its template literal plus each tool's
live snippet; hrdr by re-implementing `system.j2`'s gate logic — both ±5%,
tokens as chars/4):

| agent                               | lines | chars  | ~tokens |
| ----------------------------------- | ----- | ------ | ------- |
| hrdr main, write+shell+delegate     | 627   | 41,480 | ~10,400 |
| hrdr main, write+shell, no delegate | 468   | 29,799 | ~7,450  |
| hrdr write sub-agent                | 502   | 31,850 | ~7,960  |
| hrdr read-only sub-agent            | 108   | 6,453  | ~1,610  |
| **pi, default 7 tools**             | 34    | 2,616  | ~650    |

The `can_write` block alone is 397 lines (`:101-497`); `can_delegate` is 161
(`:499-659`).

**The architectural reason pi can be that small is the actual finding, and it is
not "pi trusts the model more".** 8 of pi's 34 prompt lines point the model at
32 on-disk markdown docs — `extensions.md` alone is 115 KB — and tell it to
`read` them on demand (`system-prompt.ts:135-140`). **Harness knowledge is paged
in, not resident.** hrdr's equivalent knowledge is resident in `system.j2` every
turn.

**pi has no per-model prompts, but richer per-model handling than hrdr one layer
down.** `packages/ai/src/types.ts:510-641` is a generated capability catalog:
`supportsStrictMode`, `supportsToolSearch`, `supportsTemperature`,
`supportsReasoningEffort`, `supportsDeveloperRole`,
`supportsCacheControlOnTools`, `thinkingLevelMap`, `deferredToolsMode`, and
more, plus hand-written model matching where the catalog can't reach
(`bedrock-converse-stream.ts:578-624`). It also repairs tool arguments per
model: `prepareEditArguments` re-parses `edits` when it arrives as a JSON
string, commented _"Some models (Opus 4.6, GLM-5.1) send edits as a JSON string
instead of an array"_ (`edit.ts:101-118`). **hrdr's only argument tolerance is
`serde(alias)` on `read`** — `grep -rn "alias = "` over `hrdr-tools/src/tools/`
hits `read.rs` and nowhere else. `edit`, `write` and `replace` — the calls that
cost most when they fail — have none.

### Findings hrdr should act on

**1. Batched `edits[]` with overlap detection.** Already on our backlog; pi
supplies the design _and_ the two hard parts our entry omits.
`edits: Array<{oldText, newText}>` (`edit.ts:44-53`); every edit matched against
the **original** content, uniqueness enforced per edit, sorted by match offset,
**overlaps rejected naming both indices** (`edit-diff.ts:332-354`), applied in
reverse so offsets stay stable, all-or-nothing before a single write. Four
prompt guidelines teach the contract including the non-obvious _"matched against
the original file, not after earlier edits are applied"_. This compounds for
hrdr specifically: every mutating call is a **serialization barrier**
(`turn_loop.rs:601-624`), so four hunks in one file is four serial calls, four
schema payloads, four echoed diffs. Keep the single-hunk shape as an alias so no
prompt churn is forced. _Caveat:_ a batched edit is a bigger blast radius per
barrier — one bad `oldText` in six currently costs the whole call. Needs
per-index errors or retry cost eats the win.

**2. Fuzzy `oldText` matching that preserves unchanged lines.** hrdr already
_detects_ this class of failure and writes a good message — _"a near-match
differing only in whitespace/indentation exists"_ (`edit.rs:167-178`) — but
still **fails the call**. pi succeeds: exact match first, then retry in a
normalized space (NFKC, per-line `trimEnd`, smart quotes → ASCII, 7 dash and 10
space variants → plain). The clever part is not the tolerance but not corrupting
the file: `applyReplacementsPreservingUnchangedLines` (`edit-diff.ts:131-172`)
widens each replacement to the lines it touches, rewrites only those from
normalized space, and copies every other line back byte-for-byte, with a guard
that duplicate normalized lines can't be aligned to the wrong occurrence and a
line-count assertion. Note hrdr has a CRLF retry (`edit.rs:148-165`) but no
trailing-whitespace or quote retry — and `read` clips at `MAX_LINE`, so the
model's view can differ from disk in exactly these ways. _Caveat:_ fuzzy
matching in an edit tool is a real correctness hazard — it normalizes Unicode as
a side effect of an unrelated change. If adopted, hrdr should **report** when a
fuzzy match was used; pi tracks `usedFuzzyMatch` (`edit-diff.ts:181-182`) but
doesn't surface it. Cheapest useful subset: trailing whitespace +
quotes/dashes/spaces, no NFKC, no new dependency.

**3. Model-invocable skills with progressive disclosure.**
`formatSkillsForPrompt` (`skills.ts:335-361`) emits a compact
`<available_skills>` block of name + description + absolute path and tells the
model to `read` the file when a task matches. The `Skill` struct deliberately
has **no body field** (`:74-81`) — nothing resident.
`disable-model-invocation: true` hides a skill so only the explicit
`/skill:name` route reaches it. This closes defect 2 above. Cost is low: we
already load, parse and index skills; it's a prompt block plus a frontmatter
key. _Caveat:_ hrdr's slash-only model is a defensible stance — skills are
_user_ intent, and self-selection means the model can pick the wrong one or skip
one you wanted. Also: skill bodies currently arrive as a _user_ message; the
model-invoked path arrives as tool output, a different trust class given
`system.j2:9-10`'s rule that tool output is data, never instruction.
`disable-model-invocation` should be the default for `:release` and `:commit`.

**4. Assemble tool guidance from the live tool set.** pi harvests
`promptSnippet`

- `promptGuidelines` off each registered tool and rebuilds whenever the tool set
  changes (`agent-session.ts:1021-1054`, `:2275`); a tool's advice lives next to
  the tool, and extensions contribute the same way. This makes defect 1
  structurally impossible, and it's the same disease as our tracked "guardrail
  rules live in two places" — guidance stored away from its mechanism. _Caveat,
  and it's decisive:_ the full design costs hrdr prefix-cache sharing across
  differently-tooled sibling sub-agents, which `prompt.rs:19-36` was explicitly
  engineered for — pi puts its varying tool list at prompt lines 3-25,
  destroying prefix sharing. **Do the cheap version:** add
  `prompt_guidelines() -> &[&str]` to the `Tool` trait, inject one block, and
  separately fix the two offending unconditional lines.

**5. Don't execute tool calls from a reply that hit the output cap.** pi: if
`stopReason === "length"`, no tool in that batch executes; each gets a synthetic
error telling the model to re-issue (`agent-loop.ts:374-406`). hrdr detects the
same condition (`hrdr-llm/src/types.rs:620-624`) and responds only with a
**user-facing** `AgentEvent::Notice` (`turn_loop.rs:538-544`) — the model is
never told, and execution proceeds. **Smaller for hrdr than for pi**, because
hrdr parses args with strict `serde_json` and has no salvage parser, so
truncated args usually fail to parse anyway. Residual gap: earlier complete
calls execute and the model resumes with no signal it lost the calls it
intended. One-line fix — append the warning to the last tool result, the shape
hrdr already uses for the round-budget wrap-up (`turn_loop.rs:635-643`).

**6. Expose session/model metadata to shell commands.** pi injects
`PI_SESSION_ID`, `PI_SESSION_FILE`, `PI_PROVIDER`, `PI_MODEL`,
`PI_REASONING_LEVEL` (deleting inherited copies first, `bash.ts:162-181`) and
tells the model they exist. hrdr's `Shell::command` configures program + args
only (`tools/shell.rs:65-70`) — a script the agent runs cannot discover which
model drives it or where the transcript is. _Caveat:_ widens what leaks into
every subprocess and its logs. pi's `exposeSessionEnvironment` toggle is the
right shape; hrdr should default it **off**.

**Explicitly not recommended: pi's rg/fd auto-download.** pi downloads ripgrep
if absent (`tools-manager.ts:326-370`), deleting hrdr's three-backend divergence
outright. But its downloader does **no integrity checking** — grep for
`createHash|sha|verify` in that file returns nothing; it resolves
`releases/latest` at runtime and executes what comes back. That's a worse trade
than a POSIX fallback, it contradicts hrdr's single-static-binary distribution,
and the degradation ladder is a feature on locked-down machines. The narrower
true finding: **hrdr's grep coverage problem is a test-harness problem** — force
each backend in tests instead of letting host detection decide.

### Where hrdr is ahead

1. **Safety is built in; in pi it's an opt-in code sample.** pi's README states
   it: _"Pi does not include a built-in permission system…"_ (`README.md:39`);
   its answer is containers or a 34-line example with three regexes
   (`examples/extensions/permission-gate.ts`). hrdr ships **15 shell guardrail
   rules** with recovery instructions (`guardrails.rs:44-158`), secret-file
   guards on read/write/edit/copy, and **read-state gating** —
   `ReadState::{Unread, Partial, Stale, Fresh}` with `write` refusing all three
   non-`Fresh` states (`write.rs:58-77`). pi's `write` overwrites
   unconditionally and its `edit` only `access()`-checks. **This is hrdr's
   largest correctness advantage over pi: pi cannot detect that a file changed
   under the model.** Plus TOCTOU-hardened `read` (`read.rs:88-104`).
2. **Sub-agents are a subsystem, not a sample.** pi core has none — grep for
   `subagent|worktree|spawnAgent` across `packages/*/src` finds only a worktree
   _detection_ helper. The capability exists solely as a 1015-line example that
   spawns `pi --json` subprocesses with **no filesystem isolation**, no
   steering, no revive, no merge verification. hrdr: 8 tools, git-worktree
   isolation, `task_steer`, `task_revive`, `task_diff`, and `task_cleanup` that
   verifies work was merged before removing a branch.
3. **MCP.** hrdr has a full client with a `Transport` seam. pi has **none** —
   grep for `mcp` across `packages/*/src` hits exactly one OAuth scope string.
4. **LSP, and specifically post-edit diagnostics folded into the edit result.**
   `apply_file_change` returns `lsp.diagnostics_note` with the success message
   (`tools/mutation.rs:70-75`) — the model learns it broke the build in the same
   tool result, not two rounds later. pi has no LSP at all.
5. **Runaway-loop protection.** hrdr caps rounds, warns three out by appending
   to the last tool result, then runs a final tool-less round forcing a text
   answer (`turn_loop.rs:633-660`), plus a `RepeatGuard`. **pi's `runLoop` is
   `while (true)` with no counter** (`agent-loop.ts:170`); termination is
   entirely the model's choice. No repeat guard.
6. **Durable writes.** hrdr: unique sibling temp, `sync_all`, permission
   preservation, intra-fs rename, with in-place fallback for symlinks and
   hardlink sets (`tools/mutation.rs:96-172`). pi: `fsWriteFile` in place
   (`edit.ts:85`) — no fsync, no permission preservation, no symlink handling; a
   SIGKILL mid-write leaves a truncated file. _pi's counterpart hrdr lacks:_
   `withFileMutationQueue` serializes per **realpath** so different files still
   go in parallel (`file-mutation-queue.ts:31-61`), where hrdr serializes all
   mutation globally. Stronger guarantee, less throughput — and the reason
   batched `edits[]` matters more for hrdr than for pi.
7. **Session retention.** hrdr zstd-compresses idle sessions and purges old
   auto-named ones, sparing user-named via a persisted flag
   (`session.rs:1187-1310`). pi has no compression and no retention; sessions
   accumulate forever, deletable only by hand in the resume picker.
8. **Prefix-cache-aware prompt ordering** (`prompt.rs:19-36`) — documented as an
   invariant. pi only trails the cwd. This is also what makes finding 4
   expensive; worth knowing both facts together.
9. **Evals: pi's is a scaffold, so our absence costs nothing.** `packages/evals`
   is `private: true`, holds **one** case (assert the model answers "Paris"),
   scored by plain `vitest` exact-match with no judge and no dataset, and **is
   not run in CI**. Its harness disables everything pi does (`noTools: "all"`,
   `noExtensions`, `noSkills`). Against hrdr's ~1,300 in-repo tests this is an
   aspiration, not an advantage. **If we build evals, build them because we want
   them.**

### Deliberate differences, not gaps

- **Toolkit vs application.** pi publishes four npm packages because someone
  else's code must reach every seam. hrdr is one static binary with private
  crates. Not being embeddable is a product decision.
- **In-process TS extensions (~35 typed hooks, `extensions/types.ts:1184-1225`,
  including per-turn `systemPrompt` replacement and `before_provider_request`
  payload rewriting) vs hrdr's 6 exit-code subprocess hooks.** pi's give total
  control at the cost of arbitrary TS in-process with full user permissions
  behind a trust prompt; hrdr's can't add a tool or provider but are
  language-agnostic and process-isolated. Different risk appetites, both
  coherent.
- **pi's TUI is a publishable library** (12,189 lines of from-scratch
  framework + 13,687 lines of tests, Kitty keyboard protocol, inline images,
  checked-in native `.node` binaries for macOS modifier polling) with **no mouse
  and no alternate screen**. hrdr's ~15,370 Ratatui lines get diffing, mouse and
  alt-screen free. pi built a framework because it ships one. **Do not read pi's
  native code as a capability hrdr lacks.**
- **Session model: tree vs linear.** pi's JSONL is a tree with `parentId`, non-
  destructive `branch()`, `forkFrom`, and **branch summarization** before you
  navigate away. Serves "explore alternatives"; hrdr's linear fold serves "one
  task, resume it" with `task_revive` for fan-out. _Worth stealing
  independently:_ `branch-summarization.ts:258-285` has a fixed output contract
  (`## Goal` / `## Constraints & Preferences` / `## Progress` with
  `### Done`/`### In Progress`/`### Blocked` / `## Key Decisions` /
  `## Next Steps`) — a better-specified summary format than a free-form
  compaction prompt.
- **Retry topology.** pi has three independent layers and honours
  `x-should-retry` / `retry-after`; hrdr has one with capped backoff plus a
  process-wide jitter counter so concurrent sub-agents don't re-trip a rate
  limit in lockstep (`turn_loop.rs:209-227`). hrdr's jitter reasoning is better;
  pi's header honouring is more correct. Split, and small.

### What could not be verified

Neither prompt was rendered by running its own code — hrdr's numbers come from a
Python re-implementation of the template's gate logic (cross-checked to ~3
lines), pi's from hand-assembly (no `node_modules`/bun available). Token counts
are chars/4. hrdr's 30 tool schemas were not totalled in bytes, so "hrdr pays
~4x more schema tokens" is inference from count. `hrdr-llm/src/catalog.rs` was
not read, so hrdr's model metadata was **not** compared against pi's capability
catalog — the suspicion that pi is richer is unverified and deliberately not
filed as a finding. pi's compaction algorithm, `packages/server/`, RPC mode
(`docs/rpc.md`, 40 KB — possibly already what our `web-ui-plan.md` wants), and
the `sandbox/`+`gondolin/` isolation examples were not read. The
read-only-`todo` defect is verified by construction (each link cited) rather
than observed at runtime — **I confirmed the chain independently.**

## hermes-agent

_Pending._

## opencode

_Pending._

---

## Cross-harness synthesis

_Written once all four are in._
