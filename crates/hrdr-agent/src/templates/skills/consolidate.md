---
name: consolidate
description: fold docs/ work-item files into the backlog and purge them
---

Consolidate the work-item files that review-style skills leave in `docs/`
(`:audit` → `docs/security-review.md`, `:tidy` → `docs/tidy-review.md`, `:plan`
→ `docs/<task>-plan.md`, and any other `*review*.md` / `*-plan*.md` / decision
record) into the single backlog, then delete them — **only once the file is more
than half done**. A plan that is barely started, or any file at or under 50%
complete, stays untouched in its original file: folding a live plan into the
backlog would bury the work that remains, and the standalone file is where
unfinished work is still being tracked. The end state the doc convention calls
for is **one work-item file** (`docs/backlog.md`, or `backlog.md` at the repo
root when there is no `docs/` dir), and `git log` as the history of everything
folded in. `:review` and `:perf` already append into the backlog, so their
output is never a sibling file — if one of those names shows up anyway, dedup,
don't duplicate.

With no arguments: every markdown file next to the backlog except the backlog
itself. With arguments naming files: only those. $ARGUMENTS

1. **List the candidates.** `ls docs/` (or the repo root); take every `*.md`
   except the backlog, narrowed to the files the arguments named when arguments
   were given. An empty list → say so and stop; do not rewrite the backlog.

2. **Score each candidate's completion** — read it in full and count its items:
   findings, work slices, decisions. Mark each **done** (fixed, shipped, cleared
   — the work landed) or **open** (still waiting). A file is consolidated only
   when **strictly more than half** its items are done; a file at exactly half,
   and any plan whose work has barely begun, stays where it is — not folded, not
   purged — and is reported as kept. A file with no open items counts as fully
   done. When a file's items carry no status, check `git log` for what landed
   since the file was written rather than guessing from its wording.

3. **Merge the files that passed the gate**, one at a time. For every claim,
   finding, item and decision in one:
   - **Verify it against the current tree.** Re-open every `file:line` and
     re-check every symbol it cites; a review from last week may describe code
     that has since moved or been deleted. What survives is carried over; what
     does not is corrected in place or dropped, recorded under step 5. Never
     carry a claim over unverified — the backlog entry becomes the only record,
     so it is your assertion.
   - **Check the backlog first.** An item already recorded is not added twice; a
     plan whose binding decisions already sit in Standing constraints, or a
     finding already under its dated section, is simply not re-merged.
   - Classify:
     - **Still open** → fold into the backlog under a dated
       `## <area> YYYY-MM-DD` heading (or into the existing matching section).
       Keep the backlog's conventions: symbol names, not line numbers; dated
       entries; state what is open and why, in the file's own words.
     - **Shipped / closed** → the Record ("closed efforts") section — or
       nothing, when the Record already covers it or the subject no longer
       exists and teaches nothing.
     - **Binding decisions / rules** from a plan or decision record → Standing
       constraints, the section that holds "Decisions from completed work that
       still govern new work".
     - **Disproven / stale** → drop, or correct under step 5.

4. **Purge — the consolidated files only.** Once every item in a file is placed
   (open → backlog, closed → Record or dropped, decisions → constraints), delete
   it by name: `git rm docs/<file>`. A file that failed the step-2 gate is not
   deleted; it keeps exactly what it had. `git log` is the history; the backlog
   is the living record.

5. **Record the merge in the backlog itself.** Add a dated **Docs consolidation
   YYYY-MM-DD** note at the top naming what was folded in and deleted ("read
   `git log` for what they said before this"), and list under **Corrections made
   during the merge** anything you fixed or dropped rather than carried over — a
   backlog that quietly fixes its own errors teaches nothing. Run
   `prettier --write` on the backlog; the markdown rule has no exceptions and
   this is the file a reflow will touch.

6. **Commit.** Stage `docs/backlog.md` and the removed files by name (`git rm`
   already stages the deletions), Conventional Commit
   `docs: consolidate <names> into the backlog`. Report to the user, briefly:
   which files were folded and deleted, where the open items landed (the section
   names), and which files were kept because they are not yet half done — not
   the re-listed items.

7. **Change nothing but the docs.** Consolidation is housekeeping, not a code
   change.
