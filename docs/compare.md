# hrdr vs other coding-agent harnesses

**Date:** 2026-07-26 · **Method:** one sub-agent per harness, each reading
_both_ hrdr and its target, each given the same set of preliminary claims to
verify or refute. Findings below are the hardened output, not the first pass.

> **Status update (2026-07-26, later the same day — commits
> `c5e5ced`..`b1a698f`):** the prompt-architecture findings below shipped.
> `system.j2` is gone — the prompt is now ten `include_str!` markdown fragments
> assembled as an ordered section list
> (`base → global_agents_md → global_memory → project_agents_md → project_memory → capability group → persona → environment`),
> minijinja is out of the workspace, memory is re-gathered at compaction (hermes
> finding 4 / "frozen memory"), persona now precedes environment (hermes sub-nit
> a), and the native Anthropic path places a `cache_control` breakpoint at the
> stable/volatile boundary before the environment block (hermes finding 1).
> Sections referring to `system.j2`, `append_environment`/`append_persona`, or
> the frozen memory index describe the **pre-rewrite** baseline; each affected
> finding carries a **Shipped** note inline.

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

The baseline everything below is measured against. (Snapshot: this section
describes the pre-rewrite prompt — see the status update above for what changed
the same day.)

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
`.git/hooks`)"_. hrdr's sandbox (shipped 2026-07-27) has no such notion: its
path guard asks only "is this under a writable root" — and a worktree's `.git`
_is_ under the worktree. So even with the sandbox on, a write sub-agent could
install `.git/hooks/pre-commit` that runs on the parent's next commit: a worse
version of the incident that motivated the whole design. Lands independently of
issue #13. _Caveat:_ hrdr's own `task_*` plumbing legitimately writes git
metadata (worktrees, commits, cherry-picks), so the rule must be "the
**model's** file tools may not; hrdr's git plumbing may" — a `ToolContext`
distinction hrdr does not currently draw. And `git commit` via `shell` writes
`.git/index`, so the shell-side half is only enforceable by the OS layer.

**2. World-state diff fragments instead of silently rewriting the prompt.**
Codex decomposes mutable model-visible state into typed sections
(`core/src/context/world_state/mod.rs:196-227`, ten of them) and emits, per
sampling step, a **developer-role fragment containing only the delta**, wrapped
in stable XML markers, byte-budgeted per section, with snapshots persisted into
the rollout and advanced by RFC 7386 merge patches.

> **Correction (from the hermes pass, verified independently).** This finding
> originally said hrdr "rebuilds the whole system prompt and replaces
> `messages[0]` in place **when memory or project docs change**." That is wrong,
> and the truth is worse. `refresh_system` has exactly three non-test callers —
> `lib.rs:1399` (MCP connect at startup), `:1439` (`clear()`), `:1749`
> (`set_cwd()`) — and `gather_memory` has exactly two production call sites,
> `:1251` (construction) and `:1469` (inside `refresh_system`). A mid-session
> `memory` write therefore **never** reaches the prompt, and `compact()` clones
> `messages[0]` verbatim (`compaction.rs:607`, `:616`) so it doesn't reach it at
> compaction either. **hrdr's injected memory is frozen for the whole session.**
> See the hermes section, finding 4, for the cheap fix.
>
> **Shipped** (`c5e5ced`, see status update): `compact()` now calls
> `refresh_system_prompt_in_place()`, which re-gathers the memory index and
> rebuilds the prompt before the history is rewritten — the one moment the
> prefix cache is already dead.

So hrdr does not have codex's problem of silently changing bytes — it has the
opposite one. What still stands from this finding is the second half: hrdr has
no way to _tell_ the model that anything changed (a tool appeared, the cwd
moved, memory was written), because the only channel is a prompt rewrite that
mostly never fires. _Caveat:_ hrdr's volatile set is much smaller than codex's —
no plugins, apps, environments or collaboration modes. If the honest list is
"memory changed, AGENTS.md changed", one appended `# Context update` developer
message gets most of the value for a tenth of the machinery. Do the cheap
version first.

**3. Runtime-built tool descriptions.** Codex builds `description` as a `String`
during spec planning: `spawn_agent` embeds the live model list
(`handlers/multi_agents_spec.rs:68-69`), `tool_search` interpolates the enabled
source list, `shell_command` branches on `cfg!(windows)`. hrdr's
`Tool::description()` returns `&'static str` (`hrdr-tools/src/lib.rs:965`),
which is why `ShellTool` can vary by dialect (a compile-time-enumerable set) but
cannot interpolate runtime values. Three live consequences: `task`'s description
can't list the actually-configured models; the guardrail-message duplication
between `guardrails.rs` and `system.j2` that hrdr already tracks as a drift risk
could be eliminated by construction; and now that the sandbox has shipped, its
writable roots can't appear in the descriptions of the tools they constrain —
which is why the "positive declaration" of the boundary had to go into a
runtime-built prompt section (`SECTION_SANDBOX`) instead. _Caveat:_
`&'static str` buys testability and cache-stability; interpolating runtime
values means the schema changes between turns, invalidating caches. If the only
real case is "list configured models in `task`", putting it in `parameters()`
(already a runtime `serde_json::Value`) is cheaper and cache-equivalent.

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
   > **Correction (from the opencode pass, verified).** This is a lead over
   > **codex specifically, not a general one.** Opencode has all three
   > transports (`mcp/index.ts:7-9`) **plus MCP OAuth**
   > (`mcp/oauth-provider.ts`, `oauth-callback.ts`, `auth.ts`, and a
   > `TransportWithAuth` type at `:110`). hrdr's MCP auth is **static headers
   > only** (`mcp/types.rs:46`, `:56`), so on MCP authentication hrdr is
   > _behind_. Also: hrdr exposes MCP **prompts** (`prompts/list`/`prompts/get`,
   > `mcp/client.rs:604`) where opencode has zero references to them — resources
   > are at parity. Net: hrdr ahead on prompts, behind on auth, level on
   > transports against opencode.

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

### Where codex's sandbox has moved past hrdr's

hrdr's sandbox (specced from codex, shipped 2026-07-27) reads codex accurately
on backends (bwrap primary with a bundled copy — note
`linux-sandbox/src/bwrap.rs` is **102 KB**, not a thin wrapper — Landlock,
seccomp, seatbelt, Windows crate) and on "on by default". Codex has since moved
on in five ways, none of which hrdr has:

1. **Codex's policy is no longer "writable roots"** (hrdr's is). It is a
   precedence-ordered entry list
   (`FileSystemSandboxPolicy { kind, glob_scan_max_depth, entries }`,
   `protocol/src/permissions.rs:223-228`) of
   `{path, access, missing_path_behavior}` with `Read|Write|Deny`,
   most-specific-wins, and **deny beats write beats read** at equal specificity.
   Paths may be globs or symbolic tokens
   (`Root, Minimal, ProjectRoots{subpath}, Tmpdir, SlashTmp, Unknown`) —
   `Unknown` retained deliberately so newer config degrades to warn-and-ignore.
2. **Protected workspace metadata is missing entirely from hrdr** — see finding
   1, the item most relevant to hrdr's motivating incident.
