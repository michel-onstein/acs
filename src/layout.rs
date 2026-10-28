//! Where acs is installed (DESIGN §8): the one place the install layout is
//! spelled.
//!
//! - **per user** — `~/.local/share/acs/<version>/acs`, linked from
//!   `~/.local/bin/acs`;
//! - **system-wide** — `/usr/local/lib/acs/<version>/acs`, linked from
//!   `/usr/local/bin/acs`, which an administrator populates once for every
//!   user.
//!
//! The remote prelude looks in both version directories, the remote install
//! and `acs upgrade` write them, and pruning keeps to the per-user one.
//! `scripts/install.sh` cannot share this code, so it spells the same four
//! paths and `tests/scripts.rs` holds the two together.

use std::path::{Path, PathBuf};

/// [`USER_ROOT`] as a literal, for `concat!` into the shell the remote
/// runs and the messages that name it: a literal costs the binary nothing,
/// where a `format!` argument costs code at every use.
macro_rules! user_root_str {
    () => {
        ".local/share/acs"
    };
}
/// [`SYSTEM_ROOT`] as a literal, like [`user_root_str!`].
macro_rules! system_root_str {
    () => {
        "/usr/local/lib/acs"
    };
}
pub(crate) use {system_root_str, user_root_str};

/// The per-user versions directory, relative to `$HOME`.
pub const USER_ROOT: &str = user_root_str!();
/// The per-user link directory, relative to `$HOME`.
pub const USER_BIN: &str = ".local/bin";
/// The system-wide versions directory.
pub const SYSTEM_ROOT: &str = system_root_str!();
/// The system-wide link directory.
pub const SYSTEM_BIN: &str = "/usr/local/bin";

// The three below are inlined: out of line, each costs the slim build an
// unwind entry for a single `join`.

/// `home/.local/share/acs`.
#[inline(always)]
pub fn user_root(home: &Path) -> PathBuf {
    home.join(USER_ROOT)
}

/// `home/.local/bin`.
#[inline(always)]
pub fn user_bin(home: &Path) -> PathBuf {
    home.join(USER_BIN)
}

/// `/usr/local/bin/acs`.
#[inline(always)]
pub fn system_link() -> PathBuf {
    Path::new(SYSTEM_BIN).join("acs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_is_the_one_design_section_8_describes() {
        let home = Path::new("/home/me");
        assert_eq!(user_root(home), Path::new("/home/me/.local/share/acs"));
        assert_eq!(user_bin(home), Path::new("/home/me/.local/bin"));
        assert_eq!(system_link(), Path::new("/usr/local/bin/acs"));
        assert_eq!(SYSTEM_ROOT, "/usr/local/lib/acs");
    }
}
