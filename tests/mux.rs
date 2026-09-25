//! The ssh master acs owns (acs-9n3, DESIGN §3, §7.1, §12 decision 10).
//!
//! The fake ssh here is a little more than `common::Ssh`: it answers
//! `ssh -O check` and `ssh -O exit` the way a real one would — **an exit
//! takes the channels on that socket down with it**, which is what makes a
//! sibling session observable (acs-n1m) — and it can be told to play a
//! **wedged** master — one whose control socket answers while its
//! connection is dead — which is the failure a shared master adds and the
//! one the fallback exists for. It can also poison one channel's stream
//! without touching the connection under it.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use common::*;

/// A remote command that says when it is up and then stays: nothing here
/// waits on a shell prompt it does not control.
const UP: &[&str] = &["--", "/bin/sh", "-c", "echo MUX-UP; exec sleep 300"];

/// The session whose channel [`Fake::poison_the_channel`] breaks. The name
/// reaches the fake ssh in the remote command (`_proxy --session …`), which
/// is how one channel out of several on a master is singled out.
const POISONED: &str = "poisoned";

/// The master persists long enough that nothing here races it, and the
/// fallback window is short enough that a test waiting it out is quick.
fn env(fake: &Fake) -> Vec<(String, String)> {
    vec![
        ("ACS_CONTROL_DIR".into(), fake.dir().display().to_string()),
        ("ACS_CONTROL_PERSIST".into(), "300".into()),
        ("ACS_CONTROL_FALLBACK_MS".into(), "600".into()),
    ]
}

/// A fake `ssh` for `--ssh` that knows about control sockets.
struct Fake {
    root: acs::testutil::TempDir,
}

impl Fake {
    /// An ssh that runs `remote`'s transport for `dest` and logs every call.
    fn new(dest: &str, remote: &Remote) -> Fake {
        Fake::hosts(&[(dest, remote)])
    }

