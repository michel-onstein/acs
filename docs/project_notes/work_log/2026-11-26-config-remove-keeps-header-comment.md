# 2026-11-26 — Keep the comment above the last entry when acs config removes it (acs-mrfq)

`remove_entry`, `remove_alias` and `keep_comments` in `src/config_cmd.rs` now return the comment lines that found
no next entry. They move above the next entry of the enclosing level and, failing that, to the document tail.
Tests in `src/config_cmd.rs`. Bug log:
[config-remove-drops-header-comment](../bugs/2026-11-26-config-remove-drops-header-comment.md).
