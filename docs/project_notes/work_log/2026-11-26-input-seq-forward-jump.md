# 2026-11-26 — Bound the input counter's forward jump and check the client's sequence arithmetic (acs-zxep)

`src/resume.rs` refuses a forward jump past 2^62 in `InputDedupe::accept`, makes `Unacked::push` refuse bytes
that cannot be numbered, and adds the WELCOME check `src/client.rs` now applies before rebasing. Tests in
`src/resume.rs` and `tests/master.rs`. Bug log: [input-seq-forward-jump](../bugs/2026-11-26-input-seq-forward-jump.md).
