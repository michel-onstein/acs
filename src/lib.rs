//! acs — persistent, reconnecting remote shells over ssh with an unfiltered
//! terminal stream. One binary plays every role; see `docs/DESIGN.md`.

use std::ffi::OsString;
use std::process::ExitCode;

pub mod alias;
pub mod alias_memory;
pub mod cli;
pub mod client;
pub mod config;
pub mod config_cmd;
pub mod install;
pub mod keys;
pub mod layout;
pub mod list;
pub mod master;
pub mod menu;
pub mod modes;
pub mod mux;
pub mod netmatch;
pub mod netwatch;
mod outq;
pub mod payload;
pub mod pick;
pub mod proto;
pub mod proxy;
pub mod prune;
pub mod reconnect;
pub mod release;
// `build.rs` includes this file as source and xtask compiles it; nothing
// in the binary runs it. The crate compiles it to test it, since a build
// script cannot be tested any other way (acs-okz).
#[cfg(test)]
mod build_channel;
pub mod resume;
pub mod safe;
pub mod session;
pub mod sha256;
pub mod signature;
pub mod ssh;
pub mod sys;
#[doc(hidden)]
pub mod testutil;
pub mod timing;
pub mod tty;
pub mod update_check;
pub mod upgrade;
pub mod yaml;

/// The crate version, baked into the remote prelude so a client always runs
/// its own version on the remote (DESIGN §8).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Which part of acs this process is (DESIGN §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The local, user-facing client: `acs <host> [session]`.
    Client,
    /// Per-connection relay spawned by sshd on the remote.
    Proxy,
    /// Per-session daemon owning the pty on the remote.
    Master,
    /// Second step of a remote self-install.
    Install,
    /// Print the version and protocol version.
    Version,
    /// `acs config …`: read and edit the configuration files.
    Config,
    /// `acs upgrade`: replace this binary with a newer release.
    Upgrade,
    /// `acs list [host]`: the client, listing sessions (DESIGN §4.3, §7.3).
    List,
    /// Background check for a newer release, started by the client.
    UpdateCheck,
    /// `acscp …` or `acs cp …`: copy files to and from a host. The name is
    /// taken; the copy itself is not built yet.
    Copy,
}

impl Role {
    /// The role of a process started with `args`, the program name first:
    /// an underscore-prefixed first argument keeps its hidden role whatever
    /// the binary is called; then a binary called [`layout::COPY_NAME`]
    /// copies; anything else is [`from_first_arg`](Self::from_first_arg).
    pub fn from_argv(args: &[OsString]) -> Role {
        let first = args.get(1);
        if first.is_some_and(|a| a.as_encoded_bytes().starts_with(b"_")) {
            return Role::from_first_arg(first);
        }
        let name = args
            .first()
            .and_then(|a| std::path::Path::new(a).file_name());
        if name.is_some_and(|n| n == layout::COPY_NAME) {
            return Role::Copy;
        }
        Role::from_first_arg(first)
    }

    /// Hidden roles are selected by an underscore-prefixed first argument, so
    /// they can never collide with a host name.
    pub fn from_first_arg(arg: Option<&OsString>) -> Role {
        match arg.and_then(|a| a.to_str()) {
            Some("_proxy") => Role::Proxy,
            Some("_master") => Role::Master,
            Some("_install") => Role::Install,
            Some("_update-check") => Role::UpdateCheck,
            Some("_version") | Some("--version") | Some("-V") => Role::Version,
            // Reserved words, only as the very first argument: a host
            // called `config`, `upgrade`, `list` or `cp` is still reachable
            // as `user@config`, or with any option before it.
            Some("config") => Role::Config,
            Some("upgrade") => Role::Upgrade,
            Some("list") => Role::List,
            Some("cp") => Role::Copy,
            _ => Role::Client,
        }
    }
}

/// `acs --version`: version, protocol, build target, and the remote
/// targets this binary can install (DESIGN §8.1).
pub fn print_version() {
    // write!, not println!: a reader that stops early (`acs --version |
    // grep -q`) closes the pipe, and println! would panic on EPIPE.
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "acs {VERSION} (protocol {}, {})",
        proto::PROTO_VERSION,
        payload::OWN_TARGET
    );
    let mut targets = vec![payload::OWN_TARGET];
    let carried = payload::Payloads::from_self();
    for t in carried.iter().flat_map(|p| p.targets()) {
        if !targets.contains(&t) {
            targets.push(t);
        }
    }
    let slim = if carried.is_none() {
        " (slim build)"
    } else {
        ""
    };
    let _ = writeln!(out, "installs remotes: {}{slim}", targets.join(", "));
}

