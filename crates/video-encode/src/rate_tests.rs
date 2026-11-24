// SPDX-License-Identifier: EUPL-1.2
//! The rate controller on synthetic frame-size traces. A model backend codes a
//! frame in bits = complexity × pixels ÷ AC step with ±20 % noise; inter frames
//! cost more the further back their reference is.

use super::*;
use crate::av1::ac_quantizer;
use std::convert::Infallible;

#[derive(Clone, Copy)]
struct Scene {
    /// Bits × AC step per pixel of a keyframe.
    intra: f64,
    /// The same for an inter frame predicting from the previous frame.
    inter: f64,
}

struct Model {
    pixels: f64,
    seed: u64,
    calls: usize,
}

impl Backend for Model {
    type Input = Scene;
    type Error = Infallible;

    fn encode(&mut self, scene: &Scene, frame: &Frame<'_>) -> Result<Vec<u8>, Infallible> {
        self.calls += 1;
        self.seed = self
            .seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let noise = 0.8 + 0.4 * (self.seed >> 11) as f64 / (1u64 << 53) as f64;
        let header = frame.header;
        let complexity = if header.key {
            scene.intra
        } else {
            let reference = frame.slots[usize::from(header.reference)].expect("a reference");
            scene.inter * (1.0 + 0.25 * (frame.id - reference - 1) as f64)
        };
        let bits = complexity * noise * self.pixels / f64::from(ac_quantizer(header.qindex));
        Ok(vec![0; (bits / 8.0).max(8.0) as usize])
    }
}

struct Run {
    encoder: Encoder<Model>,
    fps: u32,
    frame: u32,
    /// Per captured frame: its time in seconds and the frame if sent.
    log: Vec<(f64, Option<Encoded>)>,
}

impl Run {
    fn new(config: Config) -> Self {
        let pixels = f64::from(config.width) * f64::from(config.height);
        let fps = u32::from(config.fps);
        let model = Model {
            pixels,
            seed: 7,
            calls: 0,
        };
        Self {
            encoder: Encoder::new(model, config).expect("one tile"),
            fps,
            frame: 0,
            log: Vec::new(),
        }
    }

    fn seconds(&mut self, seconds: u32, scene: impl Fn(f64) -> Scene) {
        for _ in 0..seconds * self.fps {
            let now = Duration::from_secs_f64(f64::from(self.frame) / f64::from(self.fps));
            let encoded = self.encoder.encode(&scene(now.as_secs_f64()), now).unwrap();
            self.log.push((now.as_secs_f64(), encoded));
            self.frame += 1;
        }
    }

    /// Bits sent in each whole second from `from` to `to`.
    fn per_second(&self, from: u32, to: u32) -> Vec<f64> {
        (from..to)
            .map(|second| {
                self.log
                    .iter()
                    .filter(|(time, _)| time.floor() as u32 == second)
                    .filter_map(|(_, frame)| frame.as_ref())
                    .map(|frame| frame.data.len() as f64 * 8.0)
                    .sum()
            })
            .collect()
    }

    fn dropped(&self, from: u32, to: u32) -> usize {
        self.log
            .iter()
            .filter(|(time, frame)| {
                (f64::from(from)..f64::from(to)).contains(time) && frame.is_none()
            })
            .count()
    }

    fn keyframes(&self) -> usize {
        self.log
            .iter()
            .filter(|(_, frame)| frame.as_ref().is_some_and(|frame| frame.keyframe))
            .count()
    }
}

fn camera(bitrate: u32, layering: Layering) -> Config {
    Config {
        width: 1280,
        height: 720,
        fps: 30,
        bitrate,
        layering,
        screen: false,
        frame_bytes: 256 << 10,
        window: Duration::from_millis(300),
        qindex: 20..=255,
    }
}

fn within(rates: &[f64], target: f64, tolerance: f64) {
    for (second, rate) in rates.iter().enumerate() {
        let error = rate / target - 1.0;
        assert!(
            error.abs() <= tolerance,
            "second {second}: {rate:.0} bit/s for {target:.0} ({rates:.0?})"
        );
    }
}

