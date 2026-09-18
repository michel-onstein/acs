//! acs — persistent, reconnecting remote shells over ssh with an unfiltered
//! terminal stream. One binary plays every role; see `docs/DESIGN.md`.

use std::ffi::OsString;
use std::process::ExitCode;

pub mod keys;
pub mod modes;
pub mod proto;
pub mod resume;
pub mod session;
pub mod ssh;
pub mod sys;
#[doc(hidden)]
pub mod testutil;

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
}

impl Role {
    /// Hidden roles are selected by an underscore-prefixed first argument, so
    /// they can never collide with a host name.
    pub fn from_first_arg(arg: Option<&OsString>) -> Role {
        match arg.and_then(|a| a.to_str()) {
            Some("_proxy") => Role::Proxy,
            Some("_master") => Role::Master,
            Some("_install") => Role::Install,
            Some("_version") | Some("--version") | Some("-V") => Role::Version,
            _ => Role::Client,
        }
    }
}

/// Entry point shared by `main` and the integration tests.
pub fn run(args: Vec<OsString>) -> ExitCode {
    let role = Role::from_first_arg(args.get(1));
    match role {
        Role::Version => {
            println!("acs {VERSION}");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("acs: role {other:?} is not implemented yet");
            ExitCode::from(70)
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
        assert_eq!(role("_version"), Role::Version);
        assert_eq!(role("--version"), Role::Version);
    }

    #[test]
    fn anything_else_is_the_client() {
        assert_eq!(role("devbox"), Role::Client);
        assert_eq!(role("proxy"), Role::Client);
        assert_eq!(Role::from_first_arg(None), Role::Client);
    }
}
