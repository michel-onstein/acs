# 2026-11-26 — Bound a resumed copy's hashing slice by time as well as size (acs-qeqj)

`Prefix::step` in `src/cp/mod.rs` now ends a turn's slice at a megabyte or after 20 ms, whichever comes first, so
pings are answered while a `HAVE` is checked on a slow or loaded machine too. That was why
`a_download_cut_mid_file_resumes_from_its_part` failed now and then in a loaded full run: a megabyte took up to
a third of a second in the debug build, and the far end's 800 ms test dead interval ran out with both ends alive.
Test `a_slice_ends_when_its_time_is_up`; ACSCP_DESIGN §7 updated. Bug log:
[hash-slice-outlasted-dead-interval](../bugs/2026-11-26-hash-slice-outlasted-dead-interval.md).
