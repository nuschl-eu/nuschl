// SPDX-License-Identifier: EUPL-1.2
#![cfg(target_os = "linux")]

use nuschl_ipc::video::{CaptureLimits, Encoder, Output, Settings, Source};
use std::{
    io::Write as _,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

const WORKER: &str = env!("CARGO_BIN_EXE_nuschl-video-worker");
const PROBE: &str = env!("CARGO_BIN_EXE_nuschl-video-probe");
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn confined_worker_recovers_and_the_probe_produces_decodable_av1() {
    let _serial = SERIAL.lock().await;
    nuschl_testkit::sandbox();
    let settings = Settings {
        source: Source::Screen,
        width: 160,
        height: 120,
        fps: 30,
        bitrate: 1_000_000,
    };
    let path = Path::new(WORKER);
    let mut encoder = Encoder::start(path.to_owned(), settings).unwrap();
    assert!(encoder.encode(vec![0; 1]).await.is_err());
    let input = vec![128; settings.frame_bytes().unwrap()];
    assert!(matches!(
        encoder.encode(input.clone()).await.unwrap(),
        Output::Keyframe(_)
    ));

    let workers = nuschl_testkit::running(path);
    assert_eq!(workers.len(), 1);
    assert!(
        Command::new(nuschl_testkit::program("kill"))
            .args(["-KILL", &workers[0].to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert!(encoder.encode(input.clone()).await.is_err());
    assert!(matches!(
        encoder.encode(input).await.unwrap(),
        Output::Keyframe(_)
    ));
    encoder.stop().await.unwrap();
    nuschl_testkit::none_running(path, Duration::from_secs(3)).await;

    let output = Command::new(PROBE).arg("--obu").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let decoded = decode_av1(&output.stdout);
    assert_eq!(decoded.len(), 3 * settings.frame_bytes().unwrap());
    assert!(decoded[8] < 85);
    assert!(decoded[24] > 170);
}

fn decode_av1(av1: &[u8]) -> Vec<u8> {
    let mut decoder = Command::new(nuschl_testkit::program("ffmpeg"))
        .args([
            "-v", "error", "-c:v", "libdav1d", "-f", "obu", "-i", "pipe:0", "-f", "rawvideo",
            "-pix_fmt", "yuv420p", "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    decoder.stdin.take().unwrap().write_all(av1).unwrap();
    let decoded = decoder.wait_with_output().unwrap();
    assert!(decoded.status.success());
    decoded.stdout
}

/// A separate PipeWire daemon owns the generator and socket. Passing its
/// connected FD exercises the portal-FD launch path without opening a portal.
#[tokio::test]
async fn video_capture_consumes_only_the_inherited_pipewire_connection() {
    capture_with_fixture(false).await;
}

#[cfg(feature = "vaapi")]
#[tokio::test]
#[ignore = "requires a host DRM render node with VA-API AV1 encode"]
async fn vaapi_capture_from_the_inherited_pipewire_connection() {
    capture_with_fixture(true).await;
}

async fn capture_with_fixture(hardware: bool) {
    let _serial = SERIAL.lock().await;
    nuschl_testkit::sandbox();
    let directory = nuschl_testkit::temp_dir();
    let run = directory.path();
    std::fs::write(
        run.join("fixture.conf"),
        r#"
context.properties = { core.daemon = true core.name = pipewire-0 }
context.spa-libs = {
  support.* = support/libspa-support
  videotestsrc = videotestsrc/libspa-videotestsrc
}
context.modules = [
  { name = libpipewire-module-protocol-native }
  { name = libpipewire-module-spa-node-factory }
  { name = libpipewire-module-client-node }
  { name = libpipewire-module-link-factory }
  { name = libpipewire-module-access }
  { name = libpipewire-module-metadata }
]
context.objects = [
  { factory = spa-node-factory
    args = { factory.name = videotestsrc node.name = capture-fixture node.driver = true node.param.Props = { live = true } }
  }
]
"#,
    )
    .unwrap();
    let mut pipewire = tokio::process::Command::new(nuschl_testkit::program("pipewire"));
    pipewire
        .env_clear()
        .env("PIPEWIRE_RUNTIME_DIR", run)
        .env("PIPEWIRE_CONFIG_DIR", run)
        .args(["-c", "fixture.conf"])
        .kill_on_drop(true);
    let mut daemon = pipewire.spawn().unwrap();

    let pw = |name: &str| {
        let mut command = tokio::process::Command::new(nuschl_testkit::program(name));
        command.env_clear().env("PIPEWIRE_RUNTIME_DIR", run);
        command
    };
    let socket = run.join("pipewire-0");
    let remote = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(socket) = std::os::unix::net::UnixStream::connect(&socket) {
                break socket;
            }
            assert!(
                daemon.try_wait().unwrap().is_none(),
                "PipeWire exited before its socket appeared"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let result = pw("pw-dump").output().await.unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let graph: Vec<serde_json::Value> = serde_json::from_slice(&result.stdout).unwrap();
    let node = graph
        .iter()
        .find(|object| object["info"]["props"]["node.name"] == "capture-fixture")
        .and_then(|object| object["id"].as_u64())
        .and_then(|id| u32::try_from(id).ok())
        .expect("the synthetic Nuschl source appears in its private graph");
    let start = if hardware {
        nuschl_ipc::video::Capture::start_vaapi
    } else {
        nuschl_ipc::video::Capture::start
    };
    let mut capture = start(
        Path::new(WORKER).to_owned(),
        remote.into(),
        Source::Screen,
        Some(node),
    )
    .unwrap();

    let join = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let output = pw("pw-link").arg("-i").output().await.unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let ports = String::from_utf8_lossy(&output.stdout);
            if let Some(port) = ports.lines().next() {
                break port.to_owned();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let port = join.await.expect("the worker creates its video input port");
    let outputs = pw("pw-link").arg("-o").output().await.unwrap();
    assert!(outputs.status.success());
    let available = String::from_utf8(outputs.stdout).unwrap();
    let producer = available
        .lines()
        .find(|line| line.starts_with("capture-fixture:"))
        .unwrap_or_else(|| panic!("video source has no output port in {available:?}"));
    let result = pw("pw-link")
        .args([producer, &port])
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let frame = capture
        .next()
        .await
        .expect("the worker returns bounded AV1");
    assert!(!frame.av1.is_empty());
    assert!(frame.keyframe);
    assert_eq!(frame.hardware, hardware);
    assert!(frame.width >= 16 && frame.height >= 16);
    assert!(frame.capture_time_us > 0);
    let raw = decode_av1(&frame.av1);
    assert_eq!(
        raw.len(),
        usize::from(frame.width) * usize::from(frame.height) * 3 / 2
    );
    if hardware {
        let mut sequence = frame.av1;
        tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                let frame = capture.next().await.unwrap();
                sequence.extend_from_slice(&frame.av1);
                if !frame.keyframe {
                    break;
                }
            }
        })
        .await
        .expect("VA-API generates interframes");
        assert!(decode_av1(&sequence).len() >= 2 * raw.len());
        // Screens get keyframes on request, also past vaav1enc's default
        // interval of 60 frames.
        tokio::time::timeout(Duration::from_secs(10), async {
            for _ in 0..90 {
                assert!(!capture.next().await.unwrap().keyframe);
            }
        })
        .await
        .expect("VA-API keeps sending interframes");
        capture.request_keyframe();
        tokio::time::timeout(Duration::from_millis(800), async {
            loop {
                let frame = capture.next().await.unwrap();
                if frame.keyframe {
                    assert_eq!(decode_av1(&frame.av1).len(), raw.len());
                    break;
                }
            }
        })
        .await
        .expect("VA-API generates a prompt keyframe on request");
    }
    assert!(capture.set_bitrate(1).is_err());
    capture.set_bitrate(900_000).unwrap();
    let updated_at = tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let current = capture.next().await.expect("the encoder keeps running");
            // VA-API takes a new bitrate within the current sequence.
            assert!(!hardware || !current.keyframe);
            if current.bitrate_bps == 900_000 {
                assert!(!current.av1.is_empty());
                break current.capture_time_us;
            }
        }
    })
    .await
    .expect("the new target is applied within the running capture");
    assert!(updated_at > frame.capture_time_us);
    let next = capture.next().await.unwrap();
    assert_eq!(next.bitrate_bps, 900_000);
    assert!(!hardware || !next.keyframe);
    let shorter_side = frame.width.min(frame.height);
    let limits = CaptureLimits {
        // The hardware fixture negotiates the encoder's minimum size.
        height: if hardware {
            shorter_side
        } else {
            (shorter_side / 2).max(16)
        },
        fps: 10,
        bitrate: 800_000,
    };
    let expected = limits
        .dimensions(Source::Screen, frame.width, frame.height)
        .unwrap();
    if !hardware {
        assert_ne!((frame.width, frame.height), expected);
    }
    capture.set_limits(limits).unwrap();
    capture.request_keyframe();
    let updated = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let changed = capture.next().await.expect("the encoder keeps running");
            if changed.keyframe
                && changed.bitrate_bps == limits.bitrate
                && (changed.width, changed.height) == expected
            {
                break changed;
            }
        }
    })
    .await
    .expect("the new dimensions arrive in the current PipeWire grant");
    assert_eq!(updated.bitrate_bps, limits.bitrate);
    if hardware {
        let last = tokio::time::timeout(Duration::from_secs(3), async {
            let mut last = updated.capture_time_us;
            for _ in 0..5 {
                last = capture.next().await.unwrap().capture_time_us;
            }
            last
        })
        .await
        .expect("the hardware capture keeps sending at the new rate");
        assert!(last - updated.capture_time_us >= 400_000);
    }
    assert_eq!(
        decode_av1(&updated.av1).len(),
        usize::from(expected.0) * usize::from(expected.1) * 3 / 2,
    );
    daemon.kill().await.unwrap();
    if !hardware {
        let error = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Err(error) = capture.next().await {
                    break error;
                }
            }
        })
        .await
        .expect("the capture reports a closed PipeWire connection");
        assert!(
            matches!(error, nuschl_ipc::video::Error::CaptureClosed),
            "{error}"
        );
    }
    capture.stop().await.unwrap();
    nuschl_testkit::none_running(Path::new(WORKER), Duration::from_secs(3)).await;
}
