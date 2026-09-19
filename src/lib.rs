//! acs — persistent, reconnecting remote shells over ssh with an unfiltered
//! terminal stream. One binary plays every role; see `docs/DESIGN.md`.

use std::ffi::OsString;
use std::process::ExitCode;

pub mod alias;
pub mod cli;
pub mod client;
pub mod config;
pub mod config_cmd;
pub mod install;
pub mod keys;
pub mod list;
pub mod master;
pub mod menu;
pub mod modes;
pub mod netmatch;
pub mod netwatch;
pub mod payload;
pub mod pick;
pub mod proto;
pub mod proxy;
pub mod prune;
pub mod reconnect;
pub mod release;
pub mod resume;
pub mod session;
pub mod sha256;
pub mod ssh;
pub mod sys;
#[doc(hidden)]
pub mod testutil;
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
}

impl Role {
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
            // called `config`, `upgrade` or `list` is still reachable as
            // `user@config`, or with any option before it.
            Some("config") => Role::Config,
            Some("upgrade") => Role::Upgrade,
            Some("list") => Role::List,
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
    let role = Role::from_first_arg(args.get(1));
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
        assert_eq!(Role::from_first_arg(None), Role::Client);
        // A host of that name: as user@list, or behind an option.
        assert_eq!(role("me@list"), Role::Client);
        assert_eq!(role("-v"), Role::Client);
    }
}
