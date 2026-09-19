//! The per-connection proxy, `acs _proxy` (DESIGN §3, §4.3): spawned by sshd,
//! it finds or starts the session's master, announces `ACS-READY`, checks the
//! client's HELLO, and then relays bytes both ways until either side closes.
//! With `--list` it reports every session's STATUS instead; with
//! `--kill <name>` it ends that session first (the session menu, DESIGN
//! §4.4).

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::master;
use crate::proto::{self, err, Decoder, Mode, Msg};
use crate::session::{self, SocketDir};
use crate::sys;

const STDIN: RawFd = 0;
const STDOUT: RawFd = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
enum What {
    Session {
        name: Option<String>,
        mode: Mode,
    },
    List,
    /// End this session, then list the rest.
    Kill(String),
}

fn parse_args(args: &[OsString]) -> Result<What, String> {
    let mut name = None;
    let mut mode = Mode::AttachOrCreate;
    let mut new = false;
    let mut list = false;
    let mut it = args.iter().map(|a| a.to_string_lossy().into_owned());
    while let Some(a) = it.next() {
        match a.as_str() {
            "--kill" => {
                let name = it.next().ok_or("--kill needs a session")?;
                session::validate_name(&name)?;
                return Ok(What::Kill(name));
            }
            "--session" => name = Some(it.next().ok_or("--session needs a value")?),
            "--mode" => {
                mode = match it.next().as_deref() {
                    Some("attach") => Mode::Attach,
                    Some("create") => Mode::Create,
                    Some("attach-or-create") => Mode::AttachOrCreate,
                    other => return Err(format!("bad --mode {other:?}")),
                }
            }
            "--new" => new = true,
            "--list" => list = true,
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    if list {
        return Ok(What::List);
    }
    if new {
        return Ok(What::Session {
            name: None,
            mode: Mode::Create,
        });
    }
    let name = name.ok_or("missing --session")?;
    session::validate_name(&name)?;
    Ok(What::Session {
        name: Some(name),
        mode,
    })
}

/// The `--mode` spelling of a mode, for building proxy arguments.
pub fn mode_arg(mode: Mode) -> &'static str {
    match mode {
        Mode::Attach => "attach",
        Mode::Create => "create",
        Mode::AttachOrCreate => "attach-or-create",
    }
}

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("acs: {msg}");
    ExitCode::from(1)
}

/// Entry point of `acs _proxy` (arguments after the role).
pub fn main(args: &[OsString]) -> ExitCode {
    sys::close_inherited(&[]);
    let _ = sys::signals::ignore(libc::SIGPIPE);
    let what = match parse_args(args) {
        Ok(w) => w,
        Err(e) => return fail(e),
    };
    let dir = match SocketDir::open() {
        Ok(d) => d,
        Err(e) => return fail(e),
    };
    crate::prune::on_proxy_start(|| live_versions(&dir));
    match what {
        What::List => list(&dir, None),
        What::Kill(name) => list(&dir, Some(&name)),
        What::Session { name, mode } => match session(&dir, name, mode) {
            Ok(code) => code,
            Err(e) => fail(e),
        },
    }
}

/// Connect to `name`'s master, starting one when `create` allows it.
/// `Ok(None)` means there is no session and we may not create one.
fn connect_or_start(
    dir: &SocketDir,
    name: &str,
    create: bool,
) -> Result<Option<UnixStream>, String> {
    let path = dir.socket_path(name)?;
    let mut tried_spawn = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match UnixStream::connect(&path) {
            Ok(s) => return Ok(Some(s)),
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::ENOENT) | Some(libc::ECONNREFUSED)
                ) => {}
            Err(e) => return Err(format!("connect {}: {e}", path.display())),
        }
        if !create {
            return Ok(None);
        }
        if Instant::now() > deadline {
            return Err(format!("session '{name}' did not come up"));
        }
        if tried_spawn {
            std::thread::sleep(Duration::from_millis(20));
        }
        let exe = crate::sys::self_exe().map_err(|e| format!("current_exe: {e}"))?;
        match master::spawn(&exe, dir.path(), name) {
            Ok(()) => {}
            // Another proxy's master won the race: just connect to it.
            Err(e) if e.to_string().contains("already has a master") => {}
            Err(e) => return Err(format!("cannot start session '{name}': {e}")),
        }
        tried_spawn = true;
    }
}

