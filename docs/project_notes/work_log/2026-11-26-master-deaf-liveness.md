# 2026-11-26 — Keep the client while the master is not reading it (acs-009c)

`src/master.rs` no longer counts a client's silence while the client's socket is masked for the pty input high
water; `Liveness::not_listening` in `src/reconnect.rs` keeps it from ageing. DESIGN §5.3 says so. Test in
`tests/master.rs`. Bug log: [master-deaf-liveness](../bugs/2026-11-26-master-deaf-liveness.md).
