# 2026-11-26 — Signal a lost master spawn race with a token, not error text (acs-2mfo)

A master that finds its session already held writes `busy: <why>` on the ready pipe instead of `err: <why>`;
`master::spawn` returns `ErrorKind::AlreadyExists` for it and `src/proxy.rs` matches the kind, not the words. Older
masters' `err: … already has a master` still reads as busy, and the busy text keeps those words for older
proxies. DESIGN §4.2 describes the ready pipe. Bug log:
[spawn-race-by-error-text](../bugs/2026-11-26-spawn-race-by-error-text.md).
