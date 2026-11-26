# 2026-11-26 — A copy between two hosts waits for the target's WELCOME before forwarding

acs-gcx0. The relay of `acscp h1:… h2:…` forwards nothing from the source host until the target host's `WELCOME` has
said whether its target is a directory, so several sources into what is not a directory are refused before any `FILE`
reaches the target. Bug log:
[2026-11-26-relay-forwarded-a-file-before-checking-the-target.md](../bugs/2026-11-26-relay-forwarded-a-file-before-checking-the-target.md).
