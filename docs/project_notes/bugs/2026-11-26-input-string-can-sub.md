# 2026-11-26 — An unterminated terminal reply in input could swallow the command key

The input-side lexer in `src/keys.rs` left an OSC/DCS/APC string (a terminal's reply to a query) only on BEL, on
`ESC \` or after 64 KiB (`MAX_STR`). CAN (0x18) and SUB (0x1a), which xterm, VTE and the VT500 parser treat as
aborting the string, were taken as part of it, so a reply cut short by one of them kept the parser inside a string
and the command key was forwarded instead of heard. The output side had the same gap, fixed in acs-q4p6.

CAN and SUB now return the lexer to ground (the byte is still forwarded, as everything in a string is).

ESC plus another byte is deliberately not an abort on the input side, unlike the output side: the input lexer
forwards bytes untouched, so ending a string there would need the following byte re-lexed as a key, and an ESC in
a reply is also a Meta key or the first half of a doubled ESC from tmux passthrough. The idle timeout (a Meta key
mistaken for a string introducer) and the `MAX_STR` cap already bound how long such a string can hold the detector.
`ESC X` and `ESC ^` are not string introducers on input at all (acs-evv).

Regression test: `can_and_sub_end_an_input_string` in `src/keys.rs`; fails without the fix.
