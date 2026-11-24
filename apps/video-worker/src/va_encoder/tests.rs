// SPDX-License-Identifier: EUPL-1.2
//! The encoder core over VA-API on the host GPU, with synthetic screens and
//! cameras. rav1d decodes every frame in lockstep at each operating point, and
//! the tests measure the rate per second, the picture quality and the layers.
//! Local-only: they need a render node with AV1 encode (NUSCHL_TEST_RENDER_NODE,
//! default /dev/dri/renderD128).

use super::VaEncoder;
use cros_libva::{Image, Surface, UsageHint};
use nuschl_video_encode::{Config, Encoder, Layering};
use rav1d::{Decoder, PlanarImageComponent, Rav1dError, Settings};
use std::{path::PathBuf, time::Duration};

type Error = Box<dyn std::error::Error>;

/// One 4:2:0 picture.
struct Nv12 {
    width: usize,
    height: usize,
    y: Vec<u8>,
    /// Interleaved U and V at half resolution.
    uv: Vec<u8>,
}

impl Nv12 {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            y: vec![0; width * height],
            uv: vec![128; width * height / 2],
        }
    }
}

fn hash(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
    x ^ (x >> 33)
}

/// The README screenshot (1078 × 782: text, code, a photo, a formula) as BT.709
/// NV12, through the development shell's FFmpeg.
fn screenshot() -> &'static Nv12 {
    static PICTURE: std::sync::OnceLock<Nv12> = std::sync::OnceLock::new();
    PICTURE.get_or_init(|| {
        let output = std::process::Command::new(nuschl_testkit::program("ffmpeg"))
            .args(["-v", "error", "-i"])
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../docs/images/nuschl.png"
            ))
            .args([
                "-vf",
                "scale=out_color_matrix=bt709:out_range=tv,format=nv12",
                "-f",
                "rawvideo",
                "-",
            ])
            .output()
            .expect("FFmpeg runs");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let (width, height) = (1078, 782);
        assert_eq!(output.stdout.len(), width * height * 3 / 2);
        let (y, uv) = output.stdout.split_at(width * height);
        Nv12 {
            width,
            height,
            y: y.to_vec(),
            uv: uv.to_vec(),
        }
    })
}

/// A desktop of screenshot windows tiled at 1:1 from offset (`x`, `y`), with
/// the first `typed` characters of a line copied into the message box.
fn screen(width: usize, height: usize, x: usize, y: usize, typed: usize) -> Nv12 {
    let tile = screenshot();
    let mut picture = Nv12::new(width, height);
    for row in 0..height {
        let source = (row + y) % tile.height;
        for column in 0..width {
            picture.y[row * width + column] =
                tile.y[source * tile.width + (column + x) % tile.width];
        }
    }
    for row in 0..height / 2 {
        let source = (row + y / 2) % (tile.height / 2);
        for column in 0..width / 2 {
            let from = source * tile.width + 2 * ((column + x / 2) % (tile.width / 2));
            picture.uv[row * width + 2 * column..][..2].copy_from_slice(&tile.uv[from..from + 2]);
        }
    }
    // The message box of the first tile; the text comes from the first message.
    let (into_x, into_y, from_x, from_y) = (398, 734, 413, 124);
    for row in 0..22 {
        for column in 0..(typed * 9).min(500) {
            let (to_row, to_column) = (into_y + row, into_x + column);
            if to_row < height && to_column < width {
                picture.y[to_row * width + to_column] =
                    tile.y[(from_y + row) * tile.width + from_x + column];
            }
        }
    }
    picture
}

