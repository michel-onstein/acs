//! The copy's command line (ACSCP_DESIGN §4): `scp`'s spelling, so an
//! alias `scp=acscp` surprises no one in the everyday cases.
//!
//! ```text
//! acscp [-rpqvC46] [-P port] [-i identity_file] [-J destination] [-F config]
//!       [-o option=value] [--no-reconnect] [--ssh path] [--] source ... target
//! ```
//!
//! `-p` is `scp`'s: preserve. The port is `-P`, and goes to ssh as its `-p`
//! through the one list of passed-through options, [`cli::ssh_passthrough`].

use std::ffi::OsString;
use std::path::PathBuf;

use crate::cli::{self, Connection};
use crate::ssh::Transport;

pub const USAGE: &str = "\
usage: acscp [-rpqvC46] [-P port] [-i identity_file] [-J destination] [-F config]
             [-o option=value] [--no-reconnect] [--ssh path] [--] source ... target
       acs cp ...      the same, without the acscp name

  an argument is remote when it is [user@]host:[path] with no / before the
  first :, the host may be an acs alias, and [::1]:path is an IPv6 address;
  ./a:b and /a:b are local files

options:
  -r                  copy directories recursively (symlinks are followed)
  -p                  preserve modification and access times and the exact mode
  -q                  quiet: no progress meter
  -v                  verbose: ssh commands, connect timings, a summary at the end
  -C                  compress (ssh -o Compression=yes)
  -4 / -6             IPv4 / IPv6 only (ssh -o AddressFamily=inet / inet6)
  -P <port>           the port (as scp: -p is preserve here)
  -i <identity_file>  -J <jump>  -F <config>  -o <option=value>
                      passed to every ssh call acscp makes
      --no-reconnect  end on the first lost link instead of redialling
      --ssh <path>    ssh program to run (also ACS_SSH)
  -h, --help          this help
      --version       print the version
  --                  end of options: a source may begin with -

environment: ACS_CP_WINDOW (file headers sent ahead of their data, default 64)";

/// A `[user@]host:path` argument (ACSCP_DESIGN §4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub user: Option<String>,
    /// The host or alias as given; an IPv6 literal keeps its brackets.
    pub host: String,
    /// What follows the `:`: empty for the home directory, relative to it,
    /// absolute, or `~/`-prefixed.
    pub path: String,
}

impl Remote {
    /// `[user@]host`, as ssh and the alias resolution take it.
    pub fn destination(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }
}

/// One source or the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spec {
    Local(PathBuf),
    Remote(Remote),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyArgs {
    pub recursive: bool,
    pub preserve: bool,
    pub quiet: bool,
    pub verbose: u8,
    /// `--no-reconnect` clears it.
    pub reconnect: bool,
    /// `--ssh`.
    pub ssh: Option<OsString>,
    /// `--transport-cmd`, the test hook (DESIGN §9.1).
    pub transport_cmd: Option<String>,
    /// The ssh options, flag and value pairs in the order given, `-P`
    /// already turned into ssh's `-p`.
    pub user_opts: Vec<OsString>,
    pub sources: Vec<Spec>,
    pub target: Spec,
}

impl CopyArgs {
    /// What reaching `remote`'s host takes: the command line's ssh program
    /// and options, the test hook, `-v`. One per host (ACSCP_DESIGN §10.3
    /// item 2).
    pub fn connection(&self, remote: &Remote) -> Connection {
        let mut t = Transport::new(remote.destination());
        if let Some(s) = &self.ssh {
            t.ssh = s.clone();
        }
        t.user_opts = self.user_opts.clone();
        t.transport_cmd = self.transport_cmd.clone();
        Connection::new(t, self.verbose)
    }

