# 2026-11-26 — A redial killed ssh under a password typed in more than 30 s

`dial_once` counted the redial's deadline (`ACS_REDIAL_TIMEOUT_MS`, 30 s) from ssh's spawn and killed ssh when it
passed, even after ssh had taken the terminal for a password, passphrase, 2FA code or key touch. A slow answer was
thrown away and the dial made again, prompting afresh — while the network-watcher branch of the same loop already
refused to abandon a dial whose ssh had the terminal, so that a half-typed password survived. DESIGN §5.3 and
decision 18 described the 30 s as bounding the password too.

From the hand-off the deadline is now `dial_deadline`: the later of the dial's own and the hand-off plus
`answer_timeout(false)` (120 s, the first connection's window). It is not removed: a hung prompt still ends the dial
and the client returns to its backoff wait, the only way out of it that does not end acs.

Regression tests: `a_prompt_gets_its_own_window_from_the_hand_off` in `src/client.rs` and
`a_slow_answer_at_a_redial_prompt_is_not_cut_off` in `tests/reconnect.rs` (a 300 ms redial timeout, an answer
after 900 ms); both fail without the fix.
