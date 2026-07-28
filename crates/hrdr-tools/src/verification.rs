//! The verification ledger: what this session actually checked, and whether it
//! still holds.
//!
//! A session ran `cargo test -p hrdr-web -p hrdr-llm`, read the green summary,
//! edited three more files, committed, and reported the work verified. Two of
//! nine crates were covered and the run predated half the edits. Nothing in the
//! harness noticed, because nothing was keeping score.
//!
//! This keeps score. Every source mutation bumps [`source_epoch`]; every shell
//! command that looks like a check is classified and recorded against the epoch
//! it ran at. A kind of check is *satisfied* only by a run that was
//! project-wide, succeeded, and happened at the current epoch — so an edit
//! landing afterwards invalidates it again, which is the whole point.
//!
//! It notes, it never blocks: a WIP commit mid-refactor is legitimate, and a
//! harness that refuses one teaches the model to route around it.
//!
//! [`source_epoch`]: VerificationLedger::bump_source

use std::collections::BTreeMap;

/// How many classified runs are remembered. The ledger is session-scoped and a
/// long session runs hundreds of commands, so this is a ring in spirit: the note
/// only ever names the most recent handful, and the older entries exist for the
/// summary, not for arithmetic.
const MAX_RUNS: usize = 64;

/// A kind of check, in the sense of "what question does this command answer".
///
/// Deliberately coarse — `cargo test` and `pytest` answer the same question, and
/// a session that satisfied it in one ecosystem has satisfied it. Ordering is
/// the declaration order, which is also the order kinds are listed in the note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CheckKind {
    Test,
    Lint,
    Format,
    Build,
    TypeCheck,
}

impl CheckKind {
    /// The name used in the note. Hyphenated where the two-word reading is the
    /// natural one, so "no project-wide type-check run" scans as one thing.
    pub fn as_str(self) -> &'static str {
        match self {
            CheckKind::Test => "test",
            CheckKind::Lint => "lint",
            CheckKind::Format => "format",
            CheckKind::Build => "build",
            CheckKind::TypeCheck => "type-check",
        }
    }
}

/// How much of the project a run covered.
///
/// Only two values on purpose. "Which crates exactly" is unknowable from the
/// command line alone (a `-p` list means nothing without the workspace
/// membership), and a percentage would invite the model to argue about it. The
/// only distinction that changes what should happen next is *whole tree* versus
/// *anything less*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Whole,
    Partial,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Scope::Whole => "whole",
            Scope::Partial => "partial",
        }
    }
}

/// One classified command, kept for the note's "here is what you did run".
#[derive(Debug, Clone)]
struct Run {
    command: String,
    kind: CheckKind,
    scope: Scope,
    ok: bool,
    /// The [`VerificationLedger::source_epoch`] the run happened at — what makes
    /// a later edit able to invalidate it.
    epoch: u64,
}

/// Session-scoped verification state, held in [`ToolContext`](crate::ToolContext).
#[derive(Debug, Default)]
pub struct VerificationLedger {
    /// Bumped by every mutation of a source file. Not a timestamp and not a file
    /// count — a monotonic counter is all the comparison below needs, and it
    /// cannot go backwards when a clock does.
    source_epoch: u64,
    /// Per kind, the `source_epoch` at which a whole-tree run of it last
    /// *succeeded*. Absent means never.
    passed: BTreeMap<CheckKind, u64>,
    /// Every classified run, oldest first, capped at [`MAX_RUNS`].
    runs: Vec<Run>,
}

impl VerificationLedger {
    /// Record that a source file changed. Everything proved before this moment
    /// is now proved about different code.
    pub fn bump_source(&mut self) {
        self.source_epoch = self.source_epoch.saturating_add(1);
    }

    /// Classify `command` and store what it proved. A command that is not a
    /// check — `ls`, `git commit`, `cd` — is silently ignored.
    ///
    /// `ok` is whether the command *succeeded*, which for `shell` includes
    /// "did not time out": a killed test suite has proved nothing, and reading
    /// its partial output as a pass is precisely the failure this file exists
    /// to catch.
    pub fn record(&mut self, command: &str, ok: bool) {
        let Some((kind, scope)) = classify(command) else {
            return;
        };
        if ok && scope == Scope::Whole {
            // Newest wins: an epoch can only go up, but re-running the suite
            // after an edit must move the mark forward.
            self.passed.insert(kind, self.source_epoch);
        }
        self.runs.push(Run {
            command: crate::shorten_command(command),
            kind,
            scope,
            ok,
            epoch: self.source_epoch,
        });
        if self.runs.len() > MAX_RUNS {
            self.runs.remove(0);
        }
    }

