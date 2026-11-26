# 2026-11-26 — A lost master spawn race was recognised by its error text

When two proxies start a master for the same session at once, the loser's master steps aside and says so on the
ready pipe. It said it as `err: session '<name>' already has a master`, and the proxy told a lost race from a real
failure by searching for "already has a master" in that text. Rewording the message would have made every client
that lost the race fail with "cannot start session" instead of connecting to the winner.

The master now writes the status token `busy: <why>` for a lost race, which `master::spawn` maps to
`io::ErrorKind::AlreadyExists`; the proxy checks the kind. The text after the token keeps "already has a master",
and `spawn` still reads an `err:` with those words as busy, because an upgrade can put an older proxy in front of
a newer master or the reverse (DESIGN §4.2).

Regression tests: `a_lost_spawn_race_is_the_busy_token_whatever_its_words` in `src/master.rs` (fails without the
mapping) and `concurrent_starts_produce_one_master` in `tests/master.rs`, which now asserts the error kind.