/// A camera view: lit gradients, moving texture and a moving disc, with
/// sensor noise in every frame.
fn camera(frame: u64, width: usize, height: usize) -> Nv12 {
    let mut picture = Nv12::new(width, height);
    let t = frame as f64;
    let (cx, cy) = (
        width as f64 * (0.5 + 0.3 * (t / 40.0).sin()),
        height as f64 * (0.5 + 0.25 * (t / 55.0).cos()),
    );
    let radius = height as f64 / 6.0;
    let pan = (t * 1.5) as usize;
    for row in 0..height {
        for column in 0..width {
            let texture = hash(((row / 4) as u64) << 32 ^ ((column + pan) / 4) as u64) % 40;
            let base =
                60.0 + 100.0 * column as f64 / width as f64 + 40.0 * row as f64 / height as f64;
            let (dx, dy) = (column as f64 - cx, row as f64 - cy);
            let disc = if dx * dx + dy * dy < radius * radius {
                50.0
            } else {
                0.0
            };
            let noise = (hash(frame << 40 ^ (row * width + column) as u64) % 5) as f64 - 2.0;
            picture.y[row * width + column] =
                (base + disc + texture as f64 + noise).clamp(16.0, 235.0) as u8;
        }
    }
    for row in 0..height / 2 {
        for column in 0..width / 2 {
            picture.uv[row * width + 2 * column] = (110 + column * 40 / width) as u8;
            picture.uv[row * width + 2 * column + 1] = (140 - row * 30 / height) as u8;
        }
    }
    picture
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

/// Luma PSNR and mean 8 × 8 SSIM of a decoded picture against its source,
/// over the source's area.
fn quality(decoded: &rav1d::Picture, source: &Nv12) -> (f64, f64) {
    let stride = decoded.stride(PlanarImageComponent::Y) as usize;
    let luma = decoded.plane(PlanarImageComponent::Y);
    let (width, height) = (source.width, source.height);
    let mut squared = 0f64;
    for row in 0..height {
        for column in 0..width {
            let d =
                f64::from(luma[row * stride + column]) - f64::from(source.y[row * width + column]);
            squared += d * d;
        }
    }
    let mse = squared / (width * height) as f64;
    let psnr = if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    };
    let (c1, c2) = ((0.01f64 * 255.0).powi(2), (0.03f64 * 255.0).powi(2));
    let (mut total, mut blocks) = (0.0, 0);
    for by in (0..height - 7).step_by(8) {
        for bx in (0..width - 7).step_by(8) {
            let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0.0, 0.0, 0.0, 0.0, 0.0);
            for row in by..by + 8 {
                for column in bx..bx + 8 {
                    let a = f64::from(source.y[row * width + column]);
                    let b = f64::from(luma[row * stride + column]);
                    sa += a;
                    sb += b;
                    saa += a * a;
                    sbb += b * b;
                    sab += a * b;
                }
            }
            let (ma, mb) = (sa / 64.0, sb / 64.0);
            let (va, vb, cov) = (
                saa / 64.0 - ma * ma,
                sbb / 64.0 - mb * mb,
                sab / 64.0 - ma * mb,
            );
            total += ((2.0 * ma * mb + c1) * (2.0 * cov + c2))
                / ((ma * ma + mb * mb + c1) * (va + vb + c2));
            blocks += 1;
        }
    }
    (psnr, total / f64::from(blocks))
}

fn decoder(operating_point: u8) -> Result<Decoder, Error> {
    let mut settings = Settings::new();
    settings.set_n_threads(1);
    settings.set_max_frame_delay(1);
    settings.set_operating_point(operating_point);
    Ok(Decoder::with_settings(&settings)?)
}

/// Sends one temporal unit and returns the picture it shows, if any.
fn decode(decoder: &mut Decoder, unit: &[u8]) -> Result<Option<rav1d::Picture>, Error> {
    let mut sent = decoder.send_data(unit.to_vec().into_boxed_slice(), None, None, None);
    let mut shown = None;
    loop {
        match decoder.get_picture() {
            Ok(picture) => shown = Some(picture),
            Err(Rav1dError::TryAgain) => {}
            Err(error) => return Err(error.into()),
        }
        match sent {
            Err(Rav1dError::TryAgain) => sent = decoder.send_pending_data(),
            Ok(()) => return Ok(shown),
            Err(error) => return Err(error.into()),
        }
    }
}

struct Sent {
    time: f64,
    bytes: usize,
    key: bool,
    temporal: u8,
    /// Per decoded operating point: PSNR and SSIM.
    quality: Vec<Option<(f64, f64)>>,
}

struct Run {
    encoder: Encoder<VaEncoder>,
    input: Surface<()>,
    decoders: Vec<Decoder>,
    width: usize,
    height: usize,
    fps: u32,
    frame: u64,
    captured: usize,
    sent: Vec<Sent>,
    /// Time spent in the encoder, re-encodes included.
    encoding: Duration,
}

