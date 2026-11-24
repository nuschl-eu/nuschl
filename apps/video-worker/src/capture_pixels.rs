// SPDX-License-Identifier: EUPL-1.2

use nuschl_ipc::video::{
    CaptureLimits, HEADER_BYTES, Header, Kind, MAX_DESCRIPTOR, Settings, Source,
    fit_capture_preview,
};
use std::{
    io::{self, Read},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[cfg(target_os = "linux")]
pub(crate) const PREVIEW_FPS: u16 = 30;

#[derive(Default)]
pub(crate) struct Cadence {
    fps: u16,
    next: Duration,
}

impl Cadence {
    pub(crate) fn accept(&mut self, elapsed: Duration, fps: u16) -> bool {
        let interval = Duration::from_secs(1) / u32::from(fps);
        if fps != self.fps {
            self.fps = fps;
            self.next = elapsed;
        }
        if elapsed < self.next {
            return false;
        }
        self.next += interval;
        if self.next <= elapsed {
            self.next = elapsed + interval;
        }
        true
    }
}

pub(crate) struct MappedPixels<'a> {
    width: usize,
    height: usize,
    stride: usize,
    pixel_bytes: usize,
    red: usize,
    data: &'a [u8],
}

impl<'a> MappedPixels<'a> {
    pub(crate) fn new(
        raw: (u16, u16),
        stride: usize,
        pixel_bytes: usize,
        red: usize,
        data: &'a [u8],
    ) -> Option<Self> {
        let (width, height) = (usize::from(raw.0), usize::from(raw.1));
        if !matches!((pixel_bytes, red), (2, 0) | (3 | 4, 0 | 2)) {
            return None;
        }
        let row_bytes = width * pixel_bytes;
        if width == 0 || !width.is_multiple_of(2) || stride < row_bytes {
            return None;
        }
        let needed = stride
            .checked_mul(height.checked_sub(1)?)?
            .checked_add(row_bytes)?;
        Some(Self {
            width,
            height,
            stride,
            pixel_bytes,
            red,
            data: data.get(..needed)?,
        })
    }

    fn yuy2(&self) -> bool {
        self.pixel_bytes == 2
    }

    fn row(&self, row: usize, output_height: usize) -> &[u8] {
        let start = (row * self.height / output_height) * self.stride;
        &self.data[start..start + self.width * self.pixel_bytes]
    }

    fn rgb(&self, row: &[u8], x: usize) -> [u8; 3] {
        let offset = x * self.pixel_bytes;
        [
            row[offset + self.red],
            row[offset + 1],
            row[offset + 2 - self.red],
        ]
    }

    fn yuv(&self, row: &[u8], x: usize) -> (u8, u8, u8) {
        if self.yuy2() {
            let offset = x * 2;
            let pair = offset & !3;
            (row[offset], row[pair + 1], row[pair + 3])
        } else {
            let [r, g, b] = self.rgb(row, x);
            yuv(r, g, b)
        }
    }

    pub(crate) fn preview(&self) -> (u16, u16, Vec<u8>) {
        let (width, height) = fit_capture_preview(self.width as u16, self.height as u16);
        let (w, h) = (usize::from(width), usize::from(height));
        let columns: Vec<_> = (0..w).map(|x| x * self.width / w).collect();
        let mut pixels = Vec::with_capacity(w * h * 4);
        for output_row in 0..h {
            let row = self.row(output_row, h);
            for &column in &columns {
                let [r, g, b] = if self.yuy2() {
                    let (y, u, v) = self.yuv(row, column);
                    rgb_from_yuv(y, u, v)
                } else {
                    self.rgb(row, column)
                };
                pixels.extend_from_slice(&[b, g, r, 255]);
            }
        }
        (width, height, pixels)
    }
}