fn session(dir: &SocketDir, name: Option<String>, mode: Mode) -> Result<ExitCode, String> {
    // `--new`: pick the number and create it under the directory lock.
    let dir_lock = if name.is_none() {
        Some(dir.dir_lock().map_err(|e| format!("lock: {e}"))?)
    } else {
        None
    };
    let name = match name {
        Some(n) => n,
        None => dir.lowest_free_number().map_err(|e| e.to_string())?,
    };
    let master = connect_or_start(dir, &name, mode != Mode::Attach)?;
    drop(dir_lock);

    sys::write_all(STDOUT, proto::ready_line().as_bytes()).map_err(|e| e.to_string())?;

    // Read the client's HELLO before relaying anything.
    let mut dec = Decoder::new();
    let mut buf = vec![0u8; 64 * 1024];
    let hello = loop {
        match dec.next_msg() {
            Ok(Some(m)) => break m,
            Ok(None) => {}
            Err(e) => return Err(format!("bad frame from client: {e}")),
        }
        let n = sys::read(STDIN, &mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(ExitCode::SUCCESS);
        }
        dec.push(&buf[..n]);
    };
    let reply_err = |code: u16, message: String| -> Result<ExitCode, String> {
        let _ = sys::write_all(STDOUT, &Msg::Error { code, message }.to_bytes());
        Ok(ExitCode::from(1))
    };
    let mut hello = match hello {
        Msg::Hello(h) => h,
        other => return reply_err(err::BAD_REQUEST, format!("expected HELLO, got {other:?}")),
    };
    if hello.proto != proto::PROTO_VERSION {
        return reply_err(
            err::PROTO_MISMATCH,
            format!(
                "remote acs speaks protocol {} but the client speaks {}",
                proto::PROTO_VERSION,
                hello.proto
            ),
        );
    }
    let Some(master) = master else {
        return reply_err(err::NO_SESSION, format!("no session '{name}'"));
    };
    // The client does not know a `--new` session's name; it may also be
    // resuming under a name we resolved. Ours is authoritative.
    hello.session = name;
    let mut to_master = Msg::Hello(hello).to_bytes();
    // Bytes that arrived after the HELLO (e.g. early INPUT) follow it.
    while let Ok(Some(m)) = dec.next_msg() {
        m.encode(&mut to_master);
    }
    relay(master, to_master).map_err(|e| e.to_string())?;
    Ok(ExitCode::SUCCESS)
}

/// Splice stdin → master and master → stdout until either side closes.
fn relay(master: UnixStream, to_master: Vec<u8>) -> io::Result<()> {
    relay_fds(master, to_master, STDIN, STDOUT)
}

/// A poll entry, or none (`fd -1`) when there is nothing to wait for: a
/// closed pipe reports POLLHUP whatever `events` asks, which would spin the
/// loop while the other direction is stuck (acs-wza).
fn wait_for(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    sys::pollfd(if events == 0 { -1 } else { fd }, events)
}

