# 2026-11-26 — Let relay liveness fire when both directions are flow-controlled (acs-cykd)

`src/cp/relay.rs` no longer excuses either side as heard when both queues are at `HIGH`, so the splice can declare
a dead side instead of stalling. Test in `src/cp/relay.rs`. Bug log:
[relay-both-held-never-dead](../bugs/2026-11-26-relay-both-held-never-dead.md).