3. **Network is a MITM proxy, not a seccomp follow-up**
   (`codex-rs/network-proxy/`: `proxy.rs` 80 KB, `runtime.rs` 71 KB, `socks5.rs`
   42 KB, `certs.rs` 35 KB, plus netns routing). hrdr's deferred network axis is
   still framed as seccomp.
4. **Named permission profiles with inheritance** (`[permissions.<id>]` with
   `extends`, built-ins `:read-only` / `:workspace` / `:danger-full-access`)
   versus hrdr's flat three-value `SandboxMode`.
5. **A fourth mode hrdr has no slot for:**
   `PermissionProfile::External { network }` — "filesystem isolation is enforced
   by an external caller". hrdr's `SandboxMode::None` conflates "unsandboxed"
   with "sandboxed by my container", losing the ability to keep the network axis
   while disabling the FS layer.

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

> **Shipped 2026-07-27.** Discovery/parsing/expansion moved to
> `hrdr-agent/src/skills.rs` (the frontends keep only the completion popup and
> the picker filter), `prompt::skills_section` renders a name + one-line
> description menu as `SECTION_SKILLS` — 956 bytes for the ten built-ins, in the
> cached prefix, bodies excluded and **source paths excluded** so sibling
> worktree sub-agents still share the prefix — and a read-only `skill` tool
> returns the expanded body on demand. The section is gated on that tool being
> registered, so a `tools:` allow-list that drops it drops the menu too.

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

**3. Model-invocable skills with progressive disclosure.** _(Shipped 2026-07-27
— see the **Shipped** note under defect 2 above. hrdr's listing omits the path
pi includes: a write sub-agent's worktree path would differ per sibling and
split the cache prefix, and the tool names the source in its result instead.
pi's `disable-model-invocation` became `model_invocable: false`, and the
trust-class caveat below is answered by the tool result framing the body as the
user's/project's instructions.)_ `formatSkillsForPrompt` (`skills.ts:335-361`)
emits a compact `<available_skills>` block of name + description + absolute path
and tells the model to `read` the file when a task matches. The `Skill` struct
deliberately has **no body field** (`:74-81`) — nothing resident.
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

Comparison run 2026-07-26. Hermes is a general-purpose assistant, so most of its
102 tool modules are irrelevant here and are not enumerated. Two of five claims
came back **partly wrong**, and the pass **corrected a factual error already
published in the codex section** (see the callout above) — which is the
strongest argument for having run four independent comparisons rather than one.

### Corrections to the shallow reading

**The three cache tiers are real, but it's two breakpoints and the docstring
overstates.** `build_system_prompt_parts` (`agent/system_prompt.py:149`) does
return `{stable, context, volatile}`, and the stable tier becomes a genuine API
cache breakpoint via `_apply_system_cache_markers`
(`agent/prompt_caching.py:86-119`). The mechanism is the good part: the prompt
stays **one stored string**, and the split into two `{"type":"text"}` blocks
happens only in the outgoing request, gated on a `startswith` check so a
mismatch degrades silently instead of rewriting bytes. But it marks the stable
prefix and the _whole remainder_ — two markers, not three; the third tier is an
**ordering discipline** keeping volatile content behind the second marker. And
"never re-renders mid-session" is false: `invalidate_system_prompt` (`:571-580`)
exists and runs after compression. The honest rule is _"rebuild only at a
compaction boundary, where the cache is being invalidated anyway."_

**`toolset_distributions.py` is not a runtime mechanism — REFUTED.** Its
docstring is explicit: distributions of toolsets _"for data generation runs…
their probability of being selected for any given prompt during the batch
processing"_ (`:1-20`). It diversifies synthetic training trajectories. Hermes
is a **datagen harness as much as an agent** — `batch_runner.py`,
`mini_swe_runner.py`, `datagen-config-examples/` — because Nous post-trains its
own models. The shipped default is not a selected subset either:
`_HERMES_CORE_TOOLS` is a flat ~60-tool list (`toolsets.py:31-79`).

**`trajectory_compressor.py` is also datagen — REFUTED.** It is an offline
`fire` CLI that post-processes _completed_ trajectories "preserving training
signal quality" (`:1-33`). Hermes' actual runtime compaction is
`agent/conversation_compression.py` (2769 lines). **Do not compare either file
to `compaction.rs`.**

**The real tool-scaling answer is `tools/tool_search.py`, which I wasn't pointed
at.** MCP and non-core plugin tools are replaced by three bridge tools
(`tool_search`/`tool_describe`/`tool_call`); **core tools are never deferred**
(_"Always-load means always-load. No exceptions."_, `:11-13`); the gate is a
**no-op unless deferrable tools would consume more than ~10% of the context
window** (`:14-17`); the catalog is rebuilt every assembly because a
session-keyed one drifted and caused silent tool dropouts; and bridge calls
route through the normal `handle_function_call` so guardrails, hooks and
truncation fire identically. Plus `check_fn` runtime gating (~128 references) —
a tool is absent from the schema entirely unless a predicate passes
(`HASS_TOKEN`, `HERMES_KANBAN_TASK`, cua-driver installed), TTL-cached 30 s.