fn relay_fds(
    master: UnixStream,
    mut to_master: Vec<u8>,
    input: RawFd,
    output: RawFd,
) -> io::Result<()> {
    master.set_nonblocking(true)?;
    sys::set_nonblocking(input, true)?;
    sys::set_nonblocking(output, true)?;
    let m = master.as_raw_fd();
    let mut to_client: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut stdin_open = true;
    let limit = 1 << 20;
    loop {
        let mut fds = [
            wait_for(
                input,
                if stdin_open && to_master.len() < limit {
                    libc::POLLIN
                } else {
                    0
                },
            ),
            sys::pollfd(m, {
                let mut ev = 0;
                if to_client.len() < limit {
                    ev |= libc::POLLIN;
                }
                if !to_master.is_empty() {
                    ev |= libc::POLLOUT;
                }
                ev
            }),
            wait_for(
                output,
                if to_client.is_empty() {
                    0
                } else {
                    libc::POLLOUT
                },
            ),
        ];
        sys::poll(&mut fds, -1)?;

        if fds[0].revents != 0 {
            match sys::read(input, &mut buf) {
                Ok(0) => stdin_open = false,
                Ok(n) => to_master.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => stdin_open = false,
            }
        }
        if fds[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match sys::read(m, &mut buf) {
                Ok(0) => {
                    // The master is done: deliver what it said, then leave.
                    return drain(output, &to_client);
                }
                Ok(n) => to_client.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => return drain(output, &to_client),
            }
        }
        if flush(m, &mut to_master).is_err() {
            // The master closed after its last words (an EXIT just read):
            // they still go to the client (acs-er6).
            return drain(output, &to_client);
        }
        if flush(output, &mut to_client).is_err() {
            // The client side is gone.
            return Ok(());
        }
        if !stdin_open && to_master.is_empty() {
            // The client's connection ended: closing our end tells the
            // master the client is gone (detached, not killed).
            return Ok(());
        }
    }
}

fn flush(fd: RawFd, buf: &mut Vec<u8>) -> io::Result<()> {
    while !buf.is_empty() {
        match sys::write(fd, buf) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf.drain(..n);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn drain(fd: RawFd, buf: &[u8]) -> io::Result<()> {
    sys::write_all(fd, buf)
}

/// `--list`: one STATUS_REPLY frame per live session; stale sockets are
/// removed. `--kill` (`kill`) ends that session first, and sends an ERROR
/// frame ahead of the list if it could not.
fn list(dir: &SocketDir, kill: Option<&str>) -> ExitCode {
    if sys::write_all(STDOUT, proto::ready_line().as_bytes()).is_err() {
        return ExitCode::from(1);
    }
    if let Some(Err((code, message))) = kill.map(|name| end_session(dir, name)) {
        if sys::write_all(STDOUT, &Msg::Error { code, message }.to_bytes()).is_err() {
            return ExitCode::from(1);
        }
    }
    let names = match dir.sessions() {
        Ok(n) => n,
        Err(e) => return fail(format!("{}: {e}", dir.path().display())),
    };
    for name in names {
        let Ok(path) = dir.socket_path(&name) else {
            continue;
        };
        match status_of(&path) {
            Ok(info) => {
                if sys::write_all(STDOUT, &Msg::StatusReply(info).to_bytes()).is_err() {
                    return ExitCode::from(1);
                }
            }
            Err(e) if refused(&e) => {
                // Nobody listening: a master that died without cleaning up.
                // Remove it only under the create lock, as a starting master
                // does, and only if it is still dead then: a master may have
                // bound a fresh socket there since (acs-ljl).
                if let Ok(Some(_lock)) = dir.try_create_lock(&name) {
                    if UnixStream::connect(&path).is_err_and(|e| refused(&e)) {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
            Err(_) => {}
        }
    }
    ExitCode::SUCCESS
}

/// How long `--kill` waits for the session to end: the master's grace
/// between SIGHUP and SIGKILL, and then some.
const KILL_WAIT: Duration = Duration::from_secs(10);

/// Ask `name`'s master to end its session, as `x` in the session does, and
/// wait until it has: the master holds the connection open until it exits.
/// Only masters of our own uid answer (DESIGN §4.5).
fn end_session(dir: &SocketDir, name: &str) -> Result<(), (u16, String)> {
    use std::io::{Read, Write};
    let path = dir.socket_path(name).map_err(|e| (err::BAD_REQUEST, e))?;
    let mut s = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) if refused(&e) => return Err((err::NO_SESSION, format!("no session '{name}'"))),
        Err(e) => return Err((err::INTERNAL, format!("connect {}: {e}", path.display()))),
    };
    let failed = |e: io::Error| (err::INTERNAL, format!("session '{name}': {e}"));
    s.write_all(&Msg::Kill.to_bytes()).map_err(failed)?;
    s.set_read_timeout(Some(KILL_WAIT)).map_err(failed)?;
    let mut dec = Decoder::new();
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                dec.push(&buf[..n]);
                if let Ok(Some(Msg::Error { code, message })) = dec.next_msg() {
                    return Err((code, message));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => return Ok(()),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err((
                    err::INTERNAL,
                    format!(
                        "session '{name}' did not end within {} s",
                        KILL_WAIT.as_secs()
                    ),
                ))
            }
            Err(e) => return Err(failed(e)),
        }
    }
}

/// Nobody is listening on the socket (or it is gone).
fn refused(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ECONNREFUSED) | Some(libc::ENOENT)
    )
}

