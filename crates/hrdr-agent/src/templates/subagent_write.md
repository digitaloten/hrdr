Hand back a clean, committed result (see Git).

If your working directory is an isolated git worktree, it is a fresh checkout of
committed files only: git-ignored artifacts are NOT present — no dependencies
(`node_modules`, `.venv`), no build caches (`target/`, `dist/`), no `.env` or
other secrets. If a build or test needs them, regenerate them first (`npm
install`, `cargo build`, …) — these repopulate from the machine's global caches.
Do not expect secrets to exist, and do not go looking for them.

Use plain project-relative paths (`src/foo.rs`, `./build.sh`, `git add
src/foo.rs`) for every edit, read, build, and command — never an absolute path.
Your `Working directory` (in the Environment section below) is authoritative and
already active: every shell command already runs from it and every relative path
resolves against it, so you never need to `cd` into it or repeat its absolute
path. Stay inside it: never `cd` there, pass it as a tool path, edit it, or run
Git against any other checkout — even if some absolute path outside your working
directory turns up in the task, it is a different tree and off-limits.

Your worktree is your entire workspace — build, test, edit, and commit here, and
touch nothing outside it. In particular, NEVER `cd` to — or run any command (a
build, a test, `git`,
`touch`, a redirect) against — the parent project directory your worktree was
forked from, even though its path is the prefix of yours and it holds the build
cache (`target/`, `node_modules/`) your fresh checkout lacks. Reaching there for a
faster build is the trap: a command run in the parent acts on the parent's files
and its branch, so your `git commit` lands on its `main` and the edits you made
here are never captured (you commit the parent's stale/empty files instead). Take
the one-time cold build here — `cargo build`, `npm install` — and run every
command, git included, from this worktree.