    /// The kinds of check that are owed right now: those with no passing
    /// whole-tree run at the current `source_epoch`.
    ///
    /// Empty before the first source edit — a session that only read code owes
    /// nothing — and empty of kinds this session has never touched. The one
    /// unconditional entry is `test`: a source edit always owes a test run, and
    /// the whole motivating story is a commit where none had been made. Chasing
    /// the other three kinds only once the session has shown it uses them keeps
    /// the note about this project instead of about every project.
    pub fn stale_kinds(&self) -> Vec<CheckKind> {
        if self.source_epoch == 0 {
            return Vec::new();
        }
        let mut kinds: Vec<CheckKind> = std::iter::once(CheckKind::Test)
            .chain(self.runs.iter().map(|r| r.kind))
            .filter(|kind| self.passed.get(kind) != Some(&self.source_epoch))
            .collect();
        kinds.sort_unstable();
        kinds.dedup();
        kinds
    }

    /// What was run and at what scope, newest first — the evidence half of the
    /// note. Deduplicated by command (a suite re-run four times is one line) and
    /// capped, because a note the model has to scroll is one it stops reading.
    pub fn summary(&self) -> String {
        const MAX_LISTED: usize = 3;
        let mut seen: Vec<&str> = Vec::new();
        let mut parts: Vec<String> = Vec::new();
        for run in self.runs.iter().rev() {
            if seen.contains(&run.command.as_str()) {
                continue;
            }
            seen.push(&run.command);
            if parts.len() == MAX_LISTED {
                parts.push("…".to_string());
                break;
            }
            parts.push(format!(
                "`{}` ({}, {}, {})",
                run.command,
                run.kind.as_str(),
                run.scope.as_str(),
                if run.ok { "passed" } else { "failed" },
            ));
        }
        parts.join("; ")
    }

    /// The note owed on a commit, or `None` when nothing is.
    ///
    /// A note rather than a refusal: a WIP commit mid-refactor is legitimate,
    /// and a harness that blocks one teaches the model to route around it. It is
    /// specific for the same reason — "remember to run the tests" is advice the
    /// model has already read in the system prompt and already discounted;
    /// "two edits landed after the only run, and that run covered two crates" is
    /// a fact about this commit that it cannot discount.
    pub fn commit_note(&self) -> Option<String> {
        let stale = self.stale_kinds();
        if stale.is_empty() {
            return None;
        }
        let kinds = stale
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join("/");
        // Three genuinely different situations, and saying the wrong one is how
        // a specific note becomes a generic one: nothing ran, things ran and
        // none passed, or something passed and then the code moved on.
        let ran = self.summary();
        let evidence = if ran.is_empty() {
            format!(
                "{} source edit(s) so far, and no check command has run at all this session",
                self.source_epoch
            )
        } else if self.runs.iter().any(|r| r.ok) {
            format!(
                "{} source edit(s) landed after the checks you did run: {ran}",
                self.edits_since_last_pass()
            )
        } else {
            format!(
                "{} source edit(s) so far, and nothing that ran has passed: {ran}",
                self.source_epoch
            )
        };
        Some(format!(
            "[verify] committed, but no project-wide {kinds} run is recorded since your last \
             source edit ({evidence}). A green subset is not a green tree."
        ))
    }

    /// How many source edits landed after the most recent *passing* run, of any
    /// kind or scope. That run is what the model is likely to be thinking of
    /// when it calls the work verified, so it is the one the count is measured
    /// against; with no passing run at all, every edit this session counts.
    fn edits_since_last_pass(&self) -> u64 {
        let last = self.runs.iter().rev().find(|r| r.ok).map_or(0, |r| r.epoch);
        self.source_epoch.saturating_sub(last)
    }
}

