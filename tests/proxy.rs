//! The proxy, `acs _proxy` (DESIGN §3, §4.3), driven through its stdio as
//! sshd would.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use acs::proto::{err, Marker, Mode, Msg, PROTO_VERSION};
use acs::testutil::{hello, FrameConn, TempDir};

/// As `common::T`, which this file cannot reach: generous, because the
/// suite runs its targets in parallel (acs-kip).
const T: Duration = Duration::from_secs(30);

struct Proxy {
    conn: FrameConn,
    child: Child,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn proxy(dir: &Path, args: &[&str]) -> Proxy {
    let mut child = Command::new(env!("CARGO_BIN_EXE_acs"))
        .arg("_proxy")
        .args(args)
        .env("ACS_SOCKET_DIR", dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut conn = FrameConn::from_io(child.stdout.take().unwrap(), child.stdin.take().unwrap());
    match conn.expect_marker(T) {
        Some(Marker::Ready { proto, .. }) => assert_eq!(proto, PROTO_VERSION),
        other => panic!("expected ACS-READY, got {other:?}"),
    }
    Proxy { conn, child }
}

fn session(dir: &Path, name: &str) -> Proxy {
    proxy(dir, &["--session", name, "--mode", "attach-or-create"])
}

fn hello_cmd(name: &str, identity: &str, cmd: &str) -> Msg {
    let mut h = hello(name, Mode::AttachOrCreate, identity);
    h.command = vec!["/bin/sh".into(), "-c".into(), cmd.into()];
    Msg::Hello(h)
}

fn welcome(p: &mut Proxy) -> acs::proto::Welcome {
    match p.conn.recv_control(T) {
        Some(Msg::Welcome(w)) => w,
        other => panic!("expected WELCOME, got {other:?}"),
    }
}

/// Regression (acs-wza): after the client's side closes while its input is
/// still backed up towards a master that is not reading, the proxy waits
/// instead of spinning on the closed pipe.
#[test]
fn a_closed_client_with_input_backed_up_does_not_spin_the_proxy() {
    let t = TempDir::new();
    let mut p = session(t.path(), "bp");
    // The program never reads its input. Raw mode: a canonical-mode tty
    // discards input past a full line instead of pushing back.
    p.conn
        .send(&hello_cmd("bp", "me", "stty raw -echo; sleep 30"));
    let w = welcome(&mut p);
    std::thread::sleep(Duration::from_millis(200));
    let chunk = vec![b'x'; acs::proto::MAX_CHUNK];
    let mut seq = w.input_seq;
    // More than the master takes in (1 MiB) plus what the sockets hold, but
    // less than the proxy then buffers itself: it ends up holding input.
    for _ in 0..(1_600_000 / chunk.len()) {
        p.conn.send(&Msg::Input {
            seq,
            bytes: chunk.clone(),
        });
        seq += chunk.len() as u64;
    }
    std::thread::sleep(Duration::from_millis(300));
    p.conn.close_write();
    std::thread::sleep(Duration::from_millis(200));
    let burnt = acs::testutil::cpu_over(p.child.id(), Duration::from_secs(1));
    assert!(
        burnt < Duration::from_millis(300),
        "proxy used {burnt:?} of CPU in 1 s while waiting"
    );
}

/// Regression (acs-ljl): `--list` does not remove a socket while a master
/// start holds the session's create lock (it may just have bound it).
#[test]
fn list_leaves_a_socket_alone_while_a_master_starts() {
    let t = TempDir::new();
    let stale = t.path().join("st.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    let lock = acs::sys::Flock::lock(&t.path().join("st.lock")).unwrap();
    let mut p = proxy(t.path(), &["--list"]);
    assert!(p.conn.closed(T));
    assert!(p.child.wait().unwrap().success());
    assert!(stale.exists(), "removed under a master start's lock");

    drop(lock);
    let mut p = proxy(t.path(), &["--list"]);
    assert!(p.conn.closed(T));
    assert!(p.child.wait().unwrap().success());
    assert!(!stale.exists(), "a stale socket is still cleaned up");
}

#[test]
fn creates_a_session_and_relays_both_ways() {
    let t = TempDir::new();
    let mut p = session(t.path(), "s");
    p.conn
        .send(&hello_cmd("s", "me", "read l; echo got:$l; exit 4"));
    let w = welcome(&mut p);
    assert!(w.created);
    p.conn.send(&Msg::Input {
        seq: w.input_seq,
        bytes: b"hi\r".to_vec(),
    });
    p.conn.wait_output("got:hi", T);
    match p.conn.recv_control(T) {
        Some(Msg::Exit { status }) => assert_eq!(acs::sys::exit_code(status), 4),
        other => panic!("{other:?}"),
    }
    // The master closed: the proxy follows.
    assert!(p.conn.closed(T));
    assert!(p.child.wait().unwrap().success());
}

/// Regression (acs-4w8): a frame that had only half arrived when the HELLO
/// was decoded is carried over to the relay. Its remainder used to reach the
/// master headless, desyncing the master's decoder.
#[test]
fn a_frame_split_across_the_hello_hand_off_survives() {
    let t = TempDir::new();
    let mut p = session(t.path(), "sp");
    let mut first = hello_cmd("sp", "me", "read l; echo got:$l; exit 4").to_bytes();
    let input = Msg::Input {
        seq: 0,
        bytes: b"hi\r".to_vec(),
    }
    .to_bytes();
    let cut = input.len() - 2;
    first.extend_from_slice(&input[..cut]);
    p.conn.send_raw(&first);
    let w = welcome(&mut p);
    assert!(w.created);
    assert_eq!(w.input_seq, 0);
    // The rest of the frame, after the proxy handed over to the relay.
    p.conn.send_raw(&input[cut..]);
    p.conn.wait_output("got:hi", T);
    match p.conn.recv_control(T) {
        Some(Msg::Exit { status }) => assert_eq!(acs::sys::exit_code(status), 4),
        other => panic!("{other:?}"),
    }
}

#[test]
fn stale_socket_is_replaced() {
    let t = TempDir::new();
    let path = t.path().join("st.sock");
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    assert!(path.exists(), "a socket file with nobody listening");
    let mut p = session(t.path(), "st");
    p.conn.send(&hello_cmd("st", "me", "echo alive; sleep 30"));
    assert!(welcome(&mut p).created);
    p.conn.wait_output("alive", T);
}

#[test]
fn racing_proxies_converge_on_one_master() {
    let t = TempDir::new();
    let dir = t.path().to_path_buf();
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let dir = dir.clone();
            std::thread::spawn(move || {
                let mut p = session(&dir, "race");
                p.conn
                    .send(&hello_cmd("race", &format!("me{i}"), "sleep 30"));
                // Different identities: later ones may get BUSY; retry forced.
                let w = match p.conn.recv_control(T) {
                    Some(Msg::Welcome(w)) => w,
                    Some(Msg::Busy { .. }) => {
                        let mut h = hello("race", Mode::AttachOrCreate, &format!("me{i}"));
                        h.force = true;
                        p.conn.send(&Msg::Hello(h));
                        welcome(&mut p)
                    }
                    other => panic!("{other:?}"),
                };
                (w, p)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let instances: std::collections::HashSet<u64> =
        results.iter().map(|(w, _)| w.instance).collect();
    assert_eq!(instances.len(), 1, "more than one master");
    assert_eq!(results.iter().filter(|(w, _)| w.created).count(), 1);
}

#[test]
fn mismatched_protocol_is_refused_by_the_proxy() {
    let t = TempDir::new();
    let mut p = session(t.path(), "pm");
    let mut h = hello("pm", Mode::AttachOrCreate, "me");
    h.proto = PROTO_VERSION + 1;
    p.conn.send(&Msg::Hello(h));
    match p.conn.recv_control(T) {
        Some(Msg::Error { code, .. }) => assert_eq!(code, err::PROTO_MISMATCH),
        other => panic!("{other:?}"),
    }
}

#[test]
fn attaching_to_a_missing_session_is_an_error() {
    let t = TempDir::new();
    let mut p = proxy(t.path(), &["--session", "nope", "--mode", "attach"]);
    p.conn.send(&Msg::Hello(hello("nope", Mode::Attach, "me")));
    match p.conn.recv_control(T) {
        Some(Msg::Error { code, message }) => {
            assert_eq!(code, err::NO_SESSION);
            assert!(message.contains("nope"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!t.path().join("nope.sock").exists());
}

#[test]
fn new_sessions_get_increasing_numbers() {
    let t = TempDir::new();
    let mut names = Vec::new();
    let mut keep = Vec::new();
    for _ in 0..3 {
        let mut p = proxy(t.path(), &["--new"]);
        // The client does not know the name; the proxy fills it in.
        let mut h = hello("", Mode::Create, "me");
        h.command = vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()];
        p.conn.send(&Msg::Hello(h));
        let w = welcome(&mut p);
        assert!(w.created);
        names.push(w.session.clone());
        keep.push(p);
    }
    assert_eq!(names, ["1", "2", "3"]);
}

#[test]
fn a_dropped_client_leaves_the_session_detached() {
    let t = TempDir::new();
    let mut p = session(t.path(), "dd");
    p.conn
        .send(&hello_cmd("dd", "alice@one", "echo up; sleep 30"));
    welcome(&mut p);
    p.conn.wait_output("up", T);
    // The ssh connection ends: the proxy's stdin closes.
    p.conn.close_write();
    assert!(p.conn.closed(T));
    // Someone else can attach without a BUSY: nobody is attached any more.
    let mut q = session(t.path(), "dd");
    q.conn
        .send(&Msg::Hello(hello("dd", Mode::Attach, "bob@two")));
    let w = welcome(&mut q);
    assert!(!w.created);
}

/// One list from `--pick`: every frame up to and without its LIST_END.
fn pick_list(p: &mut Proxy) -> Vec<Msg> {
    let mut got = Vec::new();
    loop {
        match p.conn.recv(T) {
            Some(Msg::ListEnd) => return got,
            Some(m) => got.push(m),
            None => panic!("the list ended without LIST_END: {got:?}"),
        }
    }
}

fn names(list: &[Msg]) -> Vec<String> {
    list.iter()
        .map(|m| match m {
            Msg::StatusReply(s) => s.name.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

/// `--pick` (acs-68z): the session menu and the attach on one connection —
/// the list at once, a session ended on request with the list again, and
/// then the HELLO attaches like any session call.
#[test]
fn pick_lists_ends_on_request_and_attaches_what_the_hello_names() {
    let t = TempDir::new();
    let mut a = session(t.path(), "a");
    a.conn.send(&hello_cmd("a", "me", "sleep 30"));
    welcome(&mut a);
    let mut b = session(t.path(), "b");
    b.conn
        .send(&hello_cmd("b", "me", "read l; echo got:$l; sleep 30"));
    welcome(&mut b);
    b.conn.close_write();
    assert!(b.conn.closed(T));

    let mut p = proxy(t.path(), &["--pick"]);
    let mut first = names(&pick_list(&mut p));
    first.sort();
    assert_eq!(first, ["a", "b"]);
    p.conn.send(&Msg::EndSession {
        name: "a".into(),
        identity: "me".into(),
        force: false,
    });
    assert_eq!(names(&pick_list(&mut p)), ["b"]);
    assert!(matches!(a.conn.recv_control(T), Some(Msg::Exit { .. })));
    assert!(!t.path().join("a.sock").exists());
    // Ending one that is not there says so, and lists again.
    p.conn.send(&Msg::EndSession {
        name: "a".into(),
        identity: "me".into(),
        force: false,
    });
    let again = pick_list(&mut p);
    assert!(
        matches!(&again[..], [Msg::Error { code, message }, Msg::StatusReply(s)]
            if *code == err::NO_SESSION && message == "no session 'a'" && s.name == "b"),
        "{again:?}"
    );

    p.conn
        .send(&Msg::Hello(hello("b", Mode::AttachOrCreate, "me")));
    let w = welcome(&mut p);
    assert_eq!(w.session, "b");
    assert!(!w.created);
    p.conn.send(&Msg::Input {
        seq: w.input_seq,
        bytes: b"hi\r".to_vec(),
    });
    p.conn.wait_output("got:hi", T);
}

#[test]
fn pick_creates_a_session_by_name_or_a_new_numbered_one() {
    let t = TempDir::new();
    let mut p = proxy(t.path(), &["--pick"]);
    assert!(pick_list(&mut p).is_empty());
    p.conn.send(&hello_cmd("main", "me", "sleep 30"));
    let w = welcome(&mut p);
    assert_eq!((w.session.as_str(), w.created), ("main", true));

    // An empty name with Create: the lowest free number, as --new.
    let mut q = proxy(t.path(), &["--pick"]);
    assert_eq!(names(&pick_list(&mut q)), ["main"]);
    let mut h = hello("", Mode::Create, "me");
    h.command = vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()];
    q.conn.send(&Msg::Hello(h));
    let w = welcome(&mut q);
    assert_eq!((w.session.as_str(), w.created), ("1", true));
}

#[test]
fn pick_refuses_what_is_not_a_menu_request() {
    let t = TempDir::new();
    for (sent, why) in [
        (
            Msg::Hello(hello("", Mode::AttachOrCreate, "me")),
            "HELLO names no session",
        ),
        (
            Msg::Hello(hello("../x", Mode::AttachOrCreate, "me")),
            "../x",
        ),
        (Msg::Detach, "expected HELLO or END_SESSION"),
    ] {
        let mut p = proxy(t.path(), &["--pick"]);
        assert!(pick_list(&mut p).is_empty());
        p.conn.send(&sent);
        match p.conn.recv_control(T) {
            Some(Msg::Error { code, message }) => {
                assert_eq!(code, err::BAD_REQUEST, "{message}");
                assert!(message.contains(why), "{message}");
            }
            other => panic!("{sent:?}: {other:?}"),
        }
        assert!(p.conn.closed(T));
    }
    assert!(!t.path().join("x.sock").exists());
}

#[test]
fn leaving_the_pick_closes_the_connection_and_starts_nothing() {
    let t = TempDir::new();
    let mut p = proxy(t.path(), &["--pick"]);
    assert!(pick_list(&mut p).is_empty());
    p.conn.close_write();
    assert!(p.conn.closed(T));
    assert!(p.child.wait().unwrap().success());
    let left: Vec<_> = std::fs::read_dir(t.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "sock"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}
