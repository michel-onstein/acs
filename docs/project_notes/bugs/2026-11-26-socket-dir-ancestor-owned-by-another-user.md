# 2026-11-26 — A socket directory under another user's `0755` directory was accepted

`check_ancestors` in `src/session.rs` refused an ancestor of the socket or control directory only when it was
writable by group or other and not sticky. A real directory owned by another non-root user at `0755` passed —
yet its owner can rename or remove anything in it (and chmod it first if the mode were tighter), so they could
move the user's socket directory, or the next directory on the way, aside and put their own in its place. The
next client would then connect to their master. Only a pointed `ACS_SOCKET_DIR` or `ACS_CONTROL_DIR` could
reach this; the default `/tmp/acs-<uid>` sits under root's `/tmp`. Pre-existing, noticed during acs-vqdo
(acs-9imr).

Fix: every directory the path passes through must be the user's own or root's; any other owner is refused
whatever its mode, with `<dir> is owned by <user> (uid N), who could replace what is in it`. Root's must still
be not writable by others or sticky, as before. This keeps every tree a user legitimately uses working — `/`,
`/tmp`, `/home`, `/var`, `/run/user`, macOS's `/Users` and `/var/folders/<xx>` are root's, and the home
directory, `/run/user/<uid>` and `$TMPDIR` below them are the user's — and refuses a directory under someone
else's home or a service account's tree. Run as root, only root's directories pass. DESIGN §4.5 states the
rule.

Prevention: the rule is about owners, not modes — whoever owns a directory controls its entries.

Regression tests: `a_strangers_directory_on_the_way_is_refused_whatever_its_mode` and
`as_root_another_users_directory_is_refused` in `src/session.rs` fail without the fix;
`root_and_own_directories_on_the_way_are_fine` holds the Linux and macOS layouts that must keep working; and
`a_socket_directory_under_another_users_directory_is_refused` in `tests/multiuser.rs` checks it as root
against real users.
