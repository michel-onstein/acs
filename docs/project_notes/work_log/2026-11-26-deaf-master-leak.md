# 2026-11-26 — The deaf-master test no longer leaks its master (acs-97ox)

`tests/master.rs`: the test's shell carries a two-minute watchdog so a killed test cannot leave its master
running, and a normal run asserts the master's pid is dead within `T` after the session ends
(`assert_master_ends`). Other tests in the file checked: none leaks. Bug log:
[deaf-master-leak](../bugs/2026-11-26-deaf-master-leak.md).
