// SPDX-License-Identifier: EUPL-1.2
//! The encoder core over VA-API on the host GPU (crate::encoder_tests).
//! Local-only: they need a render node with AV1 encode (NUSCHL_TEST_RENDER_NODE,
//! default /dev/dri/renderD128).

use super::VaEncoder;
use crate::encoder_tests::{
    Error, Nv12, Picture, Run, camera, config, decode, decoder, percent, quality, screen, within,
};
use cros_libva::{Image, Surface, UsageHint};
use nuschl_ipc::video::Source;
use nuschl_video_encode::Layering;
use std::{path::PathBuf, time::Duration};

impl Picture for VaEncoder {
    fn input(&self, picture: &Nv12) -> Result<Surface<()>, Error> {
        let surface = self.surface(UsageHint::USAGE_HINT_ENCODER)?;
        upload(&surface, picture)?;
        Ok(surface)
    }
}

/// The encoder core over VA-API on the test render node.
fn run(
    width: u16,
    height: u16,
    fps: u16,
    bitrate: u32,
    layering: Layering,
    screen: bool,
) -> Result<Run<VaEncoder>, Error> {
    let node = std::env::var_os("NUSCHL_TEST_RENDER_NODE")
        .map_or_else(|| PathBuf::from("/dev/dri/renderD128"), PathBuf::from);
    let backend = VaEncoder::open(&node, width, height)?;
    Run::new(
        backend,
        config(width, height, fps, bitrate, layering, screen),
    )
}

