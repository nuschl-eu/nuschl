// SPDX-License-Identifier: EUPL-1.2
//! Fixtures for integration tests that run real processes (issue #313).
//!
//! External programs come from `PATH`: the flake's `unitExtras` sets it for
//! the `<crate>-unit` checks, `nix develop` for local runs. A missing program
//! fails the test. A package's own executables come from Cargo's
//! `CARGO_BIN_EXE_<name>`.
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub use tempfile::TempDir;

pub mod murmur;
pub mod sfu;
pub mod turn;

/// The external program `name` on `PATH`.
pub fn program(name: &str) -> PathBuf {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("{name} is not on PATH: run the tests in `nix develop`"))
}

/// Requires the programs the Linux worker sandbox launches with.
pub fn sandbox() {
    program("bwrap");
    program("prlimit");
}

/// A temporary directory outside the Nix store, removed when dropped.
pub fn temp_dir() -> TempDir {
    let directory = tempfile::tempdir().expect("temporary directory");
    assert!(!directory.path().starts_with("/nix/store"));
    directory
}

/// Host PIDs of processes running `executable`, inside a sandbox or not:
/// a confined worker sees itself as `/worker`, so this compares the file.
#[cfg(target_os = "linux")]
pub fn running(executable: &Path) -> Vec<u32> {
    use std::os::unix::fs::MetadataExt as _;
    let file = std::fs::metadata(executable).expect("executable");
    std::fs::read_dir("/proc")
        .expect("/proc")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let pid = entry.file_name().to_str()?.parse().ok()?;
            let running = std::fs::metadata(entry.path().join("exe")).ok()?;
            (running.dev() == file.dev() && running.ino() == file.ino()).then_some(pid)
        })
        .collect()
}

/// Waits up to `within` for every process running `executable` to exit.
/// A supervisor reaps its launcher; the confined worker follows it.
#[cfg(target_os = "linux")]
pub async fn none_running(executable: &Path, within: Duration) {
    let started = Instant::now();
    while !running(executable).is_empty() {
        assert!(
            started.elapsed() < within,
            "{} outlived its supervisor",
            executable.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
