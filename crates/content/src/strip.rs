// SPDX-License-Identifier: EUPL-1.2
//! An original image without its metadata, losslessly: the same format and
//! the same encoded pixels, with what draws them, the colour profile and the
//! orientation. Everything else goes: EXIF (camera, time, GPS position,
//! thumbnails), XMP, comments, text and time chunks, other applications'
//! segments, and anything after the image's end. An orientation other than
//! upright stays, as an EXIF block that holds only it.
//!
//! - JPEG keeps its frame, tables and scans, its ICC profile and Adobe's
//!   colour transform segment.
//! - PNG keeps the chunks that draw it: image, palette, transparency,
//!   colour space, ICC profile, pixel size, background and animation.
//! - WebP keeps its image, alpha, animation and ICC profile chunks.
//! - GIF keeps its images, their timing and looping, and its ICC profile.
//! - BMP holds no metadata; it ends at the size its header gives.
use image::metadata::Orientation;

/// `bytes` without its metadata, or `None` for another format or a broken
/// file.
pub fn strip_metadata(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.starts_with(b"\xff\xd8") {
        jpeg(bytes)
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        png(bytes)
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        webp(bytes)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        gif(bytes)
    } else if bytes.starts_with(b"BM") {
        let size = u32::from_le_bytes(bytes.get(2..6)?.try_into().ok()?) as usize;
        bytes.get(..size).filter(|_| size > 26).map(<[u8]>::to_vec)
    } else {
        None
    }
}

/// EXIF (TIFF) that holds only `orientation`, or nothing when upright.
fn orientation_exif(orientation: Option<Orientation>) -> Option<[u8; 26]> {
    let orientation = orientation.filter(|&o| o != Orientation::NoTransforms)?;
    // Big-endian, IFD0 at 8 with one entry: tag 0x0112, SHORT, count 1.
    let mut tiff = *b"MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\0\0\0\0\0\0\0";
    tiff[19] = orientation.to_exif();
    Some(tiff)
}

fn be16(bytes: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?).into())
}

fn jpeg(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut kept = Vec::with_capacity(bytes.len());
    let mut orientation = None;
    let mut at = 2;
    loop {
        // A marker: 0xFF, any fill bytes, its code. Stray bytes before it,
        // which decoders skip, go.
        while *bytes.get(at)? != 0xff {
            at += 1;
        }
        while *bytes.get(at)? == 0xff {
            at += 1;
        }
        let code = bytes[at];
        at += 1;
        match code {
            0xd9 => break,
            0x01 | 0xd0..=0xd7 => {
                kept.extend([0xff, code]);
                continue;
            }
            _ => {}
        }
        let length = be16(bytes, at)?;
        let segment = bytes
            .get(at..at.checked_add(length)?)
            .filter(|_| length >= 2)?;
        let body = &segment[2..];
        at += length;
        let keep = match code {
            0xe1 => {
                if let Some(exif) = body.strip_prefix(b"Exif\0\0") {
                    orientation = orientation.or(Orientation::from_exif_chunk(exif));
                }
                false
            }
            0xe2 => body.starts_with(b"ICC_PROFILE\0"),
            0xee => body.starts_with(b"Adobe") && body.len() == 12,
            0xe0..=0xef | 0xfe => false,
            _ => true,
        };
        if keep {
            kept.extend([0xff, code]);
            kept.extend_from_slice(segment);
        }
        if code == 0xda {
            // Entropy-coded data, up to the next marker other than a
            // stuffed zero or a restart.
            let start = at;
            while !(*bytes.get(at)? == 0xff && !matches!(*bytes.get(at + 1)?, 0 | 0xd0..=0xd7)) {
                at += 1;
            }
            kept.extend_from_slice(&bytes[start..at]);
        }
    }
    let mut stripped = b"\xff\xd8".to_vec();
    if let Some(exif) = orientation_exif(orientation) {
        stripped.extend([0xff, 0xe1]);
        stripped.extend((2 + 6 + exif.len() as u16).to_be_bytes());
        stripped.extend(b"Exif\0\0");
        stripped.extend(exif);
    }
    stripped.extend(kept);
    stripped.extend([0xff, 0xd9]);
    Some(stripped)
}

/// A PNG chunk of `kind` holding `data`, with its CRC.
pub(crate) fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let typed = [kind.as_slice(), data].concat();
    let crc = !typed.iter().fold(!0u32, |crc, &byte| {
        (0..8).fold(crc ^ u32::from(byte), |crc, _| {
            (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg())
        })
    });
    [
        &(data.len() as u32).to_be_bytes(),
        &typed[..],
        &crc.to_be_bytes(),
    ]
    .concat()
}