    /// The same, for several destinations.
    fn hosts(hosts: &[(&str, &Remote)]) -> Fake {
        let f = Fake {
            root: acs::testutil::TempDir::new(),
        };
        std::fs::create_dir_all(f.dir()).unwrap();
        let mut cases = String::new();
        for (dest, remote) in hosts {
            cases.push_str(&format!("    '{dest}') t='{}' ;;\n", remote.transport()));
        }
        let body = format!(
            "#!/bin/sh\n{log}\
             # The master this call names, if it names one at all.\n\
             cp=\n\
             for a in \"$@\"; do case \"$a\" in ControlPath=/*) cp=${{a#ControlPath=}} ;; esac; done\n\
             case \" $* \" in\n\
             *' -O check '*) exit 0 ;;\n\
             *' -O exit '*)\n\
             # A master takes its channels with it. Without that, a session\n\
             # here would survive an `-O exit` that a real one would have\n\
             # dropped, and acs-n1m would have nothing to observe.\n\
             [ -n \"$cp\" ] && : > \"$cp.gone\"\n\
             rm -f '{wedge}'; exit 0 ;;\n\
             esac\n\
             # Dialling with ControlMaster=auto on a socket whose master has\n\
             # gone starts a new one, so this call is the master again.\n\
             [ -n \"$cp\" ] && rm -f \"$cp.gone\"\n\
             # A master whose control socket answers while its connection is\n\
             # dead: the channel opens and nothing ever comes back.\n\
             if [ -f '{wedge}' ] && [ -n \"$cp\" ]; then exec sleep 300; fi\n\
             # On demand, bytes this channel's client cannot read as a frame\n\
             # (acs-n1m): the conversation breaks while the transport, the\n\
             # master and every other channel carry on. Enough of them that\n\
             # wherever the run lands, a frame header is read out of it.\n\
             case \" $* \" in\n\
             *'{marker}'*)\n\
             me=$$\n\
             ( while kill -0 $me 2>/dev/null; do\n\
             if [ -f '{poison}' ]; then\n\
             rm -f '{poison}'\n\
             i=0; while [ $i -lt 64 ]; do printf '\\377'; i=$((i+1)); done\n\
             break\n\
             fi\n\
             sleep 0.05\n\
             done ) & ;;\n\
             esac\n\
             while [ $# -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n\
             d=$2\nshift 2\n\
             t=\n\
             case \"$d\" in\n{cases}esac\n\
             if [ -z \"$t\" ]; then\n\
             echo \"ssh: connect to host $d port 22: Connection refused\" >&2\n\
             exit 255\n\
             fi\n\
             if [ -z \"$cp\" ]; then exec \"$t\" \"$1\"; fi\n\
             # A channel on a master, so it dies when the master does. The\n\
             # transport runs as this script's own child and only that child\n\
             # is ever signalled: a pid recorded and killed later could have\n\
             # been recycled by another target of the suite by then.\n\
             # A background job's stdin is /dev/null unless it is given one\n\
             # (POSIX), and this one is the client's frames.\n\
             exec 9<&0\n\
             \"$t\" \"$1\" <&9 &\n\
             c=$!\n\
             while kill -0 $c 2>/dev/null; do\n\
             if [ -f \"$cp.gone\" ]; then kill -9 $c 2>/dev/null; break; fi\n\
             sleep 0.05\n\
             done\n\
             wait $c\n\
             exit $?\n",
            log = log_call_sh(&f.log()),
            wedge = f.wedge().display(),
            poison = f.poison().display(),
            marker = POISONED,
        );
        std::fs::write(f.path(), body).unwrap();
        std::fs::set_permissions(f.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        f
    }

    fn path(&self) -> PathBuf {
        self.root.path().join("ssh")
    }

    /// Where acs is told to keep its control sockets.
    fn dir(&self) -> PathBuf {
        self.root.path().join("mux")
    }

    fn log(&self) -> PathBuf {
        self.root.path().join("calls")
    }

    fn wedge(&self) -> PathBuf {
        self.root.path().join("wedged")
    }

    fn poison(&self) -> PathBuf {
        self.root.path().join("poison")
    }

    /// From now on, a dial that joins acs's master never answers — until
    /// something runs `ssh -O exit` on it.
    fn wedge_the_master(&self) {
        std::fs::write(self.wedge(), b"").unwrap();
    }

    fn wedged(&self) -> bool {
        self.wedge().exists()
    }

    /// Break the [`POISONED`] session's channel, once: unreadable bytes
    /// into its stream and nothing else touched — not the transport
    /// carrying it, not the master, not another channel on that master.
    fn poison_the_channel(&self) {
        std::fs::write(self.poison(), b"").unwrap();
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    /// The calls that dialled (rather than asked the control socket
    /// something), in order.
    fn dials(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| !c.contains(" -O "))
            .collect()
    }

    /// The master's socket this call named, if it named one.
    /// `ControlPath=none` is the opt-out, not a master.
    fn control_path(call: &str) -> Option<String> {
        call.split(' ')
            .find_map(|a| a.strip_prefix("ControlPath="))
            .filter(|p| *p != "none")
            .map(String::from)
    }

    /// Wait until something has been dialled with `needle` in its
    /// arguments, and answer with that call. The fake writes its line
    /// before it becomes the connection, so this is the event itself and
    /// not a guess at how long one takes (acs-o6x).
    fn wait_dial(&self, needle: &str) -> String {
        let deadline = std::time::Instant::now() + T;
        loop {
            if let Some(c) = self.dials().into_iter().find(|c| c.contains(needle)) {
                return c;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "nothing dialled with {needle:?}: {:?}",
                self.calls()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Every dial so far reached acs's master on this one socket. Counting
    /// calls instead would count a first-contact install too, which is
    /// none of this file's business.
    fn assert_all_on(&self, path: &str) {
        for call in self.dials() {
            assert_eq!(Fake::control_path(&call).as_deref(), Some(path), "{call:?}");
            assert!(call.contains("-o ControlMaster=auto"), "{call:?}");
            assert!(call.contains("-o ControlPersist=300"), "{call:?}");
        }
    }

    /// Make the socket the master would be listening on. The fake ssh is a
    /// shell script and listens on nothing, so the file that tells acs a
    /// master is there is made here.
    fn leave_a_socket(&self, path: &str) {
        std::fs::write(path, b"").unwrap();
    }
}

/// The first connection and a side call both reach the master acs keeps —
/// one socket, ahead of the user's options, in a directory nobody else can
/// open.
#[test]
fn a_side_call_starts_the_master_the_first_connection_then_joins() {
    let remote = Remote::installed();
    let fake = Fake::new("devbox", &remote);
    let env = env(&fake);
    let out = output_of(
        acs_cmd()
            .args(["list", "--ssh", fake.path().to_str().unwrap(), "devbox"])
            .envs(env.iter().map(|(k, v)| (k, v))),
    );
    assert!(out.status.success(), "{out:?}");

    let calls = fake.dials();
    let path = Fake::control_path(&calls[0]).unwrap_or_else(|| panic!("{:?}", calls[0]));
    assert_eq!(PathBuf::from(&path).parent().unwrap(), fake.dir());
    fake.assert_all_on(&path);

    // A control socket grants a shell on the far end (acs-q4f, DESIGN
    // §4.5): nobody but its owner may even look in the directory.
    assert_eq!(
        std::fs::metadata(fake.dir()).unwrap().permissions().mode() & 0o777,
        0o700
    );

    // The attach reaches the same socket, or the handshake the side call
    // paid for would be paid again.
    let ssh = fake.path();
    let mut args = vec!["--ssh", ssh.to_str().unwrap(), "devbox", "same"];
    args.extend(UP);
    let mut c = Client::spawn(&exe(), &args, &refs(&env));
    c.wait_for("MUX-UP", T);
    assert!(fake.dials().len() > calls.len(), "the attach never dialled");
    fake.assert_all_on(&path);
}

/// A redial has its own connection and ends the master the lost link ran
/// on: the master's TCP is the one that just failed, and it would answer
/// the next client with a dead path (DESIGN §3).
#[test]
fn a_redial_dials_its_own_connection_and_ends_the_master() {
    let remote = Remote::installed();
    let fake = Fake::new("devbox", &remote);
    let mut env = env(&fake);
    env.push(("ACS_BACKOFF_MS".into(), "100".into()));
    let ssh = fake.path();
    let mut args = vec!["--ssh", ssh.to_str().unwrap(), "devbox", "drop"];
    args.extend(UP);
    let mut c = Client::spawn(&exe(), &args, &refs(&env));
    c.wait_for("MUX-UP", T);
    fake.leave_a_socket(&Fake::control_path(&fake.dials()[0]).unwrap());

    remote.cut_link();
    c.wait_resumed();

    let dials = fake.dials();
    let last = dials.last().unwrap();
    assert!(last.contains("-o ControlMaster=no"), "{last:?}");
    assert!(last.contains("-o ControlPath=none"), "{last:?}");
    assert!(
        fake.calls().iter().any(|c| c.contains(" -O exit ")),
        "the master the lost link ran on was left up: {:?}",
        fake.calls()
    );
}

/// acs-n1m: the other half of the rule above. A channel that breaks while
/// the connection under it keeps carrying bytes is not the connection
/// failing, and ending the master over it drops every other acs session
/// sharing it — each losing what its user had typed (DESIGN §5.2) for
/// nothing. Two sessions on one master, one channel poisoned: the redial
/// leaves the master alone and the sibling never notices.
#[test]
fn a_channel_that_breaks_leaves_the_master_and_the_sibling_alone() {
    let remote = Remote::installed();
    let fake = Fake::new("devbox", &remote);
    let mut env = env(&fake);
    env.push(("ACS_BACKOFF_MS".into(), "100".into()));
    let ssh = fake.path();

    // The sibling. It starts the master and must never hear about any of
    // what follows.
    let mut args = vec!["--ssh", ssh.to_str().unwrap(), "devbox", "sibling"];
    args.extend(UP);
    let mut sibling = Client::spawn(&exe(), &args, &refs(&env));
    sibling.wait_for("MUX-UP", T);
    let sibling_pid = *remote.transport_pids().last().unwrap();
    let path = Fake::control_path(&fake.dials()[0]).unwrap();

    // The session whose channel breaks, on that same master. `-v` so the
    // decision itself is on the record, not only its effects.
    let mut args = vec!["-v", "--ssh", ssh.to_str().unwrap(), "devbox", POISONED];
    args.extend(UP);
    let mut poisoned = Client::spawn(&exe(), &args, &refs(&env));
    poisoned.wait_session(POISONED);
    poisoned.wait_for("MUX-UP", T);
    fake.assert_all_on(&path);

    // The socket the master would be listening on, so that an `ssh -O exit`
    // this test says must not happen *could* have happened.
    fake.leave_a_socket(&path);
    let base = remote.connections();

    fake.poison_the_channel();
    poisoned.wait_resumed();
    remote.wait_more_connections(base, 1, T);

    // The decision itself, on the record. Read rather than waited for: the
    // resume above is already past it, and `wait_for` only searches
    // forward.
    let said = poisoned.text();
    assert!(said.contains("keeping the shared ssh master"), "{said:?}");
    assert!(
        said.contains("protocol error"),
        "the channel broke some other way: {said:?}"
    );
    // The assertion that does not race: with no `ssh -O exit` at all,
    // nothing could have taken the sibling's connection away. The two
    // below say the same thing from the sibling's side, but a master that
    // *was* ended reaches it through a 50 ms poll in the fake, so they are
    // corroboration rather than the signal.
    assert!(
        !fake.calls().iter().any(|c| c.contains(" -O exit ")),
        "the master was ended over a broken channel: {:?}",
        fake.calls()
    );
    // The redial had its own connection, as every redial does.
    let last = fake.dials().last().unwrap().clone();
    assert!(last.contains("-o ControlPath=none"), "{last:?}");
    // One new connection, and it is the redial's. A second one would be
    // the sibling coming back from a drop it should never have had.
    assert_eq!(
        remote.connections(),
        base + 1,
        "something besides the one redial dialled: {:?}",
        fake.dials()
    );

    // And the sibling is still the connection it was — the fake's channels
    // die when their master does, so this is what fails once the master
    // goes down with a broken channel.
    assert!(
        acs::sys::kill(sibling_pid, 0).is_ok(),
        "the sibling's connection was taken down with the master"
    );
    let text = sibling.text();
    assert!(!text.contains("connection lost"), "{text:?}");
}

/// An alias is resolved again **before** every redial (DESIGN §7.3), so by
/// the time the redial ends the master, the destination may already be
/// another host — whose master is a live connection of somebody else's.
/// The master ended is the one the lost link ran on, not the one the
/// transport now points at.
#[test]
fn the_master_ended_is_the_one_the_lost_link_ran_on() {
    let a = Remote::installed();
    let b = Remote::installed();
    let fake = Fake::hosts(&[("a.lan", &a), ("b.lan", &b)]);
    let net = Net::new(&["a.lan"]);
    let mut env = net.env("aliases:\n  box:\n    - host: a.lan\n    - host: b.lan\n");
    env.extend(self::env(&fake));
    env.push(("ACS_BACKOFF_MS".into(), "100".into()));

    let ssh = fake.path();
    let mut args = vec!["--ssh", ssh.to_str().unwrap(), "box", "moved"];
    args.extend(UP);
    let mut c = Client::spawn(&exe(), &args, &refs(&env));
    c.wait_for("MUX-UP", T);
    let first = fake.dials();
    assert!(first[0].contains("-- a.lan"), "{first:?}");
    let on_a = Fake::control_path(&first[0]).unwrap();
    fake.leave_a_socket(&on_a);

    // a.lan stops answering: the redial resolves the alias to b.lan first.
    // (Not `wait_resumed`: a change of host takes the status line away on
    // its own, before anything is dialled.)
    net.set_up(&["b.lan"]);
    a.cut_link();
    let redial = fake.wait_dial("-- b.lan");
    assert!(redial.contains("-o ControlPath=none"), "{redial:?}");

    let exits: Vec<String> = fake
        .calls()
        .into_iter()
        .filter(|c| c.contains(" -O exit "))
        .collect();
    assert_eq!(exits.len(), 1, "{:?}", fake.calls());
    assert_eq!(
        Fake::control_path(&exits[0]).as_deref(),
        Some(&on_a[..]),
        "b.lan's master was ended instead of a.lan's"
    );
}

/// The failure a shared master adds: its control socket answers while its
/// connection is dead. acs gives it the fallback window, then takes it down
/// and dials a connection of its own — and the session comes up anyway.
#[test]
fn a_master_that_does_not_answer_is_ended_and_the_dial_made_again() {
    let remote = Remote::installed();
    let fake = Fake::new("devbox", &remote);
    let env = env(&fake);
    let ssh = fake.path();

    // A side call, to leave a master behind for the session to find.
    let out = output_of(
        acs_cmd()
            .args(["list", "--ssh", ssh.to_str().unwrap(), "devbox"])
            .envs(env.iter().map(|(k, v)| (k, v))),
    );
    assert!(out.status.success(), "{out:?}");
    fake.leave_a_socket(&Fake::control_path(&fake.dials()[0]).unwrap());

    fake.wedge_the_master();
    let before = fake.dials().len();
    let mut args = vec!["--ssh", ssh.to_str().unwrap(), "devbox", "wedged"];
    args.extend(UP);
    let mut c = Client::spawn(&exe(), &args, &refs(&env));
    // It gets there: the master is ended and the dial made again.
    c.wait_for("MUX-UP", T);
    assert!(
        !fake.wedged(),
        "the wedged master was left up: {:?}",
        fake.calls()
    );
    // Two dials at least for the one connection: the one the master
    // swallowed, and the one that replaced it.
    assert!(fake.dials().len() >= before + 2, "{:?}", fake.dials());
    assert!(
        c.text().contains("did not answer"),
        "nothing was said about it: {:?}",
        c.text()
    );
}

/// acs-6f5 and DESIGN §7.1: a `-L` listener belongs to the session's own
/// ssh and dies with it. A master would open it instead and keep it bound
/// after acs has gone, so a session that forwards anything keeps its own
/// connection — and says so under `-v`.
#[test]
fn a_session_that_forwards_a_port_keeps_its_own_connection() {
    let remote = Remote::installed();
    let fake = Fake::new("devbox", &remote);
    let ssh = fake.path();
    let mut args = vec![
        "-v",
        "-L",
        "45999:localhost:9",
        "--ssh",
        ssh.to_str().unwrap(),
        "devbox",
        "fwd",
    ];
    args.extend(UP);
    let mut c = Client::spawn(&exe(), &args, &refs(&env(&fake)));
    c.wait_for("MUX-UP", T);
    for call in fake.calls() {
        assert!(Fake::control_path(&call).is_none(), "{call:?}");
    }
    assert!(c.text().contains("no shared ssh master"), "{:?}", c.text());
}

/// acs-odd over acs-9n3: a forward that arrives from the **configuration**
/// hits the same carve-out a `-L` does.
///
/// Only one order gets this right — the setting is merged into the
/// transport before `mux::configure` ever looks at it — and there is
/// nothing in the merge itself to say so. The other order joins a master
/// happily, and then the master opens the listener and keeps it bound after
/// acs has exited, which is exactly what DESIGN §7.1 promises never
/// happens. So this is the test for the ordering, not for the veto.
#[test]
fn a_forward_from_the_configuration_keeps_its_own_connection_too() {
    let remote = Remote::installed();
    let fake = Fake::new("devbox.lan", &remote);
    let net = Net::new(&["devbox.lan"]);
    let mut env = net
        .env("local_forwards: [45995:localhost:9]\naliases:\n  devbox:\n    - host: devbox.lan\n");
    env.extend(self::env(&fake));
    let mut args = vec![
        "-v".to_string(),
        "--ssh".into(),
        fake.path().display().to_string(),
        "devbox".into(),
        "fwd".into(),
    ];
    args.extend(UP.iter().map(|s| s.to_string()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut c = Client::spawn(&exe(), &args, &refs(&env));
    c.wait_for("MUX-UP", T);
    // Not one call names a control socket: none was started, and none was
    // joined.
    for call in fake.calls() {
        assert!(Fake::control_path(&call).is_none(), "{call:?}");
    }
    assert!(
        c.text()
            .contains("no shared ssh master: -L is on this session"),
        "{:?}",
        c.text()
    );
    // And the forward really is on the session's ssh: the veto above would
    // also read as "no master" if the setting had simply been dropped.
    let dial = fake.calls().pop().unwrap();
    let opts = dial.split_once(" -- ").expect("a destination after --").0;
    assert!(opts.contains("-L 45995:localhost:9"), "{dial:?}");
}

/// `acs list` asks every alias at once (DESIGN §7.3). Starting a master on
/// each would leave a dozen authenticated connections behind a listing.
#[test]
fn a_listing_over_every_alias_starts_no_master() {
    let remote = Remote::installed();
    let fake = Fake::new("devbox", &remote);
    let net = Net::new(&["devbox"]);
    let mut env = net.env("aliases:\n  one:\n    - host: devbox\n  two:\n    - host: devbox\n");
    env.extend(self::env(&fake));
    let out = output_of(
        acs_cmd()
            .args(["list", "--ssh", fake.path().to_str().unwrap()])
            .envs(env.iter().map(|(k, v)| (k, v))),
    );
    assert!(out.status.success(), "{out:?}");
    let calls = fake.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    for call in calls {
        assert!(call.contains("-o BatchMode=yes"), "{call:?}");
        assert!(Fake::control_path(&call).is_none(), "{call:?}");
    }
}
