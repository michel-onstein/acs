//! Client command line (DESIGN §4.4, §7, §7.1), compatible with `dsh`:
//!
//! ```text
//! acs [ssh options] [user@]<host> [session] [-l|--list] [--new]
//!     [--no-reconnect] [--force] [-v] [-- command...]
//! ```

use std::ffi::OsString;

use crate::config::Config;
use crate::session;
use crate::ssh::Transport;

pub const USAGE: &str = "\
usage: acs [ssh options] [user@]<host> [session]   attach, or create (default session: main)
       acs [ssh options] [user@]<host> --new       create a new numbered session
       acs [ssh options] [user@]<host> --list      list sessions on <host>
       acs config ...                            read and edit the configuration (acs config --help)
       acs upgrade [--version X.Y.Z] [--check]   replace this acs with the latest release

options:
  -l, --list          list sessions on the host
      --new           create a session named with the lowest free number
      --no-reconnect  exit when the connection drops instead of redialling
      --force         take over a session attached by someone else
  -v                  verbose: show ssh commands and remote login noise
      --ssh <path>    ssh program to run (also ACS_SSH)
  -h, --help          this help
  -V, --version       print the version
  -- <command...>     run <command> instead of the login shell (new sessions)

ssh options (passed to every ssh call):
  -i <identity_file>  -p <port>  -J <jump>  -F <config>  -o <option=value>

in a session: Ctrl-] Ctrl-] then  d  detach (session keeps running)
                                  x  exit (ends the session)

environment: ACS_DEFAULT_SESSION ACS_IDENTITY ACS_ESCAPE_KEY ACS_ESCAPE_TIMEOUT_MS
             ACS_SSH ACS_SOCKET_DIR

configuration: /etc/acs/config.yaml, then ~/.config/acs/config.yaml;
               the <host> of [user@]<host> may be an alias defined there (hosts:)";

/// Which session the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Attach to, or create, this session.
    Named(String),
    /// Create a new session with the lowest free number.
    New,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientArgs {
    pub transport: Transport,
    pub target: Target,
    pub list: bool,
    pub reconnect: bool,
    pub force: bool,
    pub verbose: u8,
    pub command: Vec<String>,
    /// The configuration file's settings (DESIGN §7.2); defaults until the
    /// client loads them.
    pub config: Config,
    /// The alias the destination was resolved from, as given (`[user@]<alias>`),
    /// if any (DESIGN §7.3).
    pub alias: Option<String>,
}

impl ClientArgs {
    /// The host as the user named it: the alias, or the destination.
    pub fn host_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.transport.destination)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Run(Box<ClientArgs>),
    Help,
    Version,
}

