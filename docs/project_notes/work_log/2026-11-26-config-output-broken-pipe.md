# 2026-11-26 — A closed pipe no longer panics `acs config` output (acs-nxfq)

`acs config show | true` exited 101 with a panic; output now goes through `acs::print_out`, which treats a
closed reader as done. The same fix covers `acs list`, `acs upgrade` help and the usage text. Test in
`tests/config_cmd.rs`. Bug log: [config-output-broken-pipe](../bugs/2026-11-26-config-output-broken-pipe.md).
