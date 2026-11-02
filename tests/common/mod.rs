//! What the integration tests share: running podbay and checking how it
//! exited, and private temporary directories. Standard library only.

#![allow(dead_code)] // each test file uses part of it

use std::fs::{self, DirBuilder};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, ErrorKind};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The podbay binary cargo built for these tests.
pub fn podbay_command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_podbay"))
}

/// Runs a command to completion, with stdin closed and its output
/// captured, and checks how it exited.
pub trait Run {
    /// Exits 0.
    fn run_ok(&mut self) -> Output;
    /// Exits non-zero.
    fn run_fails(&mut self) -> Output;
    /// Exits with `code`.
    fn run_code(&mut self, code: i32) -> Output;
}

impl Run for Command {
    fn run_ok(&mut self) -> Output {
        run(self, |c| c == Some(0), "exit 0")
    }
    fn run_fails(&mut self) -> Output {
        run(self, |c| c != Some(0), "a failure")
    }
    fn run_code(&mut self, code: i32) -> Output {
        run(self, |c| c == Some(code), &format!("exit {code}"))
    }
}

fn run(cmd: &mut Command, ok: impl Fn(Option<i32>) -> bool, wanted: &str) -> Output {
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("running {cmd:?}: {e}"));
    assert!(
        ok(output.status.code()),
        "{cmd:?}: expected {wanted}, got {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// A private (0700) temporary directory, removed when dropped.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn tempdir() -> io::Result<TempDir> {
    for attempt in 0u32..100 {
        let n = RandomState::new().hash_one((std::process::id(), attempt));
        let path = std::env::temp_dir().join(format!("podbay-test-{n:016x}"));
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(TempDir { path }),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        ErrorKind::AlreadyExists,
        "no free temporary name",
    ))
}