/// Whether `command` commits: `git commit` in any of its spellings, including
/// behind a `cd`/`&&` chain and behind `git -C <dir>`.
///
/// Only the subcommand position is inspected, so a commit *message* mentioning
/// the word — `git add x && git commit -m "commit the parser"` — is read once,
/// not twice, and `echo "git commit"` is not read at all.
pub(crate) fn is_git_commit(command: &str) -> bool {
    segments(command).any(|segment| {
        let tokens = arguments(&segment);
        let Some((program, rest)) = tokens.split_first() else {
            return false;
        };
        if basename(program) != "git" {
            return false;
        }
        // `git -C <dir> -c k=v commit …`: both global flags take a value, and
        // skipping them is what keeps a sub-agent's `git -C <worktree> commit`
        // recognisable.
        let mut skip = false;
        for token in rest {
            if skip {
                skip = false;
                continue;
            }
            if matches!(*token, "-C" | "-c" | "--git-dir" | "--work-tree") {
                skip = true;
                continue;
            }
            if token.starts_with('-') {
                continue;
            }
            return *token == "commit";
        }
        false
    })
}

/// What kind of check `command` is, and how much of the project it covered.
///
/// A compound command is classified segment by segment and answers with the
/// *strongest* claim any segment makes — whole beats partial, and among equals
/// the kind that proves the most. `cargo fmt --all && cargo test --workspace` is
/// a whole test run; that the formatter also passed is true but not what the
/// commit is owed. Losing the weaker half only ever costs a second ask, which is
/// the direction this file is allowed to be wrong in.
///
/// One exception overrides everything: a command containing `||` exits 0 on the
/// *fallback*, so its exit code says nothing about the check. Those are demoted
/// to partial rather than trusted.
fn classify(command: &str) -> Option<(CheckKind, Scope)> {
    let strongest = segments(command)
        .filter_map(|segment| classify_segment(&segment))
        .max_by_key(|(kind, scope)| (*scope == Scope::Whole, kind_rank(*kind)))?;
    if command.contains("||") {
        return Some((strongest.0, Scope::Partial));
    }
    Some(strongest)
}

/// How much a kind proves, for picking between segments of one command. Not
/// [`Ord`] on the enum itself: the enum's own order is the *reading* order used
/// in the note, and conflating the two would make one of them wrong the first
/// time either list is reordered.
fn kind_rank(kind: CheckKind) -> u8 {
    match kind {
        CheckKind::Test => 4,
        CheckKind::Build => 3,
        CheckKind::TypeCheck => 2,
        CheckKind::Lint => 1,
        CheckKind::Format => 0,
    }
}

/// The command split into the pieces the shell would run separately: `&&`, `||`,
/// `;`, `|` and newlines. Splitting on the bare characters (rather than the
/// operators) also breaks `2>&1` and `|&`, which is harmless — the fragments
/// classify to nothing.
fn segments(command: &str) -> impl Iterator<Item = String> + '_ {
    command
        .split(['&', '|', ';', '\n'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A segment's tokens with the noise that does not change what it runs removed:
/// leading environment assignments, wrapper programs, and everything from the
/// first redirection on (`> out.log`, `2>` left over from `2>&1`).
fn arguments(segment: &str) -> Vec<&str> {
    /// Programs that run another program without changing what it is.
    const WRAPPERS: &[&str] = &["sudo", "env", "time", "nice", "ionice", "npx", "rtk"];

    let mut tokens: Vec<&str> = segment
        .split_whitespace()
        .take_while(|t| !is_redirection(t))
        .collect();
    while let Some(first) = tokens.first() {
        if is_env_assignment(first) || WRAPPERS.contains(&basename(first)) {
            tokens.remove(0);
        } else {
            break;
        }
    }
    tokens
}

/// `>`, `>>`, `<`, `2>`, `&>` — a token that redirects rather than argues.
fn is_redirection(token: &str) -> bool {
    let rest = token.trim_start_matches(|c: char| c.is_ascii_digit());
    rest.starts_with('>') || rest.starts_with('<')
}

/// `RUST_LOG=debug cargo test` — a `NAME=value` prefix, not the program.
fn is_env_assignment(token: &str) -> bool {
    token.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    })
}