/// Motion that varies over seconds, as a camera's does.
fn moving(time: f64) -> Scene {
    Scene {
        intra: 120.0,
        inter: 10.0 * (1.0 + 0.5 * (time * 0.7).sin()),
    }
}

#[test]
fn holds_the_target_each_second() {
    for layering in [Layering::L1T1, Layering::L1T3] {
        let mut run = Run::new(camera(2_000_000, layering));
        run.seconds(20, moving);
        within(&run.per_second(2, 20), 2_000_000.0, 0.1);
        assert_eq!(run.dropped(2, 20), 0, "{layering:?}");
        assert_eq!(run.keyframes(), 1);
    }
}

#[test]
fn bitrate_changes_apply_without_keyframes() {
    let mut run = Run::new(camera(4_000_000, Layering::L1T3));
    run.seconds(8, moving);
    run.encoder.set_bitrate(1_000_000);
    run.seconds(8, moving);
    run.encoder.set_bitrate(3_000_000);
    run.seconds(8, moving);
    within(&run.per_second(2, 8), 4_000_000.0, 0.1);
    within(&run.per_second(9, 16), 1_000_000.0, 0.1);
    within(&run.per_second(17, 24), 3_000_000.0, 0.1);
    assert_eq!(run.keyframes(), 1);
}

#[test]
fn keyframes_come_on_request() {
    let mut run = Run::new(camera(2_000_000, Layering::L1T3));
    run.seconds(3, moving);
    run.encoder.request_keyframe();
    run.seconds(3, moving);
    assert_eq!(run.keyframes(), 2);
    let second = run
        .log
        .iter()
        .filter_map(|(_, frame)| frame.as_ref())
        .filter(|frame| frame.keyframe)
        .nth(1);
    assert!(second.is_some_and(|frame| frame.descriptor.len() > 3));
}

#[test]
fn drops_frames_under_a_tight_budget() {
    // Detail kept at a quantizer of at most 120, at a third of what it needs.
    let config = Config {
        qindex: 20..=120,
        ..camera(600_000, Layering::L1T3)
    };
    let mut run = Run::new(config);
    run.seconds(10, moving);
    let rates = run.per_second(1, 10);
    assert!(
        rates.iter().all(|&rate| rate <= 600_000.0 * 1.15),
        "{rates:.0?}"
    );
    let dropped = run.dropped(1, 10);
    assert!(dropped > 9 * 30 / 2, "{dropped}");
    // Sent frames keep the quantizer limit.
    assert!(
        run.log
            .iter()
            .filter_map(|(_, frame)| frame.as_ref())
            .all(|frame| frame.qindex <= 120)
    );
}

#[test]
fn a_change_after_a_still_screen_is_coded_again() {
    let config = Config {
        width: 1920,
        height: 1080,
        screen: true,
        frame_bytes: 8 << 20,
        qindex: 1..=255,
        ..camera(3_000_000, Layering::L1T3)
    };
    let mut run = Run::new(config);
    let still = Scene {
        intra: 200.0,
        inter: 0.002,
    };
    run.seconds(5, |_| still);
    let encoded = run.log.iter().filter(|(_, frame)| frame.is_some()).count();
    let calls_before = run.encoder.backend().calls;
    assert_eq!(calls_before, encoded);
    // A slide change: every frame now differs.
    run.seconds(5, |_| Scene {
        intra: 200.0,
        inter: 30.0,
    });
    assert!(
        run.encoder.backend().calls > run.log.iter().filter(|(_, frame)| frame.is_some()).count()
    );
    // No frame exceeds the 300 ms window by much, and the rate recovers.
    let window = 3_000_000.0 * 0.3;
    for (_, frame) in &run.log {
        if let Some(frame) = frame {
            assert!((frame.data.len() * 8) as f64 <= window * 1.5);
        }
    }
    within(&run.per_second(7, 10), 3_000_000.0, 0.15);
}