**SOUL.md precedence is fine, and not a finding.** It does fully replace the
built-in identity (`system_prompt.py:189-198`), but `DEFAULT_AGENT_IDENTITY` is
eight lines of bland persona — **no safety rule, no untrusted-content rule, no
tool contract lives there**. A hostile SOUL.md can change the voice, not delete
the safety floor. hrdr's analog is safe for the opposite reason: persona is
_appended_ with explicit conflict framing ("where it conflicts with the general
guidance above, the role wins", `lib.rs:862-871`).

### Findings hrdr should act on

**1. Put a cache breakpoint at hrdr's own stable/volatile boundary.** Highest
value per unit of work in this entire comparison. hrdr already does the hard
part — a documented prefix-ordering invariant with four positional tests
(`prompt.rs:15-36`, `:430`, `:471`, `:523`) — and then collects none of the
reward on Anthropic native, because `mark_last_block` puts the single system
breakpoint at the **end of the whole prompt**
(`hrdr-llm/src/anthropic.rs:90-96`). Every byte of the carefully-ordered shared
prefix sits in one cache unit that any tail change invalidates. Hermes'
`_apply_system_cache_markers` is the missing ~30 lines, and **hrdr's boundary is
already computed**: `build_system_prompt` (`lib.rs:973-988`) is literally
`render_system` → `append_memory` → `append_environment` → `append_persona`, so
`render_system`'s return value _is_ the stable prefix. Two sessions in different
projects, a session before and after `/clear`, or six sibling sub-agents would
then share a real cached prefix. Two sub-nits found while verifying: **(a)**
`append_persona` runs _after_ `append_environment` (`lib.rs:986-987`), so the
shared per-profile persona sits behind the per-agent `cwd` line — sibling
sub-agents of the same profile can't share it, and it contradicts
`prompt.rs:19-28`'s own claim that cwd is "dead last". One-line reorder. **(b)**
hermes renders its timestamp **date-only** because minute precision invalidates
KV cache on every rebuild (`system_prompt.py:520-528`); hrdr already does this
(`prompt.rs:111`) — worth a comment so nobody "improves" it. _Caveat:_ hrdr's
cache mode is `auto` and only fires where `cache_control` is real
(`config.rs:1767-1798`) — nothing for local llama.cpp or OpenRouter-envelope
routes. Anthropic caps breakpoints at 4 and hrdr spends 3; adding one leaves
zero headroom.

**Shipped** (`b1a698f`): `build_system_prompt` returns the byte offset of the
boundary (`SystemPrompt::prefix_len_before(SECTION_ENVIRONMENT)` — a fold over
section lengths, no substring search), the client carries it as
`system_cache_split`, and the native Anthropic path splits the system prompt
into two marked blocks there. All 4 breakpoints now spent: tools, stable prefix,
system tail, rolling last message. Sub-nit (a) — persona after environment — was
fixed by the section reorder in `c5e5ced`; persona now sits just above the
environment tail, and the breakpoint sits between them. The residuals from the
caveat were closed the same day: a resumed/revived session now rebuilds the
prompt so the split matches the installed text (`5adc9ff`), and the
OpenRouter/OpenAI-shape path (`apply_cache_breakpoints`) emits the system
message as two marked parts at the same boundary (`e02cb5f`).

**2. hrdr injects a cloned repo's `AGENTS.md` into its system prompt unscanned —
and silently drops it if over 64 KiB.** Two defects in one place.
`gather_agent_docs` walks from cwd upward (`prompt.rs:210-229`) and concatenates
whatever it finds under _"Project instructions (from AGENTS.md — follow these
for this project)"_ (`system.j2:699-704`) with zero inspection. `cd` into an
untrusted clone and its `AGENTS.md` becomes system-prompt-level instruction —
driving straight through `system.j2:73-79`'s otherwise-good rule that tool
output is data and never a command, because this text never arrives as tool
output. Hermes runs every context file through `_scan_context_content` first,
blocking with a `[BLOCKED: <file> contained potential prompt injection…]`
placeholder, rationale stated exactly: _"Content matching is BLOCKED at this
layer because the file would otherwise enter the system prompt verbatim and the
user has no chance to intervene"_ (`prompt_builder.py:50-74`). The second half:
`prompt.rs:213` skips any single `AGENTS.md` over 64 KiB **entirely and
silently**. Hermes' own `AGENTS.md` is 73.4 KB — a real file hrdr would ignore
without a word. Hermes caps per-file, **scales the cap to the model's context
window** (6% of window × 4 chars/token, 20 K floor / 500 K ceiling,
`prompt_builder.py:1244-1288`), and surfaces every truncation through the normal
status channel. _Caveat:_ a regex scanner over project docs will false-positive
on exactly the repos a coding agent gets pointed at — security tooling,
shell-hardening guides, this very document. Hermes needed three scopes to make
it tolerable. **The cheap 80%:** emit a notice when a new or changed `AGENTS.md`
is loaded (naming path and byte count) and reframe the block header to
distinguish project file from user instruction. Full scanning can wait for
evidence.

**3. Model-invocable skills — now two of three peers agree.** _(Shipped
2026-07-27; hermes' names-always/descriptions-first degradation is what
`prompt::skills_section` does under its byte budget. Its aggressive framing and
operator-side disable list were not copied.)_ Hermes' `<available_skills>` block
(`prompt_builder.py:1738-1766`) emits **category → `name: description`** only,
never bodies, with `skill_view(name)` to load on demand. Two refinements over
pi's flat list: category headers carry their own descriptions, and a "focus
mode" demotes off-context categories to `[names only]` — descriptions dropped,
names always kept "for recall" (`:1700-1722`). The index lives in the **stable**
tier, so it's cached. This independently confirms pi's finding and closes the
defect recorded in the pi section (`grep -ci skill` over `system.j2`/`prompt.rs`
= 0). _Caveat:_ hermes' framing is aggressive ("## Skills (mandatory)… Err on
the side of loading") — don't copy that verbatim into a coding agent. And
hermes' hide-a- skill mechanism is **worse** than pi's: an operator config list
(`skills.disabled`) that hides a skill from everyone, versus pi's per-skill
author-declared frontmatter. **Take pi's `disable-model-invocation` shape.**

