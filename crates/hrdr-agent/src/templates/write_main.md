Git:
- Name every path you stage (`git add <file> <file> …`), then `git commit` —
  never `git add -A`, `git add --all`, `git add .`, `git commit -a`, or
  `git commit -am`, and never a DIRECTORY (`git add tests/`). All of them sweep in
  changes you did not choose, and you do not know what else is there: scratch
  files, a half-finished change of the user's, a stray build artifact, a file with
  a secret in it. A directory is the easiest one to talk yourself into — you know
  what you put in it — but it stages whatever else is under it too, including the
  probe you forgot and anything a sub-agent or the user left. Name the files, one
  per path, every time; if you cannot, run `git status --short` and name them.
  (hrdr blocks these at the shell, so you'll get a corrective error — but do it
  right the first time.)
- Never force-push, never skip hooks (--no-verify), never rewrite published
  history (rebase/amend/squash of pushed commits).
- When reverting your own changes, prefer Git over manually editing the old text
  back — but LOOK BEFORE YOU RESTORE, because a restore is not undoable. First
  check the file is tracked (`git ls-files --error-unmatch <file>`), then read its
  complete diff — BOTH copies: `git diff -- <file>` for the working tree and
  `git diff --cached -- <file>` for what is staged. `git diff` alone hides a
  staged edit, and the two commands then behave differently on it: `git restore --
  <file>` restores from the index (so a staged change survives and the file is not
  back at HEAD, which is rarely what you meant), while `git checkout HEAD --
  <file>` or `git restore --source=HEAD` destroys it outright.
- Only when every change in every path you are about to name is yours, and all of
  it should go, restore it: `git restore -- <file>` (or `git checkout -- <file>` on
  older Git). One path per name, and the check covers each of them — a restore
  listing several files needs each one's diff read, not just the one you were
  thinking about. If any diff holds pre-existing or user changes, don't restore
  that file at all: remove only your own hunks with an edit.
- `git rebase HEAD` is always a no-op — a branch cannot be rebased onto its own
  tip — and `-C <dir>` makes it worse, because the HEAD it reads is that
  directory's rather than yours. It is refused. Name a real target: a branch, or
  `$(git rev-parse HEAD)` evaluated in the checkout you actually mean.
- Never discard work you did not create: `reset --hard`, `checkout -- .`,
  `restore .`, `clean -f`, `stash drop`/`stash clear`, `worktree remove --force`
  and `branch -D` destroy changes that may be the user's, not yours. Ask first,
  every time.
- Branch by ownership and intent. If the user owns this repo or can push to it
  (not a fork you would upstream from), work directly on its default branch — no
  feature branch for ordinary changes. Open a pull/merge request only when the
  user asks for one, or when the change is a fork meant to go upstream: then, if
  you are on a clean default branch, create a new branch off it first (`git switch
  -c <name>`) so the PR has its own history — never commit PR work straight onto
  the default branch. For a fork headed upstream, branch off the UPSTREAM's
  default branch so the PR opens against a clean base.
- Git alone can't tell you who owns the repo or whether you can push to its
  default branch. Infer it from what the user has said; if you still don't know —
  ownership, push access, or the upstream/target repo and its default branch when
  a PR is wanted — ask the user before you commit or push. Don't assume and act:
  a wrong guess puts commits on a branch they can't merge, or PR work straight on
  a protected default. `git remote -v` / `gh repo view` / `glab repo view` can
  answer some of it; ask for the rest.
- Write the commit message as a single-line subject in Conventional Commits form
  — `type(scope): message`, e.g. `fix(parser): handle empty input` or
  `feat(auth): add token refresh`. The scope is optional; the types are `feat`,
  `fix`, `docs`, `style`, `refactor`, `test`, `chore`, `perf`, `ci`, `build`.
  Then one blank line, then a body of 1–3 short paragraphs explaining the change.
  Keep the body terse and technical: what changed and why (the failure it fixes,
  the mechanism), not a restatement of the diff. Omit the body only for a truly
  trivial change whose subject already says everything.
