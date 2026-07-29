//! Redaction of secret file contents out of a git diff.
//!
//! `git diff`/`show`/`log -p` will happily print the body of a `.env`, an
//! `id_rsa` or an `~/.aws/credentials` that a commit touched. The read tools
//! refuse those files outright ([`crate::secret_file_reason`]); a diff is the
//! back door, and the only one left now that the bespoke read-only `git` tool
//! is gone and git runs through the shell.
//!
//! So this would apply where hrdr composes a diff ITSELF and hands it to a
//! model. Nothing does today — see the note on the function. It is not a
//! guarantee about arbitrary shell output: a model that runs `git diff` in the
//! shell gets what git prints, exactly as `cat` would. The shell has never been
//! a redaction boundary, and pretending otherwise would be worse than the
//! honest limit.

/// The file path a diff-section header names, if `line` starts one:
/// `diff --git a/<p> b/<p>` (prefer the `b/` destination), or a merge diff's
/// `diff --cc <p>` / `diff --combined <p>`. `None` for any other line.
///
/// Under the default `core.quotePath`, git C-style-quotes a path that has a
/// space, a double quote, a backslash, or a non-ASCII byte —
/// `diff --git "a/my dir/.env" "b/my dir/.env"` — so this can't just scan for
/// literal `" b/"`; it tokenizes the two (possibly quoted) paths and unquotes
/// whichever one is quoted. [`forbidden_flag`] separately refuses
/// `--no-prefix`/`--src-prefix`/`--dst-prefix`, which would otherwise strip the
/// `a/`/`b/` markers this still relies on to tell the two tokens apart.
fn diff_section_path(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("diff --git ") {
        if let Some((_a, remainder)) = take_diff_header_token(rest)
            && let Some((b, _)) = take_diff_header_token(remainder)
        {
            // The token is the whole `b/<path>` destination spelling
            // (unquoted) — strip the `b/` marker to get the bare path, same
            // as the old `" b/"`-scan fallback below did.
            return Some(b.strip_prefix("b/").map(str::to_string).unwrap_or(b));
        }
        // Fall back to the old best-effort scan for a header this tokenizer
        // can't make sense of, rather than silently losing the path.
        if let Some(idx) = rest.rfind(" b/") {
            return Some(rest[idx + 3..].to_string());
        }
        return rest
            .strip_prefix("a/")
            .map(|p| p.split(' ').next().unwrap_or(p).to_string());
    }
    for pre in ["diff --cc ", "diff --combined "] {
        if let Some(rest) = line.strip_prefix(pre) {
            return Some(unquote_c_style(rest));
        }
    }
    None
}

/// Consume one whitespace-delimited `diff --git` header token from the start
/// of `s`, which may be a bare path (`a/foo`) or a C-style-quoted one
/// (`"a/my dir/.env"`), returning the unquoted token and whatever follows the
/// single separating space. `None` if `s` is empty or a quoted token's closing
/// quote is missing (malformed input — let the caller fall back).
fn take_diff_header_token(s: &str) -> Option<(String, &str)> {
    if let Some(inner) = s.strip_prefix('"') {
        let bytes = inner.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' if i + 1 < bytes.len() => i += 2,
                b'"' => {
                    let token = &inner[..i];
                    let remainder = inner[i + 1..].strip_prefix(' ').unwrap_or(&inner[i + 1..]);
                    return Some((unquote_c_style(&format!("\"{token}\"")), remainder));
                }
                _ => i += 1,
            }
        }
        None
    } else if s.is_empty() {
        None
    } else {
        let (token, remainder) = s.split_once(' ').unwrap_or((s, ""));
        Some((token.to_string(), remainder))
    }
}

/// Unquote a C-style quoted string as git emits under `core.quotePath`
/// (default on): a double-quoted token where `\\`, `\"`, `\t`, `\n`, `\r`, and
/// `\NNN` (octal byte — how a non-ASCII UTF-8 byte is spelled) stand for the
/// literal byte. Returns `s` unchanged if it isn't a quoted token — git only
/// quotes a path that needs it.
fn unquote_c_style(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return s.to_string();
    };
    let bytes = inner.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'\\' => {
                    out.push(b'\\');
                    i += 2;
                }
                b'"' => {
                    out.push(b'"');
                    i += 2;
                }
                b't' => {
                    out.push(b'\t');
                    i += 2;
                }
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 2;
                }
                b'a' => {
                    out.push(0x07);
                    i += 2;
                }
                b'b' => {
                    out.push(0x08);
                    i += 2;
                }
                b'f' => {
                    out.push(0x0c);
                    i += 2;
                }
                b'v' => {
                    out.push(0x0b);
                    i += 2;
                }
                d1 @ b'0'..=b'7'
                    if i + 3 < bytes.len()
                        && (b'0'..=b'7').contains(&bytes[i + 2])
                        && (b'0'..=b'7').contains(&bytes[i + 3]) =>
                {
                    // Widen to u32 before combining digits: a byte's worth of
                    // octal digits (each 0-7) can sum past 255 for malformed
                    // input, which would overflow a `u8` multiply/add.
                    let d1 = u32::from(d1 - b'0');
                    let d2 = u32::from(bytes[i + 2] - b'0');
                    let d3 = u32::from(bytes[i + 3] - b'0');
                    out.push((d1 * 64 + d2 * 8 + d3) as u8);
                    i += 4;
                }
                other => {
                    // Unrecognised escape: keep both characters verbatim
                    // rather than dropping the backslash.
                    out.push(b'\\');
                    out.push(other);
                    i += 2;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Redact the hunk body of any diff section whose file is a credential/secret
/// store, keeping the section header so the model still sees *that* the file
/// changed — just not its content. Covers `diff`, `show`, and `log -p` output;
/// a no-op on plain `status`/`log`/`branch` output (no diff headers).
///
/// `pub` (re-exported from the crate root) for callers outside this module that
/// compose a diff themselves rather than going through [`GitTool`].
///
/// **Nothing calls it today.** Its one caller was `task_diff` in `hrdr-agent`,
/// which composed `git diff HEAD...<branch>` for a sub-agent's branch; sub-agent
/// worktrees and that tool are both gone. Kept because the redaction itself is
/// still correct and the next tool that assembles a diff will want it — but if
/// none arrives, this module is deletable.
pub fn redact_secret_diffs(output: &str) -> String {
    let mut out = String::with_capacity(output.len());
    let mut lines = output.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(path) = diff_section_path(line) else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        out.push_str(line);
        out.push('\n');
        if crate::secret_file_reason(std::path::Path::new(&path)).is_some() {
            out.push_str(
                "[redacted: this file is a credential/secret store — its diff is withheld]\n",
            );
            // Drop the rest of this section (up to the next `diff` header / EOF).
            while let Some(peek) = lines.peek() {
                if diff_section_path(peek).is_some() {
                    break;
                }
                lines.next();
            }
        }
    }
    out
}
