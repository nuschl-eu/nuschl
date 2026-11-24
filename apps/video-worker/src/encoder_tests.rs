// SPDX-License-Identifier: EUPL-1.2
//! The encoder core over a backend with synthetic screens and cameras. rav1d
//! decodes every frame in lockstep at each operating point, and the tests
//! measure the rate per second, the picture quality and the layers.

use nuschl_video_encode::{Backend, Config, Encoder, Layering};
use rav1d::{Decoder, PlanarImageComponent, Rav1dError, Settings};
use std::time::Duration;

pub(crate) type Error = Box<dyn std::error::Error>;

/// One 4:2:0 picture.
pub(crate) struct Nv12 {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) y: Vec<u8>,
    /// Interleaved U and V at half resolution.
    pub(crate) uv: Vec<u8>,
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
pub(crate) fn screenshot() -> &'static Nv12 {
    static PICTURE: std::sync::OnceLock<Nv12> = std::sync::OnceLock::new();
    PICTURE.get_or_init(|| {
        let output = std::process::Command::new(nuschl_testkit::program("ffmpeg"))
            .args(["-v", "error", "-i"])
            .arg(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../docs/images/nuschl.png"),
            )
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
pub(crate) fn screen(width: usize, height: usize, x: usize, y: usize, typed: usize) -> Nv12 {
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
pub(crate) fn camera(frame: u64, width: usize, height: usize) -> Nv12 {
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

/// Luma PSNR and mean 8 × 8 SSIM of a decoded picture against its source,
/// over the source's area.
pub(crate) fn quality(decoded: &rav1d::Picture, source: &Nv12) -> (f64, f64) {
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

pub(crate) fn decoder(operating_point: u8) -> Result<Decoder, Error> {
    let mut settings = Settings::new();
    settings.set_n_threads(1);
    settings.set_max_frame_delay(1);
    settings.set_operating_point(operating_point);
    Ok(Decoder::with_settings(&settings)?)
}

/// Sends one temporal unit and returns the picture it shows, if any.
pub(crate) fn decode(decoder: &mut Decoder, unit: &[u8]) -> Result<Option<rav1d::Picture>, Error> {
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

pub(crate) struct Sent {
    pub(crate) time: f64,
    pub(crate) bytes: usize,
    pub(crate) key: bool,
    pub(crate) temporal: u8,
    /// Per decoded operating point: PSNR and SSIM.
    pub(crate) quality: Vec<Option<(f64, f64)>>,
}

/// A backend's input for one test picture.
pub(crate) trait Picture: Backend {
    fn input(&self, picture: &Nv12) -> Result<Self::Input, Error>;
}

pub(crate) struct Run<B: Backend> {
    pub(crate) encoder: Encoder<B>,
    decoders: Vec<Decoder>,
    pub(crate) width: usize,
    pub(crate) height: usize,
    fps: u32,
    pub(crate) frame: u64,
    pub(crate) captured: usize,
    pub(crate) sent: Vec<Sent>,
    /// Time spent in the encoder, re-encodes included.
    encoding: Duration,
}

/// The encoder core's settings for a test stream.
pub(crate) fn config(
    width: u16,
    height: u16,
    fps: u16,
    bitrate: u32,
    layering: Layering,
    screen: bool,
) -> Config {
    Config {
        width,
        height,
        fps,
        bitrate,
        layering,
        screen,
        frame_bytes: if screen { 8 << 20 } else { 256 << 10 },
        window: Duration::from_millis(300),
        qindex: 1..=255,
    }
}

impl<B: Picture> Run<B>
where
    Error: From<B::Error>,
{
    pub(crate) fn new(backend: B, config: Config) -> Result<Self, Error> {
        let decoders = (0..config.layering.layers())
            .map(decoder)
            .collect::<Result<_, _>>()?;
        Ok(Self {
            width: usize::from(config.width),
            height: usize::from(config.height),
            fps: u32::from(config.fps),
            encoder: Encoder::new(backend, config).ok_or("the picture needs more than one tile")?,
            decoders,
            frame: 0,
            captured: 0,
            sent: Vec::new(),
            encoding: Duration::ZERO,
        })
    }

    pub(crate) fn seconds(
        &mut self,
        seconds: u32,
        source: impl Fn(u64, usize, usize) -> Nv12,
    ) -> Result<(), Error> {
        for _ in 0..seconds * self.fps {
            let picture = source(self.frame, self.width, self.height);
            let input = self.encoder.backend().input(&picture)?;
            let time = self.frame as f64 / f64::from(self.fps);
            let started = std::time::Instant::now();
            let encoded = self.encoder.encode(&input, Duration::from_secs_f64(time))?;
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
    pub(crate) fn per_second(&self, range: std::ops::Range<u32>) -> Vec<f64> {
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
    pub(crate) fn mean_quality(&self, range: std::ops::Range<f64>) -> (f64, f64) {
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

    pub(crate) fn encode_ms(&self) -> f64 {
        self.encoding.as_secs_f64() * 1000.0 / self.captured as f64
    }

    pub(crate) fn keyframes(&self) -> usize {
        self.sent.iter().filter(|sent| sent.key).count()
    }
}

pub(crate) fn percent(rates: &[f64], target: f64) -> Vec<String> {
    rates
        .iter()
        .map(|rate| format!("{:+.0}%", (rate / target - 1.0) * 100.0))
        .collect()
}

pub(crate) fn within(name: &str, rates: &[f64], target: f64, tolerance: f64) {
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
