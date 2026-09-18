//! Helpers shared by unit and integration tests. Not part of the tool.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A directory under `/tmp` removed on drop. Kept short on purpose: unix
/// socket paths inside it must stay under ~104 bytes.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    #[allow(clippy::new_without_default)]
    pub fn new() -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!(
            "/tmp/acst-{}-{}-{:x}",
            crate::sys::getpid(),
            n,
            crate::sys::random_u64() as u16
        ));
        std::fs::create_dir(&path).expect("create temp dir");
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