/// Entry point shared by `main` and the integration tests.
pub fn run(args: Vec<OsString>) -> ExitCode {
    let role = Role::from_argv(&args);
    match role {
        Role::Version => {
            print_version();
            ExitCode::SUCCESS
        }
        Role::Master => master::main(&args[2..]),
        Role::Proxy => proxy::main(&args[2..]),
        Role::Client => client::main(&args[1..], false),
        Role::List => client::main(&args[2..], true),
        Role::Install => install::finish_main(&args[2..]),
        Role::Config => config_cmd::main(&args[2..]),
        Role::Upgrade => upgrade::main(&args[2..]),
        Role::UpdateCheck => update_check::check_main(),
        Role::Copy => {
            eprintln!("acs: copying files is not built yet");
            ExitCode::from(client::code::USAGE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(a: &str) -> Role {
        Role::from_first_arg(Some(&OsString::from(a)))
    }

    #[test]
    fn hidden_roles_are_underscore_prefixed() {
        assert_eq!(role("_proxy"), Role::Proxy);
        assert_eq!(role("_master"), Role::Master);
        assert_eq!(role("_install"), Role::Install);
        assert_eq!(role("_update-check"), Role::UpdateCheck);
        assert_eq!(role("_version"), Role::Version);
        assert_eq!(role("--version"), Role::Version);
    }

    #[test]
    fn anything_else_is_the_client() {
        assert_eq!(role("devbox"), Role::Client);
        assert_eq!(role("proxy"), Role::Client);
        assert_eq!(role("me@config"), Role::Client);
        assert_eq!(role("-v"), Role::Client);
    }

    #[test]
    fn commands_are_reserved_as_the_first_argument() {
        assert_eq!(role("config"), Role::Config);
        assert_eq!(role("upgrade"), Role::Upgrade);
        assert_eq!(role("list"), Role::List);
        assert_eq!(role("cp"), Role::Copy);
        assert_eq!(Role::from_first_arg(None), Role::Client);
        // A host of that name: as user@list, or behind an option.
        assert_eq!(role("me@list"), Role::Client);
        assert_eq!(role("me@cp"), Role::Client);
        assert_eq!(role("-v"), Role::Client);
    }

    fn argv(args: &[&str]) -> Role {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        Role::from_argv(&args)
    }

    /// Run as `acscp`, by any path, the binary copies — whatever the first
    /// argument, a reserved word or a host included.
    #[test]
    fn the_acscp_name_is_the_copy() {
        for name in ["acscp", "/usr/local/bin/acscp", "../bin/acscp"] {
            assert_eq!(argv(&[name]), Role::Copy, "{name}");
            for first in ["box:f", "-r", "list", "config", "--version", "devbox"] {
                assert_eq!(argv(&[name, first, "."]), Role::Copy, "{name} {first}");
            }
        }
        // `acs cp` is the same role without the second name.
        assert_eq!(argv(&["acs", "cp", "box:f", "."]), Role::Copy);
        assert_eq!(argv(&["/opt/acs/acs", "cp"]), Role::Copy);
    }

    /// The hidden roles are the process's whatever it is called: the
    /// remote may run the binary through an `acscp` link too.
    #[test]
    fn hidden_roles_win_over_the_acscp_name() {
        assert_eq!(argv(&["acscp", "_proxy"]), Role::Proxy);
        assert_eq!(argv(&["/x/acscp", "_master", "s"]), Role::Master);
        assert_eq!(argv(&["acscp", "_version"]), Role::Version);
    }

    /// Under the name `acs`, or any name but `acscp`, the roles are what
    /// the first argument has always chosen.
    #[test]
    fn other_names_keep_the_first_argument_roles() {
        for name in ["acs", "/usr/local/bin/acs", "acs-dev", "acscp2", "xacscp"] {
            assert_eq!(argv(&[name]), Role::Client, "{name}");
            assert_eq!(argv(&[name, "devbox"]), Role::Client, "{name}");
            assert_eq!(argv(&[name, "list"]), Role::List, "{name}");
            assert_eq!(argv(&[name, "config"]), Role::Config, "{name}");
            assert_eq!(argv(&[name, "upgrade"]), Role::Upgrade, "{name}");
            assert_eq!(argv(&[name, "-V"]), Role::Version, "{name}");
            assert_eq!(argv(&[name, "_proxy"]), Role::Proxy, "{name}");
            assert_eq!(argv(&[name, "me@cp"]), Role::Client, "{name}");
        }
        // An acscp directory is not an acscp binary.
        assert_eq!(argv(&["/opt/acscp/acs", "devbox"]), Role::Client);
        assert_eq!(argv(&[]), Role::Client);
    }
}
