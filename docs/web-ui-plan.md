# Web UI for hrdr sessions — implementation spec

Status: **implementation-ready spec.** Date: 2026-07-26. Target implementer: a
weaker model driven through hrdr's own task-delegation harness, one slice per
delegation.

> **RULES FOR THE IMPLEMENTER (read before every slice):**
>
> 1. Implement the slices **in order**. Do not start a slice before the previous
>    one is committed.
> 2. Every slice must end with `cargo fmt --all`,
>    `cargo clippy --all-targets -- -D warnings`, and `cargo test` all passing,
>    and exactly one conventional commit (`feat(web): …`).
> 3. Make **no design decisions**. Everything is decided in this document. If
>    something is genuinely unspecified, stop and report — do not invent.
> 4. Do not modify `hrdr-tui`, `hrdr-editor`, `hrdr-llm`, or `hrdr-tools` in any
>    slice. `hrdr-agent` and `hrdr-app` are modified **only** where a slice
>    explicitly says so.
> 5. Delete each slice's section from this doc in the same commit that completes
>    it (repo convention: docs for finished work are deleted).
> 6. Pre-1.0 rule: **no migration or back-compat shims**, in code or protocol.

## Decided architecture (fixed — do not revisit)

- **Headless first**: `hrdr serve` hosts one session over HTTP+WS; attaching to
  a live TUI session is a later, additive capability (out of scope here).
- **Config-gated exposure**: bind `127.0.0.1` by default; a non-loopback bind
  requires explicit flag + credential backend + TLS (see Security).
- **Full parity target**, built incrementally in the slices below.
- **One UI implementation**: Rust → WASM SPA (**Dioxus**), embedded in the
  server binary; a future native shell is a webview over the same localhost WS.
- **`hrdr-web` is an embeddable library** (`serve()` + `RunningServer`), with
  the `hrdr serve` subcommand as a thin wrapper.
- **The server owns the fold.** All transcript folding, tool classification,
  diff classification, and status-segment building happen server-side with the
  existing shared code. The client only paints (plus markdown + syntax
  highlighting, which are pure formatting).
- **Transport is localhost WS + HTTP only.** No native IPC bridge, ever; a
  native shell speaks the same WS.

## 1. Verified seam inventory

Every symbol below was verified against the code on 2026-07-26. Trust this table
over the old plan's prose. "Add" column = what (if anything) a slice must add
for the web server; empty = use as-is.