**4. Refresh memory at the compaction boundary.** hrdr's injected memory is
frozen for the entire session (verified above). An agent that saves "this
project pins hjkl 0.33.6" at minute five still can't see it at minute ninety,
and a hundred-turn session that compacts three times never picks it up. Hermes
freezes deliberately for the same cache reason (`memory_tool.py:684-695`:
_"returns the state captured at `load_from_disk()` time, NOT the live state…
preserving the prefix cache"_) — but reloads at the one moment the cache dies
anyway: `invalidate_system_prompt` clears the cached prompt **and** calls
`load_from_disk()` (`system_prompt.py:571-580`). Fix is trivial: re-gather
memory inside `compact()`. _Caveat:_ it makes the post-compaction prompt differ
in a second way at once, complicating diagnosis of a bad compaction. And hrdr's
memory block is an _index_ whose topic files the model can `read` any time, so
the miss is narrower than in hermes, which injects full entry text. Two related
notes: hermes has **no** memory usage tracking at all (grep for
`usage_count`/`citation` in `memory_tool.py` returns nothing) — **codex remains
the only harness doing usage-informed pruning**. Conversely hermes has a guard
hrdr lacks: `_detect_external_drift` refuses a full-file rewrite when on-disk
content wouldn't round-trip through the tool's own parser (manual edit, sibling
session), backing up to `.bak.<ts>` instead of clobbering (`:93-120`,
`:344-363`) — directly relevant to hrdr's tracked memory-drift item, which is
currently scoped only to index/pointer structure.

**Shipped** (`c5e5ced`): memory re-gathers at the compaction boundary — see the
correction note in the codex section. The drift-guard note (hermes'
`_detect_external_drift`) still stands as an open idea, folded into the tracked
memory-drift item.

**5. Gate LSP tool registration on the project having a matching server.** hrdr
registers `definition`/`references`/`rename` whenever `config.lsp` is on
(`lib.rs:1073-1077`), then **two lines later** computes
`project_lsp_extensions(&config.cwd)` to decide whether pre-warming is worth it
(`:1085-1089`) — probing nine manifests and returning empty for anything else.
So in a Ruby, PHP, Java or docs-only tree, hrdr ships three tool schemas whose
only possible outcome is a failed call, with the information needed to suppress
them already in hand. This is hermes' `check_fn` pattern applied where hrdr has
the same shape of problem. _Caveat:_ it makes the tool set — and the prompt —
vary with cwd contents, one more axis of prefix divergence; and the manifest
probe would wrongly hide the tools in a monorepo whose `Cargo.toml` is one
directory down. Gate on the union of _configured_ server extensions rather than
the pre-warm heuristic.

**6. Session search is a real gap; the ranking lessons are the valuable half.**
hrdr persists sessions as `sessions/<cwd-slug>/<id>.json`, zstd-compressed once
idle. No index, so cross-project recall means walking every slug directory and
decompressing every archive — "what did we decide about the delegation retry
backoff three weeks ago?" is currently unanswerable. Hermes: FTS5, three modes
inferred from args, **zero LLM calls** (`session_search_tool.py:1-33`). Copy two
specifics: **exclude sub-agent sessions from results**
(`_HIDDEN_SESSION_SOURCES = ("subagent","tool")`) — hrdr's on-disk sub-agent
runs are the exact analog and would flood every query; and **demote rather than
exclude** automated sources, because repetitive scheduled-run vocabulary
dominates bare BM25 and starves interactive sessions (issue #19434). _Caveat:_
this is an index, a schema, a retention interaction and a tool — rank it below
1-5. The honest smaller version: grep the current project's slug directory,
decompressing lazily. No FTS engine, most of the value.

**7. Ship the introspection that would let hrdr verify its own prompt claims.**
Both this pass and the codex one closed with the same admission — neither binary
was instrumented, so every size claim in this document is structural, not
measured, and hrdr's prompt had to be **reconstructed in Python** to be counted.
Hermes ships both halves: `agent/context_breakdown.py` computes a live
per-category budget (system prompt, tool definitions, rules, skills index, **MCP
separately from builtin schemas**, sub-agent definitions, memory, conversation)
preferring the provider's measured `last_prompt_tokens` over its own estimate;
and `hermes prompt-size` builds a real offline agent with dummy credentials so
"the numbers match what actually ships on the wire" without a network call. hrdr
has the estimators (`compaction.rs:450`, `:457`) and a context gauge, but no
category attribution and no way to dump the assembled prompt. **This is leverage
on everything above** — you can't argue about a 705-line prompt's budget without
it. _Caveat:_ char/4 estimates invite false precision, and hermes strips its
skills block out of `stable` by regex to avoid double-counting, which is
fragile. Report bytes and labelled estimates; resist a percentage-of-window pie
chart.

### Where hrdr is ahead

1. **Sub-agent filesystem isolation — hermes has none, and its own tool
   description overstates what it has.** `grep` for
   `worktree|mkdtemp|TemporaryDirectory|os.chdir|cwd=` across
   `tools/delegate_tool.py` returns **zero matches**. The child's "workspace" is
   a prompt string pointing at the **parent's own** cwd (`:739-759`,
   `:685-690`), and the code says so — _"children share the parent's container,
   and today they inherit the parent's live env.cwd implicitly"_ (`:1957-1962`)
   — while the model-facing description claims _"Each subagent gets its own
   terminal session (separate working directory and state)"_ (`:3479`).
   "Separate" means a separate cwd-**tracking record** in a dict, not a separate
   tree. With `max_concurrent_children` defaulting to 3, three sub-agents write
   into one working tree concurrently. **hrdr's largest lead over hermes, and it
   isn't close.**
2. **Delegation lifecycle.** Hermes exposes exactly one delegation tool, whose
   `background` parameter is documented DEPRECATED/IGNORED. No model-facing
   steer, cancel-by-id, list, output-fetch, diff or revive — `/stop` and
   `/agents` are human slash commands — and background delegations are
   explicitly **not durable**: _"if the parent session is closed (/new) or the
   process exits before a subagent finishes, that subagent's work is discarded"_
   (`:3449-3454`).
3. **Merge verification.** The parent model receives a 500-char summary;
   `files_written`/`files_read`/`cost_usd` are computed then **popped before
   reaching the model** (`:2270-2316`). Hermes' answer is a prompt instruction:
   _"Subagent summaries are SELF-REPORTS, not verified facts… verify it
   yourself."_ hrdr's `task_diff` hands over commits and the full diff, and
   `task_cleanup` mechanically refuses to remove a worktree with unmerged
   commits.
4. **Read-before-write gating.** Hermes detects staleness but **never blocks** —
   `_check_file_staleness`'s own docstring says _"Does not block — the write
   still proceeds"_ (`file_tools.py:1507-1535`), and the `write_file` call site
   attaches the finding as `result_dict["_warning"]`. hrdr **refuses** all three
   non-`Fresh` states, and tracks the model's _context_ rather than disk. Same
   lead as over pi, now confirmed against a second peer.
5. **Guardrails cannot be switched off, and there is no fail-open path.**
   Hermes' default `approvals.mode` is `"smart"` — an **auxiliary-LLM judge**
   auto-approves flagged commands — and in a headless, non-cron, non-gateway run
   the gate **auto-approves without even running the scanners**
   (`approval.py:3243-3307`). `HERMES_YOLO_MODE` disables approvals wholesale
   and is frozen at import precisely because otherwise a skill could set it
   in-process and bypass everything; a contextvar race that dropped sessions
   onto the auto-approve path has a CVE (GHSA-96vc-wcxf-jjff). Tirith is a
   downloaded binary defaulting to `fail_open: True`. hrdr's 15 guardrails are
   compiled in, read no env var, have no YOLO mode and no LLM in the loop, and
   apply to sub-agents because _"those constrain tool calls, and a sub-agent
   makes those too"_ (`config.rs:383-386`). **hrdr's autonomy posture is
   coherent precisely because there is no headless carve-out.** (Hermes' cron
   path _is_ fail-closed by default — the interactive default's inconsistency is
   the finding.)
6. **Per-sub-agent cost control** — hermes caps iterations and concurrency, has
   no default wall-clock timeout and no spend cap; child cost is tracked for
   reporting only.
7. **Skill shadowing beats skill syncing.** hrdr embeds built-ins and lets
   project or user files shadow by name, first-source-wins, tested. Hermes
   copies bundled skills to `~/.hermes/skills/` and needs an MD5 origin-hash
   manifest to work out whether the user customised a copy before overwriting,
   plus a v1→v2 manifest migration and a `.no-bundled-skills` opt-out. hrdr has
   no such state to get wrong.
8. **Semantic code navigation** — as with codex, no LSP anywhere in hermes.

### Agrees / disagrees with codex and pi

**Agrees with codex** (strong signal — adopt with more confidence):

- **Per-model behaviour as data, not code.** Codex ships a remote catalog;
  hermes ships an editable substring list
  (`TOOL_USE_ENFORCEMENT_MODELS = ("gpt","codex","gemini","gemma","grok","glm","qwen","deepseek")`,
  `prompt_builder.py:321`) plus per-family text blocks. Both refuse to fork
  assembly logic per model. **hrdr is the only one of the four with zero
  per-model variation.** Notably, Claude/Anthropic is deliberately _absent_ from
  hermes' enforcement list, and the blocks carry an attributed provenance trail
  — _"Observed on DeepSeek v4-flash… returned fabricated listings"_, the Google
  block _"adapted from OpenCode's gemini.txt"_, the parallel-call block _"Ported
  from cline/cline#11514"_. Per-model prompt knowledge as a curated, dated
  corpus.
- **Post-trained models get less prompt** — codex's guidance shrinks from
  gpt-5.2 to gpt-5.6 as behaviour moves into training; hermes pointedly omits
  Claude. Two vendors independently concluding the same thing about who needs
  steering.
- **Deferred tool loading behind a search bridge**, and **both exempt core
  tools**.
- **An LLM in the approval loop.** The codex section recorded Guardian as
  needing _"a cheap, trusted, fast second model: what a single-provider vendor
  has and a multi-provider tool does not"_ — hermes **refutes that premise** (it
  is aggressively multi-provider and does it anyway). hrdr's abstention remains
  defensible; the recorded _reason_ was wrong.

**Agrees with pi** (promotes "consider" to "two of three"):

