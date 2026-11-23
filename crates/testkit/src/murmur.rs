// SPDX-License-Identifier: EUPL-1.2
//! A stock Mumble server (`mumble-server` from `PATH`) on a free loopback
//! port, with Murmur's default limits.
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// A running Murmur, stopped and removed when dropped.
pub struct Murmur {
    process: Child,
    pub port: u16,
    pub directory: tempfile::TempDir,
}

impl Murmur {
    /// Starts Murmur and waits until it accepts connections.
    pub fn start() -> Self {
        let binary = crate::program("mumble-server");
        let directory = crate::temp_dir();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("free loopback port")
            .port();
        let root = directory.path();
        let file = |name: &str| root.join(name).display().to_string();
        let ini = root.join("murmur.ini");
        std::fs::write(
            &ini,
            format!(
                "database={}\nport={port}\nhost=127.0.0.1\nautobanAttempts=0\nlogfile={}\npidfile={}\n",
                file("murmur.sqlite"),
                file("murmur.log"),
                file("murmur.pid"),
            ),
        )
        .expect("murmur.ini");
        let process = Command::new(binary)
            .args(["-ini".as_ref(), ini.as_os_str(), "-fg".as_ref()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start mumble-server");
        let started = Instant::now();
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "Murmur did not start: {}",
                log(root)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        Self {
            process,
            port,
            directory,
        }
    }

    /// Murmur's log, for a failed test's diagnostics.
    pub fn log(&self) -> String {
        log(self.directory.path())
    }
}

fn log(root: &Path) -> String {
    std::fs::read_to_string(root.join("murmur.log")).unwrap_or_default()
}

impl Drop for Murmur {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}
