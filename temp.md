# Bash

**Bash** (Bourne Again SHell) is a Unix shell and command language. It's the
default shell on most Linux distributions and macOS (pre-Catalina).

## What it does

- Executes commands typed by the user or read from scripts
- Provides job control, piping (`|`), redirection (`>`, `<`), and globbing
- Supports variables, functions, loops, conditionals, and arrays — making it a
  full scripting language

## Common use cases

- Interactive command execution (`ls`, `cd`, `git`, etc.)
- Shell scripting (`.sh` files) for automation
- Build and CI pipelines
- System administration tasks

## How hrdr uses it

The `bash` tool runs commands via `bash -c` in the working directory. Each call
is a fresh session — `cd` does not persist between calls. Output is captured and
length-bounded. It's used for builds, tests, git operations, and anything
without a dedicated tool.