- Model-invocable skills via a name+description index, bodies read on demand.
  ~~**hrdr's skill invisibility is now the outlier.**~~ **Shipped 2026-07-27.**
- Progressive disclosure of harness knowledge generally — pi pages in 32
  markdown docs; hermes points at hosted docs plus a `hermes-agent` skill and
  declares the docs authoritative where they differ. **hrdr's knowledge is
  resident.**

**Goes the other way from codex:**

- **`.git` protection.** Hermes' nearest equivalent is a regex on _skill
  content_ flagging writes to crontab, shell rc files, `authorized_keys`,
  sudoers and `AGENTS.md` — but nothing protects `.git`, and there is no
  writable-root concept unless the operator sets `HERMES_WRITE_SAFE_ROOT`.
  **Codex's finding 1 stands unweakened.**
- **World-state diffs.** Codex emits per-step deltas; hermes goes the opposite
  way — freeze everything, rebuild only at compaction. Given hrdr's small
  volatile set, **hermes' posture is the cheaper correct answer for hrdr**, and
  finding 4 is the concrete version of it.

**Refines pi:** the pi section rejected rg/fd auto-download because _"its
downloader does no integrity checking"_. Hermes downloads a security binary from
GitHub releases too, but **verifies SHA-256 always and cosign provenance when
cosign is on PATH** (`tirith_security.py:13-20`, `:293-340`). So the objection
was to pi's _implementation_, not the idea. hrdr's single-static-binary posture
still wins on distribution grounds and the narrower finding (force each grep
backend in tests) is unchanged — but **"auto-download is inherently
unacceptable" is not the lesson to carry forward.**

### Deliberate differences, not gaps

- **It is a datagen harness as much as an agent** — see the refuted claims
  above.
- **Cron/routines is not a gap for hrdr.** `cron/scheduler.py` is 194 KB and its
  default executor is an in-process 60-second poll thread inside a long-lived
  gateway daemon under launchd/systemd. It presupposes a resident daemon hrdr
  does not have and does not want; scheduled work for a coding agent lives in
  CI, and `watch` covers the intra-session case. Note the second-order cost
  hermes paid: cron sessions poisoned session-search ranking badly enough to
  need a demotion tier. The one detail worth stealing if hrdr ever ships
  anything scheduled is the _posture_ — cron runs get `skip_memory=True`
  unconditionally because _"cron system prompts would corrupt user
  representations"_, and approvals fail **closed** there.
- **A skills marketplace with a trust-tier install policy** — and its trust
  model is **weaker than the filenames suggest**: `skills_ast_audit.py` is
  explicitly "not a security gate" and never blocks, `skill_provenance.py` is a
  ContextVar unrelated to supply chain, the SHA-256 hashes are change-detection
  and cache keys rather than verification against a publisher signature, a
  claimed NVIDIA `skill.oms.sig` has no verifying code anywhere, and trust
  reduces to a hardcoded allowlist of four GitHub org strings plus post-download
  regex. Hermes' own SECURITY.md calls the guard _"in-process heuristics —
  useful, not boundaries"_. The genuinely good part is **boundary placement**:
  install/search are CLI-and-human-only; the model gets list/view/manage over
  local skills and no way to install a remote one. **hrdr's local-only skills
  already sit on the safe side of that line.**
- **A background self-improvement fork** that autonomously writes and curates
  skills — an autonomy posture hrdr has not chosen.
- **Nested delegation with orchestrator/leaf roles** versus hrdr's flat "you
  cannot delegate further". Hermes needs roles because it has no isolation to
  nest.
- **Multi-surface product** (Telegram/Discord/Slack/Feishu gateways, Electron
  app, ACP adapter, TUI gateway, per-platform prompt overrides).
- **Non-blocking secret-file policy and no filesystem confinement**, documented
  as defence-in-depth not a boundary: _"the agent can still `cat auth.json`…
  treat any user-visible framing around this as 'may help' rather than 'stops
  attackers'"_. hrdr made the same call explicitly when it removed cwd
  confinement (`f0d903a`).

### What could not be verified

No prompt was rendered by running either tool — hermes' own `prompt-size` would
settle it but constructing an `AIAgent` creates directories, out of bounds for a
read-only pass. **Every size comparison here is structural, not measured.**
Whether `tool_search` ever activates in a default install is unconfirmed (with
no MCP servers configured, nothing is deferrable and the gate is a permanent
no-op). `agent/conversation_compression.py` (2769 lines) was read only at its
header, so **no claim is made that hrdr's compaction is better** — only that
`trajectory_compressor.py` is not the comparison. `agent/coding_context.py` —
likely the most directly comparable surface to `system.j2` — was reached only
through call sites and **not read**; comparing the two prompts' _content_ is the
obvious next pass. The persona-after-environment ordering nit is verified by
construction, not observed on the wire.

## opencode

Comparison run 2026-07-26 against `sst/opencode` v1.17.13. **hrdr's closest peer
in kind** — multi-provider, terminal-first, TUI plus a web/desktop surface.
Three of five claims came back partly wrong, and the pass corrected the
MCP-transport claim in the codex section (see the callout above).

### Corrections to the shallow reading

**Prompt selection keys on the model id string only — never the provider.**
`session/system.ts:26-40` is a substring cascade on `model.api.id`:
`gpt-4|o1|o3 → beast`, `gpt+codex → codex`, `gpt → gpt`, `gemini- → gemini`,
`claude → anthropic`, `trinity → trinity`, `kimi → kimi`, else `default`. So
Claude on Bedrock, Vertex and Anthropic all get the same file, and an
unrecognised local model gets `default.txt`.

**One of the nine is dead code — the codex trap generalises.**
`copilot-gpt-5.txt` (143 lines) has **zero references** anywhere. So do
`session/prompt/plan-reminder-anthropic.txt` (which was my claimed evidence for
per-model plan reinforcement — **refuted**) and `tool/plan-enter.txt`. Only
`plan-exit.txt` is imported. **Assume any per-model prompt directory contains
dead code until you grep for the import** — codex 5-of-6, opencode 1-of-9 plus
two more dead prompt files.

**They are genuinely divergent, not drifted copies** — this is what survives and
matters. Sorted-unique-line overlap: `anthropic` vs `default` shares 13 of ~65
lines; `gpt` vs `codex` 7 of 63; `gpt` vs `copilot-gpt-5` **0**.
`gpt.txt:81-107` carries a GPT-5-specific `## Response channels` section
(`commentary`/`final`) found nowhere else; `kimi.txt` has a wholly different
section skeleton. **Blast-radius caveat:** an agent with its own `prompt`
replaces the per-model prompt entirely (`session/llm/request.ts:60`), and
`explore`/`compaction`/`title`/ `summary` all set one — so per-model selection
only fires for `build`, `plan` and user agents.