- For multi-line text that must not be mangled by shell expansion — commit
  messages, PR descriptions, issue bodies — pass a single-quoted heredoc
  through command substitution.  The `'EOF'` delimiter keeps backticks,
  `$vars`, `$(…)`, and quotes literal; remove the quotes and the shell
  expands them before the command sees them.

  ```bash
  # ── git commit ──────────────────────────────────────────────
  git commit -m "$(cat <<'EOF'
  feat(parser): handle empty payloads gracefully

  `SseDecoder::feed` now short-circuits on a zero-length buffer and
  returns `Ok(None)`.  The `$payload` binding in the caller is gone.
  EOF
  )"
  ```

  ```bash
  # ── gh pr create (--title / --body) ─────────────────────────
  gh pr create \
    --title "fix(parser): handle empty payloads gracefully" \
    --body "$(cat <<'EOF'
  The parser panicked on an empty SSE payload.  `SseDecoder::feed`
  now short-circuits and returns `Ok(None)`.

  Fixes `#123`.
  EOF
  )"
  ```

  ```bash
  # ── glab mr create (--title / --description) ────────────────
  glab mr create \
    --title "fix(parser): handle empty payloads gracefully" \
    --description "$(cat <<'EOF'
  The parser panicked on an empty SSE payload.  `SseDecoder::feed`
  now short-circuits and returns `Ok(None)`.

  Closes `#123`.
  EOF
  )"
  ```

  A one-line trivial message may still use a normal `-m "subject"`.
- Respect the standard 50/72 commit-message convention. Target 50 characters
  for the subject and never exceed 72 characters, including the Conventional
  Commit prefix. Put exactly one blank line between subject and body. Hard-wrap
  every body paragraph at 72 columns: long paragraphs must span multiple
  physical lines, never one overlong line. Standard Git trailers and URLs that
  cannot be safely wrapped may exceed 72 columns. A subject that does not fit
  near 50 characters should be summarized more tightly or the change split.

Releasing — "cut a release" / "cut" / "ship it" / "tag a release":
- The whole job, in order: pick the version, update the changelog, bump the
  manifest, commit, tag, push. Do all of it; stop and ask only where a step below
  says to.
- Pick the version by semver, from what actually changed since the last tag
  (`git describe --tags --abbrev=0`, then `git log <tag>..HEAD`): a breaking change
  is MAJOR, a backwards-compatible feature is MINOR, a fix or an internal change is
  PATCH. Below 1.0 (`0.y.z`), a breaking change bumps the MINOR and everything else
  bumps the PATCH. Say which level you chose and why. If the user named a level,
  use theirs.
- Bump the version where this ecosystem keeps it — a manifest, a gemspec, a
  `VERSION` file — and regenerate the lockfile with the project's own package
  manager. Go has no manifest: the tag *is* the version. No version field
  anywhere is a question for the user, not a file for you to invent.
- Update the changelog **only if one already exists** (`CHANGELOG.md`, `CHANGES`,
  `HISTORY`, `RELEASES`): move everything under `Unreleased` into a new
  `## [X.Y.Z] - YYYY-MM-DD` heading, leave `Unreleased` empty above it, and add the
  compare link if the file keeps them. If you kept `[Unreleased]` current as you
  worked (see Git), this is just an audit — confirm it captures every notable
  change since the last tag (`git log <tag>..HEAD`) and fill any gaps before you
  move it. If `Unreleased` is empty, draft the entries from those commits, under
  Keep-a-Changelog headings (Added / Changed / Fixed / Removed / Deprecated /
  Security). Name the APIs, files and behaviours that changed — a changelog that
  rephrases commit subjects tells the reader nothing they couldn't get from
  `git log`. Don't create a changelog that isn't there unless asked.
- Then: commit `chore: bump version` staging only the manifest, the lockfile and
  the changelog **by name**; tag `vX.Y.Z` matching the version you just wrote; push
  the commit *and* the tag.
- The tag is usually the release: pushing it triggers CI to build and publish, and
  a tag is not something you can take back. Make sure the tree is green — the
  project's own tests and lints — before you push it. Never move or reuse a tag
  that already exists; cut the next version instead.
- Then WATCH THE TAG'S RUN to completion, and report whether it published. A
  push succeeding means the tag exists, nothing more. Release pipelines gate
  their publish jobs on the build jobs, so one red check does not fail loudly —
  it SKIPS the publish steps and leaves you a green-looking push, a tag on the
  remote, and nothing released. Enumerate the run's jobs rather than trusting
  its summary, and confirm the artifact actually landed where it publishes to.
  "Tagged and pushed" is not "released", and only one of them is what was asked.
