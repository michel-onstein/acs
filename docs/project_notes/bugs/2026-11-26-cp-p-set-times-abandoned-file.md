# 2026-11-26 — A file whose times could not be set under cp -p was abandoned though it was in place

With `-p`, `Receiver::file_end` renamed the part to its name and then set the times in the same fallible
sequence. When the rename succeeded and `set_times` failed, the error went to `abandon`: a `FAIL` was sent, the
file counted failed and not written although its content was in place, and the message did not say "set times",
so it read as if nothing was written. `abandon` also removed the part path after the rename, which could delete a
new part a concurrent copy had just made there.

After a successful rename the file is kept whatever follows: it is answered `OK`, counted written, the part path
is not touched, and a failing `set_times` is reported as `set times: ...` and counted failed, so the run exits 1
like `scp -p`.

Regression test: `a_file_whose_times_cannot_be_set_is_kept_and_reported` in `src/cp/receiver.rs`; it fails
(no `OK`, a `FAIL` answer) without the fix.