**Plan mode: the enforcement is real and better designed than I described, but
the flag is off by default and the mechanism has a hole.**
`agent/agent.ts:156-181` gives the `plan` agent
`edit: { "*": "deny", ".opencode/plans/*.md": "allow", … }` — a **path-scoped
write allowlist**, evaluated `findLast`-wins (`permission/index.ts:28-38`).
Because the last `edit` rule is a _path_ not `*`, the tool stays **visible** to
the model (`Permission.disabled` requires `pattern === "*"`, `:210-211`) and is
denied per-call on the actual path. That is better than hiding the tool: the
model sees `edit` exists and gets a targeted denial. **But**
`experimentalPlanMode` is `enabledByExperimental`, i.e. off
(`effect/runtime-flags.ts:49`), and the deny covers only the `edit` permission —
`shell` inherits `"*": "allow"` from defaults. Which is exactly why
`plan.txt:4-6` screams _"Do NOT use sed, tee, echo, cat, or ANY other bash
command to manipulate files"_. **The prompt exists because the mechanism doesn't
reach the shell.** Worth knowing before copying it. `plan_exit` is itself
interesting: it calls `question.ask` for approval and on "Yes" writes a
synthetic user message with `agent: "build"` into the session
(`tool/plan.ts:53-69`) — the mode switch is a real state transition in the
transcript, not a prompt suggestion.

**The `lsp` tool is experimental, and it has no `rename`.** Nine operations
(`goToDefinition, findReferences, hover, documentSymbol, workspaceSymbol, goToImplementation, prepareCallHierarchy, incomingCalls, outgoingCalls`,
`tool/lsp.ts:11-21`) — but gated on `experimentalLspTool`, off by default
(`tool/registry.ts:233`). **hrdr's `rename`, the one operation with no shell
equivalent, has no opencode counterpart.** Conversely hrdr lacks `hover`,
`documentSymbol`, `workspaceSymbol`, `implementation` and the call-hierarchy
ops. _Judgment on one-vs-three:_ the enum wins on token cost and extensibility,
but opencode had to make `line`/`character` **required** even for
`workspaceSymbol`, which ignores them (`:26-31`, `:50-55`) — the concrete cost
of collapsing. **Don't merge hrdr's three; add ops as new tools if wanted.** The
enum only pays past ~6 operations. And the LSP _client_ is the bigger story:
`lsp/server.ts` is 53.5 KB with auto-download, and diagnostics aren't in the
tool at all — they're pushed back on every mutation (`edit.ts:197-205`,
`write.ts:75-79`, `apply_patch.ts:265-300`), which is exactly what hrdr does at
`mutation.rs:48-72`. **Parity on the mechanism that matters.**

**`.txt` descriptions: confirmed, but my implied judgment was wrong.** Opencode
composes at runtime on top of the `.txt` — `task` appends the live sub-agent
list (`tool/registry.ts:251-264`), `shell` renders `shell.txt` as a template
with per-dialect blocks for bash/pwsh/PowerShell 5.1/cmd
(`tool/shell/prompt.ts`), and plugins can rewrite any description per turn. All
three harnesses converge on "description is a runtime `String`"; the only
disagreement is where the static base lives. **Moving hrdr's text to `.txt` buys
nothing on its own** — `include_str!` would be a lateral move. Changing the
return type to `String` is the change that matters, and that's already filed in
the codex section. Not re-filed.

### Findings hrdr should act on

**1. Parse shell commands instead of regex-matching them. Strongest
cross-harness signal in this whole comparison.** Opencode parses every command
with a **tree-sitter grammar** (bash and PowerShell), walks the command nodes,
and derives two things: out-of-project path arguments for file-touching commands
(`tool/shell.ts:392-405`), and a permission pattern plus an **arity-truncated
"always" prefix** (`:406-409`, table at `permission/arity.ts`) — so
`git checkout main -b foo` generalises to `git checkout *` rather than being
either an exact string or `git *`. hrdr matches 14 hand-written regexes against
the raw command line (`guardrails.rs:44-120`), with in-tree comments admitting
the cost: _"the regex crate has no lookaround — `--force` must not also match
`--force-with-lease`, so it's anchored to a non-word boundary manually"_
(`:46-48`), and every rule carrying `[^&|;]*` to avoid crossing a separator — a
hand-rolled tokeniser spelled in regex. **codex reached the same conclusion
independently** (`shell-command/src/parse_command.rs`, 82 KB, plus a Starlark
`execpolicy` DSL). hrdr is the outlier. _Cost:_ large — `tree-sitter-bash` plus
rewriting `default_guardrails` to match parsed nodes. A week. _Or maybe not:_
hrdr's guardrails are deliberately a **small deny list on an autonomous agent**,
not an approval system — there's no human to ask, so a parse buys precision
without buying a new capability. If the goal is only "stop `rm -rf /` and
force-push", 14 tested regexes are defensible and cheaper to maintain than a
grammar dependency. **The parse becomes clearly worth it only if hrdr adopts
finding 2 or 4.**

**2. `doom_loop` — repeated-identical-tool-call detection. Cheapest real win
here.** `session/processor.ts:350-376`: on every tool call, if the last 3 parts
are the same tool with byte-identical `JSON.stringify(input)`, raise a
`doom_loop` permission ask. **hrdr has nothing equivalent** — grep for
`doom|loop_detect|repeated_call` across `crates/` returns zero (verified).
hrdr's only backstop is a _count_ cap: `max_steps` rounds with a wrap-up nudge
and a final tool-less round (`turn_loop.rs:632-656`). A model stuck re-running
the same failing `cargo test` burns the entire round budget **and the entire
cost cap** before anything notices. _Cost:_ half a day. hrdr already keeps
`self.messages`; the check is "last 3 tool calls have equal name and equal
serialised args". For hrdr the action should be an injected `Notice` ("you have
called X with identical arguments 3 times — change approach"), not an approval
prompt — fits the autonomy posture, needs no new plumbing. _Caveat:_ three
identical calls are legitimate for `watch`-style polling and `task_output` on a
running sub-agent. Needs a per-tool opt-out list; opencode has the same problem
and dodges it only by making the whole thing an _ask_.

**3. Out-of-project access as _ask_, not as _removed_.**
`tool/external-directory.ts:15-44`: any tool touching a path outside the
project/worktree raises an `external_directory` permission keyed on the
containing **directory glob**, with an allow-list pre-seeded from the overflow
dir, temp, skill dirs and reference dirs (`agent/agent.ts:108-124`) — so
legitimate paths are pre-approved and everything else prompts once per
directory. Called from `read`, `edit`, `write`, `glob`, `grep`, `lsp`, `shell`
and `apply_patch`. hrdr **removed cwd confinement entirely** (`f0d903a`), and
the tracked "sub-agent isolation guard" item is the acknowledgement that
something was lost. Opencode shows the middle position hrdr skipped: full access
retained, but **crossing the boundary is an observable event**. This is a design
for a tracked gap, not a new gap — and opencode's granularity (directory glob +
pre-seeded allow list) is the part to copy. _Caveat:_ hrdr's write sub-agents
legitimately read the parent repo (shared `Cargo.lock`, `~/.cargo`, `/usr/lib`),
so the allow-list may need to be large enough that signal-to-noise doesn't
justify it.