/// Versions the running masters report (kept by the pruner).
fn live_versions(dir: &SocketDir) -> std::collections::HashSet<String> {
    dir.sessions()
        .unwrap_or_default()
        .iter()
        .filter_map(|n| dir.socket_path(n).ok())
        .filter_map(|p| status_of(&p).ok())
        .map(|s| s.version)
        .collect()
}

fn status_of(path: &std::path::Path) -> io::Result<proto::StatusInfo> {
    use std::io::{Read, Write};
    let mut s = UnixStream::connect(path)?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    s.write_all(&Msg::Status.to_bytes())?;
    let mut dec = Decoder::new();
    let mut buf = [0u8; 4096];
    loop {
        match dec.next_msg() {
            Ok(Some(Msg::StatusReply(info))) => return Ok(info),
            Ok(Some(_)) => return Err(io::Error::other("unexpected reply")),
            Ok(None) => {}
            Err(e) => return Err(io::Error::other(e.to_string())),
        }
        let n = s.read(&mut buf)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        dec.push(&buf[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<What, String> {
        parse_args(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn arguments() {
        assert_eq!(
            p(&["--session", "main", "--mode", "attach"]),
            Ok(What::Session {
                name: Some("main".into()),
                mode: Mode::Attach
            })
        );
        assert_eq!(
            p(&["--new"]),
            Ok(What::Session {
                name: None,
                mode: Mode::Create
            })
        );
        assert_eq!(p(&["--list"]), Ok(What::List));
        assert_eq!(p(&["--kill", "work"]), Ok(What::Kill("work".into())));
        assert!(p(&["--kill"]).is_err());
        assert!(p(&["--kill", "../x"]).is_err());
        assert!(p(&["--session", "../x"]).is_err());
        assert!(p(&[]).is_err());
        assert!(p(&["--mode", "sideways", "--session", "a"]).is_err());
    }

    /// Regression (acs-er6): when the master says its last words (EXIT) and
    /// closes before our next write to it, the write fails — and what it
    /// said must still reach the client.
    #[test]
    fn last_words_reach_the_client_when_writing_to_the_master_fails() {
        use std::io::{Read, Write};
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let exit = Msg::Exit { status: 0 }.to_bytes();
        theirs.write_all(&exit).unwrap();
        drop(theirs);
        let (input, _keep_open) = sys::pipe().unwrap();
        let (out_r, out_w) = sys::pipe().unwrap();
        // Queued input for the master makes the relay write to it.
        relay_fds(
            ours,
            Msg::Ping(7).to_bytes(),
            input.as_raw_fd(),
            out_w.as_raw_fd(),
        )
        .unwrap();
        drop(out_w);
        let mut got = Vec::new();
        std::fs::File::from(out_r).read_to_end(&mut got).unwrap();
        assert_eq!(got, exit);
    }
}
