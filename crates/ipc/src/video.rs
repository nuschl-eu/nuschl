// SPDX-License-Identifier: EUPL-1.2
//! Video settings and bounded binary framing for the local video worker.

mod capture;
mod decode;
#[cfg(all(target_os = "linux", feature = "video-portal"))]
pub mod portal;
mod preview;
mod supervisor;

pub use capture::{Capture, VideoFrame};
pub use decode::{DecodeError, DecodedFrame, Decoder};
#[doc(hidden)]
pub use decode::{decode_frame_bytes, decode_packet_limit, source_from_byte, source_to_byte};
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub use preview::read_capture_start;
#[doc(hidden)]
pub use preview::{PREVIEW_HEIGHT, PREVIEW_WIDTH, fit_capture_preview, write_capture_preview};
pub use supervisor::{Encoder, Error, Output};

use std::io;

pub const HEADER_BYTES: usize = 16;
/// Whether this build can grant the video worker a VA-API render device.
pub const VAAPI_AVAILABLE: bool =
    cfg!(target_os = "linux") && option_env!("NUSCHL_SOFTWARE_VIDEO_ONLY").is_none();
const KIB: usize = 1024;
/// The largest screen frame, keyframe or interframe. The largest keyframe
/// measured from VA-API QVBR, of a 3840 × 2160 screen of noise, was 4.45 MB.
pub const SCREEN_FRAME_BYTES: usize = 8 * 1024 * KIB;
const KEY: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    Camera,
    Screen,
}

impl Source {
    pub fn max_dimensions(self) -> (u16, u16) {
        match self {
            Self::Camera => (1920, 1080),
            Self::Screen => (3840, 2160),
        }
    }
}

/// Fixed capture worker exit codes. The supervisor can report them without
/// reading diagnostics produced by native libraries.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[repr(u8)]
pub enum CaptureFailure {
    #[error("The hardware capture pipeline stopped.")]
    Hardware = 40,
    #[error("The hardware capture pipeline could not negotiate a video format.")]
    HardwareFormat = 41,
    #[error("A hardware capture resource became unavailable.")]
    HardwareResource = 42,
    #[error("The software capture pipeline stopped.")]
    Software = 43,
    #[error("Video source selection was cancelled.")]
    Cancelled = 44,
    #[error("The operating system denied video capture.")]
    Permission = 45,
}

/// The longest pipeline message a capture worker reports with its failure.
pub const CAPTURE_DETAIL_BYTES: usize = 200;

/// Report why a capture stopped: the reason byte, then the start of the
/// pipeline's message.
pub fn write_capture_failure(
    output: &mut impl io::Write,
    reason: CaptureFailure,
    detail: &str,
) -> io::Result<()> {
    let detail = &detail[..detail.floor_char_boundary(CAPTURE_DETAIL_BYTES)];
    let header = Header {
        bytes: 1 + detail.len() as u32,
        ..Header::empty(Kind::CaptureFailed, 0)
    };
    output.write_all(&header.to_bytes())?;
    output.write_all(&[reason as u8])?;
    output.write_all(detail.as_bytes())?;
    output.flush()
}

impl CaptureFailure {
    fn from_exit(code: i32) -> Option<Self> {
        [
            Self::Hardware,
            Self::HardwareFormat,
            Self::HardwareResource,
            Self::Software,
            Self::Cancelled,
            Self::Permission,
        ]
        .into_iter()
        .find(|reason| i32::from(*reason as u8) == code)
    }
}

/// The sender's current caps. Height limits the shorter side of the source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureLimits {
    pub height: u16,
    pub fps: u16,
    pub bitrate: u32,
}

impl CaptureLimits {
    pub const BYTES: usize = 8;

    pub fn for_source(source: Source, bitrate: u32) -> Self {
        Self {
            height: source.max_dimensions().1,
            fps: 120,
            bitrate,
        }
    }

    pub fn validate(self, source: Source) -> io::Result<()> {
        if !(16..=source.max_dimensions().1).contains(&self.height)
            || !(1..=120).contains(&self.fps)
            || !Settings::valid_bitrate(self.bitrate)
        {
            return Err(invalid());
        }
        Ok(())
    }