pub(crate) fn convert(raw: &MappedPixels<'_>, settings: Settings) -> Option<Vec<u8>> {
    let (width, height) = (usize::from(settings.width), usize::from(settings.height));
    if raw.width < width || raw.height < height {
        return None;
    }
    let len = settings.frame_bytes().ok()?;
    let mut planar = vec![0; len];
    let (y, uv) = planar.split_at_mut(width * height);
    let (u, v) = uv.split_at_mut(uv.len() / 2);
    let is_yuy2 = raw.yuy2();
    let columns: Vec<_> = (0..width).map(|x| x * raw.width / width).collect();
    for row in (0..height).step_by(2) {
        let top = raw.row(row, height);
        let bottom = raw.row(row + 1, height);
        for (pair, &[left, right]) in columns.as_chunks::<2>().0.iter().enumerate() {
            let chroma = (row / 2) * (width / 2) + pair;
            let luma = row * width + pair * 2;
            let quad = [
                raw.yuv(top, left),
                raw.yuv(top, right),
                raw.yuv(bottom, left),
                raw.yuv(bottom, right),
            ];
            let [first, second, third, fourth] = quad;
            [y[luma], y[luma + 1], y[luma + width], y[luma + width + 1]] =
                [first.0, second.0, third.0, fourth.0];
            let average = |a: u8, b: u8, c: u8, d: u8| {
                ((u16::from(a)
                    + u16::from(b)
                    + u16::from(c)
                    + u16::from(d)
                    + if is_yuy2 { 0 } else { 2 })
                    / 4) as u8
            };
            u[chroma] = average(first.1, second.1, third.1, fourth.1);
            v[chroma] = average(first.2, second.2, third.2, fourth.2);
        }
    }
    Some(planar)
}

fn yuv(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    let y = ((47 * r + 157 * g + 16 * b + 128) >> 8) + 16;
    let u = ((-26 * r - 87 * g + 113 * b + 128) >> 8) + 128;
    let v = ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;
    (y as u8, u as u8, v as u8)
}

fn rgb_from_yuv(y: u8, u: u8, v: u8) -> [u8; 3] {
    let y = 298 * (i32::from(y) - 16);
    let u = i32::from(u) - 128;
    let v = i32::from(v) - 128;
    [y + 459 * v, y - 55 * u - 136 * v, y + 541 * u]
        .map(|component| ((component + 128) >> 8).clamp(0, 255) as u8)
}

pub(crate) fn write_frame(
    output: &mut impl io::Write,
    kind: Kind,
    settings: Settings,
    timestamp_us: u64,
    keyframe: bool,
    av1: &[u8],
    descriptor: &[u8],
) -> io::Result<()> {
    if av1.is_empty()
        || av1.len() > settings.packet_limit(keyframe)
        || descriptor.len() > MAX_DESCRIPTOR
    {
        return Ok(());
    }
    let header = Header {
        kind,
        key: keyframe,
        id: timestamp_us,
        bytes: av1.len() as u32,
    };
    output.write_all(&header.to_bytes())?;
    output.write_all(&settings.to_bytes())?;
    output.write_all(&[descriptor.len() as u8])?;
    output.write_all(descriptor)?;
    output.write_all(av1)?;
    output.flush()
}

pub(crate) struct Control {
    limits: Mutex<CaptureLimits>,
    pub(crate) keyframe: AtomicBool,
    pub(crate) preview: AtomicBool,
}

impl Control {
    pub(crate) fn limits(&self) -> CaptureLimits {
        *self
            .limits
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

pub(crate) fn watch_control(
    source: Source,
    limits: CaptureLimits,
    mut input: impl Read + Send + 'static,
) -> io::Result<Arc<Control>> {
    let set_limits = Header {
        bytes: CaptureLimits::BYTES as u32,
        ..Header::empty(Kind::SetCaptureLimits, 0)
    };
    let set_preview = Header {
        bytes: 1,
        ..Header::empty(Kind::SetCapturePreview, 0)
    };
    let control = Arc::new(Control {
        limits: Mutex::new(limits),
        keyframe: AtomicBool::new(false),
        preview: AtomicBool::new(false),
    });
    let values = Arc::clone(&control);
    std::thread::Builder::new().spawn(move || {
        let mut header = [0; HEADER_BYTES];
        let mut target = [0; CaptureLimits::BYTES];
        loop {
            if let Err(error) = input.read_exact(&mut header) {
                std::process::exit(i32::from(error.kind() != io::ErrorKind::UnexpectedEof));
            }
            let header = Header::from_bytes(header).ok();
            if header == Some(Header::empty(Kind::CaptureKeyframe, 0)) {
                values.keyframe.store(true, Ordering::Relaxed);
                continue;
            }
            if header == Some(set_preview) {
                let mut visible = [0];
                if input.read_exact(&mut visible).is_err() || visible[0] > 1 {
                    std::process::exit(1);
                }
                values.preview.store(visible[0] != 0, Ordering::Relaxed);
                continue;
            }
            if header != Some(set_limits) || input.read_exact(&mut target).is_err() {
                std::process::exit(1);
            }
            let Ok(next) = CaptureLimits::from_bytes(source, target) else {
                std::process::exit(1);
            };
            *values
                .limits
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = next;
        }
    })?;
    Ok(control)
}