| Seam                | Path                                                                                     | Verified public symbol / signature                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      | Add for web                                                              |
| ------------------- | ---------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| Transcript entry    | `crates/hrdr-agent/src/transcript.rs`                                                    | `pub struct Entry { kind: EntryKind, time: DateTime<Local>, content_hash: u64 }` — **serde**: flat object `{"kind":…,"data":…,"time":<unix secs>}`; `content_hash` and Tool `expanded` are `#[serde(skip)]`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             | nothing (protocol mirrors its JSON; see §4)                              |
| Entry kinds         | `crates/hrdr-agent/src/transcript.rs:44`                                                 | `pub enum EntryKind { Header, User(String), Assistant(String), Reasoning{text, took_ms: Option<u64>}, Tool{id,name,args,result,ok,done,expanded}, System(String), Notice(String), Stats(String), Diff(String) }`, `#[serde(tag="kind", content="data", rename_all="snake_case")]`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       | nothing                                                                  |
| Event fold          | `crates/hrdr-agent/src/transcript.rs:513`                                                | `pub fn apply_event(transcript: &mut Vec<Entry>, ev: &AgentEvent)`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      | nothing — the server never calls it directly; `PaneSet::sync` does       |
| Tool display        | `crates/hrdr-agent/src/transcript.rs:295`                                                | `pub fn tool_display(name: &str, args: &str) -> ToolDisplay`; `pub struct ToolDisplay { headline: String, body: ToolBody }`; `pub enum ToolBody { Shell{command}, Code{lang,content}, Diff, Read, Details(Vec<(String,String)>), Text }` — **NOT serde** (`Debug, Clone, PartialEq, Eq` only)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           | nothing in core; `hrdr-web` converts to wire types                       |
| Agent events        | `crates/hrdr-agent/src/lib.rs:571`                                                       | `pub enum AgentEvent { Reasoning(String), Text(String), ToolStart{id,name,args}, ToolOutput{id,chunk}, ToolEnd{id,name,result,ok}, Usage{…}, History(Vec<ChatMessage>), Notice(String), Steered(String), TodoUpdated(Vec<TodoItem>), TurnDone }` — **NOT serde**                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | nothing                                                                  |
| Event log + cursors | `crates/hrdr-agent/src/subagent_live.rs:500`                                             | `LiveSubagents::events_since(&self, key: u64, from: usize) -> Option<(Vec<AgentEvent>, usize)>`; `compact(&self, key: u64, upto: usize)` — cursor is **`usize`**                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        | nothing — but see the single-reader warning in §5                        |
| Live registry       | `crates/hrdr-agent/src/subagent_live.rs:220`                                             | `pub struct LiveSubagents(Arc<Mutex<Vec<LiveSubagent>>>)` (`Clone`); `MAIN_KEY: u64 = 0`; `register_main(agent, steering, model, provider, base_url, usage)`; `record(key, &AgentEvent)`; `enqueue(key, Steer)`; `pending(key) -> Vec<String>`; `clear_pending(key) -> usize`; `is_running(key)`; `continue_or_finish(key) -> bool`; `begin_turn/end_turn(key)`; `handle(key) -> Option<(Arc<tokio::sync::Mutex<Agent>>, SteeringQueue)>`; `send_prompt(key, Steer, on_event) -> Option<PromptDelivery>`; `prune()`; `update(key, impl FnOnce(&mut LiveSubagent))`; `is_compacting(key)`; `attach_transcript(key, &Path)`; `detach_transcript(key)`. Also exported: `LiveSubagent` (all fields pub), `SubagentKind`, `event_log()`, `RunGuard::new(live, key)` (end_turn-on-drop guard) | nothing                                                                  |
| Panes               | `crates/hrdr-agent/src/pane.rs`                                                          | `pub enum PaneId { Main, Sub(u64) }` (**NOT serde**); `pub struct PaneSet` with `sync(&mut self, live: &LiveSubagents)`, `focus(PaneId)`, `active() -> PaneId`, `main() -> &Pane`, `main_mut() -> &mut Pane`, `subs() -> &[Pane]`, `pane_mut(PaneId)`, `active_pane()`; `pub struct Pane { id, status: PaneStatus, state: SessionState, turn: TurnStats, compacting, pending: Vec<String>, effort, auto_compact, compaction_reserved, todos: Arc<Mutex<Vec<TodoItem>>>, view }`                                                                                                                                                                                                                                                                                                         | nothing                                                                  |
| Pane rows           | `crates/hrdr-app/src/pane.rs`                                                            | `pub struct PaneRow { id: PaneId, title: String, status: PaneStatus, active: bool }`; `pub fn pane_rows(&PaneSet) -> Vec<PaneRow>`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      | nothing                                                                  |
| Steering            | `crates/hrdr-agent/src/lib.rs:646`                                                       | `pub type SteeringQueue = Arc<Mutex<VecDeque<Steer>>>`; `pub struct Steer { sent: String, display: String }`; `Steer::new(sent, display)`, `Steer::plain(text)`; `pub fn steering_queue() -> SteeringQueue`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             | nothing                                                                  |
| Turn loop           | `crates/hrdr-agent/src/turn_loop.rs:403`                                                 | `Agent::run<F: FnMut(AgentEvent)>(&mut self, steering: SteeringQueue, on_event: F) -> anyhow::Result<()>` (async)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       | nothing                                                                  |
| Agent construction  | `crates/hrdr-agent/src/lib.rs`                                                           | `Agent::new(AgentConfig) -> Result<Agent>`; `Agent::attach_live(&mut self, live: LiveSubagents, key: u64)`; `Agent::connect_mcp(&mut self) -> Vec<String>` (async)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      | nothing                                                                  |
| Turn clock          | `crates/hrdr-agent/src/turn.rs:23`                                                       | `pub struct TurnStats` — **NOT serde** (holds `Instant`); read via `inferring()`, `infer_elapsed() -> Duration`, `ttft() -> Option<f64>`, `tok_per_sec() -> f64`, fields `out_tokens`, `started_at: Option<SystemTime>`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 | nothing — `hrdr-web` snapshots it into a wire type                       |
| Commands            | `crates/hrdr-app/src/commands/host.rs:17`, `crates/hrdr-app/src/commands/dispatch.rs:12` | `pub trait CommandHost` (a **trait the frontend implements**, ~25 required methods, many defaulted); `pub fn dispatch(host: &mut dyn CommandHost, input: &str) -> bool`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 | `hrdr-web` implements the trait (`WebHost`, slice 6)                     |
| Command registry    | `crates/hrdr-app/src/lib.rs:65`                                                          | `pub const SLASH_COMMANDS: &[(&str,&str)]`; `is_known_command`, `resolve_alias`, `is_quit_command`, `help_body_for`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     | nothing                                                                  |
| Status bar model    | `crates/hrdr-app/src/status.rs`                                                          | `pub struct StatusInputs<'a>`; `pub fn status_sections(&StatusInputs) -> Vec<StatusSeg>`; `status_right_sections`; `StatusSeg { priority: u8, runs: Vec<StatusRun>, gauge: Option<CtxGauge> }`; `StatusRun { text, role: StatusRole }`; `CtxGauge { frac: f64, level: CtxLevel, label }` — **NOT serde**. There is **no `Status` struct** (the old plan's `Status(Status)` message named a type that does not exist).                                                                                                                                                                                                                                                                                                                                                                   | nothing — `hrdr-web` converts to wire types                              |
| Diff classification | `crates/hrdr-app/src/format.rs:243`                                                      | `pub enum DiffLineKind { Hunk, Add, Remove, Meta }`; `pub fn classify_diff_line(line: &str) -> DiffLineKind`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            | nothing — server classifies, wire carries the class                      |
| Git branch          | `crates/hrdr-app/src/util.rs:307`                                                        | `pub fn git_branch(cwd: &Path) -> Option<String>`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       | nothing                                                                  |
| `@file` expansion   | `crates/hrdr-app/src/util.rs:117` and `crates/hrdr-app/src/commands/helpers.rs:193`      | `pub fn prepare_outgoing(input: &str, names: &[String], cwd: &Path) -> String` (util.rs); `pub fn prepare_outgoing_via(agent: &Arc<tokio::sync::Mutex<Agent>>, input: &str) -> String` (helpers.rs — **sync** fn, re-exported as `hrdr_app::prepare_outgoing_via`); also `pub fn agent_cwd(&Arc<tokio::sync::Mutex<Agent>>) -> PathBuf` (helpers.rs:173)                                                                                                                                                                                                                                                                                                                                                                                                                                | nothing                                                                  |
| Todos               | `crates/hrdr-tools/src/lib.rs:72`                                                        | `pub struct TodoItem { content: String, status: String }` — **serde** (`status`: `pending\|in_progress\|completed\|cancelled`)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          | nothing                                                                  |
| Sessions            | `crates/hrdr-agent/src/session.rs` (re-exported from `hrdr_app`)                         | `pub struct Session { version, created, updated, state: SessionState }` (serde); `SessionState` (serde; fields incl. `name`, `id: Option<String>`, `model: ModelRef`, `base_url`, `cwd`, `messages`, `todos`, `transcript`, `usage`); `resolve_session(cwd: &str, arg: &str) -> Option<(String, Session)>`; `save_session(&SessionState) -> Result<Option<SaveOutcome>>`; `list_sessions() -> Vec<SessionMeta>`                                                                                                                                                                                                                                                                                                                                                                         | nothing                                                                  |
| Config loading      | `crates/hrdr-agent/src/config.rs:1327-1342`                                              | `AgentConfig::load_diagnosed() -> (Self, ConfigDiagnostics)`; `pub fn config_dir() -> Option<PathBuf>`; `config_file_path() -> Option<PathBuf>`; `pub fn read_config_file<T: DeserializeOwned>() -> Option<T>`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          | `hrdr-web` parses its own `[web]` table via `read_config_file` (slice 4) |
| Data dir            | `crates/hrdr-agent/src/session.rs:660`                                                   | `pub fn sessions_dir() -> PathBuf` (`hjkl_xdg::data_dir("hrdr")/sessions`)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              | web puts its SQLite DB at `hjkl_xdg::data_dir("hrdr")/web/users.sqlite`  |
| CLI                 | `apps/hrdr/src/main.rs:205`                                                              | `#[derive(Subcommand)] enum Command { Run{…}, Models }`; headless reference loop at `run_headless` (line ~651)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          | add `Command::Serve { … }` (slice 3)                                     |

**Corrections to the old plan (memorize these):**

1. **`hrdr-agent` types cannot be shared with the WASM client.** `hrdr-agent`
   depends on `tokio` (workspace `features = ["full"]`), `reqwest`, `which`,
   `zstd`, `filetime` (verified in `crates/hrdr-agent/Cargo.toml`) — it does not
   build on `wasm32-unknown-unknown`. Also `PaneId`, `TurnStats`, `AgentEvent`,
   `ToolBody`, `StatusSeg` have **no serde derives** (all verified: `PaneId`
   derives `Debug, Clone, Copy, PartialEq, Eq, Hash`; `AgentEvent`
   `Debug, Clone`; `ToolBody`/`ToolDisplay` `Debug, Clone, PartialEq, Eq`;
   `StatusSeg`/`StatusRun`/`CtxGauge` `Debug, Clone`). Therefore `hrdr-protocol`
   defines **self-contained wire types** (serde only), and `hrdr-web` converts
   core types → wire types. A round-trip test pins `WireEntry`'s JSON to
   `Entry`'s JSON (§4).
2. **The event log is effectively single-reader.** `events_since` takes a
   per-reader cursor, but `PaneSet::sync` calls `live.compact(key, next)` after
   folding (`crates/hrdr-agent/src/pane.rs:408-417`) — events below the sole
   PaneSet's cursor are dropped. A second independent reader would silently lose
   events. So: the **server owns the one `PaneSet`** and fans out to browsers
   from its own projection with a seq-numbered replay buffer (§5). Browsers
   never read the event log.
3. **`CommandHost` is a trait the frontend implements**, not a service the
   frontend calls. The web server implements it (`WebHost`) and calls
   `dispatch(&mut host, line)`.
4. There is **no `Status` type**; the status seam is
   `StatusInputs`/`status_sections`/`StatusSeg`.
5. A submitted message is not "text → queue": it is `@file`-expanded via
   `prepare_outgoing_via`, wrapped in `Steer::new(sent, display)`, and delivered
   via `LiveSubagents::send_prompt(key, steer, on_event)` — which itself decides
   steer-vs-new-turn for **any** pane, main included. (One deliberate exception,
   specified in §5: an **idle main-pane** submit is enqueued and spawned by the
   server itself so `Cancel` can reach the turn's `JoinHandle` — `send_prompt`'s
   internal spawn keeps no reachable handle.)

## 2. Resolved questions (formerly "open") — do not reopen

| Question              | Decision                                                                                                                                                                                                                          | Rationale (one line)                                                                                         |
| --------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| Client stack          | **Dioxus 0.7** (latest stable on crates.io as of 2026-07-26), web platform, built with `dx build --platform web`                                                                                                                  | Already recommended; one Rust codebase → web + future desktop/mobile; shares `hrdr-protocol`.                |
| Rendering model shape | Server sends `Entry`-shaped JSON **plus** a per-entry display model (tool classification, diff line classes) and pre-built status segments; the client does markdown + syntax highlighting only                                   | Keeps fold/classify single-sourced server-side; markdown/highlighting are pure formatting, safe client-side. |
| Multi-session         | **One session per `hrdr serve`**. `/new`, `/resume <id>`, `/rename` work through dispatch and swap the hosted session in place. A session-browser UI is deferred (post-parity list at the end)                                    | Smallest correct first cut; the commands already give session switching without new protocol.                |
| v2 attach concurrency | Deferred entirely. When built, the steering queue already serializes inputs; the blocker to solve then is item 2 above (single-reader compaction)                                                                                 | Not needed for headless-first; recorded so nobody "fixes" compaction prematurely.                            |
| TLS story             | **Both**: optional built-in rustls (`tls_cert_path`/`tls_key_path` under `[web]`, via `axum-server`), and a documented reverse-proxy path (proxy terminates TLS, hrdr binds loopback). Either satisfies the non-loopback TLS gate | Built-in covers the no-infra user; reverse proxy is the battle-tested path.                                  |
| GUI shell toolchain   | Localhost WS is the only transport; **no native IPC bridge**. Shell choice (Tauri/`wry` vs Dioxus desktop) is deferred to the post-parity list                                                                                    | The protocol is shell-agnostic by construction; nothing in this spec depends on the choice.                  |

## 3. Workspace changes

Edit the root `Cargo.toml` — **each addition lands in the slice that creates the
crate it names** (a `members` entry for a directory that does not exist yet
breaks every cargo command): slice 1 adds the `hrdr-protocol` member + workspace
dep + the `exclude`; slice 2 adds the `hrdr-web` member + workspace dep; slice 3
adds `hrdr-web.workspace = true` to `apps/hrdr/Cargo.toml`.

- Add to `[workspace] members`: `"crates/hrdr-protocol"` (slice 1),
  `"crates/hrdr-web"` (slice 2).
- Add to `[workspace]` an `exclude = ["crates/hrdr-ui"]` key (`hrdr-ui` is a
  WASM-only crate built by `dx`, kept out of the workspace so
  `cargo test`/clippy on the host stay green).
- Add to `[workspace.dependencies]`:

```toml
# internal (add beside the existing internal entries)
hrdr-protocol = { path = "crates/hrdr-protocol", version = "0.7.0" }
hrdr-web = { path = "crates/hrdr-web", version = "0.7.0" }

# web server (used only by hrdr-web)
axum = { version = "0.8", features = ["ws"] }
axum-server = { version = "0.8", features = ["tls-rustls"] }
rust-embed = "8"
rusqlite = { version = "0.40", features = ["bundled"] }
argon2 = "0.5"
subtle = "2"
hmac = "0.12"
```

Versions above were verified against crates.io on 2026-07-26 (axum 0.8.9,
axum-server 0.8.0, rust-embed 8.12, rusqlite 0.40.1, argon2 0.5.3, subtle 2.6.1,
hmac latest-compatible below). If any of them fails to resolve at implementation
time, run `cargo add <crate>` inside `crates/hrdr-web` to take the then-current
version — do not hand-guess. Two pins are deliberate, keep them:

- `hmac = "0.12"` (NOT 0.13): hmac 0.12 is the digest-0.10 line that matches the
  workspace's existing `sha2 = "0.10"`; hmac 0.13 needs digest 0.11 and will not
  accept `sha2 0.10` types.
- `rand`, `base64`, `sha2`, `chrono`, `toml`, `futures-util`, `tempfile`,
  `serde`, `serde_json`, `anyhow`, `tokio` are **already in
  `[workspace.dependencies]`** — do not re-add them; reference them with
  `.workspace = true` from `crates/hrdr-web/Cargo.toml`.

Crate manifests:

- **`crates/hrdr-protocol/Cargo.toml`** — deps: `serde` (workspace),
  `serde_json` (workspace, **dev-dependency** only, for tests). Nothing else.
  This crate must stay `wasm32`-clean: no tokio, no anyhow, no chrono.
- **`crates/hrdr-web/Cargo.toml`** — deps (all workspace where listed above):
  `hrdr-protocol`, `hrdr-app`, `hrdr-agent`, `hrdr-tools`, `anyhow`, `tokio`,
  `serde`, `serde_json`, `futures-util`, `axum`, `axum-server`, `rust-embed`,
  `rusqlite`, `argon2`, `subtle`, `hmac`, `rand`, `base64`, `sha2`, `chrono`,
  `toml`. Dev-deps: `hrdr-test-support` (workspace, and the
  `#[cfg(test)] extern crate hrdr_test_support;` line in `lib.rs` — copy the
  comment block from `hrdr-app/src/lib.rs` lines 9–15), `tempfile`. Feature:
  `ui = []` — when enabled, embeds `crates/hrdr-ui/dist` via `rust-embed`;
  default **off** (server then serves a minimal built-in HTML page). No feature
  flags on core crates.
- **`crates/hrdr-ui/Cargo.toml`** — NOT a workspace member.
  `dioxus = { version = "0.7", features = ["web"] }`,
  `hrdr-protocol = { path = "../hrdr-protocol" }`, `serde`, `serde_json`,
  `pulldown-cmark = "0.13"`, `web-sys`/`gloo` only as Dioxus pulls them. Built
  exclusively with `dx build --platform web --release`. `dx` writes its output
  under `target/dx/hrdr-ui/…` (it prints the exact path when the build ends —
  read it from the build output, do not guess); slice 7's build step copies that
  directory to `crates/hrdr-ui/dist` so the `rust-embed` folder path is stable.
  Gitignore both `crates/hrdr-ui/dist` and `crates/hrdr-ui/target`.
- **`apps/hrdr/Cargo.toml`** — add `hrdr-web.workspace = true`.

## 4. Protocol spec (`crates/hrdr-protocol`)

One module, `src/lib.rs`. Every type derives
`Debug, Clone, PartialEq, Serialize, Deserialize`. **Tag strategy:** all message
enums are internally tagged `#[serde(tag = "type", rename_all = "snake_case")]`
and every variant is a **struct variant** (internal tagging cannot represent
newtype variants of primitives — do not add any). Versioning: pre-1.0 the
protocol breaks freely; server and client are always built from the same commit;
there is **no version-negotiation field**.

```rust
/// Which conversation a message concerns. Mirrors hrdr_agent::PaneId.
/// External tagging on purpose: serializes as "main" or {"sub": 7}.
#[derive(..., Serialize, Deserialize, Eq, Hash, Copy)]
#[serde(rename_all = "snake_case")]
pub enum WirePaneId { Main, Sub(u64) }

/// Byte-for-byte the JSON of hrdr_agent::Entry (flat kind/data + unix time).
pub struct WireEntry {
    #[serde(flatten)]
    pub kind: WireEntryKind,
    pub time: i64, // unix seconds — Entry serializes DateTime<Local> this way
}

#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum WireEntryKind {
    Header,
    User(String),
    Assistant(String),
    Reasoning { text: String, #[serde(default)] took_ms: Option<u64> },
    Tool { id: String, name: String, args: String, result: String, ok: bool, done: bool },
    System(String),
    Notice(String),
    Stats(String),
    Diff(String),
}
```

(`WireEntryKind` is the one exception to "struct variants only": it must match
`EntryKind`'s existing externally-shaped serde exactly, newtype variants
included. A test pins this — below.)

```rust
/// Server-computed display model that rides beside an entry.
pub struct WireEntryView {
    pub entry: WireEntry,
    /// For Tool entries: hrdr_agent::tool_display(name, args), converted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<WireToolDisplay>,
    /// For Diff entries AND Tool entries whose body is Diff: each line of the
    /// diff text classified by hrdr_app::classify_diff_line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_lines: Option<Vec<WireDiffLine>>,
}
pub struct WireToolDisplay { pub headline: String, pub body: WireToolBody }
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireToolBody {
    Shell { command: String },
    Code { lang: String, content: String },
    Diff {},
    Read {},
    Details { rows: Vec<(String, String)> },
    Text {},
}
pub struct WireDiffLine { pub kind: WireDiffLineKind, pub text: String }
#[serde(rename_all = "snake_case")]
pub enum WireDiffLineKind { Hunk, Add, Remove, Meta }

/// Snapshot of one pane's chrome (list row + status-bar inputs live here).
pub struct WirePane {
    pub id: WirePaneId,
    pub title: String,
    pub status: WirePaneStatus,   // mirrors hrdr_agent::PaneStatus
    pub model: String,
    pub provider: String,
    pub effort: Option<String>,
    pub pending: Vec<String>,     // queued-but-undelivered user messages
    pub compacting: bool,
    pub turn: WireTurn,
    pub todos: Vec<WireTodo>,
}
#[serde(rename_all = "snake_case")]
pub enum WirePaneStatus { Running, Idle, Done }
pub struct WireTodo { pub content: String, pub status: String }
pub struct WireTurn {
    pub running: bool,
    pub inferring: bool,
    pub elapsed_ms: u64,          // TurnStats::infer_elapsed().as_millis()
    pub ttft_secs: Option<f64>,
    pub tok_per_sec: f64,
    pub out_tokens: usize,
    pub started_unix: Option<i64>, // from TurnStats::started_at
}

/// Pre-built status bar (server ran hrdr_app::status_sections).
pub struct WireStatus { pub left: Vec<WireStatusSeg>, pub right: Vec<WireStatusSeg> }
pub struct WireStatusSeg {
    pub priority: u8,
    pub runs: Vec<WireStatusRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gauge: Option<WireGauge>,
}
pub struct WireStatusRun { pub text: String, pub role: WireStatusRole }
#[serde(tag = "role", rename_all = "snake_case")]
pub enum WireStatusRole {
    Dir {}, Branch {}, TokensIn {}, TokensOut {},
    CtxFill { level: WireCtxLevel }, CtxRest {}, CtxPlain {},
    Provider {}, Model {}, Effort {}, Ttft {}, Session {},
}
#[serde(rename_all = "snake_case")]
pub enum WireCtxLevel { Ok, Warn, Critical }
pub struct WireGauge { pub frac: f64, pub level: WireCtxLevel, pub label: String }
```

The messages. Every server frame carries a global sequence number:

```rust
pub struct ServerFrame { pub seq: u64, #[serde(flatten)] pub msg: ServerMsg }

#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    /// First frame on connect (and after a failed resume): complete state.
    Snapshot {
        session_id: Option<String>,
        session_name: String,
        cwd: String,
        panes: Vec<WirePane>,
        active: WirePaneId,
        status: WireStatus,
        transcripts: Vec<PaneTranscript>, // one per pane, full
        show_thinking: bool,
    },
    /// Replace pane's entries from index `from` to the end with `entries`.
    /// `from == 0` with empty `entries` = the transcript was cleared (/new).
    Entries { pane: WirePaneId, from: usize, entries: Vec<WireEntryView> },
    /// Pane list / chrome changed (panes added, released, status, turn, todos).
    Panes { panes: Vec<WirePane>, active: WirePaneId },
    Status { status: WireStatus },
    /// A system line produced outside the fold (async command output).
    Notice { text: String },
    /// The server asks the client to replace/augment its input box
    /// (CommandHost::set_input / prepend_input / insert_input).
    SetInput { mode: InputSetMode, text: String },
    /// Resume accepted: client state is current up to `seq`; deltas follow.
    Resumed {},
    /// Auth failed / connection refused; the socket closes after this.
    Error { message: String },
}
#[serde(rename_all = "snake_case")]
pub enum InputSetMode { Replace, Prepend, InsertAtCursor }
pub struct PaneTranscript { pub pane: WirePaneId, pub entries: Vec<WireEntryView> }

#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// User pressed send. Routed via LiveSubagents::send_prompt (steer if a
    /// turn is in flight, new turn if idle). `pane` = the pane on screen.
    Submit { pane: WirePaneId, text: String },
    /// A slash command line (leading '/'). Runs through hrdr_app dispatch.
    Command { pane: WirePaneId, line: String },
    /// Cancel the active turn on `pane` (abort task + clear_pending).
    Cancel { pane: WirePaneId },
    SwitchPane { pane: WirePaneId },
    /// Reconnect: client last saw `seq`. Server replays buffered frames after
    /// `seq`, or sends a fresh Snapshot if the buffer no longer reaches back.
    Resume { seq: u64 },
}
```

### Exact wire examples (write these into protocol tests verbatim)

Snapshot (abridged to one pane, one entry):

```json
{
  "seq": 1,
  "type": "snapshot",
  "session_id": "fix-parser",
  "session_name": "fix parser",
  "cwd": "/home/me/proj",
  "panes": [
    {
      "id": "main",
      "title": "main",
      "status": "idle",
      "model": "gpt-5.5",
      "provider": "openai",
      "effort": null,
      "pending": [],
      "compacting": false,
      "turn": {
        "running": false,
        "inferring": false,
        "elapsed_ms": 0,
        "ttft_secs": null,
        "tok_per_sec": 0.0,
        "out_tokens": 0,
        "started_unix": null
      },
      "todos": []
    }
  ],
  "active": "main",
  "status": { "left": [], "right": [] },
  "transcripts": [
    {
      "pane": "main",
      "entries": [
        { "entry": { "kind": "user", "data": "hi", "time": 1753500000 } }
      ]
    }
  ],
  "show_thinking": true
}
```

Entry delta (assistant text grew — tail replacement from index 3):

```json
{
  "seq": 42,
  "type": "entries",
  "pane": "main",
  "from": 3,
  "entries": [
    {
      "entry": {
        "kind": "assistant",
        "data": "Done — it was an off-by-one.",
        "time": 1753500100
      }
    }
  ]
}
```

A tool entry with its display model:

```json
{
  "entry": {
    "kind": "tool",
    "data": {
      "id": "c1",
      "name": "shell",
      "args": "{\"command\":\"ls\"}",
      "result": "src\n",
      "ok": true,
      "done": true
    },
    "time": 1753500050
  },
  "tool": { "headline": "", "body": { "type": "shell", "command": "ls" } }
}
```

Sub-agent pane id: `{ "sub": 7 }`. Submit / command / resume:

```json
{"type":"submit","pane":"main","text":"fix the bug"}
{"type":"command","pane":{"sub":7},"line":"/status"}
{"type":"resume","seq":41}
```

## 5. Server internals (normative — implemented across slices 2–3)

`hrdr-web`'s core object (slice 2), `src/session.rs`:

```rust
pub struct WebSession { /* fields below are private */ }
// Owns: agent: Arc<tokio::sync::Mutex<Agent>>, steering: SteeringQueue,
//        live: LiveSubagents, panes: PaneSet,
//        broadcast: tokio::sync::broadcast::Sender<ServerFrame>,
//        seq: u64, replay: VecDeque<ServerFrame> (cap REPLAY_CAP = 1024),
//        sent: HashMap<WirePaneId, Vec<u64>> (content hashes last broadcast),
//        show_thinking: bool, session state mirror (id/name/cwd).
```

Construction mirrors the TUI at `crates/hrdr-tui/src/app.rs:704-739`
(`publish_main_agent`). In order:

1. Create the sub-agent transcript-dir cell **before** the agent, exactly as the
   TUI does at `app.rs:556-557`:
   `let subagent_dir: Arc<std::sync::Mutex<Option<PathBuf>>> = Default::default();`
   then `config.subagent_transcript_dir = Some(subagent_dir.clone());`. Keep the
   cell as a `WebSession` field — persistence (below) writes into it once a
   session id exists.
2. `Agent::new(config)` → `steering_queue()` → `LiveSubagents::new()` →
   `register_main(agent, steering, model, provider, base_url, usage)` →
   `attach_live(live, MAIN_KEY)` (lock the agent async, it is not contended at
   startup) → `agent.connect_mcp().await`, each returned notice broadcast as
   `Notice`.

**The sync tick** (the heart — one method, `WebSession::tick()`):

1. `live.update(MAIN_KEY, |e| e.running = <is a main turn task alive>)` then
   `panes.sync(&live)` — exactly what `sync_panes` does in the TUI
   (`app.rs:1601`). This folds new events into every pane transcript.
2. For each pane, diff `pane.transcript()` against `sent[pane_id]` using
   `Entry.content_hash`: find the first index `i` where length or hash differs;
   if any, emit `Entries { pane, from: i, entries: convert(&transcript[i..]) }`
   and update `sent`. (Entries can mutate in place — streamed text, tool results
   — which is why this is hash-diff, not append-only.)
3. Rebuild `Vec<WirePane>` + `WireStatus`; if changed since last broadcast
   (compare the serialized value), emit `Panes` / `Status`. Pane order is the
   switcher's: `std::iter::once(panes.main()).chain(panes.subs())` — the same
   main-first order `hrdr_app::pane_rows` uses. Drop `sent` map entries whose
   pane no longer exists.
4. Every emitted `ServerFrame` gets `seq += 1`, is pushed to `replay` (dropping
   the front past `REPLAY_CAP`), and sent on `broadcast`.

Drive `tick()` from a `tokio::time::interval(Duration::from_millis(100))` task,
plus an immediate tick whenever a turn event callback fires (use a
`tokio::sync::Notify`). Status inputs are built from the **active** pane
(`StatusInputs` fields, `crates/hrdr-app/src/status.rs:179`): `dir` =
`hrdr_app::display_dir(&hrdr_app::agent_cwd(&main_agent))` — NOT `state.cwd`,
which stays empty until the first save; `branch` =
`hrdr_app::git_branch(&hrdr_app::agent_cwd(&main_agent))` cached for 5s;
`tokens_in/tokens_out/ctx_used/context_window` from `state.usage`
(`usage.ctx_used()` for `ctx_used`); `auto_compact_enabled`/
`compaction_reserved` from the pane fields of the same names; model/provider
from `pane.model()/provider()`; `session` = main pane `state.name` when
non-empty, else `None`; `effort` = `pane.effort`; `ttft` = `pane.turn.ttft()`;
`nerd_icons: false`.

**Submitting** (`WebSession::submit(pane, text)`): map `WirePaneId` → key
(`Main` → `MAIN_KEY`);
`let sent = prepare_outgoing_via(&agent_for(pane), &text)` (mirror `agent_for`
at `crates/hrdr-tui/src/app.rs:1671`: main → the main agent handle, sub →
`live.handle(key)` falling back to main);
`live.send_prompt(key, Steer::new(sent, text), on_event)` where `on_event`
clones a `Notify` handle and pings the tick task (events are already recorded on
the agent's entry by `send_prompt` itself — do **not** record again, and do
**not** fold into the transcript by hand; the tick's `sync` does it).
`send_prompt` returning `None` → broadcast
`Notice("that agent has finished and been released")`. After a turn ends, the
tick task checks `!live.is_running(key) && !live.pending(key).is_empty()` and,
if so, restarts an opener-less run: `live.handle(key)` → `begin_turn(key)` →
spawn `agent.lock().await.run(steering, cb)` holding a
`hrdr_agent::RunGuard::new(live.clone(), key)` in the task (it is exported; its
drop calls `end_turn` on every exit) — this mirrors the undelivered-steer
relaunch the TUI does.

**Every server-spawned turn task** (the main-pane idle spawn below and the
relaunch above) must handle `run`'s error the way `send_prompt`'s own spawn does
(`crates/hrdr-agent/src/subagent_live.rs:617-623`): on `Err(e)`, call the same
event callback with `AgentEvent::Notice(format!("[error] {e:#}"))` and then
`AgentEvent::TurnDone` — `run` only emits `TurnDone` on success, and without
this the turn never visibly ends.

**Cancel**: keep the `tokio::task::JoinHandle` of any turn task the server
spawned; `Cancel` aborts it, then `live.clear_pending(key)` and
`live.end_turn(key)`; broadcast a `Notice("turn cancelled")`, and run the
persistence routine below (the TUI also autosaves on a cancelled turn, so the
partial reply isn't lost). (For turns started inside `send_prompt` the handle is
not reachable — for those, `Cancel` on a sub-agent pane only clears pending and
notices; main-pane turns must therefore be spawned by the server itself, not via
`send_prompt`'s idle branch. Concretely: `submit` on **Main** when idle does the
enqueue + spawn itself — first reserve the session id if none is assigned yet
(mirror `reserve_session_id`, `crates/hrdr-tui/src/app/session.rs:198`: push
`hrdr_agent::Message::user(&sent)` into `state.messages`, run the persistence
routine below, so the id + transcript writer exist before the turn's first
delegated `task`); then enqueue the `Steer` via `live.enqueue(MAIN_KEY, steer)`,
`begin_turn`, spawn `agent.run(steering, cb)` keeping the handle, `cb` = record
to `live.record(MAIN_KEY, &ev)` + notify tick, with the error handling above
(this matches TUI `spawn_turn`/`launch_turn`, `app.rs:1967` — note the TUI
records main events in the frontend, `app.rs:2378`). Submit on Main while
running → `send_prompt` (steer path only). Submit on Sub panes → `send_prompt`
always.)

**Persistence** (one routine, `WebSession::persist()`, mirroring TUI `autosave`
at `crates/hrdr-tui/src/app/session.rs:237` — calling `save_session` alone is
NOT enough):

1. Refresh the mirror: `agent.try_lock()` (skip this save if a turn holds the
   lock) →
   `state.sync_from(a.messages_owned(), todos, a.cwd().display().to_string())`
   where `todos` = `panes.main().todos.lock().clone()`. `sync_from` also
   auto-names an unnamed session from its first user message.
2. `hrdr_app::save_session(&panes.main().state)` on
   `tokio::task::spawn_blocking`.
3. **Adopt the outcome** — this is what makes the id stable: on `Ok(Some(o))`,
   set `panes.main_mut().state.id = Some(o.id)` and, when `o.open_lock` is
   `Some` (first save), store it in a
   `WebSession::active_lock: Option<hrdr_app::SessionLock>` field and hold it
   for the session's lifetime. Skipping this step mints a NEW session file on
   every save.
4. After an id exists, point the durable writers at it (mirror
   `refresh_subagent_dir`, `crates/hrdr-tui/src/app/session.rs:42`): write
   `hrdr_app::subagent_transcript_dir(&cwd, &id)` into the `subagent_dir` cell,
   and
   `live.attach_transcript(MAIN_KEY, &hrdr_app::session_transcript_path(&cwd, &id))`
   (idempotent). Without this the display transcript is never written — it is
   `skip_serializing` on `SessionState` and rebuilt from that jsonl on resume.

Run `persist()`: on `AgentEvent::TurnDone` (observed in `cb`, after the next
`sync`), on cancel, and at server shutdown. On `AgentEvent::History(msgs)`,
instead update `panes.main_mut().state.messages = msgs` and save **without**
step 1's lock read (the turn holds the lock — mirror `persist_mid_turn`,
`session.rs:154`), still running steps 3–4.

## 6. Security requirements (checklists)

### Config: `[web]` table (parsed by `hrdr-web` via `hrdr_agent::read_config_file`)

| Key                   | Type   | Default                        | Env override            | CLI flag (on `hrdr serve`)     |
| --------------------- | ------ | ------------------------------ | ----------------------- | ------------------------------ |
| `bind`                | string | `"127.0.0.1"`                  | `HRDR_WEB_BIND`         | `--bind <ADDR>`                |
| `port`                | u16    | `9911`                         | `HRDR_WEB_PORT`         | `--port <N>`                   |
| `auth`                | string | `"token"`                      | `HRDR_WEB_AUTH`         | `--auth <basic\|users\|token>` |
| `basic_user`          | string | none                           | `HRDR_WEB_BASIC_USER`   | —                              |
| `basic_password_hash` | string | none                           | —                       | —                              |
| `users_db`            | path   | `<data>/hrdr/web/users.sqlite` | `HRDR_WEB_USERS_DB`     | `--users-db <PATH>`            |
| `tls_cert_path`       | path   | none                           | `HRDR_WEB_TLS_CERT`     | `--tls-cert <PATH>`            |
| `tls_key_path`        | path   | none                           | `HRDR_WEB_TLS_KEY`      | `--tls-key <PATH>`             |
| `allow_remote`        | bool   | `false`                        | `HRDR_WEB_ALLOW_REMOTE` | `--allow-remote`               |

Precedence: CLI flag > env > file > default (same as the rest of hrdr). Env-var
problems are warnings; file problems are startup errors (match
`ConfigDiagnostics` policy).

Auth modes:

- `token` (default): on startup the server generates a 32-byte random URL-safe
  token, prints `open http://<bind>:<port>/?token=…` (the actual bind/port) to
  stderr, and requires it as `?token=` on `GET /` and on the WS upgrade (or
  `Authorization: Bearer`). **Loopback only** — `token` mode never satisfies the
  remote gate.
- `basic`: HTTP Basic against `basic_user` + `basic_password_hash` (argon2id PHC
  string; generate with `hrdr serve --hash-password`, which reads the password
  from stdin, prints the hash, and exits). WS authenticates via the upgrade
  request's `Authorization` header.
- `users`: SQLite table
  `users(username TEXT PRIMARY KEY, password_hash TEXT NOT NULL, created INTEGER NOT NULL)`;
  `POST /login` (JSON `{username,password}`) verifies argon2id and sets a signed
  session cookie; WS authenticates via the cookie. Manage with
  `hrdr serve --add-user <name>` / `--remove-user <name>` (password read from
  stdin; command exits without serving).

### Refuse-to-bind rules (hard errors at startup, checked in `serve()` before binding)

- [ ] Parse `bind`; **loopback** = `IpAddr::is_loopback()`.
- [ ] Non-loopback bind AND `allow_remote == false` → error
      `"refusing to bind <addr>: pass --allow-remote (plus auth and TLS)"`.
- [ ] Non-loopback AND `auth == "token"` → error (token mode is loopback-only).
- [ ] Non-loopback AND auth backend unconfigured (basic without user+hash; users
      with an empty/missing DB) → error.
- [ ] Non-loopback AND TLS cert/key unset → error naming both the built-in TLS
      keys and the reverse-proxy alternative (`bind` loopback behind a proxy).
- [ ] All three present → serve HTTPS via `axum-server`'s rustls binding.
- [ ] Basic auth over plain HTTP is permitted **only** on loopback.

### Hardening checklist (slice 4/5)

- [ ] WS upgrade: reject when an `Origin` header is present and its host is
      neither the request `Host` nor localhost (CSRF via browser WS).
- [ ] Credential comparison via `subtle::ConstantTimeEq` (token, cookie MAC);
      password checks via `argon2::PasswordVerifier` (already constant-time).
- [ ] Auth-failure rate limit: per-IP sliding window, max 10 failures/minute,
      then 429 with `Retry-After: 60`; in-memory
      `HashMap<IpAddr, VecDeque<Instant>>` behind a mutex — no new dependency.
- [ ] Session cookie: name `hrdr_session`, value
      `base64(username || ":" || expiry_unix || ":" || hmac_sha256(secret, username:expiry))`,
      attributes `HttpOnly; SameSite=Strict; Path=/`, plus `Secure` when TLS is
      on. `secret` = 32 random bytes generated per server start (a restart logs
      everyone out — acceptable pre-1.0, no persistence shim). Expiry: 7 days.
- [ ] The token/credentials never appear in any log line.
- [ ] `/healthz` is the only unauthenticated route (returns `ok`, no state).

## 7. Slices

> Reminder: one slice = one commit = fmt + clippy + tests green. Titles are the
> commit subjects.

### Slice 1 — `feat(web): hrdr-protocol wire types`

**Goal:** the shared wire vocabulary exists, JSON-pinned by tests.

- Create `crates/hrdr-protocol/{Cargo.toml,src/lib.rs}` with **every** type in
  §4, documented. Edit root `Cargo.toml`: the `hrdr-protocol` member + its
  workspace dep + the `exclude` for the future `hrdr-ui` — NOT the `hrdr-web`
  entries, that crate does not exist until slice 2 (see §3).
- Tests (in `src/lib.rs` `#[cfg(test)]`):
  - `wire_examples_round_trip` — parse each JSON example from §4 into its type,
    re-serialize, compare `serde_json::Value`s.
  - `pane_id_wire_shape` — `Main` ⇄ `"main"`, `Sub(7)` ⇄ `{"sub":7}`.
  - `server_frame_flattens_seq` — a `ServerFrame{seq:1, msg: Notice{..}}`
    serializes to `{"seq":1,"type":"notice","text":…}`.
- **Out of scope:** any dependency beyond serde/serde_json; any conversion from
  core types (that's slice 2 — this crate must not depend on hrdr-agent).

### Slice 2 — `feat(web): WebSession headless host`

**Goal:** a network-free session host that owns the agent, folds panes, and
emits `ServerFrame` deltas on a broadcast channel.

- Create
  `crates/hrdr-web/{Cargo.toml,src/lib.rs,src/session.rs,src/convert.rs}`, and
  add the `"crates/hrdr-web"` member + `hrdr-web` workspace dep to the root
  `Cargo.toml` (deferred from §3).
- `src/convert.rs`: `pub fn wire_entry(&Entry) -> WireEntry`,
  `wire_entry_view(&Entry) -> WireEntryView` (calls `tool_display` for Tool
  entries; classifies diff text via `classify_diff_line` for `Diff` entries and
  Tool entries whose `ToolBody` is `Diff` **and** `done`),
  `wire_pane(&Pane) -> WirePane` (`title` from `Pane::title()`),
  `wire_turn(&TurnStats, running: bool) -> WireTurn` (caller passes
  `running = pane.status == PaneStatus::Running`;
  `inferring/elapsed_ms/ttft_secs/tok_per_sec/out_tokens/started_unix` map 1:1
  onto the `TurnStats` accessors in §1),
  `wire_status(&StatusInputs) -> WireStatus`, `wire_pane_id`/`core_pane_id`.
- `src/session.rs`: `WebSession` exactly per §5 —
  `pub async fn new(config: AgentConfig) -> anyhow::Result<Self>`,
  `pub fn subscribe(&self) -> (Snapshot as ServerFrame, broadcast::Receiver<ServerFrame>)`
  (snapshot built on demand, seq-stamped),
  `pub async fn submit(&mut self, pane: WirePaneId, text: String)`, `cancel`,
  `switch_pane`, `pub fn tick(&mut self)`,
  `replay_after(seq) -> Option<Vec<ServerFrame>>`. Wrap the whole thing in
  `pub struct SharedSession(Arc<tokio::sync::Mutex<WebSession>>)` with the tick
  task spawned by `SharedSession::start(config)`.
- Tests (`crates/hrdr-web/src/session.rs` `#[cfg(test)]`, using the mock-server
  pattern from `hrdr-agent`'s turn-loop tests is NOT required — drive the fold
  directly):
  - `tick_broadcasts_entry_deltas` — `live.record(MAIN_KEY, Text("he"))`,
    `tick()`, expect `Entries{from:0}` with one assistant entry;
    `record(Text("llo"))`, `tick()`, expect `Entries{from:0}` again (same entry
    mutated) whose text is `"hello"`; seq strictly increasing.
  - `tick_is_quiet_when_nothing_changed` — two ticks, one delta.
  - `tool_entries_carry_display_model` — record ToolStart/ToolEnd for `shell`,
    assert the broadcast `WireEntryView.tool` is `Shell{command}`.
  - `replay_after_returns_gap_or_none` — fill past `REPLAY_CAP`, assert old seq
    → `None`, recent seq → the exact frames.
  - `wire_entry_matches_core_entry_json` — for one Entry of every `EntryKind`
    variant:
    `serde_json::to_value(entry) == serde_json::to_value(wire_entry(entry))`.
    **This is the drift tripwire — do not skip it.**
- **Out of scope:** axum, auth, CLI, any HTTP; implementing `CommandHost` (slice
  6); touching `hrdr-agent`/`hrdr-app` (nothing needs to change there).

### Slice 3 — `feat(web): axum server, WS loop, and hrdr serve (loopback only)`

**Goal:** `hrdr serve` works end-to-end from a WS client on 127.0.0.1.

- `crates/hrdr-web/src/server.rs`:
  `pub struct ServeConfig { bind: IpAddr, port: u16 }` (defaults in this slice
  are hardcoded: `127.0.0.1` / `9911` — the `[web]` config table lands in slice
  4; auth fields come in slice 4 too; **hardcode loopback**: `serve()` returns
  an error if `!bind.is_loopback()` with message "authentication is not
  implemented yet");
  `pub async fn serve(session: SharedSession, cfg: ServeConfig) -> anyhow::Result<RunningServer>`;
  `pub struct RunningServer { pub addr: SocketAddr, /* JoinHandle + shutdown */ }`
  with `RunningServer::wait(self)` and `shutdown(self)`. Routes: `GET /healthz`
  → `ok`; `GET /` → embedded placeholder page (a static `const INDEX: &str` with
  a title and "connect a client to /ws" — replaced in slice 7); `GET /ws` →
  upgrade.
- WS handler: on connect send the snapshot frame, subscribe to broadcast,
  forward frames as JSON text messages; read loop parses `ClientMsg` and calls
  `submit`/`command`(slice 6 wires dispatch — until then reply
  `Notice("commands land in a later slice")`)/`cancel`/`switch_pane`/`resume`
  (`Resume` → `replay_after`; `Some(frames)` → send `Resumed{}` then frames;
  `None` → send fresh snapshot). A lagged broadcast receiver
  (`RecvError::Lagged`) → send fresh snapshot.
- `apps/hrdr/src/main.rs`: add
  `Command::Serve { bind: Option<String>, port: Option<u16> }`; the match arm
  builds `AgentConfig` the same way `Run` does (it is already built above the
  match), constructs `SharedSession::start(config).await?`, `serve(...)`, prints
  `serving http://<addr>/ (Ctrl-C to stop)`, and `wait()`s.
- Tests:
  - `healthz_answers_ok` and `ws_snapshot_then_delta` — integration test in
    `crates/hrdr-web/tests/server.rs`: start `serve` on port 0 and connect using
    dev-dependency `tokio-tungstenite = "0.30"` (add it to `hrdr-web`'s
    dev-deps; do not hand-roll the WS upgrade). Assert first frame is
    `snapshot`; `live.record`… is not reachable here, so assert a `submit`
    against an unreachable model endpoint produces an `entries` frame containing
    the user's message (the `Steered` fold) and eventually a `system` error
    entry (the agent's error notice) — generous timeout, 30s.
  - `serve_refuses_non_loopback` — `bind 0.0.0.0` → `Err` containing
    "authentication".
- Manual check: `cargo run -- serve` then `websocat ws://127.0.0.1:9911/ws` →
  snapshot arrives; `{"type":"submit","pane":"main","text":"hi"}` → entries
  frames stream.
- **Out of scope:** auth of any kind, TLS, `--allow-remote`, the real UI,
  dispatch/commands.

### Slice 4 — `feat(web): auth (token + basic), [web] config, bind gating`

**Goal:** every route and the WS upgrade are authenticated; the refuse-to-bind
matrix is enforced; still no TLS (non-loopback therefore still impossible).

- `crates/hrdr-web/src/config.rs`: `WebConfig` with **all** keys from §6,
  `WebConfig::load(cli: &CliOverrides) -> (WebConfig, Vec<String> /*warnings*/)`
  implementing file
  (`#[derive(Deserialize)] struct WebFileConfig { web: Option<…> }` via
  `hrdr_agent::read_config_file`) + `HRDR_WEB_*` env + CLI precedence, where
  `pub struct CliOverrides { pub bind: Option<String>, pub port: Option<u16>, pub auth: Option<String>, pub allow_remote: bool, pub users_db: Option<PathBuf>, pub tls_cert: Option<PathBuf>, pub tls_key: Option<PathBuf> }`
  (slices 4/5 fill it from the matching `Command::Serve` flags; fields whose
  flag hasn't landed yet stay `None`).
- `crates/hrdr-web/src/auth.rs`: token generation (the workspace pins
  `rand = "0.10"` — use the current API, `rand::rng()` +
  `.random::<[u8; 32]>()`, then base64 URL-safe no-pad; `thread_rng`/`gen` are
  the pre-0.9 names and will not compile), argon2 verify, the rate limiter, the
  constant-time comparisons, an axum middleware/extractor `RequireAuth` applied
  to `/` , `/ws`, and every future route except `/healthz`; the WS-upgrade
  `Origin` check from the §6 hardening list (reject when an `Origin` header is
  present and its host is neither the request `Host` nor localhost).
  `--hash-password` helper fn exported for the CLI.
- Enforce the **entire** refuse-to-bind checklist from §6 in `serve()`
  (non-loopback now errors on the TLS condition instead of the slice-3
  placeholder).
- Extend `Command::Serve` with `--auth`, `--allow-remote`, `--hash-password`
  (bool flag: read stdin, print PHC hash, exit 0).
- Tests: `token_auth_gates_ws_and_index` (401 without, 101/200 with);
  `basic_auth_challenges_then_accepts`; `rate_limiter_locks_out_after_10`;
  `bind_matrix` — table-driven over (loopback?, allow_remote, auth mode, tls
  set?) asserting exactly the §6 outcomes (TLS rows expect the error since TLS
  lands in slice 5).
- **Out of scope:** SQLite users, cookies, TLS, UI.

### Slice 5 — `feat(web): sqlite users, login cookie, TLS`

**Goal:** the `users` backend and the built-in TLS path; a fully-gated
non-loopback bind now actually works.

- `crates/hrdr-web/src/users.rs`: DB open/create (`rusqlite`, schema in §6),
  `add_user`, `remove_user`, `verify(username, password) -> bool`.
- `POST /login` + cookie mint/verify per the §6 hardening checklist; WS/`/`
  accept the cookie in `users` mode. `POST /logout` clears it.
- TLS: when cert+key set, bind via
  `axum_server::bind_rustls(addr, RustlsConfig::from_pem_file(cert, key).await?)`.
- CLI: `--add-user`, `--remove-user`, `--users-db`, `--tls-cert`, `--tls-key`.
- Tests: `login_sets_cookie_and_ws_accepts_it`;
  `bad_password_401_and_rate_limited`; `cookie_tamper_rejected` (flip one byte
  of the MAC); `add_then_remove_user`; `tls_serves_when_configured` (self-signed
  cert generated in the test via dev-dependency `rcgen = "0.14"`, client with
  cert verification disabled).
- **Out of scope:** UI, dispatch.

### Slice 6 — `feat(web): WebHost CommandHost + dispatch over WS`

**Goal:** `ClientMsg::Command` runs every shared slash command.

- `crates/hrdr-web/src/host.rs`:
  `pub struct WebHost<'a> { session: &'a mut WebSession }` implementing
  `hrdr_app::CommandHost`. Required methods (these have **no** default body —
  implement all): `info` (broadcast `Notice`), `agent` (active pane's agent —
  `live.handle(key)` fallback main, mirroring `crates/hrdr-tui/src/app.rs:1666`
  `active_agent`), `cwd` (`hrdr_app::agent_cwd(&main_agent)`, mirroring
  `crates/hrdr-tui/src/app/commands.rs:357`), `base_url` (active pane
  `state.base_url`), `model_ref`, `set_model_ref`, `show_thinking`,
  `set_show_thinking`, `clear_conversation` (mirror the TUI's `clear_all`,
  `crates/hrdr-tui/src/app/commands.rs:41`, minus its view-only bits: abort a
  running turn first; `agent.clear()` under the lock (spawn an async lock task
  if `try_lock` fails); clear the steering queue and
  `live.clear_pending(MAIN_KEY)`; clear the main pane's `todos` list; reset
  `state.usage` keeping `context_window`; `state.id = None`; drop `active_lock`;
  `live.detach_transcript(MAIN_KEY)` and clear the `subagent_dir` cell; clear
  `state.name` and `state.transcript` + `state.messages`; broadcast a full
  snapshot), `session_id`, `set_session_label` (set `state.name` AND
  `state.named_by_user = true`), `autosave` (call `WebSession::persist()` from
  §5), `resume` (mirror the TUI's `CommandHost::resume` at
  `crates/hrdr-tui/src/app/commands.rs:398` +
  `resume_locked_path`/`apply_session`/`adopt_state` at
  `crates/hrdr-tui/src/app/session.rs:80/271/324`, minus view-only bits:
  busy-guard with `hrdr_app::RESUME_BUSY_MSG` if a turn is running;
  `let path = hrdr_app::session_file_path(&session.state.cwd, &id)`;
  `hrdr_app::Session::open_path(&path)` — on `Ok((session, lock))` store `lock`
  in `active_lock`, adopt `session.state.restored()` into the main pane with the
  id, push its messages/todos back to the agent as `adopt_state` does,
  `detach_transcript` + re-run §5 persistence step 4, broadcast a full snapshot;
  on `Err(OpenError::Busy{pid,..})` broadcast
  `Notice("session is open in another hrdr instance (pid …)")` — the web has no
  fork prompt; on `Err(OpenError::Load(e))` a `Notice` with the error),
  `copy_to_clipboard` (return "clipboard isn't available over the web — select
  the text in your browser"), `last_reply`, `transcript_text`,
  `nth_message_text`, `line_poster` (clone of a
  `tokio::sync::mpsc::UnboundedSender<(LineKind,String)>` drained by the tick
  task: `LineKind::System` → broadcast `ServerMsg::Notice`; `LineKind::Diff` →
  push `Entry::diff(text)` onto the main pane transcript so the hash-diff
  broadcasts it with `diff_lines` classified), `is_busy`
  (`live.is_running(MAIN_KEY) || live.is_compacting(MAIN_KEY)`), `send_prompt`
  (with `show_as_user=true` route to `WebSession::submit`; with
  `show_as_user=false` mirror the TUI's `launch_hidden`, `app.rs:2041`:
  `agent.push_user_note(prompt)` under the lock, then an opener-less
  server-spawned turn — do NOT enqueue a `Steer` with an empty display, that
  folds an empty user entry into the transcript), `set_input`/`prepend_input`/
  `insert_input` (broadcast `SetInput` with the matching mode),
  `set_tool_expansion` (return "use the expand toggle on each tool block"),
  `start_compaction` (spawn `hrdr_app` compaction the way the TUI's
  `spawn_compaction` does — `crates/hrdr-tui/src/app.rs:2085`; copy the pattern,
  post completion via `line_poster`). Overrides worth setting:
  `supports_command` — return `false` for
  `"edit" | "paste" | "copy" | "theme" | "reload"` (terminal-bound; `/help` then
  hides them), `effort`, `session_label`, `context_usage`, `context_window`,
  `session_tokens`, `session_cost`, `session_cost_partial` (all read the active
  pane like the TUI host in `crates/hrdr-tui/src/app/commands.rs:459-482`).
- WS `Command` handling: if `is_quit_command(line)` →
  `Notice("use your browser's close button")`; else if it starts with `/` →
  `dispatch(&mut WebHost{…}, &line)`; unknown command (dispatch returns false) →
  `Notice("unknown command — /help")`.
- Tests: `help_lists_only_supported_commands` (no `/edit`);
  `status_command_reports_over_ws`; `new_clears_and_snapshots`;
  `rename_updates_the_status_session_badge` (dispatch `/rename webby`; assert
  the next `Status` frame's right side contains a run with text `webby` and role
  `session` — NOTE: `/model <arg>` cannot be used for a mutation test, the
  dispatcher's `model` arm ignores the argument and always opens the picker,
  `crates/hrdr-app/src/commands/dispatch.rs:39-44`);
  `thinking_off_flips_show_thinking` (dispatch `/thinking off`; assert a fresh
  `subscribe()` snapshot carries `show_thinking: false`).
- **Out of scope:** UI; picker modals (the defaulted `begin_*_selector` methods
  already degrade to text listings — leave them).

### Slice 7 — `feat(web): dioxus client skeleton (embedded)`

**Goal:** `hrdr serve` (built with `--features ui`) serves a real SPA that
renders the transcript and sends messages.

- Create `crates/hrdr-ui` (NOT a workspace member; confirm root `exclude`).
  `Dioxus.toml` per the Dioxus 0.7 web template (generate one with
  `dx new --platform web` in a scratch dir and copy the shape rather than
  writing it from memory); `main.rs` with: WS connect to `ws(s)://<host>/ws`
  (carry `?token=` from the page URL through to the upgrade), a `ServerFrame`
  reducer mirroring §4 semantics (`Entries{from}` = truncate-then-extend;
  `Snapshot` = replace world), a transcript view (plain `<pre>` text per entry
  kind for now), an input box + send button emitting `Submit`,
  autoscroll-to-bottom.
- `hrdr-web`: `ui` feature —
  `#[derive(rust_embed::Embed)] #[folder = "../hrdr-ui/dist"] struct Assets;`
  served at `/` and `/assets/*` with correct `Content-Type` (match extension;
  `wasm` → `application/wasm`) when the feature is on. Document the build in
  `crates/hrdr-ui/README.md`: `cargo install --locked dioxus-cli@^0.7`,
  `dx build --platform web --release`, then copy the output directory `dx`
  prints at the end of the build (under `target/dx/hrdr-ui/…` — read the real
  path from the build output, don't guess) to `crates/hrdr-ui/dist` (one
  `cp -r <printed-path> dist` line in the README), then
  `cargo run --features hrdr-web/ui -- serve`. The `rust-embed` folder stays
  fixed at `../hrdr-ui/dist` regardless of where `dx` writes.
- Acceptance: `cargo test` (workspace, feature off) green; with the feature on
  and `dist/` built, browser at `http://127.0.0.1:9911/?token=…` shows the
  transcript and a round-trip message. Client-side reducer logic that is pure
  (frame → state) lives in `crates/hrdr-ui/src/state.rs` with host-runnable unit
  tests (`cargo test` inside `crates/hrdr-ui` on the host target must pass —
  keep DOM types out of `state.rs`).
- **Out of scope:** styling beyond readable defaults, markdown, panes, status.

### Slice 8 — `feat(web): transcript fidelity (markdown, tools, diffs, reasoning)`

**Goal:** the transcript renders like the TUI's, adapted to HTML.

- In `hrdr-ui`: assistant/user markdown via `pulldown-cmark` (HTML sanitized:
  render to events, drop raw-HTML events entirely); tool blocks rendered from
  `WireEntryView.tool` (shell = `$ command` + output, code = `<pre>` with the
  lang class, details = key/value rows, read = tail of result, spinner while
  `done == false`, ✓/✗ by `ok`, collapsed result with an expand toggle —
  client-side state only); diff blocks colored from `diff_lines`; reasoning
  entries collapsed by default behind "Thought for X s" (`took_ms`), hidden
  entirely when `show_thinking` is false; `notice`/`system`/`stats` styled as
  dim lines.
- Code highlighting: NOT in this slice (post-parity list) — a `<pre>` with the
  language class is the deliverable.
- Acceptance: unit tests in `state.rs`/`render.rs` for pure helpers
  (markdown-sanitize drops `<script>`, diff colors map 1:1 from
  `WireDiffLineKind`); manual browser pass over a session exercising a shell
  tool, an edit (diff), and reasoning.
- **Out of scope:** status bar, panes, todos, pickers.

### Slice 9 — `feat(web): status bar, turn loader, todos`

**Goal:** the chrome reaches parity.

- `hrdr-ui`: header renders `WireStatus` segments (role → CSS class; the ctx
  gauge from `WireGauge` as a real bar), right side the session badge; a footer
  loader while the active pane's `WireTurn.running` (spinner + `tok_per_sec` +
  elapsed, hidden while `!inferring` exactly like the TUI hides it during tool
  calls); a collapsible todo panel from the active `WirePane.todos`; the
  pending-message queue rendered under the transcript from `WirePane.pending`.
- Acceptance: `state.rs` tests: status frame replaces status; todos follow the
  **active** pane. Manual: watch tok/s tick during a streamed reply.
- **Out of scope:** panes UI (next slice).

### Slice 10 — `feat(web): sub-agent panes`

**Goal:** delegated sub-agents are visible, switchable, and drivable.

- `hrdr-ui`: pane tab bar (desktop) that collapses to a drawer under 640px
  width, built from the `Panes` frame (main first — server sends them in
  `pane_rows` order); marker per `WirePaneStatus` (running spinner / ✓ / ·);
  switching sends `SwitchPane` AND locally re-renders from the already-held
  per-pane transcript (all pane transcripts arrive in the snapshot and stay
  updated by `Entries` frames — the client holds them all); input box submits to
  the active pane; per-pane draft preserved on switch (client-side map).
- `hrdr-web`: `switch_pane` calls `panes.focus(core_pane_id(id))` then an
  immediate `tick()` — focus drives the **pin** that keeps the viewed sub-agent
  alive (see `PaneSet::sync` pinning), so it must reach the server even though
  rendering is client-side. Hide the tab bar when only main exists
  (`PaneSet::show_switcher` semantics — client checks `panes.len() == 1`).
- Acceptance: `hrdr-web` test `switching_pins_the_viewed_subagent` (register a
  finished+delivered sub in `LiveSubagents` by **copying the `LiveSubagent`
  literal** from `crates/hrdr-agent/src/pane.rs`'s `live_with` test helper —
  that helper is `#[cfg(test)]`-private to `hrdr-agent`, you cannot call it, but
  `LiveSubagent`'s fields, `SubagentKind`, and `event_log()` are all exported so
  the literal compiles here; then switch to it over the session API, `prune()`,
  tick, assert its pane survives in the next `Panes` frame); client `state.rs`
  test: entries route to the right pane.
- **Out of scope:** steering UX niceties; per-pane status bar variations (status
  already follows the active pane from slice 2).

### Slice 11 — `feat(web): command palette + input polish`

**Goal:** every slash command is reachable comfortably.

- `hrdr-ui`: typing `/` at the start of the empty input opens a filterable
  command palette fed from a `Commands` list — add to the **snapshot** a field
  `commands: Vec<WireCommand { name: String, desc: String }>` (protocol + server
  change in the same slice: server filters `SLASH_COMMANDS` through
  `WebHost::supports_command`); Enter on a palette row inserts the command;
  lines starting with `/` go out as `Command`, everything else as `Submit`;
  `SetInput` frames drive the box (replace/prepend/insert); Escape cancels the
  running turn (sends `Cancel`) after a confirm.
- Mobile: sticky bottom input, `viewport-fit` + keyboard-aware scroll
  (`scrollIntoView` on focus), 44px touch targets on tabs/toggles.
- Acceptance: protocol test updated for the snapshot field; client test: palette
  filters by substring; manual phone-width pass (devtools) of slices 7–11
  screens.
- **Out of scope:** modal pickers (text fallbacks remain), themes.

### Slice 12 — `feat(web): reconnect polish + release wiring`

**Goal:** flaky-network resilience and the release checklist.

- `hrdr-ui`: on WS close — exponential backoff reconnect (1s→30s cap) sending
  `Resume{seq}`; on `Resumed` continue, on fresh `Snapshot` replace state; a
  "reconnecting…" banner; page-visibility handler reconnects immediately on
  foreground (mobile background-resume).
- `hrdr-web`: idle WS ping every 30s (axum WS ping frame), drop after 2 missed
  pongs.
- CI (`.github/workflows/ci.yml`): one new job `web-ui` — install the same
  pinned CLI as the README (`cargo install --locked dioxus-cli@^0.7`), build
  `hrdr-ui` release, record gzipped `.wasm` size in the job summary and fail
  over a 3 MB gzipped budget; runs only on linux.
- README: a `## Web UI` section (serve quickstart, auth modes, the reverse-
  proxy TLS recipe, the webview caveats table from the old plan).
- Acceptance: kill/restart the server mid-stream → client resumes or resnapshots
  without duplicated entries (client test on the reducer:
  `resume_replay_is_idempotent` — applying the same `Entries` frame twice yields
  the same state); CI job green.
- **Out of scope:** nothing new — this closes the parity target.

## 8. Deferred (post-parity — keep this list, delete finished slices above)

- Session-browser UI (list + open other sessions from the client; server gains
  `list_sessions()`-backed message pair).
- Syntax highlighting in code blocks (syntect-wasm or highlight.js interop).
- Modal pickers (model/effort/theme/session) as bottom sheets over the
  `begin_*_selector` hooks.
- v2: attach to a live TUI session (requires making event-log compaction
  min-cursor-aware across readers — see correction 2).
- Native desktop/mobile shell (webview over embedded `hrdr-web`).
- Read-only/observer auth mode.

## 9. Pitfalls for the implementer

1. **Never re-implement fold/classify client-side.** If you find yourself
   parsing tool args or diffing text in `hrdr-ui`, stop — the server already
   sent it in `WireEntryView`.
2. **Entries mutate in place** (streamed text, tool results, reasoning close).
   Delta = hash-diff tail replacement (§5), never append-only.
3. **Do not read the `LiveSubagents` event log from the web layer.**
   `PaneSet::sync` compacts it; a second reader loses events. All fan-out goes
   through `WebSession`'s replay buffer.
4. **Do not fold events into transcripts yourself** on the submit path —
   `send_prompt`/`record` + the tick's `sync` already do; hand-folding shows
   every message twice (the TUI comment at `app.rs:1616` explains).
5. **Do not touch `hrdr-tui`** or add web deps to `hrdr-agent`/`hrdr-app`/
   `hrdr-tools`/`hrdr-llm`. New deps live in `hrdr-web`/`hrdr-ui` only.
6. **`hrdr-ui` must stay out of the workspace** (`exclude`), or host-side
   `cargo clippy --all-targets` breaks on wasm-only deps.
7. rust-analyzer diagnostics in this repo are often stale — trust only
   `cargo build`/`clippy`/`test` output.
8. There is a known ~3s-timeout flake in a background-transcript test
   (`hrdr-agent`). If an unrelated test with a 3s timeout fails once, rerun
   before investigating.
9. Tests that touch `$HOME`/XDG (sessions, config, the users DB default path)
   need the `hrdr-test-support` ctor — it is wired via the
   `#[cfg(test)] extern crate hrdr_test_support;` line; do not remove it, and in
   tests prefer explicit temp paths (`--users-db`) anyway.
10. `Instant`/`SystemTime` (in `TurnStats`) and `DateTime<Local>` never cross
    the wire — only the derived numbers in `WireTurn` and unix seconds do.
11. axum 0.8 route syntax is `/{param}` (not `/:param`), and WS upgrades need
    the `ws` feature — both are already decided in §3; don't downgrade axum to
    match an old example.
12. The steering queue is `Arc<std::sync::Mutex<…>>` (sync mutex, fine to lock
    briefly in async code); the agent is `Arc<tokio::sync::Mutex<Agent>>` and is
    held for a **whole turn** — never `lock().await` it from the tick/WS path
    while a turn may be running; go through `LiveSubagents` accessors, which
    read the registry, not the agent.