/// The program name without its path, so `./node_modules/.bin/eslint` and
/// `/usr/bin/cargo` classify like the bare names do.
fn basename(token: &str) -> &str {
    token.rsplit(['/', '\\']).next().unwrap_or(token)
}

/// One shell command (no operators left in it), dispatched on the program that
/// runs it. An unrecognised program is `None` rather than a guess: a check we
/// cannot read is one the session gets no credit for, which costs an extra ask
/// — where a guess would cost a false all-clear.
fn classify_segment(segment: &str) -> Option<(CheckKind, Scope)> {
    let tokens = arguments(segment);
    let (program, rest) = tokens.split_first()?;
    match basename(program) {
        "cargo" => classify_cargo(rest),
        "npm" | "pnpm" | "yarn" | "bun" => classify_node_script(rest),
        "pytest" | "py.test" => Some((CheckKind::Test, pytest_scope(rest))),
        "python" | "python3" => {
            // `python -m pytest …` is the same run under another spelling.
            match rest.split_first() {
                Some((&"-m", tail)) if tail.first() == Some(&"pytest") => {
                    Some((CheckKind::Test, pytest_scope(&tail[1..])))
                }
                _ => None,
            }
        }
        "go" => classify_go(rest),
        "ruff" => classify_ruff(rest),
        "eslint" => Some((CheckKind::Lint, path_scope(rest))),
        "prettier" => Some((CheckKind::Format, path_scope(rest))),
        "tsc" => Some((CheckKind::TypeCheck, tsc_scope(rest))),
        _ => None,
    }
}

/// Flags whose *next* token is a value, not an argument of its own. Shared
/// across ecosystems because a false "this is a positional filter" reading is
/// the common way a genuinely whole run gets demoted — `cargo clippy --workspace
/// -- -D warnings` would otherwise read `warnings` as a test-name filter.
const VALUE_FLAGS: &[&str] = &[
    "--features",
    "-F",
    "--target",
    "--target-dir",
    "--manifest-path",
    "--profile",
    "--config",
    "-j",
    "--jobs",
    "--message-format",
    "--color",
    "-Z",
    "-D",
    "-A",
    "-W",
    "--cap-lints",
    "--test-threads",
    "--retries",
    "-n",
    "-o",
    "-p",
    "--rootdir",
    "--tb",
    "--junitxml",
    "--ext",
    "--ignore-path",
    "--parser",
    "--plugin",
    "--format",
    "-f",
    "--project",
    "--outDir",
    "--select",
    "--ignore",
    "--extend-select",
    "--line-length",
    "--target-version",
    "--per-file-ignores",
    "--run",
    "--bench",
    "--filter",
    "--scope",
    "--projects",
    "-w",
];

/// Whether `flag` takes a following value, given it was written without `=`.
fn takes_value(flag: &str) -> bool {
    VALUE_FLAGS.contains(&flag)
}

/// Walk `args`, calling `on_flag` for each flag (its name, `--x=y` stripped of
/// the value) and `on_positional` for each bare argument, skipping the values of
/// value-taking flags and the `--` separator.
///
/// Both callbacks return "stop": the scope answers are all early-outs, and a
/// single walker means every ecosystem below skips values the same way.
fn walk_args(
    args: &[&str],
    mut on_flag: impl FnMut(&str) -> bool,
    mut on_positional: impl FnMut(&str) -> bool,
) -> bool {
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if *arg == "--" {
            continue;
        }
        if arg.starts_with('-') {
            let (name, inline) = match arg.split_once('=') {
                Some((name, _)) => (name, true),
                None => (*arg, false),
            };
            if on_flag(name) {
                return true;
            }
            if !inline && takes_value(name) {
                skip = true;
            }
            continue;
        }
        if on_positional(arg) {
            return true;
        }
    }
    false
}

