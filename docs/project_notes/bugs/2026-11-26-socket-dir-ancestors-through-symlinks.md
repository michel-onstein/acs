# 2026-11-26 — The socket directory's ancestors were judged by a symlink's own mode

`check_ancestors` in `src/session.rs`, which refuses an `ACS_SOCKET_DIR` or `ACS_CONTROL_DIR` sitting under a
directory others could replace (acs-hjk), walked the path's lexical ancestors and `lstat`ed each. A symlink in
that chain was judged by the link's own mode. On Linux every link is `0777`, so a link owned by anyone but the
user — root's `/home -> var/home` on Fedora Silverblue — was refused as replaceable, and every socket
directory under a home there was unusable. On macOS links are `0755`, which hid it. The other way round, the
link's target and the target's ancestors were never looked at, so a directory under a group- or world-writable
directory passed whenever the path reached it through a link.

The check now resolves the parent the way the kernel does, one name at a time, following each link into its
target (up to 40, then an error), and judges every directory a name is looked up in: the one holding each link
and every directory of each target's chain. A link's mode is never judged; its owner is — it must be the user
or root, since whoever owns a link can remove it even from a sticky directory and point a new one anywhere
(the case `0777` used to refuse by accident). For a path with no links the directories judged are exactly the
lexical ancestors, so nothing changed for real directories.

Regression tests in `src/session.rs`, over a made-up file system so root's links and strangers' directories
need no root: `a_root_owned_symlink_in_the_path_is_followed_not_judged` and
`a_replaceable_directory_behind_a_symlink_is_refused` both fail on the old walk; `real_directories_are_judged_as_before`
holds the acs-hjk behaviour. `a_socket_directory_is_judged_through_symlinks` in `tests/multiuser.rs` checks both
cases as root with real users under `scripts/test_linux.sh`.
