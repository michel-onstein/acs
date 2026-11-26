# 2026-11-26 — Apply the alias memory guard under prefer_local_network (acs-hzzw)

`src/alias.rs` now refuses a remembered host that an unchecked entry would beat, also when `prefer_local_network`
leaves the rank unknown. Test in `src/alias.rs`. Bug log:
[alias-memory-guard-prefer-local-network](../bugs/2026-11-26-alias-memory-guard-prefer-local-network.md).
