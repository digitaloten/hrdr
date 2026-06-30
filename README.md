# hrdr

**Herder** — a fast, agentic coding harness for OpenAI-compatible models.

hrdr drives a model through native tool calls to complete software-engineering
tasks in a terminal. It is provider-agnostic: point it at any
`/v1/chat/completions` endpoint — [`infr`](https://github.com/kryptic-sh/infr),
OpenAI, llama.cpp, OpenRouter — and it streams tokens and runs tools until the
job is done.

> Early WIP. The agent loop, tool set, OpenAI client, and a vim-keybound TUI are
> in place; see the roadmap below.

## Design

- **Provider-agnostic client.** Speaks clean OpenAI chat-completions with native
  `tools`/`tool_calls` and SSE streaming. The server owns chat-template
  application; hrdr only ever sends structured `messages[]` + `tools[]`.
- **Efficient, locked tool set.** Fewer, more powerful tools beat a big menu:
  `read_file`, `write_file`, `edit`, `bash`, `grep`, `glob`, `todo_write`.
  Token-bounded outputs, line-numbered reads for precise edits, ripgrep search.
- **Vim editing via [hjkl](https://github.com/kryptic-sh/hjkl).** The input pane
  is a real hjkl editor. The integration is **FSM-agnostic**: hrdr talks only to
  an `EditorEngine` trait projected from hjkl's `CoarseMode`, so when hjkl's
  pluggable-FSM work lands a VSCode/Helix discipline, hrdr swaps it in with zero
  churn.
- **Jinja prompt templating.** hrdr's own system prompt is assembled with
  minijinja templates — editable without a recompile.

## Workspace

| Crate         | Role                                                              |
| ------------- | ---------------------------------------------------------------- |
| `hrdr-llm`    | OpenAI-compatible client: types, streaming, tool-call assembly.  |
| `hrdr-tools`  | The seven MVP tools + registry.                                  |
| `hrdr-agent`  | The agent loop + minijinja system prompt.                        |
| `hrdr-editor` | FSM-agnostic hjkl embedding (`EditorEngine` seam).               |
| `hrdr-tui`    | Ratatui UI: transcript + vim input pane, live streaming.         |
| `hrdr`        | Binary: TUI by default, `hrdr run <task>` for headless.          |

## Usage

```bash
# interactive TUI (Insert to type, Esc for Normal, Enter to send, Ctrl+C quit)
hrdr

# one-shot headless run, streamed to stdout
hrdr run "add a --json flag to the status command"
```

Configuration (CLI flags override env):

| Env             | Default                       | Meaning                       |
| --------------- | ----------------------------- | ----------------------------- |
| `HRDR_BASE_URL` | `http://localhost:8080/v1`    | OpenAI-compatible endpoint.   |
| `HRDR_MODEL`    | `default`                     | Model id.                     |
| `HRDR_API_KEY`  | _(falls back to `OPENAI_API_KEY`)_ | Bearer token, if required. |

## Status / roadmap

- [x] OpenAI client (streaming + tool calls)
- [x] Tool set (read/write/edit/bash/grep/glob/todo)
- [x] Agent loop with tool execution
- [x] hjkl vim input pane (FSM-agnostic seam)
- [x] Interactive TUI + headless `run`
- [ ] In-flight turn cancellation
- [ ] TODO panel + wrap-aware transcript scrolling
- [ ] Config file (`~/.config/hrdr/config.toml`), `hrdr models`
- [ ] Broader tool + client unit tests

## License

MIT
