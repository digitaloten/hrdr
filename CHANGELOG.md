# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Initial scaffold: a Cargo workspace for an agentic coding harness driving
  OpenAI-compatible models.
- `hrdr-llm`: provider-agnostic `/v1/chat/completions` client with SSE streaming
  and tool-call reassembly (`Accumulator`).
- `hrdr-tools`: the locked MVP tool set — `read_file`, `write_file`, `edit`,
  `bash`, `grep`, `glob`, `todo_write` — with a registry and token-bounded
  outputs.
- `hrdr-agent`: the tool-calling agent loop with a minijinja system prompt.
- `hrdr-editor`: FSM-agnostic `EditorEngine` seam embedding the hjkl vim engine,
  projected from hjkl's `CoarseMode` so future disciplines plug in without
  churn.
- `hrdr-tui`: ratatui UI with a streaming transcript and a vim-keybound input
  pane.
- `hrdr` binary: interactive TUI by default, `hrdr run <task>` for headless,
  scriptable single-turn runs.
- `AgentConfig::load()`: layered config from `~/.config/hrdr/config.toml` with
  precedence CLI flag > env var > file > built-in default (never auto-written).
- `hrdr models` subcommand + `Client::list_models()` over `GET /models`.
- TUI: in-flight turn cancellation (`Esc` in Normal or `Ctrl+C` while running),
  transcript scrolling (`Ctrl+U`/`Ctrl+D`, `PageUp`/`PageDown`) with bottom
  auto-follow, and a live TODO panel driven by the `todo_write` tool.
- ANSI banner shown in `hrdr --help`.
- Offline unit tests for the tool set and the streaming `Accumulator`.

[Unreleased]: https://github.com/kryptic-sh/hrdr/commits/main
