# 2026-11-26 — Release 0.24.0 was tagged but failed to publish: over the size budget

`scripts/version-bump.sh` tagged 0.24.0, then `cargo xtask dist` refused to package it: the 36 fixes since
0.23.0 had taken x86_64 Linux to 1,192,856 B slim and 2,392,626 B complete, past the 1.18 MB / 2.36 MB budget.
Nothing caught it earlier, because the gate (`scripts/verify.sh`) does not build release binaries; each fix was
small, and only the release build added them up. The tag exists without binaries.

The maintainer decided "trim, then raise", as for port knocking (DESIGN decision 23, after decision 20). The cuts,
measured on x86_64 Linux slim: `close_range_except` finds each gap with a scan instead of `sort_unstable`, the
binary's only sort of `RawFd`s (4,184 B); the socket directory's refusals share one owner helper and its symlink walk
uses a `Vec` stack instead of a `VecDeque` (672 B). Then the budget went to slim ≤ 1.2 MB and complete ≤ 2.4 MB,
leaving 12,000 B and 17,903 B of x86_64 Linux room. DESIGN §8.1 has every target. 0.24.1 is the first release with
these fixes in it.

Regression test: `close_inherited_closes_without_a_descriptor_directory` in `src/sys.rs` now keeps descriptors out
of order, repeated and with a standard one among them, so the scan has to find each gap. It runs on Linux
(`scripts/test_linux.sh`). The socket-directory walk keeps its `FakeFs` tests in `src/session.rs`. `cargo xtask dist`
is the size check itself.

Prevention: the size check runs only when a release is cut. Putting `cargo xtask dist` in the gate would add four
LTO release builds and the macOS signing to every landing, minutes each time. A cheaper check would build only the
x86_64 musl slim binary, the tight target, and compare it with `SLIM_BUDGET` (one LTO build, about a minute and a
half). It is not added here. Until it is, whoever lands a batch of fixes should run `cargo xtask dist` before
`scripts/version-bump.sh`.

Since added (acs-i3vj): `cargo xtask size` builds both Linux slim binaries with `dist`'s own code, in about 25 s,
and runs in `scripts/verify.sh` and in `cargo xtask bump` before it commits or tags (docs/VERSIONING.md, "The
size check").
