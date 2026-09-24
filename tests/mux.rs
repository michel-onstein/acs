//! The ssh master acs owns (acs-9n3, DESIGN §3, §7.1, §12 decision 10).
//!
//! The fake ssh here is a little more than `common::Ssh`: it answers
//! `ssh -O check` and `ssh -O exit` the way a real one would, and it can be
//! told to play a **wedged** master — one whose control socket answers
//! while its connection is dead — which is the failure a shared master
//! adds and the one the fallback exists for.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use common::*;

/// A remote command that says when it is up and then stays: nothing here
/// waits on a shell prompt it does not control.
const UP: &[&str] = &["--", "/bin/sh", "-c", "echo MUX-UP; exec sleep 300"];

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
            cases.push_str(&format!(
                "    '{dest}') exec '{}' \"$1\" ;;\n",
                remote.transport()
            ));
        }
        let body = format!(
            "#!/bin/sh\n{log}\
             case \" $* \" in\n\
             *' -O check '*) exit 0 ;;\n\
             *' -O exit '*) rm -f '{wedge}'; exit 0 ;;\n\
             esac\n\
             # A master whose control socket answers while its connection is\n\
             # dead: the channel opens and nothing ever comes back.\n\
             if [ -f '{wedge}' ]; then\n\
             case \" $* \" in *' ControlPath=/'*) exec sleep 300 ;; esac\n\
             fi\n\
             while [ $# -gt 0 ] && [ \"$1\" != -- ]; do shift; done\n\
             d=$2\nshift 2\n\
             case \"$d\" in\n{cases}esac\n\
             echo \"ssh: connect to host $d port 22: Connection refused\" >&2\n\
             exit 255\n",
            log = log_call_sh(&f.log()),
            wedge = f.wedge().display(),
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

    /// From now on, a dial that joins acs's master never answers — until
    /// something runs `ssh -O exit` on it.
    fn wedge_the_master(&self) {
        std::fs::write(self.wedge(), b"").unwrap();
    }

    fn wedged(&self) -> bool {
        self.wedge().exists()
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
