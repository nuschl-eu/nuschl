// SPDX-License-Identifier: EUPL-1.2
//! The program version and the commit it was built from, such as
//! `0.3.0 (7884c04)`, for `--version`, Settings and logs.
use std::{fs::File, io::Read, path::Path, sync::LazyLock};

/// Where an installation records its commit, relative to the executable's
/// directory: beside the programs (Windows), above `bin/`, above `lib/nuschl/`
/// or `libexec/nuschl/`, and in the app bundle's resources (macOS).
const COMMIT_FILES: [&str; 4] = [
    "share/nuschl/commit",
    "../share/nuschl/commit",
    "../../share/nuschl/commit",
    "../Resources/commit",
];

static VERSION: LazyLock<String> = LazyLock::new(|| {
    let installed = std::env::current_exe()
        .ok()
        .and_then(|executable| installed(&executable));
    format(installed.as_deref().or(option_env!("NUSCHL_GIT_COMMIT")))
});

/// This program's version with the installation's commit, else the commit of
/// the checkout it was built in, else `dev`.
pub fn version() -> &'static str {
    &VERSION
}

fn format(commit: Option<&str>) -> String {
    let commit = commit.unwrap_or("dev");
    format!("{} ({commit})", env!("CARGO_PKG_VERSION"))
}

/// The first valid commit file of the installation `executable` is in.
fn installed(executable: &Path) -> Option<String> {
    let directory = executable.parent()?;
    COMMIT_FILES.iter().find_map(|name| {
        let mut text = String::new();
        File::open(directory.join(name))
            .ok()?
            .take(64)
            .read_to_string(&mut text)
            .ok()?;
        let commit = text.trim_end();
        valid(commit).then(|| commit.to_owned())
    })
}

/// 7 to 40 lowercase hex digits, with `-dirty` for a tree with changes.
fn valid(commit: &str) -> bool {
    let hex = commit.strip_suffix("-dirty").unwrap_or(commit);
    (7..=40).contains(&hex.len()) && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_names_the_commit() {
        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(format(Some("7884c04")), format!("{version} (7884c04)"));
        assert_eq!(format(None), format!("{version} (dev)"));
    }

    #[test]
    fn commit_files_are_found_and_validated() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("share/nuschl/commit");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let at = |executable: &str| {
            let executable = root.path().join(executable);
            std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
            installed(&executable)
        };
        std::fs::write(&file, "7884c04-dirty\n").unwrap();
        assert_eq!(at("bin/nuschld").as_deref(), Some("7884c04-dirty"));
        assert_eq!(at("lib/nuschl/nuschl").as_deref(), Some("7884c04-dirty"));
        assert_eq!(at("nuschl.exe").as_deref(), Some("7884c04-dirty"));
        for invalid in [
            "",
            "dirty",
            "7884C04",
            "788",
            "7884c04 x",
            &"a".repeat(41),
            &"a".repeat(80),
        ] {
            std::fs::write(&file, invalid).unwrap();
            assert_eq!(at("bin/nuschld"), None, "{invalid:?}");
        }
    }
}