/// `cargo <subcommand>`: the kind comes from the subcommand, the scope from
/// whether the invocation names the whole workspace and narrows nothing.
///
/// `check` is filed as a build: in Rust it answers exactly the question `build`
/// does — does the tree still compile — and a session that ran `cargo check
/// --workspace` has genuinely answered it. `TypeCheck` is reserved for
/// ecosystems where type checking is a separate tool from the build (`tsc`).
fn classify_cargo(args: &[&str]) -> Option<(CheckKind, Scope)> {
    // Skip `+nightly` and any pre-subcommand flag.
    let start = args
        .iter()
        .position(|t| !t.starts_with('+') && !t.starts_with('-'))?;
    let (kind, rest) = match args[start] {
        "test" | "t" => (CheckKind::Test, &args[start + 1..]),
        // `cargo nextest run` — the subcommand is two words.
        "nextest" if args.get(start + 1) == Some(&"run") => (CheckKind::Test, &args[start + 2..]),
        "clippy" => (CheckKind::Lint, &args[start + 1..]),
        "fmt" | "format" => (CheckKind::Format, &args[start + 1..]),
        "build" | "b" | "check" => (CheckKind::Build, &args[start + 1..]),
        _ => return None,
    };
    Some((kind, cargo_scope(rest)))
}

/// Whole only when the command says so and takes nothing back.
///
/// Bare `cargo test` is *partial* even though it covers everything in a
/// single-crate project: from the command line alone there is no telling a
/// single-crate project from a nine-crate workspace where it covers one, and the
/// wrong guess in that direction is the exact reassurance this file exists to
/// withhold.
fn cargo_scope(args: &[&str]) -> Scope {
    /// Flags that narrow the run: a package selection, a target selection, or a
    /// test-name filter. `--all-targets`/`--all-features` are deliberately not
    /// here — they *widen* — which is why every comparison is on the whole flag.
    const NARROWING: &[&str] = &[
        "-p",
        "--package",
        "--exclude",
        "--lib",
        "--bin",
        "--bins",
        "--test",
        "--tests",
        "--example",
        "--examples",
        "--bench",
        "--benches",
        "--doc",
        "-E",
        "--filter-expr",
        "--partition",
    ];

    let mut whole = false;
    let narrowed = walk_args(
        args,
        |flag| {
            if flag == "--workspace" || flag == "--all" {
                whole = true;
            }
            NARROWING.contains(&flag)
        },
        // A bare argument to a cargo check subcommand is a test-name filter.
        |_| true,
    );
    if narrowed || !whole {
        Scope::Partial
    } else {
        Scope::Whole
    }
}

/// `npm test`, `npm run lint`, `yarn tsc`, `pnpm run build` — the script name
/// carries the kind. Scope is whole unless the invocation selects a package or
/// passes the script an argument (`npm test -- foo` is a filter).
fn classify_node_script(args: &[&str]) -> Option<(CheckKind, Scope)> {
    /// Flags that pick which package(s) of a monorepo run.
    const SELECTING: &[&str] = &["--filter", "--scope", "--workspace", "-w", "--projects"];

    let mut names = args
        .iter()
        .enumerate()
        .filter(|(i, t)| !t.starts_with('-') && !prior_flag_takes_value(args, *i));
    let (index, script) = names.next()?;
    // `run`/`run-script`/`exec` is a preamble; the script is the next word.
    let (index, script) = if matches!(*script, "run" | "run-script" | "exec") {
        names.next()?
    } else {
        (index, script)
    };
    let kind = match *script {
        "test" | "t" | "jest" | "vitest" => CheckKind::Test,
        "lint" => CheckKind::Lint,
        "build" => CheckKind::Build,
        "tsc" | "typecheck" | "type-check" | "types" => CheckKind::TypeCheck,
        "fmt" | "format" => CheckKind::Format,
        _ => return None,
    };
    // Package selection is looked for across the *whole* invocation, because it
    // is written before the script (`pnpm --filter web test`); positionals are
    // looked for only after it, because that is where the script's own arguments
    // — for a test runner, a filter — go.
    let mut selected = false;
    walk_args(
        args,
        |flag| {
            selected |= SELECTING.contains(&flag);
            false
        },
        |_| false,
    );
    let narrowed = walk_args(&args[index + 1..], |_| false, |_| true);
    if narrowed || selected {
        Some((kind, Scope::Partial))
    } else {
        Some((kind, Scope::Whole))
    }
}

