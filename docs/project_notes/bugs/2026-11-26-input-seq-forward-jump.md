# 2026-11-26 — A forward jump could park the input counter next to `u64::MAX`

The twin of acs-hpf and acs-shj7, on the client side and in the master's jump. `InputDedupe::accept` takes a
frame that starts beyond what was written whole and moves the counter to its end, with no bound but the end
fitting in a `u64`. A same-uid peer could send one frame at `u64::MAX - 10` and park the counter there. Every
client after it was welcomed at that position and rebased `Unacked` onto it, whose `next_seq` was an unchecked
`base + len`: a debug build aborted on the second keystroke, and in release the master refused each frame as
"input sequence out of range", so the client redialled over and over and the input was lost.

Three guards (acs-zxep):

- `InputDedupe::accept` refuses a forward jump whose end lies beyond `MAX_INPUT_JUMP` (2^62); input that
  continues from `written` is still bounded only by fitting. The master answers BAD_REQUEST, as for acs-hpf.
- `Unacked::push` returns `None`, recording nothing, for bytes whose end cannot be numbered; the client does not
  send them.
- The client refuses a WELCOME whose `input_seq` is beyond `MAX_WELCOME_INPUT_SEQ` (2^63), saying "the
  session's input sequence is out of range" and exiting with an error.

Regression tests: `a_forward_jump_is_bounded`, `unacked_refuses_bytes_it_cannot_number` and
`a_welcome_input_seq_must_leave_room` in `src/resume.rs`, and
`a_forward_jump_past_the_bound_is_refused_and_does_not_poison_the_counter` in `tests/master.rs`; all fail
without the fix.
