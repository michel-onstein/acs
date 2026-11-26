# 2026-11-26 — A relay with both directions flow-controlled could never declare a side dead

In the `acscp h1:a h2:b` splice, a side whose queue partner was at `HIGH` was not read and was excused as heard on
every pass. With both queues at `HIGH` each side was held by the other, so both were excused every pass and
neither liveness check could fire: the relay could stall forever, and a relay link has no netwatch path out. It
needs the control queue (GO/OK/PONG) at 256 KiB as well, so it was rare.

A side is now excused only when the other direction is not also held (`excused` in `src/cp/relay.rs`).

Regression test: `a_held_side_is_excused_unless_both_directions_are_jammed` in `src/cp/relay.rs`; it fails with
the old unconditional excuse.