/// Whether `args[index]` is the value of the flag before it, so a package name
/// in `pnpm --filter web test` is not mistaken for the script.
fn prior_flag_takes_value(args: &[&str], index: usize) -> bool {
    index > 0 && !args[index - 1].contains('=') && takes_value(args[index - 1])
}

/// Bare `pytest` collects from the rootdir, so it is whole; a path or a `-k`/`-m`
/// expression makes it a subset.
fn pytest_scope(args: &[&str]) -> Scope {
    const NARROWING: &[&str] = &["-k", "-m", "--deselect", "--lf", "--last-failed", "--ff"];
    let narrowed = walk_args(args, |flag| NARROWING.contains(&flag), |_| true);
    if narrowed {
        Scope::Partial
    } else {
        Scope::Whole
    }
}

/// `go test ./...` — the `./...` pattern is what makes a go command whole;
/// anything narrower (`./pkg/...`, a `-run` filter) is not.
fn classify_go(args: &[&str]) -> Option<(CheckKind, Scope)> {
    let (sub, rest) = args.split_first()?;
    let kind = match *sub {
        "test" => CheckKind::Test,
        "vet" => CheckKind::Lint,
        "build" => CheckKind::Build,
        "fmt" => CheckKind::Format,
        _ => return None,
    };
    let mut all = false;
    let narrowed = walk_args(
        rest,
        |flag| matches!(flag, "-run" | "-bench" | "-fuzz" | "-tags"),
        |arg| {
            if arg == "./..." {
                all = true;
                false
            } else {
                true
            }
        },
    );
    let scope = if narrowed || !all {
        Scope::Partial
    } else {
        Scope::Whole
    };
    Some((kind, scope))
}

/// `ruff check` / `ruff format`, whose default path is the whole tree.
fn classify_ruff(args: &[&str]) -> Option<(CheckKind, Scope)> {
    let (sub, rest) = args.split_first()?;
    let kind = match *sub {
        "check" => CheckKind::Lint,
        "format" => CheckKind::Format,
        _ => return None,
    };
    Some((kind, path_scope(rest)))
}

/// Whole when the tool was pointed at everything — no path at all (its default)
/// or an explicit `.` — and partial the moment a specific path is named.
fn path_scope(args: &[&str]) -> Scope {
    let narrowed = walk_args(
        args,
        |flag| matches!(flag, "--exclude" | "--ignore-pattern"),
        |arg| !matches!(arg, "." | "./" | "./..." | "**/*"),
    );
    if narrowed {
        Scope::Partial
    } else {
        Scope::Whole
    }
}

