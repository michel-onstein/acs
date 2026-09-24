//! Client command line (DESIGN §4.4, §7, §7.1), compatible with `dsh`:
//!
//! ```text
//! acs [ssh options] [user@]<host> [session] [--new]
//!     [--no-reconnect | --persist] [--force] [-v] [-- command...]
//! acs list [ssh options] [-v] [[user@]<host>]
//! ```
//!
//! `list` is a reserved first argument, as `config` and `upgrade` are
//! (DESIGN §7.4): a host called `list` is reached as `user@list` or with an
//! option before it. Listing is a command, not an option: `-l`/`--list` are
//! refused with a pointer to `acs list`.

use std::ffi::OsString;

use crate::config::Config;
use crate::session;
use crate::ssh::Transport;

pub const USAGE: &str = "\
usage: acs [ssh options] [user@]<host>             pick a detached session from a menu, or create one
       acs [ssh options] [user@]<host> <session>   attach, or create
       acs [ssh options] [user@]<host> --new       create a new numbered session
       acs list [ssh options] [[user@]<host>]      list sessions on <host>, or on every host alias
       acs config ...                            read and edit the configuration (acs config --help)
       acs upgrade [--version X.Y.Z] [--check]   replace this acs with the latest release

options:
      --new           create a session named with the lowest free number
      --no-reconnect  exit when the connection drops instead of redialling
      --persist       never give up on a lost host: ping it every reachability_interval
                      (default 5s) and dial as soon as it answers, from the first connect on
      --force         take over a session attached by someone else
  -v                  verbose: show ssh commands, remote login noise, connect timings
                      and what each network change the kernel reports was worth
      --ssh <path>    ssh program to run (also ACS_SSH)
  -h, --help          this help
  -V, --version       print the version
  -- <command...>     run <command> instead of the login shell (new sessions)

ssh options:
  -i <identity_file>  -p <port>  -J <jump>  -F <config>  -o <option=value>
                      passed to every ssh call acs makes
  -L [bind:]port:host:hostport
                      forward a local port, as ssh -L does; repeatable,
                      and on the session's own connection alone

in a session: Ctrl-] Ctrl-] then  d  detach (session keeps running)
                                  x  exit (ends the session)
              a bell says it waits for the key (off: command_bell: false)

environment: ACS_DEFAULT_SESSION ACS_IDENTITY ACS_ESCAPE_KEY ACS_ESCAPE_TIMEOUT_MS
             ACS_COMMAND_BELL ACS_PERSIST ACS_SSH ACS_SOCKET_DIR

configuration: /etc/acs/config.yaml, then ~/.config/acs/config.yaml;
               the <host> of [user@]<host> may be an alias defined there (aliases:)";