    /// Fit the worker's frame budget and the selected quality while keeping
    /// the source's proportions, rounded down to even I420 dimensions.
    pub fn dimensions(self, source: Source, width: u16, height: u16) -> Option<(u16, u16)> {
        let (width, height) = (u32::from(width), u32::from(height));
        if width.min(height) < 16 {
            return None;
        }
        let (max_width, max_height) = source.max_dimensions();
        let (numerator, denominator) = [
            (width, width),
            (max_width.into(), width),
            (max_height.into(), height),
            (self.height.into(), width.min(height)),
        ]
        .into_iter()
        .min_by(|(left, ld), (right, rd)| (left * rd).cmp(&(right * ld)))?;
        let [width, height] =
            [width, height].map(|side| ((side * numerator / denominator) & !1) as u16);
        (width.min(height) >= 16).then_some((width, height))
    }

    pub fn to_bytes(self) -> [u8; Self::BYTES] {
        let [h1, h2] = self.height.to_be_bytes();
        let [f1, f2] = self.fps.to_be_bytes();
        let [b1, b2, b3, b4] = self.bitrate.to_be_bytes();
        [h1, h2, f1, f2, b1, b2, b3, b4]
    }

    pub fn from_bytes(source: Source, bytes: [u8; Self::BYTES]) -> io::Result<Self> {
        let [h1, h2, f1, f2, b1, b2, b3, b4] = bytes;
        let limits = Self {
            height: u16::from_be_bytes([h1, h2]),
            fps: u16::from_be_bytes([f1, f2]),
            bitrate: u32::from_be_bytes([b1, b2, b3, b4]),
        };
        limits.validate(source)?;
        Ok(limits)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Settings {
    pub source: Source,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub bitrate: u32,
}

impl Settings {
    pub const BYTES: usize = 11;

    pub fn frame_bytes(self) -> io::Result<usize> {
        let (width, height) = self.source.max_dimensions();
        if !(16..=width).contains(&self.width)
            || !(16..=height).contains(&self.height)
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
            || !(1..=120).contains(&self.fps)
            || !Self::valid_bitrate(self.bitrate)
        {
            return Err(invalid());
        }
        Ok(usize::from(self.width) * usize::from(self.height) * 3 / 2)
    }

    pub fn valid_bitrate(bitrate: u32) -> bool {
        (64_000..=50_000_000).contains(&bitrate)
    }

    pub fn packet_limit(self, key: bool) -> usize {
        match self.source {
            Source::Camera if key => 256 * KIB,
            Source::Camera => 128 * KIB,
            Source::Screen => SCREEN_FRAME_BYTES,
        }
    }

    pub fn to_bytes(self) -> [u8; Self::BYTES] {
        let mut bytes = [0; Self::BYTES];
        bytes[0] = match self.source {
            Source::Camera => 0,
            Source::Screen => 1,
        };
        bytes[1..3].copy_from_slice(&self.width.to_be_bytes());
        bytes[3..5].copy_from_slice(&self.height.to_be_bytes());
        bytes[5..7].copy_from_slice(&self.fps.to_be_bytes());
        bytes[7..].copy_from_slice(&self.bitrate.to_be_bytes());
        bytes
    }

    pub fn from_bytes(bytes: [u8; Self::BYTES]) -> io::Result<Self> {
        let [source, w1, w2, h1, h2, f1, f2, b1, b2, b3, b4] = bytes;
        let settings = Self {
            source: match source {
                0 => Source::Camera,
                1 => Source::Screen,
                _ => return Err(invalid()),
            },
            width: u16::from_be_bytes([w1, w2]),
            height: u16::from_be_bytes([h1, h2]),
            fps: u16::from_be_bytes([f1, f2]),
            bitrate: u32::from_be_bytes([b1, b2, b3, b4]),
        };
        settings.frame_bytes()?;
        Ok(settings)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Kind {
    Configure = 1,
    Frame,
    Ready,
    Encoded,
    Dropped,
    CaptureFrame,
    HardwareFrame,
    Decode = 10,
    Decoded,
    ResetDecoder,
    CaptureKeyframe = 20,
    CaptureStart,
    SetCaptureLimits,
    SetCapturePreview,
    CapturePreview,
    CaptureFailed,
}

/// Each reply carries the ID from its request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub kind: Kind,
    pub key: bool,
    pub id: u64,
    pub bytes: u32,
}

impl Header {
    pub fn to_bytes(self) -> [u8; HEADER_BYTES] {
        let mut bytes = [0; HEADER_BYTES];
        bytes[..2].copy_from_slice(&crate::PROTOCOL_VERSION.to_be_bytes());
        bytes[2] = self.kind as u8;
        bytes[3] = u8::from(self.key) * KEY;
        bytes[4..12].copy_from_slice(&self.id.to_be_bytes());
        bytes[12..].copy_from_slice(&self.bytes.to_be_bytes());
        bytes
    }

    pub fn from_bytes(bytes: [u8; HEADER_BYTES]) -> io::Result<Self> {
        let [
            v1,
            v2,
            kind,
            flags,
            i1,
            i2,
            i3,
            i4,
            i5,
            i6,
            i7,
            i8,
            b1,
            b2,
            b3,
            b4,
        ] = bytes;
        if u16::from_be_bytes([v1, v2]) != crate::PROTOCOL_VERSION || flags & !KEY != 0 {
            return Err(invalid());
        }
        let kind = match kind {
            1 => Kind::Configure,
            2 => Kind::Frame,
            3 => Kind::Ready,
            4 => Kind::Encoded,
            5 => Kind::Dropped,
            6 => Kind::CaptureFrame,
            7 => Kind::HardwareFrame,
            10 => Kind::Decode,
            11 => Kind::Decoded,
            12 => Kind::ResetDecoder,
            20 => Kind::CaptureKeyframe,
            21 => Kind::CaptureStart,
            22 => Kind::SetCaptureLimits,
            23 => Kind::SetCapturePreview,
            24 => Kind::CapturePreview,
            25 => Kind::CaptureFailed,
            _ => return Err(invalid()),
        };
        let key = flags == KEY;
        if key
            && !matches!(
                kind,
                Kind::Encoded | Kind::CaptureFrame | Kind::HardwareFrame | Kind::Decode
            )
        {
            return Err(invalid());
        }
        Ok(Self {
            kind,
            key,
            id: u64::from_be_bytes([i1, i2, i3, i4, i5, i6, i7, i8]),
            bytes: u32::from_be_bytes([b1, b2, b3, b4]),
        })
    }

    pub fn empty(kind: Kind, id: u64) -> Self {
        Self {
            kind,
            key: false,
            id,
            bytes: 0,
        }
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid video worker message")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_reject_unknown_versions_kinds_and_flags() {
        let header = Header::empty(Kind::Ready, 29);
        let bytes = header.to_bytes();
        assert_eq!(Header::from_bytes(bytes).unwrap(), header);
        for (index, value) in [(0, 255), (2, 255), (3, 2), (3, 1)] {
            let mut bytes = bytes;
            bytes[index] = value;
            assert!(Header::from_bytes(bytes).is_err());
        }
    }

    #[test]
    fn capture_limits_keep_an_ultrawide_at_its_native_tier() {
        let limits = CaptureLimits {
            height: 1440,
            fps: 60,
            bitrate: 8_000_000,
        };
        assert_eq!(
            CaptureLimits::from_bytes(Source::Screen, limits.to_bytes()).unwrap(),
            limits
        );
        assert!(CaptureLimits::from_bytes(Source::Camera, limits.to_bytes()).is_err());
        assert_eq!(
            limits.dimensions(Source::Screen, 3440, 1440),
            Some((3440, 1440))
        );
        assert_eq!(
            limits.dimensions(Source::Screen, 2560, 1080),
            Some((2560, 1080))
        );
        assert_eq!(
            CaptureLimits {
                height: 720,
                ..limits
            }
            .dimensions(Source::Screen, 3440, 1440),
            Some((1720, 720))
        );
    }

    #[test]
    fn i420_dimensions_and_codec_budgets() {
        let settings = Settings {
            source: Source::Screen,
            width: 3440,
            height: 1440,
            fps: 100,
            bitrate: 25_000_000,
        };
        assert_eq!(Settings::from_bytes(settings.to_bytes()).unwrap(), settings);
        assert_eq!(settings.frame_bytes().unwrap(), 7_430_400);
        assert_eq!(settings.packet_limit(true), 8 * 1024 * KIB);
        assert_eq!(settings.packet_limit(false), 8 * 1024 * KIB);
        let camera = Settings {
            source: Source::Camera,
            ..settings
        };
        assert_eq!(camera.packet_limit(true), 256 * KIB);
        assert_eq!(camera.packet_limit(false), 128 * KIB);
        for invalid in [
            Settings {
                width: 3439,
                ..settings
            },
            Settings {
                height: 2162,
                ..settings
            },
            Settings { fps: 0, ..settings },
            Settings {
                bitrate: u32::MAX,
                ..settings
            },
        ] {
            assert!(Settings::from_bytes(invalid.to_bytes()).is_err());
        }
    }
}