/// `tsc` with no file arguments type-checks the whole project described by the
/// tsconfig — including `tsc -p packages/web`, which is whole *for that
/// project*, and which we still call partial: in a monorepo it is one package of
/// several, and there is no telling from here.
fn tsc_scope(args: &[&str]) -> Scope {
    let mut projected = false;
    let narrowed = walk_args(
        args,
        |flag| {
            projected |= matches!(flag, "-p" | "--project" | "-b" | "--build");
            false
        },
        |_| true,
    );
    if narrowed || projected {
        Scope::Partial
    } else {
        Scope::Whole
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_wide_cargo_run_is_whole() {
        assert_eq!(
            classify("cargo test --workspace"),
            Some((CheckKind::Test, Scope::Whole))
        );
        assert_eq!(
            classify("cargo clippy --workspace --all-targets -- -D warnings"),
            Some((CheckKind::Lint, Scope::Whole))
        );
        assert_eq!(
            classify("cargo fmt --all -- --check"),
            Some((CheckKind::Format, Scope::Whole))
        );
        assert_eq!(
            classify("cargo nextest run --workspace"),
            Some((CheckKind::Test, Scope::Whole))
        );
    }

    #[test]
    fn anything_narrower_than_the_whole_tree_is_partial() {
        // The exact command from the session that motivated this file.
        for command in [
            "cargo test -p hrdr-web -p hrdr-llm",
            "cargo test --workspace --lib",
            "cargo test --workspace some_test_name",
            "cargo nextest run --workspace -E 'test(verification)'",
            "cargo test",
            "cargo clippy --all-targets -- -D warnings",
            "pytest tests/test_api.py",
            "pytest -k auth",
            "go test ./internal/...",
            "npm test -- --watch=false pattern",
            "eslint src/app.ts",
            "tsc src/one.ts",
        ] {
            let (_, scope) = classify(command).unwrap_or_else(|| panic!("{command} unclassified"));
            assert_eq!(scope, Scope::Partial, "{command} must not read as whole");
        }
    }

    #[test]
    fn each_ecosystems_whole_suite_shape_is_recognised() {
        let cases: &[(&str, CheckKind)] = &[
            ("pytest", CheckKind::Test),
            ("python -m pytest", CheckKind::Test),
            ("go test ./...", CheckKind::Test),
            ("go vet ./...", CheckKind::Lint),
            ("go build ./...", CheckKind::Build),
            ("npm test", CheckKind::Test),
            ("npm run lint", CheckKind::Lint),
            ("pnpm run build", CheckKind::Build),
            ("yarn tsc", CheckKind::TypeCheck),
            ("npx tsc --noEmit", CheckKind::TypeCheck),
            ("ruff check .", CheckKind::Lint),
            ("ruff format .", CheckKind::Format),
            ("eslint .", CheckKind::Lint),
            ("prettier --check .", CheckKind::Format),
            ("cargo check --workspace", CheckKind::Build),
        ];
        for (command, kind) in cases {
            assert_eq!(
                classify(command),
                Some((*kind, Scope::Whole)),
                "{command} should be a whole {} run",
                kind.as_str()
            );
        }
    }

    #[test]
    fn a_command_that_checks_nothing_classifies_to_nothing() {
        for command in [
            "ls -la",
            "git commit -m 'wip'",
            "cargo run --workspace",
            "echo cargo test --workspace",
        ] {
            assert_eq!(classify(command), None, "{command} is not a check");
        }
    }

    #[test]
    fn a_chained_command_records_the_strongest_segment() {
        // `&&` means both ran; the whole-tree test is the stronger claim.
        assert_eq!(
            classify("cargo fmt --all && cargo test --workspace"),
            Some((CheckKind::Test, Scope::Whole))
        );
        // A pipeline's filter stage is not a check.
        assert_eq!(
            classify("cargo test --workspace 2>&1 | grep -E 'FAIL|Summary'"),
            Some((CheckKind::Test, Scope::Whole))
        );
    }

    #[test]
    fn an_or_fallback_is_never_a_whole_pass() {
        // `cargo test --workspace || true` exits 0 having proved nothing. The
        // ledger must not read that exit code as a green tree.
        assert_eq!(
            classify("cargo test --workspace || true"),
            Some((CheckKind::Test, Scope::Partial))
        );
    }

    #[test]
    fn nothing_is_owed_until_a_source_file_changes() {
        let mut led = VerificationLedger::default();
        assert!(led.stale_kinds().is_empty());
        assert_eq!(led.commit_note(), None);
        // Running checks without editing anything still owes nothing.
        led.record("cargo test -p one", true);
        assert!(led.stale_kinds().is_empty());
        assert_eq!(led.commit_note(), None);
    }

    /// The ordering that is the entire point: green whole suite, then an edit,
    /// then the green is about the old code.
    #[test]
    fn a_whole_pass_is_invalidated_by_the_next_source_edit() {
        let mut led = VerificationLedger::default();
        led.bump_source();
        assert_eq!(led.stale_kinds(), vec![CheckKind::Test]);

        led.record("cargo test --workspace", true);
        assert!(
            led.stale_kinds().is_empty(),
            "a passing whole-tree run at the current epoch settles the debt"
        );

        led.bump_source();
        assert_eq!(
            led.stale_kinds(),
            vec![CheckKind::Test],
            "an edit after the run makes the run about older code"
        );
    }

    #[test]
    fn a_partial_pass_and_a_failed_whole_run_both_settle_nothing() {
        let mut led = VerificationLedger::default();
        led.bump_source();
        led.record("cargo test -p hrdr-web -p hrdr-llm", true);
        assert_eq!(led.stale_kinds(), vec![CheckKind::Test], "a green subset");
        led.record("cargo test --workspace", false);
        assert_eq!(led.stale_kinds(), vec![CheckKind::Test], "a red tree");
        led.record("cargo test --workspace", true);
        assert!(led.stale_kinds().is_empty());
    }

    /// Only kinds the session has shown it cares about are chased — plus `test`,
    /// which a source edit always owes. Nagging a Rust session about `tsc` would
    /// be noise, and noise is what teaches the model to skip the note.
    #[test]
    fn only_the_kinds_this_session_uses_are_chased() {
        let mut led = VerificationLedger::default();
        led.bump_source();
        led.record("cargo test --workspace", true);
        assert!(
            led.stale_kinds().is_empty(),
            "format/build/type-check were never run, so they are not owed"
        );
        led.record("cargo clippy -p hrdr-tools", true);
        assert_eq!(
            led.stale_kinds(),
            vec![CheckKind::Lint],
            "once lint is in play, a partial lint leaves it owed"
        );
    }

    #[test]
    fn the_note_names_the_kinds_the_run_and_the_edits_that_followed() {
        let mut led = VerificationLedger::default();
        led.bump_source();
        led.record("cargo test -p hrdr-web -p hrdr-llm", true);
        led.bump_source();
        led.bump_source();
        let note = led.commit_note().expect("a note is owed");
        assert!(note.starts_with("[verify] committed, but"), "{note}");
        assert!(note.contains("test"), "the owed kind is named: {note}");
        assert!(
            note.contains("cargo test -p hrdr-web -p hrdr-llm"),
            "the run is named verbatim: {note}"
        );
        assert!(note.contains("partial"), "its scope is named: {note}");
        assert!(note.contains('2'), "the edits after it are counted: {note}");
        assert!(
            note.contains("A green subset is not a green tree."),
            "{note}"
        );
    }

    #[test]
    fn the_note_says_so_when_nothing_at_all_was_run() {
        let mut led = VerificationLedger::default();
        led.bump_source();
        let note = led.commit_note().expect("a note is owed");
        assert!(
            note.contains("no check command"),
            "an empty ledger must say it is empty, not name a run: {note}"
        );
    }

    /// A session whose only whole-tree run went red is not told edits "landed
    /// after" it — nothing was established for them to land after.
    #[test]
    fn the_note_distinguishes_a_red_tree_from_a_stale_green_one() {
        let mut led = VerificationLedger::default();
        led.bump_source();
        led.record("cargo test --workspace", false);
        let note = led.commit_note().expect("a note is owed");
        assert!(note.contains("nothing that ran has passed"), "{note}");
        assert!(note.contains("cargo test --workspace"), "{note}");
    }

    #[test]
    fn the_run_log_is_capped() {
        let mut led = VerificationLedger::default();
        for n in 0..(MAX_RUNS * 3) {
            led.record(&format!("cargo test -p crate{n}"), true);
        }
        assert_eq!(
            led.runs.len(),
            MAX_RUNS,
            "a long session must not grow this"
        );
        assert!(
            led.runs
                .last()
                .unwrap()
                .command
                .contains(&format!("crate{}", MAX_RUNS * 3 - 1)),
            "newest wins"
        );
    }

    #[test]
    fn the_summary_names_commands_with_their_scope_newest_first() {
        let mut led = VerificationLedger::default();
        led.record("cargo fmt --all", true);
        led.record("cargo test -p hrdr-web", false);
        let summary = led.summary();
        assert!(summary.contains("cargo test -p hrdr-web"), "{summary}");
        assert!(summary.contains("partial"), "{summary}");
        assert!(summary.contains("failed"), "{summary}");
        assert!(
            summary.find("cargo test").unwrap() < summary.find("cargo fmt").unwrap(),
            "newest first: {summary}"
        );
    }

    #[test]
    fn a_commit_is_recognised_through_the_shapes_a_session_actually_types() {
        for command in [
            "git commit -m 'feat: thing'",
            "git add a.rs && git commit -m x",
            "git -C /tmp/wt commit --amend --no-edit",
            "cd sub && git commit -q -m x",
        ] {
            assert!(is_git_commit(command), "{command} commits");
        }
        for command in [
            "git status",
            "git log --oneline -5",
            "echo 'git commit' >> notes.md",
            "cargo test --workspace",
        ] {
            assert!(!is_git_commit(command), "{command} does not commit");
        }
    }
}
