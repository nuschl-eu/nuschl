// SPDX-License-Identifier: EUPL-1.2
//! In a git checkout, the commit it is at, with `-dirty` when tracked files
//! have changes. Outside one, such as the Nix sandbox, nothing.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn main() {
    let Some(commit) = git(&["rev-parse", "--short=7", "HEAD"]) else {
        return;
    };
    let paths = git(&["rev-parse", "--git-path", "HEAD", "--git-path", "index"]);
    for path in paths.iter().flat_map(|paths| paths.lines()) {
        println!("cargo:rerun-if-changed={path}");
    }
    let status = git(&["status", "--porcelain", "--untracked-files=no"]);
    let dirty = if status.is_some_and(|status| !status.is_empty()) {
        "-dirty"
    } else {
        ""
    };
    println!("cargo:rustc-env=NUSCHL_GIT_COMMIT={}{dirty}", commit.trim());
}