fn upload(surface: &Surface<()>, picture: &Nv12) -> Result<(), Error> {
    let mut image = Image::derive_from(surface, (picture.width as u32, picture.height as u32))?;
    let va = *image.image();
    let data = image.as_mut();
    for (plane, rows, source) in [
        (0, picture.height, &picture.y),
        (1, picture.height / 2, &picture.uv),
    ] {
        let (offset, pitch) = (va.offsets[plane] as usize, va.pitches[plane] as usize);
        for row in 0..rows {
            let line = &source[row * picture.width..][..picture.width];
            data[offset + row * pitch..][..picture.width].copy_from_slice(line);
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires a host DRM render node with VA-API AV1 encode"]
fn a_camera_holds_its_rate_at_every_layer() -> Result<(), Error> {
    let mut run = run(1280, 720, 30, 2_000_000, Layering::L1T3, false)?;
    run.seconds(12, camera)?;
    within(
        "camera 720p30 L1T3 2 Mbit/s",
        &run.per_second(2..12),
        2_000_000.0,
        0.15,
    );
    eprintln!("{:.2} ms per encode", run.encode_ms());
    assert_eq!(run.keyframes(), 1);
    // Each operating point decodes its frames to the source.
    for point in 0..3 {
        let values: Vec<_> = run
            .sent
            .iter()
            .filter_map(|sent| sent.quality[point])
            .collect();
        let psnr = values.iter().map(|v| v.0).sum::<f64>() / values.len() as f64;
        let ssim = values.iter().map(|v| v.1).sum::<f64>() / values.len() as f64;
        eprintln!(
            "operating point {point}: {} frames, PSNR {psnr:.2} dB, SSIM {ssim:.4}",
            values.len()
        );
        assert!(psnr > 30.0 && ssim > 0.85, "operating point {point}");
    }
    let layers: Vec<_> = (0..3)
        .map(|layer| {
            run.sent
                .iter()
                .filter(|sent| sent.temporal <= layer)
                .map(|sent| sent.bytes)
                .sum::<usize>()
        })
        .collect();
    eprintln!(
        "layer shares: T0 {:.0}%, T0+T1 {:.0}%",
        layers[0] as f64 / layers[2] as f64 * 100.0,
        layers[1] as f64 / layers[2] as f64 * 100.0
    );
    Ok(())
}

#[test]
#[ignore = "requires a host DRM render node with VA-API AV1 encode"]
fn bitrate_changes_apply_without_keyframes_and_keyframes_come_on_request() -> Result<(), Error> {
    let mut run = run(1920, 1080, 30, 4_000_000, Layering::L1T3, false)?;
    run.seconds(6, camera)?;
    run.encoder.set_bitrate(1_500_000);
    run.seconds(6, camera)?;
    run.encoder.request_keyframe();
    run.encoder.set_bitrate(3_000_000);
    run.seconds(6, camera)?;
    within(
        "camera 1080p30 at 4 Mbit/s",
        &run.per_second(2..6),
        4_000_000.0,
        0.15,
    );
    within(
        "after the change to 1.5 Mbit/s",
        &run.per_second(7..12),
        1_500_000.0,
        0.15,
    );
    within(
        "after the change to 3 Mbit/s",
        &run.per_second(13..18),
        3_000_000.0,
        0.15,
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
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[1], 12.0);
    Ok(())
}

#[test]
#[ignore = "requires a host DRM render node with VA-API AV1 encode"]
fn a_screen_holds_its_rate_through_typing_scrolling_and_a_page_change() -> Result<(), Error> {
    let mut run = run(2560, 1440, 30, 1_500_000, Layering::L1T3, true)?;
    run.seconds(4, |_, w, h| screen(w, h, 0, 0, 0))?;
    run.seconds(4, |frame, w, h| {
        screen(w, h, 0, 0, (frame % 120) as usize / 2)
    })?;
    run.seconds(4, |frame, w, h| {
        screen(w, h, 0, (frame % 120) as usize * 6, 0)
    })?;
    run.seconds(4, |_, w, h| screen(w, h, 539, 391, 0))?;
    let rates = run.per_second(0..16);
    eprintln!(
        "screen 1440p30 L1T3 1.5 Mbit/s per second: {:?}",
        percent(&rates, 1_500_000.0)
    );
    let peak = run.sent.iter().map(|sent| sent.bytes).max().unwrap_or(0);
    let (still_psnr, still_ssim) = run.mean_quality(2.0..4.0);
    let (typing_psnr, typing_ssim) = run.mean_quality(5.0..8.0);
    let (scroll_psnr, scroll_ssim) = run.mean_quality(9.0..12.0);
    let (page_psnr, page_ssim) = run.mean_quality(14.0..16.0);
    eprintln!(
        "still {still_psnr:.2} dB {still_ssim:.4}, typing {typing_psnr:.2} dB {typing_ssim:.4}, \
         scrolling {scroll_psnr:.2} dB {scroll_ssim:.4}, new page {page_psnr:.2} dB {page_ssim:.4}; \
         largest frame {peak} bytes, {} of {} frames sent, {:.2} ms per encode",
        run.sent.len(),
        run.captured,
        run.encode_ms()
    );
    for rate in &rates[1..] {
        assert!(
            *rate <= 1_500_000.0 * 1.25,
            "{:?}",
            percent(&rates, 1_500_000.0)
        );
    }
    assert_eq!(run.keyframes(), 1);
    assert!(still_psnr > 30.0 && page_psnr > 25.0);
    Ok(())
}

/// The same frames through today's path: GStreamer's vaav1enc in QVBR at the
/// quality capture/vaapi.rs gives screens. Returns the temporal units.
#[cfg(feature = "vaapi")]
fn qvbr(picture: &Nv12, frames: usize, fps: u32, bitrate: u32) -> Result<Vec<Vec<u8>>, Error> {
    use gstreamer::{self as gst, prelude::*};
    use gstreamer_app::{AppSink, AppSrc};
    gst::init()?;
    for path in env!("NUSCHL_VIDEO_GST_PLUGINS").split(':') {
        gst::Plugin::load_file(path)?;
    }
    let (width, height) = (picture.width, picture.height);
    let description = format!(
        "appsrc name=source format=time caps=video/x-raw,format=NV12,width={width},height={height},framerate={fps}/1 ! \
         vaav1enc name=encoder bitrate={} target-percentage=100 hierarchical-level=1 ref-frames=1 key-int-max=1024 ! \
         av1parse ! video/x-av1,stream-format=obu-stream,alignment=tu ! appsink name=sink sync=false",
        bitrate / 1000
    );
    let pipeline = gst::parse::launch(&description)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline")?;
    let encoder = pipeline.by_name("encoder").ok_or("no encoder")?;
    encoder.set_property_from_str("rate-control", "qvbr");
    encoder.set_property("qp", 112u32);
    let source = pipeline
        .by_name("source")
        .and_then(|element| element.downcast::<AppSrc>().ok())
        .ok_or("no source")?;
    let sink = pipeline
        .by_name("sink")
        .and_then(|element| element.downcast::<AppSink>().ok())
        .ok_or("no sink")?;
    pipeline.set_state(gst::State::Playing)?;
    for frame in 0..frames {
        let mut buffer = gst::Buffer::with_size(width * height * 3 / 2)?;
        let writable = buffer.get_mut().ok_or("shared buffer")?;
        writable.set_pts(gst::ClockTime::from_nseconds(
            frame as u64 * 1_000_000_000 / u64::from(fps),
        ));
        let mut map = writable.map_writable()?;
        map[..width * height].copy_from_slice(&picture.y);
        map[width * height..].copy_from_slice(&picture.uv);
        drop(map);
        source.push_buffer(buffer)?;
    }
    source.end_of_stream()?;
    let mut units = Vec::new();
    while let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_seconds(10)) {
        units.push(sample.buffer().ok_or("no buffer")?.map_readable()?.to_vec());
        if units.len() == frames {
            break;
        }
    }
    pipeline.set_state(gst::State::Null)?;
    Ok(units)
}

#[cfg(feature = "vaapi")]
#[test]
#[ignore = "requires a host DRM render node with VA-API AV1 encode"]
fn a_still_screen_against_todays_qvbr() -> Result<(), Error> {
    for (width, height, bitrate) in [(2560, 1440, 6_000_000), (3440, 1440, 8_000_000)] {
        let fps = 30;
        let picture = screen(width, height, 0, 0, 0);
        let mut run = run(
            width as u16,
            height as u16,
            fps as u16,
            bitrate,
            Layering::L1T3,
            true,
        )?;
        run.seconds(4, |_, w, h| screen(w, h, 0, 0, 0))?;
        let (psnr, ssim) = run.mean_quality(1.0..4.0);
        let rate: f64 = run.per_second(1..4).iter().sum::<f64>() / 3.0;
        let first = &run.sent[0];
        let first_quality = first.quality[0].unwrap_or_default();

        let units = qvbr(&picture, 4 * fps as usize, fps, bitrate)?;
        let mut decoder = decoder(0)?;
        let mut measured = Vec::new();
        for unit in &units {
            if let Some(decoded) = decode(&mut decoder, unit)? {
                measured.push(quality(&decoded, &picture));
            }
        }
        let still = &measured[fps as usize..];
        let qvbr_psnr = still.iter().map(|v| v.0).sum::<f64>() / still.len() as f64;
        let qvbr_ssim = still.iter().map(|v| v.1).sum::<f64>() / still.len() as f64;
        let qvbr_rate = units[fps as usize..]
            .iter()
            .map(|unit| unit.len() as f64 * 8.0)
            .sum::<f64>()
            / 3.0;
        eprintln!(
            "{width}×{height} still screen at {:.0} Mbit/s, seconds 1 to 4:\n  \
             encoder core: {psnr:.2} dB, SSIM {ssim:.4}, {:.2} Mbit/s; keyframe {} bytes at {:.2} dB, SSIM {:.4}\n  \
             QVBR 112:     {qvbr_psnr:.2} dB, SSIM {qvbr_ssim:.4}, {:.2} Mbit/s; keyframe {} bytes at {:.2} dB, SSIM {:.4}",
            f64::from(bitrate) / 1e6,
            rate / 1e6,
            first.bytes,
            first_quality.0,
            first_quality.1,
            qvbr_rate / 1e6,
            units[0].len(),
            measured[0].0,
            measured[0].1,
        );
        assert!(psnr >= qvbr_psnr && ssim >= qvbr_ssim);
    }
    Ok(())
}

/// VCN 4 codes 1920 × 1080 as 1920 × 1082 and 3440 × 1440 as 3456 × 1440,
/// with the picture's size as the render size. The decode worker shows the
/// picture's size, from rav1d and from VA-API.
#[test]
#[ignore = "requires a host DRM render node with VA-API AV1 encode and decode"]
fn decoders_show_the_picture_size() -> Result<(), Error> {
    for (source, width, height, layering) in [
        (Source::Camera, 1920, 1080, Layering::L1T1),
        (Source::Screen, 3440, 1440, Layering::L1T3),
    ] {
        let screen = source == Source::Screen;
        let mut run = run(width, height, 30, 4_000_000, layering, screen)?;
        let mut units = Vec::new();
        for frame in 0..4 {
            let input = run
                .encoder
                .backend()
                .input(&camera(frame, run.width, run.height))?;
            let time = Duration::from_secs(frame) / 30;
            units.extend(
                run.encoder
                    .encode(&input, time)?
                    .map(|encoded| encoded.data),
            );
        }
        let decoded = decode(&mut decoder(0)?, &units[0])?.ok_or("no picture")?;
        eprintln!(
            "{width} × {height} codes as {} × {}",
            decoded.width(),
            decoded.height()
        );
        for hardware in [false, true] {
            let shown = crate::decode::shown_sizes(hardware, source, &units)?;
            assert_eq!(
                shown,
                [(width, height)].repeat(units.len()),
                "hardware {hardware}"
            );
        }
    }
    Ok(())
}
