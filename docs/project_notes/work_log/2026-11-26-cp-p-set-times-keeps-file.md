# 2026-11-26 — Keep a copied file as written when only cp -p set_times fails (acs-n19p)

`src/cp/receiver.rs`: after the rename into place a failing `set_times` no longer abandons the file. It is
answered `OK` and counted written, `set times: ...` is reported and the run exits 1, and the part path is not
removed. Test in the same file; `docs/ACSCP_DESIGN.md` (Times) says so. Bug log:
[cp-p-set-times-abandoned-file](../bugs/2026-11-26-cp-p-set-times-abandoned-file.md).
