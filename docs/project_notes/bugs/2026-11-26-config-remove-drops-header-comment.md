# 2026-11-26 — Removing the last entry of a mapping dropped the comment above it

`acs config` removals (`unset`, `host remove`, `host unset`) hand the comment lines above a removed entry to the
entry that takes its place. When the removed entry was the last of its mapping or of the hosts list, nothing took
its place and the comment — typically a section header — was deleted with it.

The lines now go above the next entry of the level above: the next alias for a host, the next top-level setting
for an alias. When there is none, they lead the end of the document, ahead of any comment already there. Removing
the only host of an alias, which removes the alias and an emptied `aliases`, keeps every comment the same way.

Regression tests in `src/config_cmd.rs`: `unset_of_the_last_setting_keeps_its_header_comment`,
`removing_the_last_host_keeps_the_comment_above_it` and the last step of
`host_set_moves_the_hosts_under_the_alias_and_unset_moves_them_back`; all three fail without the fix.