    /// Whether any argument names a host.
    pub fn any_remote(&self) -> bool {
        self.sources
            .iter()
            .chain([&self.target])
            .any(|s| matches!(s, Spec::Remote(_)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Run(Box<CopyArgs>),
    Help,
    Version,
}

/// Split `arg` by `scp`'s rule (ACSCP_DESIGN §4.1): remote when it holds a
/// `:` with no `/` before the first one, a `:` inside `[…]` not counting.
/// `Ok(None)` is a local path.
pub fn split_remote(arg: &str) -> Result<Option<Remote>, String> {
    if arg.starts_with(':') {
        return Err(format!("'{arg}': no host before the ':'"));
    }
    let mut bracket = false;
    let mut colon = None;
    for (i, c) in arg.char_indices() {
        match c {
            '/' if !bracket => return Ok(None),
            '[' => bracket = true,
            ']' => bracket = false,
            ':' if !bracket => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(colon) = colon else {
        if bracket && arg.contains(':') {
            return Err(format!(
                "'{arg}': an IPv6 address goes in brackets: [::1]:path"
            ));
        }
        return Ok(None);
    };
    let (dest, path) = (&arg[..colon], &arg[colon + 1..]);
    let (user, host) = crate::alias::split_user(dest);
    if host.is_empty() || host.contains('@') {
        return Err(format!("'{arg}': no host before the ':'"));
    }
    if host.starts_with('[') != host.ends_with(']') || host == "[]" {
        return Err(format!(
            "'{arg}': an IPv6 address goes in brackets: [::1]:path"
        ));
    }
    if host.starts_with('-') {
        return Err(format!("bad host '{host}'"));
    }
    if let Some(rest) = path.strip_prefix('~') {
        if !rest.is_empty() && !rest.starts_with('/') {
            return Err(format!(
                "'{arg}': another user's home (~user) is not supported; give the path"
            ));
        }
    }
    Ok(Some(Remote {
        user: user.map(str::to_string),
        host: host.to_string(),
        path: path.to_string(),
    }))
}

/// One positional argument as a [`Spec`]. A local path may be any bytes;
/// a remote one must be UTF-8, since it travels in a frame.
fn spec(arg: OsString) -> Result<Spec, String> {
    if arg.is_empty() {
        return Err("an empty argument is not a path".into());
    }
    match arg.to_str() {
        Some(s) => Ok(match split_remote(s)? {
            Some(r) => Spec::Remote(r),
            None => Spec::Local(arg.into()),
        }),
        None => match split_remote(&arg.to_string_lossy())? {
            Some(_) => Err(format!(
                "'{}': a remote path must be UTF-8",
                arg.to_string_lossy()
            )),
            None => Ok(Spec::Local(arg.into())),
        },
    }
}

/// Parse the copy's arguments, without the program name (or `cp`).
pub fn parse<I>(args: I) -> Result<Parsed, String>
where
    I: IntoIterator<Item = OsString>,
{
    use lexopt::prelude::*;

    let mut p = lexopt::Parser::from_args(args);
    let mut a = CopyArgs {
        recursive: false,
        preserve: false,
        quiet: false,
        verbose: 0,
        reconnect: true,
        ssh: None,
        transport_cmd: None,
        user_opts: Vec::new(),
        sources: Vec::new(),
        target: Spec::Local(PathBuf::new()),
    };
    let mut positionals = Vec::new();
    let ssh_opt = |a: &mut CopyArgs, flag: &str, v: &str| {
        a.user_opts.push(flag.into());
        a.user_opts.push(v.into());
    };
    while let Some(arg) = p.next().map_err(|e| e.to_string())? {
        match arg {
            Short('r') => a.recursive = true,
            Short('p') => a.preserve = true,
            Short('q') => a.quiet = true,
            Short('v') | Long("verbose") => a.verbose = a.verbose.saturating_add(1),
            Short('C') => ssh_opt(&mut a, "-o", "Compression=yes"),
            Short('4') => ssh_opt(&mut a, "-o", "AddressFamily=inet"),
            Short('6') => ssh_opt(&mut a, "-o", "AddressFamily=inet6"),
            Long("no-reconnect") => a.reconnect = false,
            Long("ssh") => a.ssh = Some(p.value().map_err(|e| e.to_string())?),
            Long("transport-cmd") => {
                let v = p.value().map_err(|e| e.to_string())?;
                a.transport_cmd = Some(
                    v.into_string()
                        .map_err(|_| "--transport-cmd is not valid UTF-8".to_string())?,
                );
            }
            Short('h') | Long("help") => return Ok(Parsed::Help),
            Short('V') | Long("version") => return Ok(Parsed::Version),
            Value(v) => positionals.push(v),
            Short(c) => match cli::ssh_passthrough(c, 'P') {
                Some(o) => {
                    let v = p.value().map_err(|e| e.to_string())?;
                    a.user_opts.push(format!("-{o}").into());
                    a.user_opts.push(v);
                }
                None => return Err(format!("unknown option -{c}")),
            },
            Long(l) => return Err(format!("unknown option --{l}")),
        }
    }
    if positionals.len() < 2 {
        return Err("need a source and a target (see acscp --help)".into());
    }
    let target = positionals.pop().expect("two or more");
    a.target = spec(target)?;
    a.sources = positionals
        .into_iter()
        .map(spec)
        .collect::<Result<_, _>>()?;
    Ok(Parsed::Run(Box::new(a)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(user: Option<&str>, host: &str, path: &str) -> Option<Remote> {
        Some(Remote {
            user: user.map(str::to_string),
            host: host.into(),
            path: path.into(),
        })
    }

    /// ACSCP_DESIGN §4.1, as a table.
    #[test]
    fn split_remote_follows_scps_rule() {
        for (arg, want) in [
            ("host:p", remote(None, "host", "p")),
            ("u@host:p", remote(Some("u"), "host", "p")),
            ("u@host:/abs/p", remote(Some("u"), "host", "/abs/p")),
            ("[::1]:p", remote(None, "[::1]", "p")),
            ("me@[fe80::1]:a/b", remote(Some("me"), "[fe80::1]", "a/b")),
            ("host:", remote(None, "host", "")),
            ("host:~", remote(None, "host", "~")),
            ("host:~/x", remote(None, "host", "~/x")),
            ("host:a:b", remote(None, "host", "a:b")),
            ("./a:b", None),
            ("/a:b", None),
            ("a/b:c", None),
            ("plain", None),
            ("~", None),
            ("~/x", None),
            ("", None),
        ] {
            assert_eq!(split_remote(arg), Ok(want), "{arg}");
        }
    }

    #[test]
    fn split_remote_refuses_what_it_cannot_mean() {
        for arg in [
            ":p",
            "@host:p",
            "u@:p",
            "[::1:p",
            "host]:p",
            "[]:p",
            "-o:p",
            "host:~bob/x",
            "host:~bob",
        ] {
            assert!(split_remote(arg).is_err(), "{arg}");
        }
    }

    fn run(args: &[&str]) -> Result<CopyArgs, String> {
        match parse(args.iter().map(OsString::from))? {
            Parsed::Run(a) => Ok(*a),
            other => panic!("{other:?}"),
        }
    }

    fn local(p: &str) -> Spec {
        Spec::Local(p.into())
    }

    #[test]
    fn sources_then_the_target() {
        let a = run(&["a", "b", "host:d"]).unwrap();
        assert_eq!(a.sources, [local("a"), local("b")]);
        assert_eq!(a.target, Spec::Remote(remote(None, "host", "d").unwrap()));
        assert!(a.any_remote());
        assert!(!run(&["a", "b"]).unwrap().any_remote());
        assert!(run(&["a"]).is_err());
        assert!(run(&[]).is_err());
        assert!(run(&["a", ""]).is_err());
    }

    /// `-p` preserves and `-P` is the port, which ssh gets as its `-p`.
    #[test]
    fn little_p_preserves_and_big_p_is_the_port() {
        let a = run(&["-p", "a", "b"]).unwrap();
        assert!(a.preserve);
        assert!(a.user_opts.is_empty());
        let a = run(&["-P", "2222", "-rp", "a", "b"]).unwrap();
        assert!(a.preserve && a.recursive);
        assert_eq!(a.user_opts, ["-p", "2222"]);
        let a = run(&["-P2200", "a", "b"]).unwrap();
        assert_eq!(a.user_opts, ["-p", "2200"]);
    }

    #[test]
    fn ssh_options_pass_through_in_order() {
        let a = run(&[
            "-i", "k", "-C", "-J", "jump", "-4", "-F", "cfg", "-o", "X=1", "-6", "a", "b",
        ])
        .unwrap();
        let want: Vec<OsString> = [
            "-i",
            "k",
            "-o",
            "Compression=yes",
            "-J",
            "jump",
            "-o",
            "AddressFamily=inet",
            "-F",
            "cfg",
            "-o",
            "X=1",
            "-o",
            "AddressFamily=inet6",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        assert_eq!(a.user_opts, want);
        let c = a.connection(&remote(Some("u"), "h", "p").unwrap());
        assert_eq!(c.transport.destination, "u@h");
        assert_eq!(c.transport.user_opts, want);
    }

    #[test]
    fn flags_and_knobs() {
        let a = run(&["-q", "-vv", "--no-reconnect", "--ssh", "/x/ssh", "a", "b"]).unwrap();
        assert!(a.quiet && !a.reconnect);
        assert_eq!(a.verbose, 2);
        assert_eq!(a.ssh.as_deref(), Some("/x/ssh".as_ref()));
        let a = run(&["--transport-cmd", "sh x", "a", "h:b"]).unwrap();
        assert_eq!(a.transport_cmd.as_deref(), Some("sh x"));
        let c = a.connection(&remote(None, "h", "b").unwrap());
        assert_eq!(c.transport.transport_cmd.as_deref(), Some("sh x"));
        assert!(run(&["-l", "100", "a", "b"]).is_err());
        assert!(run(&["-L", "1:h:2", "a", "b"]).is_err());
        assert!(run(&["--bogus", "a", "b"]).is_err());
    }

    /// `--` ends the options: a source may begin with `-`.
    #[test]
    fn double_dash_ends_the_options() {
        let a = run(&["-r", "--", "-v", "-p", "dst"]).unwrap();
        assert!(a.recursive && !a.preserve && a.verbose == 0);
        assert_eq!(a.sources, [local("-v"), local("-p")]);
        assert_eq!(a.target, local("dst"));
    }

    #[test]
    fn help_and_version() {
        assert_eq!(parse(["-h".into()]), Ok(Parsed::Help));
        assert_eq!(parse(["--help".into()]), Ok(Parsed::Help));
        assert_eq!(parse(["--version".into()]), Ok(Parsed::Version));
    }

    /// A local path may be any bytes; a remote one must be UTF-8.
    #[test]
    fn non_utf8_paths() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(b"caf\xe9".to_vec());
        let a = parse([bad.clone(), "dst".into()]).unwrap();
        let Parsed::Run(a) = a else { panic!() };
        assert_eq!(a.sources, [Spec::Local(bad.into())]);
        let bad_remote = OsString::from_vec(b"h:caf\xe9".to_vec());
        assert!(parse([bad_remote, "dst".into()]).is_err());
    }
}