/// Parse the client's arguments (without the program name).
/// `default_session` is `$ACS_DEFAULT_SESSION`.
pub fn parse<I>(args: I, default_session: Option<&str>) -> Result<Parsed, String>
where
    I: IntoIterator<Item = OsString>,
{
    use lexopt::prelude::*;

    let mut p = lexopt::Parser::from_args(args);
    let mut user_opts: Vec<OsString> = Vec::new();
    let mut positionals: Vec<String> = Vec::new();
    let mut list = false;
    let mut new = false;
    let mut reconnect = true;
    let mut force = false;
    let mut verbose = 0u8;
    let mut command = Vec::new();
    let mut ssh: Option<OsString> = None;
    let mut transport_cmd: Option<String> = None;

    let text = |v: OsString, what: &str| -> Result<String, String> {
        v.into_string()
            .map_err(|_| format!("{what} is not valid UTF-8"))
    };

    loop {
        // `--` ends option parsing; everything after it is the command.
        if p.try_raw_args()
            .is_some_and(|r| r.peek() == Some("--".as_ref()))
        {
            let mut raw = p.raw_args().map_err(|e| e.to_string())?;
            raw.next();
            for a in raw {
                command.push(text(a, "command")?);
            }
            break;
        }
        let arg = p.next().map_err(|e| e.to_string())?;
        let Some(arg) = arg else { break };
        match arg {
            Short(c @ ('i' | 'p' | 'J' | 'F' | 'o')) => {
                let v = p.value().map_err(|e| e.to_string())?;
                user_opts.push(format!("-{c}").into());
                user_opts.push(v);
            }
            Short('l') | Long("list") => list = true,
            Long("new") => new = true,
            Long("no-reconnect") => reconnect = false,
            // dsh's redial flag: reconnecting is the default now.
            Short('r') | Long("reconnect") => {}
            Long("force") => force = true,
            Short('v') | Long("verbose") => verbose = verbose.saturating_add(1),
            Long("ssh") => ssh = Some(p.value().map_err(|e| e.to_string())?),
            Long("transport-cmd") => {
                transport_cmd = Some(text(
                    p.value().map_err(|e| e.to_string())?,
                    "--transport-cmd",
                )?)
            }
            Short('h') | Long("help") => return Ok(Parsed::Help),
            Short('V') | Long("version") => return Ok(Parsed::Version),
            Value(v) => positionals.push(text(v, "argument")?),
            Short(c) => return Err(format!("unknown option -{c}")),
            Long(l) => return Err(format!("unknown option --{l}")),
        }
    }

    let mut positionals = positionals.into_iter();
    let host = positionals
        .next()
        .ok_or_else(|| "need a host (see acs --help)".to_string())?;
    if host.starts_with('-') {
        return Err(format!("bad host '{host}'"));
    }
    let session = positionals.next();
    if let Some(extra) = positionals.next() {
        return Err(format!("too many arguments: {extra}"));
    }

    if list && (session.is_some() || new) {
        return Err(
            "--list takes no session name and no --new (ssh's -l <login> is not supported: use user@host or -o User=<login>)"
                .into(),
        );
    }
    if list && !command.is_empty() {
        return Err("--list takes no command".into());
    }
    if new && session.is_some() {
        return Err("--new picks the session name itself; give either --new or a name".into());
    }

    let target = if new {
        Target::New
    } else {
        let name = match session {
            Some(s) => s,
            None => session::default_name_from(default_session)?,
        };
        session::validate_name(&name)?;
        Target::Named(name)
    };

    let mut transport = Transport::new(host);
    if let Some(s) = ssh {
        transport.ssh = s;
    }
    transport.user_opts = user_opts;
    transport.transport_cmd = transport_cmd;

    Ok(Parsed::Run(Box::new(ClientArgs {
        transport,
        target,
        list,
        reconnect,
        force,
        verbose,
        command,
        config: Config::default(),
        alias: None,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<ClientArgs, String> {
        match parse(args.iter().map(OsString::from), None)? {
            Parsed::Run(a) => Ok(*a),
            other => panic!("{other:?}"),
        }
    }

    fn opts(a: &ClientArgs) -> Vec<String> {
        a.transport
            .user_opts
            .iter()
            .map(|s| s.to_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn dsh_forms() {
        let a = run(&["devbox"]).unwrap();
        assert_eq!(a.transport.destination, "devbox");
        assert_eq!(a.target, Target::Named("main".into()));
        assert!(a.reconnect && !a.list);

        let a = run(&["devbox", "work"]).unwrap();
        assert_eq!(a.target, Target::Named("work".into()));

        // dsh's -r is accepted in any position and ignored.
        for args in [
            &["devbox", "work", "-r"][..],
            &["-r", "devbox", "work"],
            &["devbox", "--reconnect"],
        ] {
            assert!(run(args).unwrap().reconnect);
        }

        let a = run(&["devbox", "--list"]).unwrap();
        assert!(a.list);
        assert!(run(&["-l", "devbox"]).unwrap().list);
    }

    #[test]
    fn ssh_options_pass_through_in_order() {
        let a = run(&[
            "-i",
            "~/.ssh/id_work",
            "-p2222",
            "-o",
            "IdentitiesOnly=yes",
            "-J",
            "jump",
            "-F",
            "cfg",
            "me@box",
        ])
        .unwrap();
        assert_eq!(
            opts(&a),
            [
                "-i",
                "~/.ssh/id_work",
                "-p",
                "2222",
                "-o",
                "IdentitiesOnly=yes",
                "-J",
                "jump",
                "-F",
                "cfg"
            ]
        );
        assert_eq!(a.transport.destination, "me@box");
    }

    #[test]
    fn new_force_verbose_no_reconnect() {
        let a = run(&["h", "--new", "--force", "-vv", "--no-reconnect"]).unwrap();
        assert_eq!(a.target, Target::New);
        assert!(a.force);
        assert_eq!(a.verbose, 2);
        assert!(!a.reconnect);
    }

    #[test]
    fn command_after_double_dash() {
        let a = run(&["h", "top", "--", "htop", "-d", "10", "--list"]).unwrap();
        assert_eq!(a.target, Target::Named("top".into()));
        assert_eq!(a.command, ["htop", "-d", "10", "--list"]);
        assert!(!a.list);
    }

    #[test]
    fn default_session_from_environment() {
        match parse(["h"].iter().map(OsString::from), Some("michel")).unwrap() {
            Parsed::Run(a) => assert_eq!(a.target, Target::Named("michel".into())),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn errors() {
        let cases: &[(&[&str], &str)] = &[
            (&[], "need a host"),
            (&["h", "a", "b"], "too many arguments"),
            (&["h", "bad name"], "bad session name"),
            (&["h", "x", "--list"], "--list takes no session"),
            (&["-l", "bob", "host"], "user@host"),
            (&["h", "--new", "--list"], "--list takes no session"),
            (&["h", "x", "--new"], "--new picks"),
            (&["h", "--list", "--", "ls"], "no command"),
            (&["-q", "h"], "unknown option -q"),
            (&["--bogus", "h"], "unknown option --bogus"),
            (&["-i"], "missing argument"),
        ];
        for (args, want) in cases {
            let e = run(args).unwrap_err();
            assert!(e.contains(want), "{args:?}: {e}");
        }
    }

    #[test]
    fn help_and_version() {
        assert_eq!(
            parse(["-h"].iter().map(OsString::from), None).unwrap(),
            Parsed::Help
        );
        assert_eq!(
            parse(["--version"].iter().map(OsString::from), None).unwrap(),
            Parsed::Version
        );
    }

    #[test]
    fn ssh_program_and_transport_hook() {
        let a = run(&["--ssh", "/opt/ssh", "--transport-cmd", "sh -c", "h"]).unwrap();
        assert_eq!(a.transport.ssh, OsString::from("/opt/ssh"));
        assert_eq!(a.transport.transport_cmd.as_deref(), Some("sh -c"));
    }
}
