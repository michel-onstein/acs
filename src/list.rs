//! `acs <host> --list` (DESIGN §4.3).

use std::process::ExitCode;

use crate::cli::ClientArgs;

pub fn run(_args: &ClientArgs) -> ExitCode {
    eprintln!("acs: --list is not implemented yet");
    ExitCode::from(1)
}
