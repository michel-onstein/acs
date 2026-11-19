//! A command run without a terminal (DESIGN §7.7, §5.5): `acs -T host --
//! cmd`, or `acs host -- cmd` with stdin or stdout not a terminal.
//!
//! A pty is the wrong pipe for data: it turns `\n` into `\r\n`, mixes
//! stderr into stdout, edits what stdin carries and has no end-of-file to
//! pass on. So such a command runs in a session of its own on **pipes**:
//! the master gives it a stdin, a stdout and a stderr, carries the two
//! outputs in its ring as records, and closes its stdin where the client's
//! ended — and a dropped link resumes as a session's does, byte for byte.

use crate::cli::{ClientArgs, Pty};

/// Whether the command runs without a terminal: `-T`, `-n`, or a command
/// of its own (no session named) with stdin or stdout not a terminal — the
/// case in which ssh, given a command, asks for no pty either.
pub fn without_terminal(args: &ClientArgs, stdin_tty: bool, stdout_tty: bool) -> bool {
    if args.command.is_empty() || args.list {
        return false;
    }
    match args.pty {
        Pty::Never => true,
        Pty::Force => false,
        Pty::Auto if args.null_stdin => true,
        // A named session is a session, terminal or not: its own refusal
        // ("stdin is not a terminal") says so.
        Pty::Auto => args.one_shot && !(stdin_tty && stdout_tty),
    }
}

/// Make the transport ready for a run without a terminal, in place of
/// `client::prepare_session`: the shared master, but no forwards — a
/// forward is a terminal session's (DESIGN §7.7). A `-L` on the command
/// line is refused rather than dropped.
pub fn prepare(args: &mut ClientArgs) -> Result<(), String> {
    if !args.conn.transport.local_forwards.is_empty() {
        return Err(
            "-L forwards a session's own connection, and a command without a terminal has none: drop -L, or give -t"
                .into(),
        );
    }
    let decided = crate::mux::configure(&mut args.conn.transport);
    if args.conn.verbose > 0 {
        crate::client::note(&decided);
    }
    Ok(())
}

/// The client's side of a command on pipes (DESIGN §5.5): the stream
/// records of its output split back out to stdout and stderr, and where
/// its input stands.
#[derive(Default)]
pub struct Pipe {
    /// stdin has ended (or `-n`): every link, once WELCOMEd, sends the
    /// empty INPUT that says so.
    pub eof: bool,
    /// Asked to end the command (a signal): every link sends KILL.
    pub kill: bool,
    /// The output offset last acknowledged to the master.
    pub acked: u64,
    /// The record being written: its descriptor and the bytes still to
    /// come; `head`, a record header cut off by the end of a DATA frame.
    fd: i32,
    left: usize,
    head: Vec<u8>,
}

impl Pipe {
    /// `eof`: stdin is `/dev/null` (`-n`), ended before it starts.
    pub fn new(eof: bool) -> Pipe {
        Pipe {
            eof,
            ..Pipe::default()
        }
    }

    /// Write a DATA frame's bytes, record by record, each record's bytes
    /// with `put` to its descriptor. A record header that is not one is
    /// refused (`InvalidData`): nothing but stdout and stderr is written.
    pub fn write(
        &mut self,
        mut b: &[u8],
        mut put: impl FnMut(i32, &[u8]) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        while let [first, rest @ ..] = b {
            if self.left == 0 {
                self.head.push(*first);
                b = rest;
                if let [fd, hi, lo] = self.head[..] {
                    if !(1..=2).contains(&fd) {
                        return Err(std::io::ErrorKind::InvalidData.into());
                    }
                    self.fd = fd as i32;
                    self.left = u16::from_be_bytes([hi, lo]) as usize;
                    self.head.clear();
                }
                continue;
            }
            let n = self.left.min(b.len());
            put(self.fd, &b[..n])?;
            self.left -= n;
            b = &b[n..];
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{parse, Parsed};

    fn args(a: &[&str]) -> ClientArgs {
        match parse(a.iter().map(std::ffi::OsString::from), None).unwrap() {
            Parsed::Run(a) => *a,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_command_of_its_own_into_a_pipe_runs_without_a_terminal() {
        let a = args(&["h", "--", "cat"]);
        assert!(!without_terminal(&a, true, true), "a terminal: a session");
        assert!(without_terminal(&a, false, true), "stdin a pipe");
        assert!(without_terminal(&a, true, false), "stdout a pipe");
        // -t keeps the session; -T and -n drop it even on a terminal.
        assert!(!without_terminal(
            &args(&["-t", "h", "--", "x"]),
            true,
            false
        ));
        assert!(without_terminal(&args(&["-T", "h", "--", "x"]), true, true));
        assert!(without_terminal(&args(&["-n", "h", "--", "x"]), true, true));
    }

    #[test]
    fn a_session_is_never_run_without_a_terminal() {
        assert!(!without_terminal(&args(&["h"]), false, false));
        assert!(!without_terminal(&args(&["h", "s"]), false, false));
        assert!(!without_terminal(
            &args(&["h", "s", "--", "x"]),
            false,
            false
        ));
    }

    /// acs-182f: records split back out whatever the DATA frames' cuts —
    /// a header cut in two, an empty record, a record across frames — and
    /// a tag that names neither stream refused.
    #[test]
    fn records_go_to_their_stream_however_the_frames_cut_them() {
        let mut ring = vec![1, 0, 5];
        ring.extend_from_slice(b"hello");
        ring.extend_from_slice(&[2, 0, 0, 2, 0, 3]);
        ring.extend_from_slice(b"err");
        ring.extend_from_slice(&[1, 0, 1, b'!']);
        for cut in 0..=ring.len() {
            for cut2 in cut..=ring.len() {
                let mut p = Pipe::new(false);
                let mut got: Vec<(i32, Vec<u8>)> = Vec::new();
                let mut put = |fd, b: &[u8]| {
                    match got.last_mut() {
                        Some((f, v)) if *f == fd => v.extend_from_slice(b),
                        _ => got.push((fd, b.to_vec())),
                    }
                    Ok(())
                };
                for part in [&ring[..cut], &ring[cut..cut2], &ring[cut2..]] {
                    p.write(part, &mut put).unwrap();
                }
                let want = [
                    (1, b"hello".to_vec()),
                    (2, b"err".to_vec()),
                    (1, b"!".to_vec()),
                ];
                assert_eq!(got, want, "cut at {cut} and {cut2}");
            }
        }
        let mut p = Pipe::new(false);
        let e = p.write(&[0, 0, 1, b'x'], |_, _| Ok(())).unwrap_err();
        assert_eq!(
            e.kind(),
            std::io::ErrorKind::InvalidData,
            "fd 0 is no stream"
        );
        let e = Pipe::new(false)
            .write(&[3, 0, 0], |_, _| Ok(()))
            .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_forward_is_refused_not_dropped() {
        let mut a = args(&["-T", "-L", "8080:localhost:80", "h", "--", "x"]);
        assert!(prepare(&mut a).unwrap_err().contains("-L"));
    }
}