**4. One permission evaluator instead of three unrelated mechanisms.** Opencode
has a single primitive — an ordered list of `{permission, pattern, action}`
evaluated `findLast`-wins with globbing on both fields
(`permission/index.ts:28-38`) — and from it gets plan mode, sub-agent
restriction, read-only agents, out-of-project confinement, `.env` gating, loop
detection and headless mode, all as **data**. hrdr has three mechanisms that
don't compose: `guardrails` (shell only, terminal `bail!`), `read_only` (a
registry-level name filter), and per-tool secret-file `bail!`s (which
`deferred-improvements.md` records as _deliberately_ not shared). Adding a
fourth restriction means writing a fourth mechanism. _Caveat, and it's the real
objection:_ opencode's three actions are `allow|ask|deny`; hrdr's autonomy
posture collapses that to `allow|deny`, and a two-valued evaluator over globs is
worth much less than a three-valued one. **The honest MVP:** keep the
mechanisms, express _what they check_ as one rule list so `read_only` and
`guardrails` stop being independent. A refactor, not a feature — wait until hrdr
actually wants a second path-scoped restriction (finding 3, or codex's `.git`
protection).

**5. Per-provider tool-JSON-schema rewriting.**
`provider/transform.ts:1419-1490` rewrites every tool schema per model before it
goes on the wire, with three quirks and the reason in a comment each:
OpenAI/Azure sanitisation; Moonshot/Kimi strip every sibling key of a `$ref`
(_"Moonshot expands `$ref` before validation and rejects sibling keywords"_) and
collapse tuple-style `items`; Gemini converts integer enums to string enums.
hrdr ships one schema shape to every provider — grep for
`sanitize|additionalProperties|\$ref` across `hrdr-llm/src/` returns nothing
relevant — and hrdr targets a **wider** provider spread than opencode's default
set. _Caveat, strongest in this section:_ **there is no evidence hrdr is
currently broken on any provider.** This is where to put a seam _when a bug
arrives_, not a speculative port. File as "known-good design for when a provider
rejects a schema", not as work.

### Where hrdr is ahead

1. **Sub-agent filesystem isolation — opencode has none.**
   `tool/task.ts:142-158` creates a child session in the **same directory** with
   the same tree; the only isolation is a permission ruleset
   (`agent/subagent-permissions.ts:14-27`). Two concurrent opencode sub-agents
   editing one file race with no lock, no worktree, no branch.
   `packages/opencode/src/worktree/index.ts` (22.6 KB) exists but is a
   _user-facing_ feature, **not referenced from `task.ts`**. Same shape as
   hrdr's lead over codex and hermes — **all three peers lack it.**
2. **Pruned tool output stays recoverable.** Opencode replaces content with the
   literal `"[Old tool result content cleared]"`
   (`session/message-v2.ts:293-296`) with **no re-read path** — the bytes are in
   SQLite but the model is never told where. hrdr saves the body to
   `tool_output_dir()` and substitutes a pointer with recovery instructions
   (`compaction.rs:415-425`). And opencode's prune is **opt-in, off by default**
   (`session/compaction.ts:245`) with a flat `PRUNE_MINIMUM = 20_000` gate,
   where hrdr's runs by default behind an ROI gate.
3. **USD cost budgeting.** Opencode computes per-session USD with tiered pricing
   and `Decimal` arithmetic — but grep for
   `budget|spendLimit|costLimit|max_cost` yields **only token budgets**. No
   monetary cap, warning or stop anywhere. **Three of three peers lack this**,
   which makes it hrdr's most distinctive feature, not an oddity.
4. **`watch`** — no opencode equivalent; waiting on CI is a sleep-and-recheck
   loop costing a round trip per look.
5. **A structured `git` tool** — 44.8 KB, 14 subcommands, output through the
   same bounded path as every other tool. Opencode's git surface is `shell` plus
   the snapshot shadow-repo.
6. **Semantic `rename`** — `lsp_nav.rs:362-400`, with apply/rollback. Opencode's
   `lsp` has no rename op; codex and hermes have no LSP at all. **The single
   capability no other harness in this comparison has.**
7. **LSP on by default vs experimental.**
8. **MCP prompts** — hrdr exposes `prompts/list`/`prompts/get`; opencode has
   zero references. (But see the MCP correction above: hrdr is _behind_ on MCP
   OAuth.)
9. **Skill frontmatter fails closed for agents.** Opencode's skill loading fails
   _open_ — a YAML parse error logs and skips (`skill/index.ts:111-121`), and a
   file that parses but fails the shape check is **silently dropped** at `:123`;
   `InvalidError`/`NameMismatchError` are declared and never thrown in
   production.
10. **Built-in skills: 10 vs 1.** Opencode ships exactly one
    (`customize-opencode`); everything else is disk-discovered, including
    reading `~/.claude/skills/`.
11. **Session retention** — `grep -n "retention|expire|vacuum|compress|purge"`
    over opencode's session and storage modules returns **0**. Sessions never
    expire. **Three of three peers lack this too.**

### Agrees / disagrees with codex

**Agreements — two independent harnesses, strong signal:**

- **Parse shell commands, don't regex them.** Codex: 82 KB parser + Starlark
  DSL. Opencode: tree-sitter + arity table. Different implementations, same
  conclusion. **The single strongest cross-harness signal here; hrdr is the
  outlier.**
- **`apply_patch` is a per-model tool, mutually exclusive with structured
  editing.** Codex gates it on `apply_patch_tool_type` from its catalog;
  opencode gates on model id and makes it exclusive —
  `usePatch = modelID.includes("gpt-") && !oss && !gpt-4`, and `edit`/`write`
  are **excluded** when it's on (`tool/registry.ts:272-277`). Both concluded
  GPT-5-class models want a diff envelope and everyone else wants string
  replacement. hrdr sends `edit`/`write` to all models.
- **Descriptions are runtime `String`s.** Corroboration of the codex finding,
  not a new item.
- **Per-model prompt variation is real, and dead prompt files are common.**
- **Interactive approvals as a posture**, both explicitly disabled headless.

**Disagreements:**

- **Enforced plan mode: opencode yes, codex no.** So the tracked hrdr gap is
  **one-for-two among peers, not universal** — but opencode's path-scoped
  write-allowlist design is the better of the two if hrdr ever builds it.
- **Code mode: codex shipped it as mandatory on its newest models; opencode's is
  unshipped and narrower.** `CodeModeTool` is **not** in opencode's builtin
  registry (referenced only from tests) and exposes **MCP/CodeMode tools only,
  explicitly not top-level tools** (`tool/code-mode.ts:30`) — the opposite of
  codex's `code_mode_only`, which replaces the entire surface. **Not the same
  idea; hrdr should ignore both.**
- **Sandboxing: codex has an OS sandbox on by default; opencode has none** — its
  only confinement is the `external_directory` ask, a userspace convention.
  Issue #13 has one peer that solved it and one that didn't.

### Deliberate differences, not gaps

- **Server-first architecture, and it's the best reference for
  `docs/web-ui-plan.md` in this set.** One Effect `HttpApi` route table (~140
  endpoints), SSE for events, WebSockets for PTY only. Every frontend is an HTTP
  client of it — **including the TUI, which by default routes through an
  in-process worker fetch** (`cli/cmd/tui.ts:229-245`) rather than a socket,
  with identical HTTP semantics. Two generated clients, one from OpenAPI and one
  from the `HttpApi` contract, with a `git diff --exit-code` drift check. **That
  in-process-transport trick is the transferable idea:** it gets you a web UI
  without a second codepath and without forcing a socket in the single-process
  case. But it's a whole-architecture commitment and opencode's TUI is
  TypeScript/SolidJS on OpenTUI (~27 kLOC), so the ergonomics don't transfer to
  Ratatui directly.
