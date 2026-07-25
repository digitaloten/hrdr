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
