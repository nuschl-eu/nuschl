// SPDX-License-Identifier: EUPL-1.2

use super::Error;
use nuschl_ipc::video::{DecodedFrame, Source, decode_frame_bytes};
use rav1d::{
    Picture, PixelLayout, PlanarImageComponent as Component,
    pixel::{MatrixCoefficients, TransferCharacteristic, YUVRange},
};

pub fn to_bgra(
    picture: &Picture,
    source: Source,
    render: Option<(u16, u16)>,
) -> Result<DecodedFrame, Error> {
    let frame = (
        u16::try_from(picture.width())?,
        u16::try_from(picture.height())?,
    );
    let (width, height) = super::shown(frame, render);
    let length = decode_frame_bytes(source, width, height)?;
    let bits = picture
        .bits_per_component()
        .ok_or("invalid video component depth")?
        .0;
    if matches!(
        picture.transfer_characteristic(),
        TransferCharacteristic::PerceptualQuantizer | TransferCharacteristic::HybridLogGamma
    ) {
        return Err("the software video decoder displays SDR".into());
    }

    let mut bgra = vec![0; length];
    let matrix = Conversion::new(picture, bits)?;
    if bits == 8 {
        write_pixels::<false>(picture, &matrix, width.into(), height.into(), &mut bgra)?;
    } else {
        write_pixels::<true>(picture, &matrix, width.into(), height.into(), &mut bgra)?;
    }
    Ok(DecodedFrame {
        width,
        height,
        bgra,
    })
}

fn write_pixels<const WIDE: bool>(
    picture: &Picture,
    conversion: &Conversion,
    width: usize,
    height: usize,
    output: &mut [u8],
) -> Result<(), Error> {
    let layout = picture.pixel_layout();
    let monochrome = layout == PixelLayout::I400;
    let shift_x = usize::from(matches!(layout, PixelLayout::I420 | PixelLayout::I422));
    let shift_y = usize::from(layout == PixelLayout::I420);
    let chroma_width = width.div_ceil(1 << shift_x);
    let y = Plane::new(picture, Component::Y);
    let chroma = (!monochrome).then(|| {
        [
            Plane::new(picture, Component::U),
            Plane::new(picture, Component::V),
        ]
    });
    for (row_index, output_row) in (0..height).zip(output.chunks_exact_mut(width * 4)) {
        let y = y.row::<WIDE>(row_index, width)?;
        let row_chroma = if let Some([u, v]) = &chroma {
            let row = row_index >> shift_y;
            Some([
                u.row::<WIDE>(row, chroma_width)?,
                v.row::<WIDE>(row, chroma_width)?,
            ])
        } else {
            None
        };
        let (pixels, _) = output_row.as_chunks_mut::<4>();
        for (column, pixel) in pixels.iter_mut().enumerate() {
            let y = sample::<WIDE>(y, column);
            let neutral = if conversion.identity {
                y
            } else {
                conversion.center
            };
            let [u, v] = row_chroma.map_or([neutral; 2], |[u, v]| {
                let index = column >> shift_x;
                [sample::<WIDE>(u, index), sample::<WIDE>(v, index)]
            });
            *pixel = conversion.bgra(y, u, v);
        }
    }
    Ok(())
}

struct Plane<'a> {
    data: &'a [u8],
    stride: usize,
}

impl<'a> Plane<'a> {
    fn new(picture: &'a Picture, component: Component) -> Self {
        Self {
            data: picture.plane(component),
            stride: picture.stride(component) as usize,
        }
    }

    fn row<const WIDE: bool>(&self, index: usize, width: usize) -> Result<&'a [u8], Error> {
        let start = index
            .checked_mul(self.stride)
            .ok_or("invalid decoder plane stride")?;
        let bytes = if WIDE { width * 2 } else { width };
        self.data
            .get(start..start.saturating_add(bytes))
            .ok_or_else(|| "invalid decoder plane size".into())
    }
}

fn sample<const WIDE: bool>(row: &[u8], x: usize) -> i32 {
    if WIDE {
        i32::from(u16::from_ne_bytes([row[x * 2], row[x * 2 + 1]]))
    } else {
        i32::from(row[x])
    }
}

struct Conversion {
    identity: bool,
    black: i32,
    center: i32,
    luma: i32,
    blue_from_u: i32,
    green_from_u: i32,
    green_from_v: i32,
    red_from_v: i32,
}

impl Conversion {
    fn new(picture: &Picture, bits: u8) -> Result<Self, Error> {
        let matrix = picture.matrix_coefficients();
        let (red, blue) = match matrix {
            MatrixCoefficients::Identity | MatrixCoefficients::BT709 => (0.2126, 0.0722),
            MatrixCoefficients::BT470BG | MatrixCoefficients::ST170M => (0.299, 0.114),
            MatrixCoefficients::BT470M => (0.30, 0.11),
            MatrixCoefficients::ST240M => (0.212, 0.087),
            MatrixCoefficients::BT2020NonConstantLuminance => (0.2627, 0.0593),
            MatrixCoefficients::Unspecified if picture.height() < 720 => (0.299, 0.114),
            MatrixCoefficients::Unspecified => (0.2126, 0.0722),
            _ => return Err("unsupported SDR video color matrix".into()),
        };
        let green = 1.0 - red - blue;
        let full = (1 << bits) - 1;
        let unit = 1 << (bits - 8);
        let limited = picture.color_range() == YUVRange::Limited;
        let (black, luma_range, chroma_range) = if limited {
            (16 * unit, 219 * unit, 224 * unit)
        } else {
            (0, full, full)
        };
        let gain = |range| 255.0 * 65536.0 / f64::from(range);
        let chroma = gain(chroma_range);
        Ok(Self {
            identity: matrix == MatrixCoefficients::Identity,
            black,
            center: 128 * unit,
            luma: gain(luma_range).round() as i32,
            blue_from_u: (chroma * 2.0 * (1.0 - blue)).round() as i32,
            green_from_u: (chroma * 2.0 * blue * (1.0 - blue) / green).round() as i32,
            green_from_v: (chroma * 2.0 * red * (1.0 - red) / green).round() as i32,
            red_from_v: (chroma * 2.0 * (1.0 - red)).round() as i32,
        })
    }

    fn bgra(&self, y: i32, u: i32, v: i32) -> [u8; 4] {
        let to_byte = |sample: i32| ((sample + (1 << 15)) >> 16).clamp(0, 255) as u8;
        let luma = (y - self.black) * self.luma;
        if self.identity {
            return [
                to_byte((u - self.black) * self.luma),
                to_byte(luma),
                to_byte((v - self.black) * self.luma),
                255,
            ];
        }
        let u = u - self.center;
        let v = v - self.center;
        [
            to_byte(luma + self.blue_from_u * u),
            to_byte(luma - self.green_from_u * u - self.green_from_v * v),
            to_byte(luma + self.red_from_v * v),
            255,
        ]
    }
}
