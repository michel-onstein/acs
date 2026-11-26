# 2026-11-26 — A program not reading its input got its client dropped as silent

Past 1 MiB of input the pty has not taken (`PTY_IN_HIGH_WATER`), the master masks `POLLIN` on the client's
socket, so it reads nothing from the client — `PONG`s included. Only `read_conn` refreshes liveness, and the
tick had no exemption, so a program that read nothing for `ACS_DEAD_MS` (10 s) — a paused pager, a blocked
pipe — got its client declared dead and dropped, forcing a redial. The resume was lossless, but the link churned
for no reason.

The master now calls `Liveness::not_listening` on every pass while the mask holds: silence it is not listening
for does not age, and the dead interval counts from when it reads again. A vanished client is still found one
dead interval after the program drains.

Regression test: `a_program_not_reading_its_input_does_not_drop_the_client` in `tests/master.rs` queues
1.25 MiB of input for a program that sleeps for several dead intervals, then expects its output on the same
connection; without the fix the master hangs up and the test's write fails with a broken pipe.