fn png(bytes: &[u8]) -> Option<Vec<u8>> {
    const DRAWING: [&[u8; 4]; 17] = [
        b"IHDR", b"PLTE", b"IDAT", b"tRNS", b"cHRM", b"gAMA", b"iCCP", b"sBIT", b"sRGB", b"cICP",
        b"mDCV", b"cLLI", b"pHYs", b"bKGD", b"acTL", b"fcTL", b"fdAT",
    ];
    let mut kept = Vec::with_capacity(bytes.len());
    let mut orientation = None;
    let mut at = 8;
    loop {
        let length = u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize;
        let end = at.checked_add(12)?.checked_add(length)?;
        let chunk = bytes.get(at..end)?;
        let kind: &[u8; 4] = chunk[4..8].try_into().ok()?;
        at = end;
        if kind == b"IEND" {
            break;
        }
        if kind == b"eXIf" {
            orientation = orientation.or(Orientation::from_exif_chunk(&chunk[8..8 + length]));
        } else if DRAWING.contains(&kind) {
            kept.extend_from_slice(chunk);
        }
    }
    // IHDR comes first; the orientation follows it, before the image data.
    let header = kept.get(..25).filter(|header| header[4..8] == *b"IHDR")?;
    let mut stripped = [&bytes[..8], header].concat();
    if let Some(exif) = orientation_exif(orientation) {
        stripped.extend(png_chunk(b"eXIf", &exif));
    }
    stripped.extend_from_slice(&kept[25..]);
    stripped.extend(png_chunk(b"IEND", b""));
    Some(stripped)
}

fn webp(bytes: &[u8]) -> Option<Vec<u8>> {
    const DRAWING: [&[u8; 4]; 7] = [
        b"VP8X", b"VP8 ", b"VP8L", b"ALPH", b"ANIM", b"ANMF", b"ICCP",
    ];
    let size = u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?) as usize;
    let riff = bytes.get(..size.checked_add(8)?)?;
    let mut kept = Vec::with_capacity(riff.len());
    let mut orientation = None;
    let mut at = 12;
    while at < riff.len() {
        let length = u32::from_le_bytes(riff.get(at + 4..at + 8)?.try_into().ok()?) as usize;
        let data = riff.get(at + 8..at.checked_add(8)?.checked_add(length)?)?;
        let end = (at + 8 + length + length % 2).min(riff.len());
        let kind: &[u8; 4] = riff[at..at + 4].try_into().ok()?;
        if kind == b"EXIF" {
            let exif = data.strip_prefix(b"Exif\0\0").unwrap_or(data);
            orientation = orientation.or(Orientation::from_exif_chunk(exif));
        } else if DRAWING.contains(&kind) {
            kept.extend_from_slice(&riff[at..end]);
            if length % 2 == 1 && end == riff.len() {
                kept.push(0);
            }
        }
        at = end;
    }
    // Only the extended format (VP8X) has metadata. Its flags say which:
    // EXIF (0x08) as kept, XMP (0x04) gone.
    if kept.starts_with(b"VP8X") {
        *kept.get_mut(8)? &= !0x0c;
        if let Some(exif) = orientation_exif(orientation) {
            kept[8] |= 0x08;
            kept.extend(b"EXIF");
            kept.extend((exif.len() as u32).to_le_bytes());
            kept.extend(exif);
        }
    }
    let mut stripped = b"RIFF".to_vec();
    stripped.extend((kept.len() as u32 + 4).to_le_bytes());
    stripped.extend(b"WEBP");
    stripped.extend(kept);
    Some(stripped)
}

/// The end of the data sub-blocks starting at `at`, after their terminator.
fn sub_blocks(bytes: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let length = usize::from(*bytes.get(at)?);
        at += 1 + length;
        if length == 0 {
            return Some(at);
        }
    }
}

fn gif(bytes: &[u8]) -> Option<Vec<u8>> {
    let table = |packed: u8| {
        if packed & 0x80 != 0 {
            3 << ((packed & 7) + 1)
        } else {
            0
        }
    };
    let mut at = 13 + table(*bytes.get(10)?);
    let mut stripped = bytes.get(..at)?.to_vec();
    loop {
        let start = at;
        match *bytes.get(at)? {
            0x3b => break,
            // An image: its descriptor, local colour table, code size and data.
            0x2c => {
                let packed = *bytes.get(at + 9)?;
                at = sub_blocks(bytes, at + 10 + table(packed) + 1)?;
                stripped.extend_from_slice(bytes.get(start..at)?);
            }
            0x21 => {
                let label = *bytes.get(at + 1)?;
                at = sub_blocks(bytes, at + 2)?;
                // Timing, looping and the ICC profile; not comments, text or
                // other applications' data such as XMP.
                let application = bytes.get(start + 3..start + 14).unwrap_or_default();
                let keep = label == 0xf9
                    || label == 0xff
                        && [&b"NETSCAPE2.0"[..], b"ANIMEXTS1.0", b"ICCRGBG1012"]
                            .contains(&application);
                if keep {
                    stripped.extend_from_slice(bytes.get(start..at)?);
                }
            }
            _ => return None,
        }
    }
    stripped.push(0x3b);
    Some(stripped)
}
