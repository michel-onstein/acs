//! A command run without a terminal (DESIGN §7.7): `acs -T host -- cmd`,
//! or `acs host -- cmd` with stdin or stdout not a terminal.
//!
//! A session is a pty, and a pty is the wrong pipe for data: it turns `\n`
//! into `\r\n`, mixes stderr into stdout, edits what stdin carries and has
//! no end-of-file to pass on. So a command meant for a pipe runs as
//! `ssh -T host command` does — acs execs ssh, over the transport it
//! resolved (alias, identity, options, its shared master), and ssh's own
//! stdin, stdout, stderr and exit status are the command's. What that gives
//! up is the session: nothing to resume after a dropped link.

use std::process::Stdio;

use crate::cli::{ClientArgs, Pty};
use crate::client::{code, note_max};
use crate::ssh::{self, Call};

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
/// forward lives on a session's own connection (DESIGN §7.1), and there is
/// none. A `-L` on the command line is refused rather than dropped.
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

/// The program and arguments that run the command over `args`' transport.
pub fn argv(args: &ClientArgs) -> Vec<std::ffi::OsString> {
    args.conn
        .transport
        .argv(Call::Side, &ssh::command_line(&args.command))
}

/// Run the command: exec ssh, so its exit status and signals are the
/// command's, as `ssh host command` gives them. Returns only when ssh
/// could not be started.
pub fn run(args: &ClientArgs) -> u8 {
    use std::os::unix::process::CommandExt;
    if let Err(c) = knock(args) {
        return c;
    }
    let argv = argv(args);
    if args.conn.verbose > 0 {
        note_max(
            &format!("running {}", ssh::display_argv(&argv)),
            16 * crate::safe::MAX_FIELD,
        );
    }
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if args.null_stdin {
        cmd.stdin(Stdio::null());
    }
    let e = cmd.exec();
    eprintln!("acs: cannot run {}: {e}", argv[0].to_string_lossy());
    code::UNREACHABLE
}

/// Knock before ssh is exec'd (KNOCK_DESIGN §3.1), unless acs's master is
/// up for ssh to join. `Err` is the exit code, the reason told: the error
/// code for a knock that cannot work as configured, else unreachable.
fn knock(args: &ClientArgs) -> Result<(), u8> {
    let tr = &args.conn.transport;
    let joining = tr.knock.is_some() && crate::mux::joinable(tr, Call::Side);
    if !crate::client::dial_knocks(tr, joining) {
        return Ok(());
    }
    let tr = tr.for_dial(Call::Side, true);
    let e = match crate::client::knock_before(&args.conn, &tr) {
        Ok(_) => return Ok(()),
        Err(e) => e,
    };
    crate::client::note(&e.to_string());
    Err(match crate::client::knock_refusal(&e) {
        Some(crate::knock::Refusal::Config(_)) => code::ERROR,
        _ => code::UNREACHABLE,
    })
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

    /// The command line goes to ssh as one string, its words joined with
    /// spaces, after `--` and the destination — `ssh -T host 'a b'`.
    #[test]
    fn the_command_goes_to_ssh_as_ssh_would_send_it() {
        let a = args(&["-p", "2222", "h", "--", "ls", "-l", "*.log", "|", "wc"]);
        let v: Vec<String> = argv(&a)
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect();
        assert!(v.contains(&"-T".to_string()), "{v:?}");
        let n = v.len();
        assert_eq!(&v[n - 3..], ["--", "h", "ls -l *.log | wc"]);
        assert!(v.windows(2).any(|w| w == ["-p", "2222"]), "{v:?}");
    }

    #[test]
    fn a_forward_is_refused_not_dropped() {
        let mut a = args(&["-T", "-L", "8080:localhost:80", "h", "--", "x"]);
        assert!(prepare(&mut a).unwrap_err().contains("-L"));
    }
}
