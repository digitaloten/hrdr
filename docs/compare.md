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

_Pending._

## hermes-agent

_Pending._

## opencode

_Pending._

---

## Cross-harness synthesis

_Written once all four are in._