impl Run {
    fn new(
        width: u16,
        height: u16,
        fps: u16,
        bitrate: u32,
        layering: Layering,
        screen: bool,
    ) -> Result<Self, Error> {
        let node = std::env::var_os("NUSCHL_TEST_RENDER_NODE")
            .map_or_else(|| PathBuf::from("/dev/dri/renderD128"), PathBuf::from);
        let backend = VaEncoder::open(&node, width, height)?;
        let input = backend.surface(UsageHint::USAGE_HINT_ENCODER)?;
        let config = Config {
            width,
            height,
            fps,
            bitrate,
            layering,
            screen,
            frame_bytes: if screen { 8 << 20 } else { 256 << 10 },
            window: Duration::from_millis(300),
            qindex: 1..=255,
        };
        let decoders = (0..layering_count(layering))
            .map(decoder)
            .collect::<Result<_, _>>()?;
        Ok(Self {
            encoder: Encoder::new(backend, config).ok_or("the picture needs more than one tile")?,
            input,
            decoders,
            width: usize::from(width),
            height: usize::from(height),
            fps: u32::from(fps),
            frame: 0,
            captured: 0,
            sent: Vec::new(),
            encoding: Duration::ZERO,
        })
    }

    fn seconds(
        &mut self,
        seconds: u32,
        source: impl Fn(u64, usize, usize) -> Nv12,
    ) -> Result<(), Error> {
        for _ in 0..seconds * self.fps {
            let picture = source(self.frame, self.width, self.height);
            upload(&self.input, &picture)?;
            let time = self.frame as f64 / f64::from(self.fps);
            let started = std::time::Instant::now();
            let encoded = self
                .encoder
                .encode(&self.input, Duration::from_secs_f64(time))?;
            self.encoding += started.elapsed();
            if let Some(encoded) = encoded {
                let mut measured = Vec::new();
                let points = self.decoders.len();
                for (point, decoder) in self.decoders.iter_mut().enumerate() {
                    let highest = (points - 1 - point) as u8;
                    let shown = decode(decoder, &encoded.data)?;
                    let expected = encoded.temporal_id <= highest;
                    assert_eq!(
                        shown.is_some(),
                        expected,
                        "operating point {point}, frame {}",
                        self.frame
                    );
                    measured.push(shown.map(|decoded| quality(&decoded, &picture)));
                }
                self.sent.push(Sent {
                    time,
                    bytes: encoded.data.len(),
                    key: encoded.keyframe,
                    temporal: encoded.temporal_id,
                    quality: measured,
                });
            }
            self.captured += 1;
            self.frame += 1;
        }
        Ok(())
    }

    /// Bits sent in each whole second of `range`.
    fn per_second(&self, range: std::ops::Range<u32>) -> Vec<f64> {
        range
            .map(|second| {
                self.sent
                    .iter()
                    .filter(|sent| sent.time.floor() as u32 == second)
                    .map(|sent| sent.bytes as f64 * 8.0)
                    .sum()
            })
            .collect()
    }

    /// Mean PSNR and SSIM at operating point 0 over frames sent in `range`.
    fn mean_quality(&self, range: std::ops::Range<f64>) -> (f64, f64) {
        let values: Vec<_> = self
            .sent
            .iter()
            .filter(|sent| range.contains(&sent.time))
            .filter_map(|sent| sent.quality[0])
            .collect();
        let n = values.len() as f64;
        (
            values.iter().map(|v| v.0).sum::<f64>() / n,
            values.iter().map(|v| v.1).sum::<f64>() / n,
        )
    }

    fn encode_ms(&self) -> f64 {
        self.encoding.as_secs_f64() * 1000.0 / self.captured as f64
    }

    fn keyframes(&self) -> usize {
        self.sent.iter().filter(|sent| sent.key).count()
    }
}

fn layering_count(layering: Layering) -> u8 {
    match layering {
        Layering::L1T1 => 1,
        Layering::L1T2 => 2,
        Layering::L1T3 => 3,
    }
}

fn percent(rates: &[f64], target: f64) -> Vec<String> {
    rates
        .iter()
        .map(|rate| format!("{:+.0}%", (rate / target - 1.0) * 100.0))
        .collect()
}

fn within(name: &str, rates: &[f64], target: f64, tolerance: f64) {
    eprintln!(
        "{name}: per second against {:.1} Mbit/s: {:?}",
        target / 1e6,
        percent(rates, target)
    );
    for rate in rates {
        assert!(
            (rate / target - 1.0).abs() <= tolerance,
            "{name}: {:?}",
            percent(rates, target)
        );
    }
}

#[test]
#[ignore = "requires a host DRM render node with VA-API AV1 encode"]
fn a_camera_holds_its_rate_at_every_layer() -> Result<(), Error> {
    let mut run = Run::new(1280, 720, 30, 2_000_000, Layering::L1T3, false)?;
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
    let mut run = Run::new(1920, 1080, 30, 4_000_000, Layering::L1T3, false)?;
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
    let mut run = Run::new(2560, 1440, 30, 1_500_000, Layering::L1T3, true)?;
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
        let mut run = Run::new(
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
