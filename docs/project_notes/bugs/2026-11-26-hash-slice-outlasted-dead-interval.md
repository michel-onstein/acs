# 2026-11-26 — A resumed copy's hashing outlasted the far end's dead interval under load (acs-qeqj)

`tests/cp_remote.rs` `a_download_cut_mid_file_resumes_from_its_part` failed now and then in a full
`scripts/verify.sh` run and passed alone. The failing runs all asserted `connections == base + 2` and saw 3: the
redial that resumed from the part was itself lost, and a third connection finished the copy. Two ways, both seen:

- `acscp: devbox: no answer from the far end — reconnecting in 1s` — the receiver gave the link up while the
  sender hashed its source against the `HAVE`;
- `acscp: devbox: the link closed before the copy finished` — the sender (the remote end) gave the link up while
  the receiver hashed its part, and the receiver's next write failed.

The cause was in acs, not the test. Both ends hash a resumed part's prefix inside the poll loop a slice per turn
(ACSCP_DESIGN §7, decision 12), and a slice was a fixed megabyte. With timing lines added to `Prefix::step`, a
megabyte took 36–45 ms unloaded in the debug build the suite runs, and up to 317 ms with one `yes` per CPU running
(443 ms under `taskpolicy -c background`). A turn that long costs the far end about two of them before it hears a
`PONG`: the `PING` read in one turn is queued after that turn's `poll`, so it goes out only after the next turn's
slice. The tests' `FAST` timers ping after 200 ms and give up after 800 ms, so 200 ms plus two slices of 300 ms
ran the dead interval out with both ends alive. A release build hashes a megabyte in a few milliseconds and the
default dead interval is 10 s, so this needed a slow machine to bite outside the tests — but the guarantee the
slice exists for, that pings go on while a `HAVE` is checked, depended on CPU speed.

`Prefix::step` now ends a slice at a megabyte or after 20 ms (`SLICE_TIME`), whichever comes first, and always
makes at least one 64 KB read, so a turn is short on any machine. Regression test:
`a_slice_ends_when_its_time_is_up` in `src/cp/mod.rs`, which with no time budget expects one read per turn and a
digest equal to the whole file's; on the old code the first turn hashed a whole megabyte.
`a_part_on_disk_is_offered_and_resumed_or_restarted` in `src/cp/receiver.rs` now asks for at least three turns,
not exactly three.

Reproduced by looping the test alone with one `yes` per CPU running: 2 failures in 90 runs before the fix (both
above), and none in 15 runs of the whole `cp_remote` binary — rare, as in the full gate. To make it reproducible
the test's timers were scaled down by hand to `ACS_PING_MS=40`, `ACS_DEAD_MS=120` against the unloaded debug build:
before the fix every run failed, each redial lost right after `resuming from the 2981888 bytes already here`
with `no answer from the far end`, backing off until it was stopped; with the fix, 20 of 20 passed. With the
test's own timers and the fix, 60 of 60 passed under the same load.