- **Runtime npm install of provider SDKs** (`provider/provider.ts:1751-1769`) —
  a Bun affordance with no Rust equivalent, and one hrdr shouldn't want.
- **Config as JSONC with a published JSON Schema**, 35 top-level keys, 9-layer
  merge ending in macOS MDM managed preferences. Enterprise surface.
- **Filesystem snapshots via a shadow git repo** with `objects/info/alternates`
  pointed at the real object DB so blob hashes are reused, powering
  `session/revert.ts`. hrdr deliberately removed checkpoints/undo (`ba07063`) —
  a reversed decision, not a gap.
- **No Anthropic OAuth** — opencode has OAuth for ChatGPT/Codex, Copilot, xAI,
  Snowflake, DigitalOcean, GitLab and its own console, but **not** Claude
  Pro/Max. hrdr's Codex OAuth is at parity; neither has Anthropic subscription
  auth.
- **Effect-TS throughout** — not portable, not a lesson.

**One number flagged as an open question, not a finding.** Truncation caps:
opencode 50 KB / 2000 lines (`tool/truncate.ts:15-16`); hrdr **5120 bytes / 50
lines** (`hrdr-tools/src/lib.rs:62`, `:68`). A 10× / 40× gap. Both spill to a
re-readable file so nothing is lost, but hrdr's 50-line default means a
`cargo test` failure or a 60-line diff costs a second round trip that opencode
wouldn't pay. **Not measured in hrdr's traces — worth one experiment.**

### What could not be verified

The clone is **1 commit deep**, so no history evidence for anything — whether
`copilot-gpt-5.txt` was ever wired, or whether the dead prompts are recently
orphaned. Whether `experimentalPlanMode` is on in _shipped_ builds (only the
source default was verified). Whether the plan-agent shell hole is exploitable
in practice — traced statically, not executed. Whether the three schema quirks
apply to hrdr's schema shapes. `agent/prompt/summary.txt` has no production
consumer either agent could find — possibly a fourth dead prompt, unproven. Real
assembled per-turn context size for either tool: **same caveat as every other
pass — nothing was instrumented, so no "X sends less than Y" claim in this
document is measured.**

---

## Cross-harness synthesis

### Where hrdr is the outlier — ranked by how many peers disagree with us

| #   | Thing                                | codex             | hermes                            | opencode          | pi  | hrdr                   |
| --- | ------------------------------------ | ----------------- | --------------------------------- | ----------------- | --- | ---------------------- |
| 1   | Per-model prompt/behaviour variation | ✅ remote catalog | ✅ substring list + family blocks | ✅ 9 prompt files | ✗   | **✗**                  |
| 2   | Model-invocable skills               | ✅                | ✅                                | ✅                | ✅  | **✅ (shipped)**       |
| 3   | Shell commands parsed, not regexed   | ✅                | —                                 | ✅                | ✗   | **✗**                  |
| 4   | Runtime-composed tool descriptions   | ✅                | ✅                                | ✅                | ✅  | **✗ (`&'static str`)** |
| 5   | Ask-the-user affordance              | ✅                | ✅                                | ✅                | ✗   | **✗ (tracked)**        |
| 6   | Repeated-call / loop detection       | —                 | —                                 | ✅                | ✗   | **✗**                  |
| 7   | Deferred tool loading                | ✅                | ✅                                | —                 | ✗   | **✗**                  |

**Items 1, 2 and 4 are three-of-four or four-of-four against us** — those are
the ones where being the outlier is most likely to be a mistake rather than a
deliberate stance. Item 2 was the cheapest of the three and closed a defect this
comparison found: **shipped 2026-07-27** (`SECTION_SKILLS` + the `skill` tool,
with pi's per-skill `model_invocable:` opt-out; see the pi section's defect 2).
Items 1 and 4 remain open.

### Where hrdr leads all four

- **Sub-agent filesystem isolation.** codex has two generations of sub-agent
  tooling and no worktrees; hermes' children share the parent's cwd while its
  tool description claims otherwise; opencode's share the directory; pi's exists
  only as an example. **hrdr's git-worktree isolation is unique across all four,
  and `task_cleanup`'s merge verification has no peer.**
- **Semantic `rename`.** No LSP at all in codex or hermes; opencode's `lsp` tool
  has no rename op and is experimental.
- **USD cost budgeting.** Absent in all four.
- **Session retention/compression.** Absent in all four.
- **ROI-gated mid-history pruning with recoverable pointers.** Codex has only
  truncate-or-compact with no rung between; opencode's prune is off by default
  and destroys the content.
- **Guardrails with no off switch.** hermes has `HERMES_YOLO_MODE`, a default
  `"smart"` mode that lets an aux LLM auto-approve, and a headless path that
  auto-approves **without running the scanners** (plus a CVE for a contextvar
  race onto that path). hrdr's are compiled in, read no env var, and apply to
  sub-agents.

### What every peer got wrong that we should not copy

- **Dead prompt files that look live.** codex 5-of-6, opencode 1-of-9 plus two
  more. If we ever ship per-model prompts, wire them with a test that every file
  in the directory is reachable.
- **Read-before-write that warns instead of blocking.** hermes detects staleness
  and its own docstring says _"Does not block — the write still proceeds"_; pi's
  `write` overwrites unconditionally. hrdr's `ReadState` refusal is right, and
  it is a lead over **two** peers independently.
- **Sub-agent self-reports treated as facts.** hermes' answer is a prompt
  telling the model that summaries _"are SELF-REPORTS, not verified facts"_,
  having popped `files_written` before the model sees it. hrdr's `task_diff` is
  the mechanical answer.

### The four things I'd actually do, in order

1. ~~**Model-invocable skills** (pi + hermes + opencode agree; closes a defect
   found here; low cost).~~ **Shipped 2026-07-27** — skill discovery moved into
   `hrdr-agent`, `prompt::skills_section` lists name + description (956 bytes
   for the ten built-ins, in the cached prefix, no bodies and no source paths),
   and a read-only `skill` tool returns the expanded body. Took pi's opt-out
   shape as `model_invocable: false` (`:release` ships marked — its last step
   pushes a tag); did **not** take hermes' "err on the side of loading" framing
   or its operator-side disable list.
2. ~~**Cache breakpoint at hrdr's own stable/volatile boundary** + re-gather
   memory at compaction (hermes; ~30 lines each; the memory freeze is a live
   defect).~~ **Shipped** — `c5e5ced` (memory unfreeze + ordered sections),
   `5f6e386` (markdown fragments, no template engine), `6274c80` (global/project
   scope split), `b1a698f` (the breakpoint). See the status update at the top.
3. **`doom_loop` detection** (opencode; half a day; currently a stuck model
   burns the whole cost cap).
4. **Protect `.git` inside the worktree** (codex; independent of issue #13;
   closes the `.git/hooks` escalation our own sandbox design misses).

Then the two prompt defects found by the pi pass — the unconditional preamble
naming tools some agents lack, and unscanned/silently-dropped `AGENTS.md` — both
of which are small and neither of which needed a peer to justify.
