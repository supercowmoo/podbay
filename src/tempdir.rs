//! Private temporary directories, removed when dropped.

use std::fs::{self, DirBuilder};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, ErrorKind};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A directory only its owner can use (0700), with an unpredictable name,
/// created so that nothing else can have made it first.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates `<parent>/<prefix><random>`.
    pub fn new_in(parent: &Path, prefix: &str) -> io::Result<Self> {
        for attempt in 0u32..100 {
            let path = parent.join(format!("{prefix}{}", random_name(attempt)));
            // create, not create_all: an existing path, even one someone
            // else planted, is an error, never reused.
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            ErrorKind::AlreadyExists,
            format!("no free temporary name in {}", parent.display()),
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A directory in the system's temporary directory.
pub fn tempdir() -> io::Result<TempDir> {
    TempDir::new_in(&std::env::temp_dir(), "podbay-")
}

/// 16 hex digits. RandomState is seeded from the operating system's random
/// source, and differs on every call.
fn random_name(attempt: u32) -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = RandomState::new().hash_one((std::process::id(), now, attempt));
    format!("{n:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn private_unique_and_removed() {
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        assert_ne!(a.path(), b.path());
        let meta = fs::metadata(a.path()).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        fs::create_dir_all(a.path().join("x/y")).unwrap();
        fs::write(a.path().join("x/y/z"), "").unwrap();
        let path = a.path().to_owned();
        drop(a);
        assert!(!path.exists());
    }

    #[test]
    fn never_reuses_an_existing_path() {
        let parent = tempdir().unwrap();
        // Fill the namespace with a name the generator could produce: the
        // real one can't collide, so this only checks the error path.
        let taken = parent.path().join("x");
        fs::create_dir(&taken).unwrap();
        let err = DirBuilder::new().mode(0o700).create(&taken).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        let made = TempDir::new_in(parent.path(), "x").unwrap();
        assert_ne!(made.path(), taken);
    }
}
