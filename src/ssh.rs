//! Building every ssh invocation (DESIGN §3, §7.1) and the remote prelude
//! that finds or asks for the acs binary (DESIGN §8).

use std::ffi::{OsStr, OsString};
use std::process::Command;

/// Options the session transport depends on. They come before the user's so
/// they win: ssh keeps the first value it sees for an option.
pub const TRANSPORT_OPTS: &[&str] = &[
    "-T",
    "-e",
    "none",
    "-o",
    "ControlMaster=no",
    "-o",
    "ControlPath=none",
    "-o",
    "ServerAliveInterval=0",
    // A redial into a dead network must fail fast so the client returns to
    // its backoff wait, where the command keys work.
    "-o",
    "ConnectTimeout=10",
];

/// Options for side calls (`acs list`, install): no pty and no escape char,
/// but the user's connection multiplexing is kept.
pub const SIDE_OPTS: &[&str] = &["-T", "-e", "none"];

/// Side calls made to several hosts at once (`acs list`, DESIGN §7.3): no
/// ssh may ask for a password or a host key on the shared terminal, and a
/// dead host must fail within the time the others take to answer.
pub const BATCH_OPTS: &[&str] = &[
    "-T",
    "-e",
    "none",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
];

/// Which kind of ssh call to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    Session,
    Side,
    Batch,
}

/// Everything needed to reach a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transport {
    /// The ssh program (`--ssh`, `ACS_SSH`, default `ssh`).
    pub ssh: OsString,
    /// The user's `-i`/`-p`/`-J`/`-F`/`-o` options, flag and value pairs in
    /// the order given.
    pub user_opts: Vec<OsString>,
    /// `[user@]host`.
    pub destination: String,
    /// The configuration's key for the destination (DESIGN §7.3), passed as
    /// `-i` unless the user's options name one: the command line wins.
    pub identity_file: Option<String>,
    /// `-L` specs, in the order given, passed only to the session's ssh
    /// (DESIGN §7.1): the side and batch calls run their own ssh, and a
    /// forward on those would have several of them binding one local port.
    pub local_forwards: Vec<String>,
    /// Test hook: run this (split on whitespace) with the remote command as
    /// its last argument instead of ssh (DESIGN §9.1).
    pub transport_cmd: Option<String>,
}

impl Transport {
    pub fn new(destination: impl Into<String>) -> Self {
        Transport {
            ssh: std::env::var_os("ACS_SSH")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "ssh".into()),
            user_opts: Vec::new(),
            destination: destination.into(),
            identity_file: None,
            local_forwards: Vec::new(),
            transport_cmd: None,
        }
    }

    /// Whether the user's options name a key: `-i`, or `-o IdentityFile`.
    pub fn user_identity(&self) -> bool {
        self.user_opts.chunks(2).any(|p| match p {
            [flag, _] if flag == "-i" => true,
            [flag, v] if flag == "-o" => v.to_str().is_some_and(|o| {
                o.trim_start()
                    .split(|c: char| c == '=' || c.is_whitespace())
                    .next()
                    .is_some_and(|k| k.eq_ignore_ascii_case("IdentityFile"))
            }),
            _ => false,
        })
    }

    /// Program and arguments for a call running `remote` on the host.
    pub fn argv(&self, call: Call, remote: &str) -> Vec<OsString> {
        if let Some(cmd) = &self.transport_cmd {
            let mut v: Vec<OsString> = cmd.split_whitespace().map(OsString::from).collect();
            v.push(remote.into());
            return v;
        }
        let mut v = vec![self.ssh.clone()];
        let fixed = match call {
            Call::Session => TRANSPORT_OPTS,
            Call::Side => SIDE_OPTS,
            Call::Batch => BATCH_OPTS,
        };
        v.extend(fixed.iter().map(OsString::from));
        v.extend(self.user_opts.iter().cloned());
        // The user's session, and nothing else, gets the forwards: the side
        // and batch calls are separate ssh processes, and several of them
        // binding one local port is noise at best (DESIGN §7.1).
        if call == Call::Session {
            for spec in &self.local_forwards {
                v.push("-L".into());
                v.push(spec.into());
            }
        }
        if let Some(key) = self
            .identity_file
            .as_ref()
            .filter(|_| !self.user_identity())
        {
            v.push("-i".into());
            v.push(key.into());
        }
        // `--` keeps a destination starting with `-` from being an option.
        v.push("--".into());
        v.push(self.destination.clone().into());
        v.push(remote.into());
        v
    }

    pub fn command(&self, call: Call, remote: &str) -> Command {
        let argv = self.argv(call, remote);
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]);
        c
    }
}