/// Which session the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Attach to, or create, this session.
    Named(String),
    /// Create a new session with the lowest free number.
    New,
    /// No session named: pick a detached one from a menu, or create one
    /// (DESIGN §4.4). The name is `$ACS_DEFAULT_SESSION` or `main`, which
    /// a new session gets if it is free, and which is attached or created
    /// without a terminal for the menu.
    Pick(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientArgs {
    pub transport: Transport,
    pub target: Target,
    pub list: bool,
    pub reconnect: bool,
    /// `--persist`: keep waiting for a lost host (DESIGN §5.3), whatever the
    /// configuration says.
    pub persist: bool,
    pub force: bool,
    pub verbose: u8,
    pub command: Vec<String>,
    /// The configuration file's settings (DESIGN §7.2); defaults until the
    /// client loads them.
    pub config: Config,
    /// The alias the destination was resolved from, as given (`[user@]<alias>`),
    /// if any (DESIGN §7.3).
    pub alias: Option<String>,
    /// The index of the alias's entry in use, once one was chosen.
    pub entry: Option<usize>,
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
    /// `acs list` without a host: every alias in the configuration (DESIGN
    /// §7.3). The transport has no destination; each alias supplies one.
    ListAll(Box<ClientArgs>),
    Help,
    Version,
}

/// Parse the client's arguments (without the program name).
/// `default_session` is `$ACS_DEFAULT_SESSION`.
pub fn parse<I>(args: I, default_session: Option<&str>) -> Result<Parsed, String>
where
    I: IntoIterator<Item = OsString>,
{
    parse_as(args, default_session, false)
}

/// Parse `acs list`'s arguments (after `list`): the ssh options, `-v`, and
/// at most a host; `list` is set.
pub fn parse_list<I>(args: I) -> Result<Parsed, String>
where
    I: IntoIterator<Item = OsString>,
{
    parse_as(args, None, true)
}

fn parse_as<I>(args: I, default_session: Option<&str>, list: bool) -> Result<Parsed, String>
where
    I: IntoIterator<Item = OsString>,
{
    use lexopt::prelude::*;

    let mut p = lexopt::Parser::from_args(args);
    let mut user_opts: Vec<OsString> = Vec::new();
    let mut local_forwards: Vec<String> = Vec::new();
    let mut positionals: Vec<String> = Vec::new();
    let mut new = false;
    let mut reconnect = true;
    let mut persist = false;
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
            // Not a user_opt: a forward goes to the session's ssh alone
            // (DESIGN §7.1), and its spec is checked here rather than by
            // ssh, per dial, on the session's terminal.
            Short('L') => {
                let spec = text(p.value().map_err(|e| e.to_string())?, "-L")?;
                crate::ssh::check_local_forward(&spec)?;
                local_forwards.push(spec);
            }
            // Listing is the `acs list` command; ssh's -l <login> was never
            // taken.
            Short('l') | Long("list") => {
                return Err(
                    "no option --list: list sessions with acs list [<host>] (and for a login name, use user@host or -o User=<login>)"
                        .into(),
                )
            }
            Long("new") => new = true,
            Long("no-reconnect") => reconnect = false,
            Long("persist") => persist = true,
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
    // Only `acs list` goes without a host: it then lists every alias.
    let host = positionals.next();
    match &host {
        None if !list => return Err("need a host (see acs --help)".into()),
        Some(h) if h.starts_with('-') => return Err(format!("bad host '{h}'")),
        _ => {}
    }
    let session = positionals.next();
    if let Some(extra) = positionals.next() {
        return Err(format!("too many arguments: {extra}"));
    }

    if list && (session.is_some() || new) {
        return Err("acs list takes at most a host: no session name and no --new".into());
    }
    if list && !command.is_empty() {
        return Err("acs list takes no command".into());
    }
    if persist && !reconnect {
        return Err("--persist keeps reconnecting and --no-reconnect never does: give one".into());
    }
    if new && session.is_some() {
        return Err("--new picks the session name itself; give either --new or a name".into());
    }

    let target = match session {
        _ if new => Target::New,
        Some(name) => {
            session::validate_name(&name)?;
            Target::Named(name)
        }
        None => Target::Pick(session::default_name_from(default_session)?),
    };

    let mut transport = Transport::new(host.clone().unwrap_or_default());
    if let Some(s) = ssh {
        transport.ssh = s;
    }
    transport.user_opts = user_opts;
    transport.local_forwards = local_forwards;
    transport.transport_cmd = transport_cmd;

    let args = Box::new(ClientArgs {
        transport,
        target,
        list,
        reconnect,
        persist,
        force,
        verbose,
        command,
        config: Config::default(),
        alias: None,
        entry: None,
    });
    Ok(match host {
        Some(_) => Parsed::Run(args),
        None => Parsed::ListAll(args),
    })
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
        assert_eq!(a.target, Target::Pick("main".into()));
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
        assert!(!run(&["devbox"]).unwrap().list);
    }

    fn list(args: &[&str]) -> Result<Parsed, String> {
        parse_list(args.iter().map(OsString::from))
    }

    /// acs-m0l: `acs list` (the word itself is taken off by the role
    /// dispatch, lib.rs) lists every alias, `acs list <host>` one host.
    #[test]
    fn acs_list_lists_every_alias_or_one_host() {
        for args in [&[][..], &["-p", "2222", "-v"], &["-v", "-p2222"]] {
            match list(args).unwrap() {
                Parsed::ListAll(a) => {
                    assert!(a.list);
                    assert_eq!(a.transport.destination, "");
                    if !args.is_empty() {
                        assert_eq!(opts(&a), ["-p", "2222"]);
                        assert_eq!(a.verbose, 1);
                    }
                }
                other => panic!("{args:?}: {other:?}"),
            }
        }
        for args in [&["devbox"][..], &["-i", "k", "devbox", "-v"]] {
            match list(args).unwrap() {
                Parsed::Run(a) => {
                    assert!(a.list);
                    assert_eq!(a.transport.destination, "devbox");
                }
                other => panic!("{args:?}: {other:?}"),
            }
        }
        // A host called `list`, listed: `acs list list`, or `acs list me@list`.
        match list(&["list"]).unwrap() {
            Parsed::Run(a) => assert_eq!(a.transport.destination, "list"),
            other => panic!("{other:?}"),
        }
        // Reached as a session host, behind an option or as user@list.
        for args in [&["-v", "list"][..], &["me@list", "work"]] {
            let a = run(args).unwrap();
            assert!(!a.list);
            assert!(a.transport.destination.ends_with("list"), "{args:?}");
        }
    }

    #[test]
    fn listing_is_a_command_not_an_option() {
        for args in [
            &["--list"][..],
            &["-l"],
            &["devbox", "--list"],
            &["-l", "devbox"],
            &["-p", "2222", "-v", "--list"],
        ] {
            let e = parse(args.iter().map(OsString::from), None).unwrap_err();
            assert!(e.contains("acs list [<host>]"), "{args:?}: {e}");
        }
        let e = list(&["-l", "bob", "devbox"]).unwrap_err();
        assert!(e.contains("user@host"), "{e}");
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

    /// acs-6f5: `-L` is kept apart from the pass-through options, because
    /// only the session's ssh gets it, and a bad spec is an acs error
    /// before any ssh runs.
    #[test]
    fn local_forwards_are_repeatable_and_checked() {
        let a = run(&[
            "-L",
            "8080:localhost:80",
            "-p",
            "2222",
            "-L5432:db.internal:5432",
            "me@box",
        ])
        .unwrap();
        assert_eq!(
            a.transport.local_forwards,
            ["8080:localhost:80", "5432:db.internal:5432"]
        );
        // They are not ssh pass-through options.
        assert_eq!(opts(&a), ["-p", "2222"]);
        assert!(run(&["box"]).unwrap().transport.local_forwards.is_empty());

        // `acs list` may attach the session it picks from the menu (§7.3),
        // so a forward is taken there too.
        match list(&["-L", "8080:localhost:80"]).unwrap() {
            Parsed::ListAll(a) => assert_eq!(a.transport.local_forwards, ["8080:localhost:80"]),
            other => panic!("{other:?}"),
        }

        let e = run(&["-L", "8080:localhost", "box"]).unwrap_err();
        assert!(e.contains("bad -L '8080:localhost'"), "{e}");
        let e = run(&["-L", "eighty:localhost:80", "box"]).unwrap_err();
        assert!(e.contains("is not a number"), "{e}");
        let e = list(&["-L", "8080:localhost", "box"]).unwrap_err();
        assert!(e.contains("bad -L"), "{e}");
        let e = run(&["-L", "box"]).unwrap_err();
        assert!(e.contains("bad -L 'box'"), "{e}");
    }

    #[test]
    fn persist_and_its_contradiction() {
        let a = run(&["devbox", "--persist"]).unwrap();
        assert!(a.persist && a.reconnect);
        assert!(!run(&["devbox"]).unwrap().persist);
        let e = run(&["devbox", "--persist", "--no-reconnect"]).unwrap_err();
        assert!(e.contains("give one"), "{e}");
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
            Parsed::Run(a) => assert_eq!(a.target, Target::Pick("michel".into())),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn errors() {
        let cases: &[(&[&str], &str)] = &[
            (&[], "need a host"),
            (&["--new"], "need a host"),
            (&["h", "a", "b"], "too many arguments"),
            (&["h", "bad name"], "bad session name"),
            (&["-l", "bob", "host"], "user@host"),
            (&["h", "x", "--new"], "--new picks"),
            (&["-q", "h"], "unknown option -q"),
            (&["--bogus", "h"], "unknown option --bogus"),
            (&["-i"], "missing argument"),
        ];
        for (args, want) in cases {
            let e = run(args).unwrap_err();
            assert!(e.contains(want), "{args:?}: {e}");
        }
        let list_cases: &[(&[&str], &str)] = &[
            (&["--new"], "acs list takes at most a host"),
            (&["h", "x"], "acs list takes at most a host"),
            (&["h", "--new"], "acs list takes at most a host"),
            (&["--", "ls"], "acs list takes no command"),
            (&["h", "--", "ls"], "acs list takes no command"),
            (&["h", "x", "y"], "too many arguments"),
        ];
        for (args, want) in list_cases {
            let e = list(args).unwrap_err();
            assert!(e.contains(want), "list {args:?}: {e}");
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
