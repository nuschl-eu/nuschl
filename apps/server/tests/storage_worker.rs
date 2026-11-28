// SPDX-License-Identifier: EUPL-1.2
//! The storage worker's restarts, against a worker that fails every time.
use nuschl_ipc::server::ServerStorageRequest as Request;
use nuschld::storage::{Backend, start};
use std::{
    fs,
    io::Write as _,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

fn write_file(path: &Path, body: &[u8]) {
    let mut file = fs::File::create(path).expect("a file in the temporary directory");
    file.write_all(body).expect("written");
}

/// A worker that records its start and exits at once, as one that cannot open
/// its database does.
fn failing_worker(directory: &Path, starts: &Path) -> PathBuf {
    let worker = directory.join("failing-storage-worker");
    write_file(
        &worker,
        format!("#!/bin/sh\necho started >> {}\nexit 1\n", starts.display()).as_bytes(),
    );
    fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).expect("the worker runs");
    worker
}

/// Requests that find the worker gone fail, and the supervisor starts it once
/// and then waits out its budget rather than spawning a worker per request.
#[tokio::test]
async fn repeated_start_failures_do_not_spawn_a_worker_per_request() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let starts = directory.path().join("starts");
    let worker = failing_worker(directory.path(), &starts);
    let storage = start(Backend::Sqlite {
        worker,
        path: directory.path().join("history.sqlite3"),
    });
    for _ in 0..5 {
        assert!(
            storage.request(Request::OperatorLock).await.is_none(),
            "a request without a worker fails"
        );
    }
    let started = fs::read_to_string(&starts).unwrap_or_default();
    assert_eq!(
        started.lines().count(),
        1,
        "the worker is started once, then waits out its budget"
    );
    assert!(storage.failures() >= 5, "every failed request is counted");
}