/// Split a forward spec on the colons that separate its fields, leaving the
/// ones inside an IPv6 literal's `[…]` alone. `None`: the brackets do not
/// balance.
fn colon_fields(spec: &str) -> Option<Vec<&str>> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0u32;
    for (i, c) in spec.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => depth = depth.checked_sub(1)?,
            ':' if depth == 0 => {
                out.push(&spec[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    (depth == 0).then(|| {
        out.push(&spec[start..]);
        out
    })
}

/// An address field: a name or an IPv4 literal, or an IPv6 one in `[…]`.
fn addr_ok(s: &str) -> bool {
    let inner = match s.strip_prefix('[') {
        Some(rest) => match rest.strip_suffix(']') {
            Some(inner) => inner,
            None => return false,
        },
        None => s,
    };
    !inner.is_empty() && !inner.contains(['[', ']']) && !inner.contains(char::is_whitespace)
}

/// Check a `-L` spec before any ssh is spawned: ssh would only find a bad
/// one per dial, printing its own complaint onto the session's terminal
/// mid-reconnect (DESIGN §7.1).
///
/// Only the TCP forms are taken — `[bind_address:]port:host:hostport`, an
/// IPv6 literal in `[…]` — because a unix-socket forward's grammar is
/// ambiguous enough that checking it would reject specs ssh accepts.
/// `-o LocalForward=…` stays the unchecked pass-through for those.
pub fn check_local_forward(spec: &str) -> Result<(), String> {
    let bad = |why: String| {
        Err(format!(
            "bad -L '{spec}': {why} — want [bind_address:]port:host:hostport, \
             as ssh spells it (a unix socket needs -o LocalForward=… instead)"
        ))
    };
    if spec.contains('/') {
        return bad("acs does not take socket paths here".into());
    }
    let Some(fields) = colon_fields(spec) else {
        return bad("the [] around an address do not balance".into());
    };
    let (bind, port, host, hostport) = match fields[..] {
        [p, h, hp] => (None, p, h, hp),
        [b, p, h, hp] => (Some(b), p, h, hp),
        _ => {
            return bad(format!(
                "want 3 or 4 colon-separated fields, not {}",
                fields.len()
            ))
        }
    };
    // An empty bind address, like `*`, means every interface to ssh.
    if let Some(b) = bind.filter(|b| !b.is_empty() && *b != "*" && !addr_ok(b)) {
        return bad(format!("'{b}' is not a bind address"));
    }
    for (what, p) in [("port", port), ("hostport", hostport)] {
        if !matches!(p.parse::<u16>(), Ok(n) if n > 0) {
            return bad(format!("{what} '{p}' is not a number from 1 to 65535"));
        }
    }
    if !addr_ok(host) {
        return bad(format!("'{host}' is not a host"));
    }
    Ok(())
}

/// Quote a string for POSIX `sh`.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@,+%".contains(&b))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Where the remote side looks for the binary of exactly `version`.
///
/// The version is quoted like every other value that reaches the remote
/// shell (acs-ciz). It sits inside double quotes in [`prelude`], where `$`,
/// a backtick and a backslash are all still live, so an unquoted one would
/// be command execution for any caller that ever passed something other
/// than the compile-time constant — and nothing in the signature says it
/// may not.
pub fn remote_candidates(version: &str) -> [String; 2] {
    let v = sh_quote(version);
    [
        // `$HOME` is quoted on its own so a home with a space still works,
        // and the rest is already shell-safe, so the caller must not wrap
        // these in quotes again — that would make the quoting literal.
        format!("\"$HOME\"/.local/share/acs/{v}/acs"),
        format!("/usr/local/lib/acs/{v}/acs"),
    ]
}

/// The POSIX sh script run on the remote: exec our own version of acs with
/// `args`, or report what is missing with an `ACS-NEED` line.
pub fn prelude(version: &str, args: &[&str]) -> String {
    let args: Vec<String> = args.iter().map(|a| sh_quote(a)).collect();
    let [home, system] = remote_candidates(version);
    // Before exec'ing anything, check it is ours and that nobody else can
    // write it or the directory holding it (acs-08m). The path is fixed and
    // acs runs it on every single connection, so on a host where `$HOME` or
    // `~/.local/share` is group-writable — umask 002 with a shared group,
    // which lab and appliance images still ship — another local user plants
    // a binary there once and owns every later session.
    //
    // `ls -ldn` rather than `test -O`, which is not POSIX and is missing
    // from dash: field 1 is the mode string, where character 6 is group
    // write and character 9 is other write, and field 3 is the numeric
    // owner.
    format!(
        "u=$(id -u); \
         acs_safe() {{ \
         [ -e \"$1\" ] || return 1; \
         set -- \"$1\" $(ls -ldn \"$1\" 2>/dev/null); \
         case \"$2\" in ?????w*|????????w*) return 1;; esac; \
         [ \"$4\" = \"$u\" ]; }}; \
         for b in {home} {system}; do \
         if [ -x \"$b\" ]; then \
         if acs_safe \"$b\" && acs_safe \"$(dirname \"$b\")\"; then exec \"$b\" {args}; fi; \
         printf 'acs: refusing to run %s: it or its directory is writable by others, or not yours\\n' \"$b\" >&2; \
         fi; \
         done; \
         printf '\\nACS-NEED %s %s\\n' \"$(uname -s)\" \"$(uname -m)\"",
        args = args.join(" ")
    )
}

/// The command string handed to ssh. The remote login shell may be fish or
/// tcsh, so the script runs under `sh -c` and is passed as one quoted word.
pub fn remote_command(script: &str) -> String {
    format!("sh -c {}", sh_quote(script))
}

/// Convenience: the full remote command to run `acs <args>` of `version`.
pub fn remote_acs(version: &str, args: &[&str]) -> String {
    remote_command(&prelude(version, args))
}

/// Render argv for `-v` diagnostics.
pub fn display_argv(argv: &[OsString]) -> String {
    argv.iter()
        .map(|a| sh_quote(&OsStr::to_string_lossy(a)))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::os::unix::fs::PermissionsExt;

    fn t() -> Transport {
        Transport {
            ssh: "ssh".into(),
            user_opts: [
                "-i",
                "~/.ssh/id_work",
                "-o",
                "ControlMaster=auto",
                "-p",
                "2222",
            ]
            .iter()
            .map(OsString::from)
            .collect(),
            destination: "me@box".into(),
            identity_file: None,
            local_forwards: Vec::new(),
            transport_cmd: None,
        }
    }

    fn strs(v: Vec<OsString>) -> Vec<String> {
        v.into_iter().map(|s| s.into_string().unwrap()).collect()
    }

    #[test]
    fn session_call_puts_transport_options_first() {
        assert_eq!(
            strs(t().argv(Call::Session, "REMOTE")),
            [
                "ssh",
                "-T",
                "-e",
                "none",
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=none",
                "-o",
                "ServerAliveInterval=0",
                "-o",
                "ConnectTimeout=10",
                "-i",
                "~/.ssh/id_work",
                "-o",
                "ControlMaster=auto",
                "-p",
                "2222",
                "--",
                "me@box",
                "REMOTE"
            ]
        );
    }

    #[test]
    fn side_call_keeps_the_users_multiplexing() {
        let v = strs(t().argv(Call::Side, "R"));
        assert_eq!(v[..4], ["ssh", "-T", "-e", "none"]);
        assert!(!v.iter().any(|a| a == "ControlMaster=no"));
        assert_eq!(
            v[4..],
            [
                "-i",
                "~/.ssh/id_work",
                "-o",
                "ControlMaster=auto",
                "-p",
                "2222",
                "--",
                "me@box",
                "R"
            ]
        );
    }

    #[test]
    fn batch_call_never_prompts_and_gives_up_on_a_dead_host() {
        let v = strs(t().argv(Call::Batch, "R"));
        assert_eq!(
            v[..8],
            [
                "ssh",
                "-T",
                "-e",
                "none",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10"
            ]
        );
        // The user's options follow, multiplexing included.
        assert_eq!(v[8..], strs(t().argv(Call::Side, "R"))[4..]);
    }

    #[test]
    fn a_configured_key_goes_to_ssh_unless_the_command_line_names_one() {
        let mut tr = t();
        tr.identity_file = Some("/keys/devbox".into());
        // The user's -i wins: ssh would otherwise try both.
        let v = strs(tr.argv(Call::Session, "R"));
        assert_eq!(v.iter().filter(|a| *a == "-i").count(), 1, "{v:?}");
        assert!(!v.iter().any(|a| a == "/keys/devbox"), "{v:?}");

        tr.user_opts = ["-p", "2222", "-o", "IdentitiesOnly=yes"]
            .iter()
            .map(OsString::from)
            .collect();
        for call in [Call::Session, Call::Side, Call::Batch] {
            let v = strs(tr.argv(call, "R"));
            let n = v.len();
            assert_eq!(
                v[n - 9..],
                [
                    "-p",
                    "2222",
                    "-o",
                    "IdentitiesOnly=yes",
                    "-i",
                    "/keys/devbox",
                    "--",
                    "me@box",
                    "R"
                ],
                "{call:?}"
            );
        }

        // -o IdentityFile, in any spelling ssh takes, names a key too.
        for o in [
            "IdentityFile=~/.ssh/k",
            "identityfile ~/.ssh/k",
            " IDENTITYFILE=k",
        ] {
            tr.user_opts = vec!["-o".into(), o.into()];
            assert!(tr.user_identity(), "{o}");
            let v = strs(tr.argv(Call::Session, "R"));
            assert!(!v.iter().any(|a| a == "-i"), "{o}: {v:?}");
        }
        tr.user_opts = vec!["-o".into(), "IdentitiesOnly=yes".into()];
        assert!(!tr.user_identity());
        // A value that only looks like the flag is not one.
        tr.user_opts = vec!["-J".into(), "-i".into()];
        assert!(!tr.user_identity());
    }

    /// acs-6f5: a forward belongs to the user's session. The side and batch
    /// calls are ssh processes of their own — `acs list`, the remote
    /// install, the session menu over every alias — and a `-L` on those
    /// would have several of them binding the same local port at once.
    #[test]
    fn a_local_forward_goes_to_the_session_ssh_and_no_other_call() {
        let mut tr = t();
        tr.local_forwards = vec!["8080:localhost:80".into(), "5432:db:5432".into()];
        let v = strs(tr.argv(Call::Session, "R"));
        let n = v.len();
        assert_eq!(
            v[n - 7..],
            [
                "-L",
                "8080:localhost:80",
                "-L",
                "5432:db:5432",
                "--",
                "me@box",
                "R"
            ],
            "{v:?}"
        );
        // After the user's options, so an -o of theirs still wins (§7.1).
        assert!(
            v.iter().position(|a| a == "-L") > v.iter().position(|a| a == "-p"),
            "{v:?}"
        );
        for call in [Call::Side, Call::Batch] {
            let v = strs(tr.argv(call, "R"));
            assert!(!v.iter().any(|a| a == "-L"), "{call:?}: {v:?}");
            assert!(!v.iter().any(|a| a.contains("8080")), "{call:?}: {v:?}");
            // Nothing else about those calls changed.
            assert_eq!(v, strs(t().argv(call, "R")), "{call:?}");
        }
    }

    #[test]
    fn a_configured_key_still_follows_the_forwards() {
        let mut tr = t();
        tr.local_forwards = vec!["8080:localhost:80".into()];
        tr.identity_file = Some("/keys/devbox".into());
        tr.user_opts = vec!["-p".into(), "2222".into()];
        assert_eq!(
            strs(tr.argv(Call::Session, "R")),
            [
                "ssh",
                "-T",
                "-e",
                "none",
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=none",
                "-o",
                "ServerAliveInterval=0",
                "-o",
                "ConnectTimeout=10",
                "-p",
                "2222",
                "-L",
                "8080:localhost:80",
                "-i",
                "/keys/devbox",
                "--",
                "me@box",
                "R"
            ]
        );
    }

    #[test]
    fn a_local_forward_spec_is_checked_before_any_ssh_runs() {
        for good in [
            "8080:localhost:80",
            "127.0.0.1:8080:db.internal:5432",
            ":8080:h:80",
            "*:8080:h:80",
            "8080:[::1]:80",
            "[::1]:8080:[fe80::1%eth0]:80",
            "65535:h:1",
        ] {
            assert_eq!(check_local_forward(good), Ok(()), "{good}");
        }
        let cases = [
            ("", "want 3 or 4"),
            ("8080", "want 3 or 4"),
            ("8080:localhost", "want 3 or 4"),
            ("8080:localhost:80:9:9", "want 3 or 4"),
            ("http:localhost:80", "port 'http' is not a number"),
            ("0:localhost:80", "port '0' is not a number"),
            ("99999:localhost:80", "port '99999' is not a number"),
            ("8080:localhost:www", "hostport 'www' is not a number"),
            ("8080:localhost:0", "hostport '0' is not a number"),
            ("8080::80", "'' is not a host"),
            ("1.2.3.4:8080::80", "'' is not a host"),
            ("nope:8080:h:80/x", "socket paths"),
            ("/tmp/s:localhost:80", "socket paths"),
            ("[::1:8080:h:80", "do not balance"),
            ("]:8080:h:80", "do not balance"),
            ("[a][b]:8080:h:80", "is not a bind address"),
            ("8080:host name:80", "is not a host"),
        ];
        for (bad, want) in cases {
            let e = check_local_forward(bad).unwrap_err();
            assert!(e.contains(want), "{bad:?}: {e}");
            // Every complaint says what a spec should look like.
            assert!(e.contains("port:host:hostport"), "{bad:?}: {e}");
        }
    }

    #[test]
    fn transport_cmd_replaces_ssh() {
        let mut tr = t();
        tr.transport_cmd = Some("sh -c".into());
        assert_eq!(
            strs(tr.argv(Call::Session, "echo hi")),
            ["sh", "-c", "echo hi"]
        );
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("main"), "main");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
    }

    fn run_sh(script: &str, home: &std::path::Path) -> String {
        let out = Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("HOME", home)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    /// acs-08m: the prelude execs a fixed path on every connection. On a
    /// host where `$HOME` or `~/.local/share` is group-writable — umask 002
    /// with a shared group, which lab and appliance images still ship —
    /// another local user plants a binary there once and owns every later
    /// session. So it is checked before it is run.
    #[test]
    fn a_binary_others_could_have_written_is_not_run() {
        let home = TempDir::new();
        let dir = home.path().join(".local/share/acs/9.9.9");
        std::fs::create_dir_all(&dir).unwrap();
        let acs = dir.join("acs");
        std::fs::write(&acs, "#!/bin/sh\necho RAN\n").unwrap();

        let run = || {
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(prelude("9.9.9", &["--version"]))
                .env("HOME", home.path())
                .output()
                .unwrap();
            (
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        };

        // Ours, and private: it runs.
        std::fs::set_permissions(&acs, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (out, _) = run();
        assert!(out.contains("RAN"), "an honest binary was not run: {out}");

        // Writable by the group: refused, and said so.
        std::fs::set_permissions(&acs, std::fs::Permissions::from_mode(0o775)).unwrap();
        let (out, err) = run();
        assert!(!out.contains("RAN"), "a group-writable binary was run");
        assert!(out.contains("ACS-NEED"), "no marker after refusing: {out}");
        assert!(err.contains("refusing to run"), "no reason given: {err}");

        // Writable by anyone: refused.
        std::fs::set_permissions(&acs, std::fs::Permissions::from_mode(0o707)).unwrap();
        assert!(!run().0.contains("RAN"), "a world-writable binary was run");

        // The binary is fine, but its directory is not.
        std::fs::set_permissions(&acs, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let (out, err) = run();
        assert!(
            !out.contains("RAN"),
            "a binary in a shared directory was run"
        );
        assert!(err.contains("refusing to run"), "{err}");

        // Put it back so the temporary directory can be removed.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// acs-ciz: the version reaches the remote shell inside double quotes,
    /// where `$`, a backtick and a backslash are still live. Every other
    /// value there is quoted; this one was not, so any future caller
    /// passing something other than the compile-time constant would have
    /// been command execution on the remote.
    #[test]
    fn a_hostile_version_cannot_run_a_command_on_the_remote() {
        let home = TempDir::new();
        for bad in [
            "0.1.0\"; touch pwned; \"",
            "$(touch pwned)",
            "`touch pwned`",
            "0.1.0/../../../tmp",
        ] {
            let script = prelude(bad, &["--version"]);
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&script)
                .env("HOME", home.path())
                .output()
                .unwrap();
            assert!(
                !home.path().join("pwned").exists(),
                "{bad:?} ran a command: {script}"
            );
            // It simply finds nothing and says so, as any other unknown
            // version would.
            let said = String::from_utf8_lossy(&out.stdout);
            assert!(said.contains("ACS-NEED"), "{bad:?}: {said}");
        }
    }

    #[test]
    fn prelude_reports_need_when_nothing_is_installed() {
        let home = TempDir::new();
        let out = run_sh(&prelude("9.9.9-test", &["_proxy"]), home.path());
        let uname_s =
            String::from_utf8(Command::new("uname").arg("-s").output().unwrap().stdout).unwrap();
        let uname_m =
            String::from_utf8(Command::new("uname").arg("-m").output().unwrap().stdout).unwrap();
        assert_eq!(
            out,
            format!("\nACS-NEED {} {}\n", uname_s.trim(), uname_m.trim())
        );
    }

    #[test]
    fn prelude_execs_the_versioned_binary_with_args_intact() {
        let home = TempDir::new();
        let dir = home.path().join(".local/share/acs/9.9.9-test");
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("acs");
        std::fs::write(
            &fake,
            "#!/bin/sh\nfor a in \"$@\"; do printf '<%s>' \"$a\"; done\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hostile = "x'; echo pwned; '$(id)";
        let script = prelude("9.9.9-test", &["_proxy", "--session", hostile]);
        assert_eq!(
            run_sh(&script, home.path()),
            format!("<_proxy><--session><{hostile}>")
        );
        // And through the extra `sh -c` layer ssh adds.
        let remote = remote_command(&script);
        assert_eq!(
            run_sh(&remote, home.path()),
            format!("<_proxy><--session><{hostile}>")
        );
    }

    #[test]
    fn remote_command_survives_non_posix_login_shells() {
        // The whole script is a single quoted word after `sh -c`.
        let r = remote_acs("1.0.0", &["_proxy", "--list"]);
        assert!(r.starts_with("sh -c '"), "{r}");
        assert!(r.ends_with('\''), "{r}");
        assert!(!r.contains('!'), "tcsh history expansion: {r}");
    }
}
