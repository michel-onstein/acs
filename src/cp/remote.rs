//! `acs _cp`: the remote end of a copy (ACSCP_DESIGN §8).
//!
//! Started by the session's prelude with `_cp` for its argument, so it is
//! always the client's own version, installed as a session's remote is. It
//! prints `ACS-READY` before it reads anything, so the client's `HELLO`
//! may already be in the pipe; takes its paths from that `HELLO`, never
//! from the command line (decision 4), resolving `~` and relative paths
//! against `$HOME` itself; and runs the sender or the receiver — the code
//! the local end runs — on its stdin and stdout. It touches its version
//! directory and never prunes, and never looks at the session directory.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::proto::{err, Hello, Msg, Role};
use super::{drive, receiver, sender, walk, Endpoint, Options};
use crate::outq::OutQueue;
use crate::proto::Decoder;
use crate::sys;

const STDIN: i32 = 0;
const STDOUT: i32 = 1;

/// `path` from a `HELLO` as a path on this host: `~` and `~/…` under
/// `home`, a relative path inside it, the empty path `home` itself.
pub fn expand(home: &Path, path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => home.join(path),
    }
}

/// Why a `HELLO` from another build is refused: the message set may
/// change between releases, since both ends of a copy are one build
/// (ACSCP_DESIGN §5.1), so a mismatch is refused by name rather than
/// misread.
pub fn version_refusal(h: &Hello) -> Option<String> {
    (h.version != crate::VERSION)
        .then(|| format!("acs {} cannot copy with acs {}", crate::VERSION, h.version))
}

/// Read the client's `HELLO` from stdin, and what came after it.
fn read_hello() -> Option<(Hello, Decoder)> {
    let mut dec = Decoder::new();
    let mut buf = [0u8; 4096];
    loop {
        match Msg::next(&mut dec) {
            Ok(Some(Msg::Hello(h))) => return Some((h, dec)),
            Ok(None) => {}
            _ => return None,
        }
        match sys::read(STDIN, &mut buf) {
            Ok(n @ 1..) => dec.push(&buf[..n]),
            _ => return None,
        }
    }
}

/// Answer `h` into `out` and make the endpoint it asks for, or `None`
/// when it is refused (the `ERROR` then queued).
fn endpoint(mut h: Hello, home: &Path, out: &mut OutQueue) -> Option<Box<dyn Endpoint>> {
    if let Some(m) = version_refusal(&h) {
        Msg::Error {
            code: err::VERSION,
            message: m,
        }
        .encode(out.tail());
        return None;
    }
    let mut opts = Options::new(h.recursive, h.preserve);
    // Only one end reports, and it is the one with the user's terminal:
    // what this end skips or cannot write goes back as SKIP and FAIL.
    opts.report = false;
    match h.role {
        Role::Receive => {
            h.target = expand(home, &h.target).to_string_lossy().into_owned();
            let mut r = receiver::Receiver::serving(&opts);
            r.on_msg(Msg::Hello(h), out).ok()?;
            Some(Box::new(r))
        }
        Role::Send => {
            Msg::Welcome {
                umask: receiver::umask(),
                target_is_dir: false,
            }
            .encode(out.tail());
            let mut good = Vec::new();
            for p in h.paths {
                let path = expand(home, &p);
                match walk::check_source(&path, h.recursive) {
                    Ok(name) => good.push((name, path)),
                    Err(reason) => Msg::Skip { name: p, reason }.encode(out.tail()),
                }
            }
            Some(Box::new(sender::Sender::new(good, &opts, true)))
        }
    }
}

/// Entry point of `acs _cp`. Nothing but frames goes to stdout after the
/// marker; a refusal or a broken copy is the client's to tell, from the
/// `ERROR` or the link's end, so stderr stays quiet.
pub fn main() -> ExitCode {
    crate::prune::touch_own();
    if sys::write_all(STDOUT, crate::proto::ready_line().as_bytes()).is_err() {
        return ExitCode::FAILURE;
    }
    let Some((h, dec)) = read_hello() else {
        return ExitCode::FAILURE;
    };
    let home = PathBuf::from(sys::env_nonempty("HOME").unwrap_or_else(|| ".".into()));
    let mut out = OutQueue::default();
    let done = match endpoint(h, &home, &mut out) {
        Some(mut ep) => drive(ep.as_mut(), STDIN, STDOUT, dec, out).is_ok(),
        None => {
            let _ = sys::write_all(STDOUT, out.queued());
            false
        }
    };
    match done {
        true => ExitCode::SUCCESS,
        false => ExitCode::FAILURE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_resolved_against_home() {
        let home = Path::new("/home/me");
        for (p, want) in [
            ("", "/home/me"),
            ("~", "/home/me"),
            ("~/", "/home/me"),
            ("~/a/b", "/home/me/a/b"),
            ("a/b", "/home/me/a/b"),
            ("/etc/hosts", "/etc/hosts"),
            // Not another user's home: a name that begins with a tilde.
            ("~bob", "/home/me/~bob"),
        ] {
            assert_eq!(expand(home, p), Path::new(want), "{p:?}");
        }
    }

    fn hello(version: &str, role: Role) -> Hello {
        Hello {
            version: version.into(),
            role,
            recursive: false,
            preserve: false,
            verbose: false,
            target: String::new(),
            paths: vec!["a".into()],
        }
    }

    /// Another build's `HELLO` is refused by name, before any endpoint is
    /// made, whichever role it asks for.
    #[test]
    fn a_version_mismatch_is_refused_by_name() {
        for role in [Role::Send, Role::Receive] {
            let mut out = OutQueue::default();
            let ep = endpoint(hello("0.0.1", role), Path::new("/nonexistent"), &mut out);
            assert!(ep.is_none());
            let mut d = Decoder::new();
            d.push(out.queued());
            let Ok(Some(Msg::Error { code, message })) = Msg::next(&mut d) else {
                panic!("no ERROR")
            };
            assert_eq!(code, err::VERSION);
            assert!(message.contains("0.0.1") && message.contains(crate::VERSION));
        }
        assert_eq!(version_refusal(&hello(crate::VERSION, Role::Send)), None);
    }
}
