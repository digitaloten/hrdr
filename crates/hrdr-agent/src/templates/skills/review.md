---
name: review
description: review the pending diff for correctness bugs
args: [low, high]
---

Review the pending changes for bugs. Depth: $ARGUMENTS (default `low` — report
only high-confidence findings; `high` — broader coverage, may include uncertain
findings clearly marked as such).

1. Determine the scope:
   - If the working tree has **pending changes** (staged, unstaged, or
     untracked), review only those. On a feature branch, also diff against the
     merge-base with the default branch.
   - If `git status` is **clean** (nothing pending), or you are not in a git
     repo, review the **entire codebase**.
2. Hunt for correctness problems only: logic errors, broken edge cases (empty,
   zero, unicode, concurrent), error paths that swallow or corrupt state,
   resource leaks, API misuse, behavior changes callers don't expect. Skip
   style, naming, and formatting — that's not this review.
3. Verify every candidate finding by EXECUTING it in your head against the real
   code, not by describing it. A failure scenario that was never traced is a
   story, and it will read exactly like a real one:
   - Take your concrete triggering input and follow it line by line from where
     it enters to the line you claim breaks. Name what every guard, parse, split
     and branch in between does to it. Most false findings die here, at a step
     the narrative skipped.
   - Where the trigger is a string, actually apply the string operations. If the
     code does `strip_prefix` then `split('-').next()` then `parse()`, work out
     what your input yields at each step — the answer is often "the parse fails
     and the dangerous line is never reached".
   - Re-read every line you are about to cite, and quote from that read. Do not
     cite a symbol you found in one file as if it were in another; if you are
     comparing two pieces of code, open both and give each its own `file:line`.
   - If a guard, type, or caller makes the failure unreachable today, it is not
     a finding. It may be a hardening note (step 5) — say so honestly instead of
     promoting it.
   - Prefer cutting to hedging. A dropped true finding costs one bug; a
     confident false one costs the user's trust in the whole report.
4. Write the findings ranked most-severe first, each with `file:line`, a
   one-sentence statement of the defect, and the traced failure scenario. If
   nothing survives verification, say so plainly — a short honest report is the
   good outcome, and padding it with what you already disproved is not.
5. Add three short sections after the findings:
   - **Cleared** — the things you suspected and disproved, one line each with
     the reason they are safe. This is worth as much as the findings: it is the
     expensive half of the work, it stops the next reviewer re-treading it, and
     it shows which stones you turned over.
   - **Hardening** (only if you have any) — things that are correct today but
     fragile: an invariant held by convention rather than by a type, a guard
     that exists in one place and not its sibling. Explicitly not defects, so
     the user can triage them separately.
   - **Coverage** — what you actually examined and what you did not, in a few
     lines. Name the areas you skipped and why (out of scope, no time, needs
     runtime behaviour you can't observe). "Reviewed everything" is almost never
     true; saying where you stopped is what lets the user judge the report.
6. Route the findings by where you're working:
   - **Inside a git repo with a `docs/` directory** → write the full report to
     `docs/code-review.md`.
   - **Inside a git repo with no `docs/` directory** → write it to
     `code-review.md` at the repo root.
   - **Not inside a git repo** (working on something git doesn't track) → do NOT
     write to disk.

   When you write the report to disk, tell the user only a high-level summary
   (counts and the top issues) plus the path you wrote — not the full list. When
   you do NOT write to disk, give the user the full findings in your reply.

7. Report only — don't change any code unless asked to fix the findings.
