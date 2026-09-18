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

/// Options for side calls (`--list`, install): no pty and no escape char,
/// but the user's connection multiplexing is kept.
pub const SIDE_OPTS: &[&str] = &["-T", "-e", "none"];

/// Which kind of ssh call to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    Session,
    Side,
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
            transport_cmd: None,
        }
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
        };
        v.extend(fixed.iter().map(OsString::from));
        v.extend(self.user_opts.iter().cloned());
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
pub fn remote_candidates(version: &str) -> [String; 2] {
    [
        format!("$HOME/.local/share/acs/{version}/acs"),
        format!("/usr/local/lib/acs/{version}/acs"),
    ]
}

/// The POSIX sh script run on the remote: exec our own version of acs with
/// `args`, or report what is missing with an `ACS-NEED` line.
pub fn prelude(version: &str, args: &[&str]) -> String {
    let args: Vec<String> = args.iter().map(|a| sh_quote(a)).collect();
    let [home, system] = remote_candidates(version);
    format!(
        "for b in \"{home}\" \"{system}\"; do \
         if [ -x \"$b\" ]; then exec \"$b\" {args}; fi; \
         done; \
         printf 'ACS-NEED %s %s\\n' \"$(uname -s)\" \"$(uname -m)\"",
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
            format!("ACS-NEED {} {}\n", uname_s.trim(), uname_m.trim())
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
