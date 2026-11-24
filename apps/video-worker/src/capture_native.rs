// SPDX-License-Identifier: EUPL-1.2

#[cfg(windows)]
#[path = "capture/windows.rs"]
mod windows;

use crate::{
    capture_pixels::{Cadence, MappedPixels, convert, watch_control, write_frame},
    codec,
};
use nuschl_ipc::video::{
    CaptureFailure, CaptureLimits, HEADER_BYTES, Header, Kind, Settings, Source,
    write_capture_preview,
};
use std::{
    io::{self, Read as _, Write as _},
    sync::atomic::Ordering,
    time::Instant,
};

pub(crate) fn run(source: Source) -> Result<(), CaptureFailure> {
    capture(source).map_err(
        |error| match error.downcast_ref::<io::Error>().map(io::Error::kind) {
            Some(io::ErrorKind::Interrupted) => CaptureFailure::Cancelled,
            Some(io::ErrorKind::PermissionDenied) => CaptureFailure::Permission,
            _ => CaptureFailure::Software,
        },
    )
}

type Error = Box<dyn std::error::Error + Send + Sync>;

fn capture(source: Source) -> Result<(), Error> {
    let mut input = io::stdin();
    let mut header = [0; HEADER_BYTES];
    input.read_exact(&mut header)?;
    if Header::from_bytes(header)?
        != (Header {
            bytes: CaptureLimits::BYTES as u32,
            ..Header::empty(Kind::CaptureStart, 0)
        })
    {
        return Err("expected capture settings".into());
    }
    let mut limits = [0; CaptureLimits::BYTES];
    input.read_exact(&mut limits)?;
    let control = watch_control(source, CaptureLimits::from_bytes(source, limits)?, input)?;
    let clock = Instant::now();
    let mut encode_cadence = Cadence::default();
    let mut preview_cadence = Cadence::default();
    let on_frame = move |width: u32, height: u32, stride, bgra: &[u8]| {
        let elapsed = clock.elapsed();
        let mut limits = control.limits();
        limits.fps = limits.fps.min(15);
        limits.height = limits.height.min(720);
        let encode = encode_cadence.accept(elapsed, limits.fps);
        let preview =
            control.preview.load(Ordering::Relaxed) && preview_cadence.accept(elapsed, 15);
        if !(encode || preview) {
            return Ok(());
        }
        let raw = (
            u16::try_from(width & !1).map_err(io::Error::other)?,
            u16::try_from(height & !1).map_err(io::Error::other)?,
        );
        let Some(mapped) = MappedPixels::new(raw, stride, 4, 2, bgra) else {
            return Ok(());
        };
        let mut output = io::stdout().lock();
        if preview {
            let (width, height, pixels) = mapped.preview();
            write_capture_preview(&mut output, width, height, &pixels)?;
            output.flush()?;
        }
        if encode && let Some((width, height)) = limits.dimensions(source, raw.0, raw.1) {
            let settings = Settings {
                source,
                width,
                height,
                fps: limits.fps,
                bitrate: limits.bitrate,
            };
            if let Some(i420) = convert(&mapped, settings) {
                let packet = codec::encode(&codec::config(settings), settings, &i420)
                    .map_err(|_| io::Error::other("video encoder stopped"))?;
                let timestamp = u64::try_from(elapsed.as_micros())
                    .map_err(io::Error::other)?
                    .max(1);
                write_frame(
                    &mut output,
                    Kind::CaptureFrame,
                    settings,
                    timestamp,
                    true,
                    &packet,
                    &[],
                )?;
            }
        }
        Ok(())
    };
    #[cfg(windows)]
    return windows::run(source, on_frame);
    #[cfg(target_os = "macos")]
    nuschl_capture_macos::run(source == Source::Screen, on_frame).map_err(Into::into)
}
