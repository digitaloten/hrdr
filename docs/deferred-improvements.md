# Deferred improvements / backlog

Smaller items that were identified but not yet done or tracked elsewhere. The
one larger effort that still has its own doc is `compare.md` (harness comparison
— its open shortlist: doom_loop detection, `.git` protection in worktrees, and
the two prompt defects #2/#3; model-invocable skills shipped 2026-07-27).
`security-audit.md` is fully closed (kept as a methodology record). The web UI
shipped 2026-07-26 (plan doc deleted; its leftovers are the **Web UI
follow-ups** section below). The OS sandbox (issue #13) shipped 2026-07-27 — its
spec doc is deleted, the design now lives in
`crates/hrdr-tools/src/sandbox.rs`'s doc comments and its tests; what still
binds is **Sandbox follow-ups** below plus the sandbox rules under **Standing
constraints**. The Codex catalog pin is issue #2.

Docs for finished work are deleted rather than kept as history — read the code
and `git log`. What survives from a completed effort is only what still binds
future work; those are collected under **Standing constraints** at the bottom.

## Tooling / agent capability

- **Memory drift detection.** A periodic prune/verify pass over the `memory`
  store — check each `<slug>.md` still has a `MEMORY.md` pointer (and vice
  versa) and flag/prune stale or contradicted memories. Cheap now that the tool
  regenerates the pointer index on every change, so the files and index can't
  drift structurally; this is the leftover §G7 thread from the (now-shipped)
  memory-tool design.
- **LSP diagnostics dedup.** The same diagnostic can surface more than once
  (overlapping ranges / re-published sets); dedupe before showing the model.
- **Skills follow-ups** (the feature shipped 2026-07-27 — `hrdr-agent`'s
  `skills.rs` plus `prompt::skills_section`). Left out on purpose: no `skill`
  usage signal (nothing records whether the model ever loads one, so there is no
  evidence for or against the listing's wording); the listing carries no
  categories, unlike hermes' category→skills grouping, which only pays off past
  a few dozen skills; and a skill body still arrives as one tool result, so a
  procedure longer than 24 KiB spills to a file the model must read.
- **A revived sub-agent always runs write-capable.** `task_revive` reuses the
  worktree and takes a write slot, because read-only-ness was never persisted in
  `SessionState` — so a revived former read-only explorer runs write-capable in
  the recorded/main dir. Not central to the shipped use cases (both target write
  sub-agents), but it is a real gap: persist the flag and honour it on revive.

## Sandbox follow-ups

Declared when the sandbox was specced and deliberately left out of v1, plus what
bring-up turned up. None are scheduled; the rules that govern any of this work
are under **Standing constraints**.

- **No network axis.** Every backend leaves the network wide open on purpose
  (`--unshare-net` unused on bwrap, `(allow network*)` in the Seatbelt profile).
  The declared route was seccomp on Linux; note Codex has since moved past that
  to a MITM proxy with netns routing (`codex-rs/network-proxy/`, see
  `compare.md`), so revisit the mechanism before building the old plan.
- **Bundled `bwrap`.** Hosts without bubblewrap degrade to Landlock (weaker: no
  read axis). Codex ships its own copy (`linux-sandbox/src/bundled_bwrap.rs`);
  doing the same removes the most common degradation.
- **Curated read allow-list for `write` mode.** Reads are unrestricted there by
  decision, so a shell command can read `~/.ssh` — the file tools'
  `guard_secret_read` has no shell-side equivalent. A read allow-list (or a
  secret-path deny-list applied at the mount level) closes it.
- **Windows has no OS layer.** Software path-guard only, permanently for v1;
  AppContainer or a restricted token is the eventual answer (Codex has
  `windows-sandbox-rs/`). Until then every Windows session gets the "no OS-level
  sandbox" notice.
- **No shell-command pre-flight.** A write outside the roots reaches the model
  as the kernel's `Read-only file system`, not as an explanation. A heuristic
  parser (Codex's `shell-command/src/parse_command.rs` is the reference) could
  say "this would write outside your roots" first — in front of the sandbox,
  never instead of it.
- **Re-evaluate the `git` tool now that Read mode exists.** The read-only
  allow-list of git subcommands is currently the only git access a read-only
  agent has, and it spawns a subprocess the path guard cannot inspect — a
  `git -C /elsewhere log` succeeds even in `read` mode. With OS confinement
  shipped, the codex-style alternative is now possible: give read-only agents a
  sandboxed shell and let the allow-list shrink or disappear.
- **macOS Seatbelt has never run.** The profile is pure-tested only; the e2e
  test is `cfg(target_os = "macos")` and no Mac was available. It is also a
  coarsening of Codex's `seatbelt_base_policy.sbpl` — no `pseudo-tty`, no
  `/dev/null` write, no `iokit-open`/`user-preference-read` — so the first real
  run should read a denial as "profile too tight" before "sandbox broken"; pty
  and `/dev/null` writes are the likely first additions.
- **The degradation-notice cell is process-global.** `set_sandbox_notice` /
  `take_sandbox_notice` share one queue for the whole process, so with several
  agents in flight one agent's turn loop can drain a notice another produced. It
  is also the cause of the known `sandbox_notice_reaches_the_event_stream` flake
  (a parallel test swallows the seeded notice). A per-agent channel owned beside
  the policy fixes both.

## Consistency / robustness

- **Guardrail rules live in two places.** The shell guardrail rule set is
  encoded both in `crates/hrdr-tools/src/guardrails.rs` (mechanical enforcement)
  and in the prompt fragments under `crates/hrdr-agent/src/templates/*.md`
  (guidance that tells the model not to attempt them). Adding a rule means
  editing both, or they drift. Not worth auto-deriving (the prompt phrasing is
  deliberately more nuanced than the terse guardrail messages) — but a
  checklist/test that the two sets agree would catch drift.
- **The pipe-to-shell guardrail assumes POSIX.** Its recovery text
  (`curl -fsSL … -o /tmp/script.sh`) and its nested-shell regex live in
  `guardrails.rs`, outside the `Shell` seam, because `default_guardrails()` has
  no shell in scope. Correct today — the shell is always bash/sh — but it is the
  one place a new shell dialect would have to be threaded through by hand.
- **Windows-drift audit pass — done, three fixes landed** (`8e5bc9d`). All ~130
  `cfg` gates were classified. Roughly 25 are `#[cfg(unix)]` on _tests_ (needing
  bash, python3 or symlinks) and are not findings; `proc.rs`, the pid-liveness
  probes in `session.rs`/`delegation.rs`, and `prompt.rs`'s package-manager
  names are deliberate and documented. Three were real and are fixed: the
  credential `sync_all` gated on unix though it is portable, `atomic_write`'s
  symlink guard running only on unix though `is_symlink` is portable, and
  owner-only file creation being re-decided at four sites. What is left is the
  honest residual: **hrdr sets no Windows ACL on any file it writes** — the
  guarantee is the containing per-user directory's inherited default, stated
  once on `hrdr_llm::owner_only_options`. Setting per-user ACLs needs a new
  dependency in `hrdr-llm` and is a deliberate non-goal until someone runs hrdr
  on Windows in anger.
- **`O_NOFOLLOW` covers only the final path component.** A symlinked _parent_
  directory is still traversed on the wire-log open, and there is no Windows
  equivalent applied at all, so callers relying on it keep their own preflight
  check. Recorded on `owner_only_options_no_follow`; closing it properly means
  resolving the whole path under a directory handle.

## Web UI follow-ups (post-parity; the implemented spec's deferred list + review residuals)

- **Session-browser UI** — list + open other sessions from the client; the
  server gains a `list_sessions()`-backed message pair.
- **Syntax highlighting in code blocks** (syntect-wasm or highlight.js interop).
- **Modal pickers** (model/effort/theme/session) as bottom sheets over the
  `begin_*_selector` hooks.
- **v2: attach to a live TUI session** — blocked on making the event-log
  compaction min-cursor-aware across readers (`PaneSet::sync` calls
  `live.compact` after folding, so the log is effectively single-reader today).
- **Native desktop/mobile shell** — webview over embedded `hrdr-web`.
- **Read-only/observer auth mode.**
- **Cookie-attempt rate-limiting** — `check_auth`'s Users branch 401s on an
  invalid session cookie without calling `rate_limit_record` (the cookie is
  HMAC-signed so not brute-forceable; counting attempts is still cleaner).
- **WebHost chrome posters** — no `identity_poster`/`context_window_poster`, so
  an async `/model` switch updates chrome only via the agent's republish; and a
  failed autosave is silent (no web equivalent of the TUI's
  `record_session_save` notice).
- **WS origin check allows any localhost port** — a malicious page served by
  another local app could open a WS with the victim's cookie; tighten the
  localhost allowance to the served port.

## Test coverage gaps

- **TUI history up/down fix** (`6ff0172`, `suppress_completions`) shipped
  without a regression test — a test that Up/Down after a slash-command history
  entry navigates history rather than the completion popup.
- **Wire log on the native backends.** `error_response` and `sse` records are
  emitted by `anthropic.rs` / `codex.rs` but untested: backend selection keys on
  the host, so a mock server on `127.0.0.1` cannot reach those paths. Only the
  `request` record is covered (`hrdr-llm/tests/wire_log_native_backends.rs`).

## Known behaviour to revisit

- **Building a sandbox policy touches the parent repo's `.git`.**
  `git_metadata_roots` `create_dir_all`s `refs/heads/hrdr` and
  `logs/refs/heads/hrdr` so they exist to be canonicalized and bind-mounted —
  which means `Agent::new` with a linked-worktree cwd creates those two dirs in
  the **parent** repo's `.git` at construction time. Harmless (git ignores empty
  ref dirs) but worth knowing: constructing an agent is not read-only with
  respect to the repo.
- **A worktree commit can print a `packed-refs.lock` EROFS line.** Ref
  maintenance triggered by a commit inside a sandboxed worktree may fail to
  create `<parent>/.git/packed-refs.lock` while the commit itself lands and
  exits 0. Observed during bring-up, asserted by no test — treat it as possible,
  not guaranteed, and do not widen the roots to silence it.
- **Input-path unification UX.** After the "every user message is a queued
  `Steer`" refactor, a submitted message renders when its `Steered` event is
  pumped (a beat after submit) rather than synchronously, matching sub-agent
  behaviour. Intended and imperceptible with a fast pump; if it ever reads as
  laggy, the opener could be pumped synchronously.
- **tok/s excludes tool time.** The generating marker's throughput figure
  divides streamed tokens by _model working time_: `infer_elapsed()` pauses
  while `tools_running > 0`, and the loader is hidden entirely during a tool
  call. By design (it reports model speed, not wall-clock throughput). Showing
  "running tool…" instead of hiding the loader, or tracking wall-clock
  throughput separately, would be a new feature rather than a bug fix.

## Considered and declined

Duplication and near-duplication that two audits (a DRY pass and a seam pass,
both since closed and deleted) examined and deliberately left alone. Recorded so
the next audit does not re-litigate them — if you disagree, argue with the
reason, don't just re-file the finding.

- **Batched `edits[]` on the `edit` tool — declined 2026-07-26.** Design was
  worked through (flat `edits: [{path, old_string, new_string}]`, anchors
  resolved against as-read content, two-phase all-or-nothing) and rejected on
  cost/benefit: single edits are what models handle best; with prompt caching
  the marginal token cost of a second edit call is just its own args + the (now
  trimmed) result, so the batch's real saving is only round-trip latency — not
  worth the validation/overlap/error-reporting machinery, which is its own bug
  surface. The failure-retry cost that motivated batching was fixed instead at
  the root (formatter-aware staleness + apply-anyway, `da714e1`). Note the wire
  format constrains any future revival: tool args must be object-rooted, so a
  bare array can never be the schema.
- **Slash-command dispatch is mirrored** between `hrdr-tui/src/app/commands.rs`
  and `hrdr-app/src/commands/dispatch.rs`. Intentional: the TUI handler
  intercepts TUI-only commands (`edit`, `reload`, `goto`, `find`, `next`/`prev`)
  then falls through to the shared dispatcher. The `CommandHost` trait is the
  DRY mechanism; the split is explained in a comment at the call site.
- **Two project-dir walks** — `skills.rs::skill_dirs` and
  `prompt.rs::gather_agent_docs` both walk cwd → `/` plus XDG dirs. Same
  pattern, different payloads (skills vs AGENTS.md). A shared iterator would DRY
  the traversal, but each is ~15 lines and what they collect diverges; judged
  borderline over-engineering at this scale.
- **Three `CommandHost` impls** — the real TUI host plus `TestHost` and
  `TestLoginHost`. The trait is the shared mechanism; the two test hosts share
  some trivial no-op bodies, but the login host carries login-specific state. A
  shared test base would remove a few no-ops for very little gain.
- **Secret-file write/edit guards are tailored, not shared.** `write.rs`,
  `edit.rs` and `fileops.rs` each `bail!` with their own message ("refusing to
  write…", "refusing to edit…", "copying it would place its contents…"). The
  structure repeats; the wording is deliberately specific and meaningful to the
  model. The read side (`guard_secret_read`) is already shared.
- **`tree.rs` and `replace.rs` build their own walkers.** Genuinely different
  configuration — variable `max_depth` and no ignore toggles in `tree.rs`;
  `hidden(false)` with no `.gitignore` handling at all in `replace.rs` — so they
  stayed out of the shared `ignore_walker` that `find` and `grep` now use.
- **The three grep backends keep separate bodies.** `grep_ripgrep` /
  `grep_posix` / `grep_builtin` have divergent flag sets (ripgrep's
  `--hidden`/`--glob`, POSIX grep's documented `--exclude-dir` trap, the
  built-in `ignore::Walk`). `GrepBackend` already dispatches them by exhaustive
  match; shared methods would be a thin wrapper over nothing.
- **Two "is this the ChatGPT/Codex endpoint" checks, on purpose.**
  `hrdr-llm::detect_backend` uses a permissive host+substring test to pick a
  wire protocol (a mirror or gateway still needs the Responses-API body shape);
  `is_codex_oauth` uses strict equality against one constant to gate OAuth
  credential injection. Unifying them would weaken a security boundary that is
  documented at its call site.
- **`AgentEvent` is matched in two places** — `transcript.rs` and
  `subagent_transcript.rs` — but they build different artifacts (live TUI
  transcript vs serializable `Record`) from the shared `apply_event` fold. Not a
  fork.
- **`lsp.rs` and `mcp/client.rs` spawn without `proc::spawn_group`.** They hold
  `Option<ProcessGroup>` in long-lived struct fields, rely purely on the guard's
  `Drop` with documented field ordering, and never kill explicitly — the
  `GroupKill` handle would be dead weight.

Seams already done right, worth copying rather than reinventing: `Shell`
(`tools/shell.rs`), `EditorEngine` (`hrdr-editor`, trait + 2 impls with zero
call-site branching), `Transport` (`mcp/types.rs`), `GrepBackend`, `ModelRef`,
`ChatErrorKind`, `proc::ProcessGroup`.

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
- **The sandbox is a boundary, not a hint** (rules from the shipped OS sandbox;
  the code is `crates/hrdr-tools/src/sandbox.rs`).
  - _Never confine the hrdr process itself._ No Landlock `restrict_self`, no
    prctl, outside a child `pre_exec`. hrdr does its own session/config/memory
    I/O in-process; confining it breaks the app.
  - _Never silently pretend to sandbox._ Any path that ends up running a command
    with less confinement than the mode asks for must set its notice first (each
    notice at most once per process). `read` degrading to write-confinement
    under Landlock is decided and allowed — being quiet about it is not.
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
    the follow-up work listed above, not a bug fix.
- **A skill the model can load is still the user's procedure.** Rules from the
  model-invocable skills work (`hrdr-agent/src/skills.rs`,
  `prompt::skills_section`).
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
    refused by the tool, with an error that tells the model to ask the user to
    run `:name`. Only a literal `false` opts out (a typo fails open, visible,
    rather than silently hiding a skill). Built-in `:release` carries it because
    its last step pushes a tag.
  - _The prompt section is gated on the tool._ A profile whose `tools:`
    allow-list drops `skill` gets no listing: naming a tool an agent lacks is
    the defect the pi comparison found, not a pattern to repeat.
- **A new tool picks its interface shape by rule, not by taste** (taxonomy from
  the 2026-07-27 survey of all 31 tools). The shape is load-bearing: the
  harness's cross-cutting layer (read-guard, staleness culprit naming, secret
  guard, LSP-on-edit, spool nudges) keys on JSON-schema'd fields, so a tool the
  harness cannot introspect is a tool it cannot protect.
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
    tool: model CLI fluency comes from CLIs seen in training, so an invented CLI
    grammar is less familiar than JSON function-calling, gives no field-level
    guidance, reimports shell-escaping failure modes, and blinds the
    cross-cutting layer. A model that wants raw CLI already has `shell`.
  - _Time is seconds, always_ — `timeout_secs`, `interval_secs`; never `_ms` in
    a model-facing schema (`shell.timeout_ms` renamed 2026-07-27, old name
    poisoned).
  - _Shared vocabulary across tools_ — one concept keeps one field name and one
    default polarity everywhere: `pattern` + `literal: true` opt-out is the
    matching shape for both `grep` and `replace` (aligned 2026-07-27; their
    previously inverted regex defaults were a silent trap).
