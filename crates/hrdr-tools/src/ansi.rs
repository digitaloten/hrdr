//! Turn terminal output into the text a terminal would have *shown*.
//!
//! A program writing to a pipe still often emits colour: `rustfmt --check`
//! colours its diff unconditionally, and any tool with `color.ui = always` in the
//! user's config does the same. Those bytes are addressed to a terminal, and
//! nothing downstream of a tool call is one — the model reads
//! `\x1b[31m-        let b1 = …` where a terminal shows `-        let b1 = …`, and
//! pays tokens for the difference. Worse, the escape survives into the transcript
//! and out to whatever renders it, so the noise is charged twice.
//!
//! So a tool's captured output is cleaned before it becomes a result: escape
//! sequences are dropped, and a line that a carriage return overwrote is reduced
//! to what would have been left on screen. What is removed is *presentation* —
//! nothing that carries meaning for a reader of the text.
//!
//! A caller that genuinely wants the raw bytes — a model checking that its own
//! CLI emits the right colour, or that a progress line redraws — asks for them
//! (`keep_ansi` on the `shell` tool) and this is skipped entirely. That is the
//! only way to see escapes, which is the point: the escape hatch is explicit, so
//! the default can be clean without hiding anything.

use std::borrow::Cow;

/// Clean one chunk of captured terminal output — typically a single line.
///
/// Returns a borrow when there is nothing to strip, which is the common case: a
/// build that prints plain text allocates nothing here.
pub fn clean(text: &str) -> Cow<'_, str> {
    if !needs_clean(text) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(clean_owned(text))
}

/// Whether [`clean`] would change `text` — a byte scan, no allocation.
///
/// Separate from `clean` so a caller holding an owned `String` (every line the
/// `shell` tool reads) can skip cleaning entirely rather than round-tripping a
/// `Cow` back into a fresh allocation.
pub fn needs_clean(text: &str) -> bool {
    text.bytes().any(|b| b == 0x1b || b == b'\r')
}

fn clean_owned(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => skip_escape(&mut chars),
            // A carriage return returns the cursor to the start of the line, and
            // what follows overwrites what came before it — that is how a progress
            // bar redraws in place. What a reader would see is the LAST thing
            // written, so drop everything written so far on this line.
            //
            // Only mid-line. A trailing `\r` is a CRLF line ending with its `\n`
            // already consumed by the line reader, and dropping the line's whole
            // content for that would delete every line on Windows.
            '\r' => {
                if chars.peek().is_some() {
                    out.clear();
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Consume the rest of one escape sequence, having already taken the `ESC`.
///
/// Covers what a program writes to a pipe in practice:
///
/// * **CSI** (`ESC [ … final`) — colour, cursor movement, line erase. Parameter
///   and intermediate bytes run `0x20..=0x3f`; the first byte outside that range
///   ends the sequence.
/// * **OSC** (`ESC ] … BEL` or `… ESC \`) — window titles, and the hyperlinks
///   `cargo`-adjacent tools now emit around file paths.
/// * **Two-byte** escapes (`ESC (B`, `ESC =`, …) — charset and keypad selection.
///
/// An `ESC` at the very end of the chunk, or one whose sequence is cut off by the
/// line boundary, simply disappears: it is presentation either way, and a
/// half-sequence is exactly what should not reach a reader.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match chars.next() {
        Some('[') => {
            // CSI: parameters/intermediates, then one final byte that ends it.
            while let Some(&c) = chars.peek() {
                chars.next();
                if !('\x20'..='\x3f').contains(&c) {
                    break;
                }
            }
        }
        Some(']') => {
            // OSC: terminated by BEL, or by ST (`ESC \`).
            while let Some(c) = chars.next() {
                match c {
                    '\x07' => break,
                    '\x1b' => {
                        // `ESC \` ends it; anything else starts a fresh sequence,
                        // so let the outer loop see it.
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
        // `ESC (B`, `ESC )0`, … take one more byte; `ESC =`, `ESC >`, `ESC c` and
        // friends take none. Peeking for the pair keeps a following printable
        // character from being eaten.
        Some('(') | Some(')') | Some('*') | Some('+') | Some('%') | Some('#') => {
            chars.next();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reported case: `rustfmt --check` colours its diff, and every one of
    /// those sequences reached the model as literal text.
    #[test]
    fn a_coloured_diff_reads_as_the_diff() {
        let raw = "\x1b[31m-        let b1 = make_snapshot(\n\x1b[m\x1b[32m+        let b1 = make_snapshot(TickId::from_raw(1));\n\x1b[m";
        assert_eq!(
            clean(raw),
            "-        let b1 = make_snapshot(\n+        let b1 = make_snapshot(TickId::from_raw(1));\n"
        );
    }

    /// Clean text is passed through untouched, and without an allocation — most
    /// output is not coloured, and this runs on every line of every command.
    #[test]
    fn clean_text_is_borrowed_not_rebuilt() {
        let plain = "   Compiling hrdr-tools v0.8.4\n";
        assert!(matches!(clean(plain), Cow::Borrowed(_)));
        assert_eq!(clean(plain), plain);
        assert!(!needs_clean(plain));
    }

    /// A progress bar redraws in place: what a reader would see is the last thing
    /// written, not all forty attempts concatenated.
    #[test]
    fn a_redrawn_progress_line_collapses_to_what_is_left_on_screen() {
        assert_eq!(clean("  0/10\r  5/10\r 10/10 done"), " 10/10 done");
        // A CRLF line ending, whose `\n` the line reader already took. The line's
        // content must survive — this is every line of output on Windows.
        assert_eq!(clean("Compiling\r"), "Compiling");
    }

    /// Cursor and erase sequences go the same way as colour: a build that erases
    /// the line before rewriting it leaves only the text.
    #[test]
    fn cursor_and_erase_sequences_are_dropped() {
        assert_eq!(clean("\x1b[2K\x1b[1G\x1b[Kbuilding"), "building");
        assert_eq!(clean("a\x1b[3Db"), "ab");
    }

    /// OSC — window titles, and the hyperlinks tools now wrap paths in — under
    /// both terminators. The link *text* is content and stays.
    #[test]
    fn osc_sequences_are_dropped_but_their_text_stays() {
        assert_eq!(clean("\x1b]0;a title\x07ready"), "ready");
        assert_eq!(
            clean("\x1b]8;;file:///tmp/a.rs\x1b\\a.rs\x1b]8;;\x1b\\"),
            "a.rs"
        );
    }

    /// A sequence cut in half by the line boundary must not leak its tail, and
    /// must not swallow the text after it either.
    #[test]
    fn a_truncated_sequence_does_not_leak() {
        assert_eq!(clean("done\x1b"), "done");
        assert_eq!(clean("done\x1b["), "done");
        // Charset selection takes exactly one more byte — not the word after it.
        assert_eq!(clean("\x1b(Bplain"), "plain");
    }

    /// What is NOT presentation stays: tabs and newlines are layout a reader
    /// depends on, and a stray `\x1b`-free control char is left alone.
    #[test]
    fn real_layout_survives() {
        assert_eq!(clean("a\tb\nc\n"), "a\tb\nc\n");
        assert_eq!(
            clean("warning: \x1b[33munused\x1b[0m\tsrc/a.rs"),
            "warning: unused\tsrc/a.rs"
        );
    }
}
