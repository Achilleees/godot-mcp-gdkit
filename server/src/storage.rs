//! Temporary storage beside a destination, allowing a completed file to be renamed into place.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

pub struct ScratchDir(PathBuf);

impl ScratchDir {
    pub fn new(parent: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(parent)?;
        for _ in 0..100 {
            let path = parent.join(format!(
                ".gdkit-tmp-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::other("cannot allocate temporary storage"))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // Only a directory successfully created by this instance is eligible for cleanup.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
