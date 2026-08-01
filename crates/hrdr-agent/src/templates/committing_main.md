- Keep the changelog current as you work, not only at release time. If the
  project already has a `CHANGELOG.md` (or `CHANGES` / `HISTORY` / `RELEASES`),
  then whenever you finish a notable, user-facing change — a feature, a behavior
  or API change, a fix, a removal, a deprecation, a security fix — add an entry
  under the `## [Unreleased]` heading in the matching Keep-a-Changelog section
  (Added / Changed / Fixed / Removed / Deprecated / Security), naming the API,
  file, or behavior that changed rather than restating the commit subject. Do it
  in the SAME commit as the change and stage `CHANGELOG.md` by name. The point is
  that `[Unreleased]` is always complete, so cutting a release is just an audit —
  moving finished entries under a version heading — not the moment the changelog
  gets written. Skip purely internal churn a release note would not mention (a
  refactor with no outward effect, a test-only or docs-only change), and never
  create a changelog that is not already there. When you integrate sub-agents'
  changes, YOU add their entries — they leave the changelog alone by design —
  but batch them: add all of them in one `docs:` commit after every task in the
  batch is merged, not one per merge (see the delegation notes).
- Keep a backlog current the same way, IF the project already has one
  (`docs/backlog.md` or similar — do not create one, per the rule about files the
  task didn't ask for). Anything raised in this session and left unfinished
  belongs in it before you finish: work deferred, a finding you did not fix and
  why, a decision that needs the user, something considered and declined with the
  reason, and what you did NOT review or verify, stated plainly as a gap.
  Otherwise the only record is a conversation nobody reopens, and the next
  session rediscovers it from nothing. Name symbols and files rather than line
  numbers — line numbers rot — and when an entry is finally done, DELETE it
  rather than annotating it as finished; `git log` is the history, and a backlog
  full of closed items is one nobody reads.
