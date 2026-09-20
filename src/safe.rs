//! Making text that came from somewhere else safe to print (acs-w1z).
//!
//! A session's byte stream is passed through untouched — that is the
//! unfiltered guarantee of DESIGN §5.1, and the local terminal is meant to
//! see exactly what the remote program wrote. acs's *own* interface is a
//! different matter: the listing, the session menu, the takeover question
//! and every `acs:` note are drawn by acs out of strings a remote host
//! chose (a session name, the identity attached to it, the command it
//! runs, the text of an `ERROR` frame).
//!
//! Printed raw, those strings can move the cursor, erase the rows around
//! them and redraw the list the user is about to act on — so the line read
//! as `trusted-host  main  detached` need not be the row the cursor is on.
//! Worse, a sequence the terminal *answers* (a title report, OSC 52) puts
//! the reply on the client's stdin, where it becomes input to whichever
//! session is attached next. Clipping by display width does not help: ESC
//! is one column wide and truncation can even cut a sequence in half.
//!
//! So every such string passes through [`display`] on its way to the
//! terminal.

/// How much of one field ever reaches the terminal. Long enough for any
/// real name, identity or command line; short enough that a megabyte of
/// padding cannot push the rest of the row off the screen.
pub const MAX_FIELD: usize = 256;

/// `s` with everything that could steer the terminal replaced by `?`, and
/// no longer than [`MAX_FIELD`] characters.
///
/// Replaced: the C0 controls and DEL (`ESC`, `CR`, `BEL`, …), the C1
/// controls (`U+0080`-`U+009F`, which some terminals still act on), and the
/// bidirectional overrides, which reorder a line on screen without changing
/// a byte of it — another way to make a row read as something it is not.
pub fn display(s: &str) -> String {
    display_max(s, MAX_FIELD)
}

/// [`display`] with an explicit cap. A truncated string ends in `…` so the
/// reader can tell it was cut.
pub fn display_max(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max));
    for (i, c) in s.chars().enumerate() {
        if i == max {
            out.push('…');
            break;
        }
        out.push(if steers_the_terminal(c) { '?' } else { c });
    }
    out
}

/// Whether `c` does something to a terminal other than occupy a cell.
fn steers_the_terminal(c: char) -> bool {
    matches!(c,
        '\0'..='\u{1f}'        // C0: ESC, CR, LF, BEL, …
        | '\u{7f}'             // DEL
        | '\u{80}'..='\u{9f}'  // C1, including the 8-bit CSI
        | '\u{202a}'..='\u{202e}' // bidi embedding and override
        | '\u{2066}'..='\u{2069}' // bidi isolate
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_text_is_untouched() {
        assert_eq!(display("main"), "main");
        assert_eq!(display("alice@laptop"), "alice@laptop");
        assert_eq!(display("vim docs/DESIGN.md"), "vim docs/DESIGN.md");
        // Not every non-ASCII character is a threat.
        assert_eq!(display("café ✓ 日本語"), "café ✓ 日本語");
    }

    #[test]
    fn escape_sequences_cannot_survive() {
        // The row-rewriting payload of acs-w1z.
        assert_eq!(display("main\x1b[1A\x1b[2Kfake"), "main?[1A?[2Kfake");
        // A carriage return alone rewrites the line it is on.
        assert_eq!(display("a\rb"), "a?b");
        // OSC with a BEL terminator (window title, clipboard).
        assert_eq!(display("\x1b]0;x\x07"), "?]0;x?");
        // The 8-bit CSI, for terminals that still take it.
        assert_eq!(display("a\u{9b}31m"), "a?31m");
        assert!(!display("x\x1b[2J").contains('\x1b'));
    }

    #[test]
    fn bidi_overrides_cannot_reorder_a_row() {
        assert_eq!(display("main\u{202e}detached"), "main?detached");
        assert_eq!(display("\u{2066}x\u{2069}"), "?x?");
    }

    #[test]
    fn long_fields_are_capped_and_marked() {
        let long = "x".repeat(MAX_FIELD * 4);
        let out = display(&long);
        assert_eq!(out.chars().count(), MAX_FIELD + 1);
        assert!(out.ends_with('…'));
        // Exactly at the cap nothing is added.
        let exact = "y".repeat(MAX_FIELD);
        assert_eq!(display(&exact), exact);
    }

    #[test]
    fn the_cap_counts_characters_not_bytes() {
        // A multi-byte character must not be cut in half.
        let s = "é".repeat(MAX_FIELD + 10);
        let out = display_max(&s, 4);
        assert_eq!(out, "éééé…");
    }
}
