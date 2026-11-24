// SPDX-License-Identifier: EUPL-1.2
//! The encoder core over libaom (crate::encoder_tests), at the software
//! path's 15 frames per second.

use super::Aom;
use crate::encoder_tests::{Error, Nv12, Picture, Run, camera, config, percent, screen, within};
use nuschl_video_encode::{Config, Layering};

/// Per second, against the target. A second holds 15 frames here, of an L1T3
/// stream 3 or 4 T0 frames that carry most of its bits, and libaom codes each
/// picture once, so frames after an overshoot drop in the next second.
const PER_SECOND: f64 = 0.35;

/// Each second within `PER_SECOND` of `target`, and their mean within 10 %.
fn holds(name: &str, rates: &[f64], target: f64) {
    within(name, rates, target, PER_SECOND);
    let mean = rates.iter().sum::<f64>() / rates.len() as f64;
    assert!(
        (mean / target - 1.0).abs() <= 0.1,
        "{name}: mean {mean:.0} bit/s"
    );
}

impl Picture for Aom {
    fn input(&self, picture: &Nv12) -> Result<Vec<u8>, Error> {
        let mut i420 = picture.y.clone();
        for offset in [0, 1] {
            i420.extend(picture.uv.iter().skip(offset).step_by(2));
        }
        Ok(i420)
    }
}

fn run(
    width: u16,
    height: u16,
    bitrate: u32,
    layering: Layering,
    screen: bool,
) -> Result<Run<Aom>, Error> {
    let config = Config {
        qindex: 4..=255,
        ..config(width, height, 15, bitrate, layering, screen)
    };
    let backend = Aom(nuschl_aom::Encoder::new(&nuschl_aom::Config {
        width,
        height,
        fps: 15,
        layers: layering.layers(),
        screen,
        threads: 2,
    })?);
    Run::new(backend, config)
}

#[test]
fn a_camera_holds_its_rate_and_each_operating_point_decodes() -> Result<(), Error> {
    let mut run = run(640, 360, 600_000, Layering::L1T3, false)?;
    run.seconds(10, camera)?;
    holds(
        "camera 360p15 L1T3 600 kbit/s",
        &run.per_second(2..10),
        600_000.0,
    );
    eprintln!("{:.2} ms per encode", run.encode_ms());
    assert_eq!(run.keyframes(), 1);
    for point in 0..3 {
        let values: Vec<_> = run
            .sent
            .iter()
            .filter_map(|sent| sent.quality[point])
            .collect();
        let psnr = values.iter().map(|v| v.0).sum::<f64>() / values.len() as f64;
        eprintln!(
            "operating point {point}: {} frames, PSNR {psnr:.2} dB",
            values.len()
        );
        assert!(psnr > 30.0, "operating point {point}");
    }
    Ok(())
}

#[test]
fn bitrate_changes_apply_without_keyframes_and_keyframes_come_on_request() -> Result<(), Error> {
    let mut run = run(960, 540, 1_200_000, Layering::L1T1, false)?;
    run.seconds(5, camera)?;
    run.encoder.set_bitrate(500_000);
    run.seconds(5, camera)?;
    run.encoder.request_keyframe();
    run.encoder.set_bitrate(900_000);
    run.seconds(5, camera)?;
    holds("at 1.2 Mbit/s", &run.per_second(2..5), 1_200_000.0);
    holds(
        "after the change to 500 kbit/s",
        &run.per_second(6..10),
        500_000.0,
    );
    holds(
        "after the change to 900 kbit/s",
        &run.per_second(11..15),
        900_000.0,
    );
    let keys: Vec<_> = run
        .sent
        .iter()
        .filter(|sent| sent.key)
        .map(|sent| sent.time)
        .collect();
    eprintln!(
        "keyframes at {keys:?} s, {} of {} frames sent, {:.2} ms per encode",
        run.sent.len(),
        run.captured,
        run.encode_ms()
    );
    assert_eq!(keys, [0.0, 10.0]);
    Ok(())
}

#[test]
fn a_screen_holds_its_rate_through_typing_and_scrolling() -> Result<(), Error> {
    let mut run = run(1280, 720, 800_000, Layering::L1T3, true)?;
    run.seconds(3, |_, w, h| screen(w, h, 0, 0, 0))?;
    run.seconds(3, |frame, w, h| screen(w, h, 0, 0, (frame % 45) as usize))?;
    run.seconds(3, |frame, w, h| {
        screen(w, h, 0, (frame % 45) as usize * 6, 0)
    })?;
    let rates = run.per_second(0..9);
    let (still, _) = run.mean_quality(1.0..3.0);
    let (typing, _) = run.mean_quality(4.0..6.0);
    let (scrolling, _) = run.mean_quality(7.0..9.0);
    let base: usize = run
        .sent
        .iter()
        .filter(|sent| sent.temporal == 0)
        .map(|sent| sent.bytes)
        .sum();
    let total: usize = run.sent.iter().map(|sent| sent.bytes).sum();
    eprintln!(
        "screen 720p15 L1T3 800 kbit/s per second: {:?}; still {still:.2} dB, typing \
         {typing:.2} dB, scrolling {scrolling:.2} dB; T0 {:.0}% of the bits, {:.2} ms per encode",
        percent(&rates, 800_000.0),
        base as f64 / total as f64 * 100.0,
        run.encode_ms()
    );
    for rate in &rates[1..] {
        assert!(
            *rate <= 800_000.0 * (1.0 + PER_SECOND),
            "{:?}",
            percent(&rates, 800_000.0)
        );
    }
    assert_eq!(run.keyframes(), 1);
    assert!(still > 30.0 && typing > 30.0);
    Ok(())
}
