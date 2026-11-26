# 2026-11-26 — A slow disk no longer makes a copy redial (acs-wufw)

`acs cp` now tells `Liveness::wrote_for` how long it spent handling each read, and counts the read as heard after
it, so a disk stall after a network change is not taken for a dead link. Test in `src/cp/mod.rs`. Bug log:
[cp-disk-stall-kills-link](../bugs/2026-11-26-cp-disk-stall-kills-link.md).
