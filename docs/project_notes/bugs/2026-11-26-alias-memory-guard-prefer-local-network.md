# 2026-11-26 — The alias memory ignored its guard under prefer_local_network

With `prefer_local_network` on, the rank is known only once the host names resolve, so `known` is `None` and the
filter that refuses a remembered host ranked behind a `reachability_check: false` entry returned true for any
remembered host. After a config edit added an unchecked entry ahead of the remembered one, the connection was dialled
to the remembered host over an entry the walk would have taken outright. The background confirmation corrected it on
the next connection.

The remembered host is now taken only if it would beat every unchecked entry even if the resolver put both on a local
network: its `prefer` flag and configured position must come before each unchecked entry's.

Regression test: `with_prefer_local_network_a_memory_behind_an_unchecked_entry_is_ignored` in `src/alias.rs`; it fails
without the fix.
