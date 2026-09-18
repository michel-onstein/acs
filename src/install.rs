//! Installing acs on a remote (DESIGN §8).

use crate::cli::ClientArgs;

/// The remote has no acs of our version for `os`/`arch`.
pub fn install(args: &ClientArgs, os: &str, arch: &str) -> Result<(), String> {
    Err(format!(
        "acs {} is not installed on {} ({os} {arch}); install it at ~/.local/share/acs/{}/acs",
        crate::VERSION,
        args.transport.destination,
        crate::VERSION
    ))
}
