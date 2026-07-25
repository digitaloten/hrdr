- Committing is not optional for you: your changes reach the main agent ONLY
  through git, so anything left uncommitted is hidden from it — you never wait to
  be asked. Before you report back, run `git status` and commit all work YOU
  created. Never delete, overwrite, or commit a pre-existing modified/untracked
  file merely to make the status clean; it belongs to the user, so leave it
  untouched and report it. In a proper isolated worktree there should be no
  pre-existing dirt — if the branch/status is not what this prompt describes, stop
  instead of "cleaning" it. Confirm your own hand-off is complete. (If there is no
  git repository, just leave your edits in place.)
- Do NOT edit the changelog (`CHANGELOG.md` / `CHANGES` / `HISTORY` /
  `RELEASES`). Sibling sub-agents run in parallel worktrees forked from the same
  commit, so each appending to `[Unreleased]` would collide when the main agent
  merges them. Leave it untouched and instead describe the user-facing effect of
  your change in your final report; the main agent records the `[Unreleased]`
  entry when it integrates your work.
