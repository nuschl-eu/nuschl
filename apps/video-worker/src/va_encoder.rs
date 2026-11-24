// SPDX-License-Identifier: EUPL-1.2
//! The encoder core's VA-API backend: AV1 at the quantizer, reference and
//! refreshed slots the core chooses for each frame, in the driver's
//! constant-quantizer mode.
//!
//! The core's sequence and frame headers go to the driver as packed headers.
//! Mesa's radeonsi reads the operating points, refresh flags, order hint and
//! temporal layer from them and writes its own headers into the bitstream; a
//! driver that sends packed headers as they are sends the core's.

use cros_libva::{
    AV1EncLoopFilterFlags, AV1EncLoopRestorationFlags, AV1EncModeControlFlags, AV1EncPictureFlags,
    AV1EncQMatrixFlags, AV1EncSeqFields, AV1EncTileGroupObuHdrInfo, BufferType, Context, Display,
    EncCodedBuffer, EncPackedHeaderParameter, EncPackedHeaderType, EncPictureParameter,
    EncPictureParameterBufferAV1, EncSegParamAV1, EncSegParamFlagsAV1, EncSequenceParameter,
    EncSequenceParameterBufferAV1, EncSliceParameter, EncTileGroupBufferAV1,
    EncWarpedMotionParamsAV1, ExternalBufferDescriptor, MappedCodedBuffer, MemoryType, Picture,
    RefFrameCtrlAV1, Surface, SurfaceMemoryDescriptor, UsageHint,
    VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK, VA_ENC_PACKED_HEADER_PICTURE,
    VA_ENC_PACKED_HEADER_SEQUENCE, VA_FOURCC_NV12, VA_INVALID_SURFACE, VA_RC_CQP,
    VA_RT_FORMAT_YUV420, VAConfigAttrib, VAConfigAttribType, VADRMPRIMESurfaceDescriptor,
    VADRMPRIMESurfaceDescriptorLayer, VADRMPRIMESurfaceDescriptorObject, VAEntrypoint, VAProfile,
};
use nuschl_video_encode::{Backend, Frame, temporal_delimiter};
use std::{marker::PhantomData, os::fd::RawFd, path::Path, rc::Rc};

type Error = Box<dyn std::error::Error>;

/// AV1's TX_MODE_SELECT: the encoder picks transform sizes per block.
const TX_MODE_SELECT: u32 = 2;
/// AV1's PRIMARY_REF_NONE.
const PRIMARY_REF_NONE: u8 = 7;

/// The backend for input frames in surfaces of descriptor `D`.
pub struct VaEncoder<D = ()> {
    display: Rc<Display>,
    context: Rc<Context>,
    width: u16,
    height: u16,
    coded: EncCodedBuffer,
    coded_bytes: usize,
    /// Reconstructed frames, each with the id of the frame it holds.
    reconstructed: Vec<(Option<u64>, Surface<()>)>,
    input: PhantomData<D>,
}

impl<D: SurfaceMemoryDescriptor> VaEncoder<D> {
    /// An encoder for `width` × `height` NV12 frames on the render node at
    /// `device`.
    pub fn open(device: &Path, width: u16, height: u16) -> Result<Self, Error> {
        let display = Display::open_drm_display(device)?;
        let config = display.create_config(
            vec![
                attribute(
                    VAConfigAttribType::VAConfigAttribRTFormat,
                    VA_RT_FORMAT_YUV420,
                ),
                attribute(VAConfigAttribType::VAConfigAttribRateControl, VA_RC_CQP),
                attribute(
                    VAConfigAttribType::VAConfigAttribEncPackedHeaders,
                    VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE,
                ),
            ],
            VAProfile::VAProfileAV1Profile0,
            VAEntrypoint::VAEntrypointEncSlice,
        )?;
        let context = display.create_context::<()>(
            &config,
            u32::from(width),
            u32::from(height),
            None,
            true,
        )?;
        // Room for the uncompressed frame, as FFmpeg's VA-API encoders reserve.
        let coded_bytes = 3 * usize::from(width) * usize::from(height) + (64 << 10);
        let coded = context.create_enc_coded(coded_bytes)?;
        Ok(Self {
            display,
            context,
            width,
            height,
            coded,
            coded_bytes,
            reconstructed: Vec::new(),
            input: PhantomData,
        })
    }

    /// An NV12 surface of the encoder's size for input frames.
    pub fn surface(&self, usage: UsageHint) -> Result<Surface<()>, Error> {
        let mut surfaces = self.display.create_surfaces(
            VA_RT_FORMAT_YUV420,
            Some(VA_FOURCC_NV12),
            u32::from(self.width),
            u32::from(self.height),
            Some(usage),
            vec![()],
        )?;
        surfaces
            .pop()
            .ok_or_else(|| "the driver returned no surface".into())
    }

    /// The reconstructed surface for frame `id`, after releasing the ones no
    /// slot holds.
    fn reconstruct(&mut self, id: u64, slots: &[Option<u64>; 8]) -> Result<usize, Error> {
        for (held, _) in &mut self.reconstructed {
            if held.is_some_and(|frame| !slots.contains(&Some(frame))) {
                *held = None;
            }
        }
        let index = match self
            .reconstructed
            .iter()
            .position(|(held, _)| held.is_none())
        {
            Some(index) => index,
            None => {
                let surface = self.surface(UsageHint::USAGE_HINT_ENCODER)?;
                self.reconstructed.push((None, surface));
                self.reconstructed.len() - 1
            }
        };
        self.reconstructed[index].0 = Some(id);
        Ok(index)
    }

    fn surface_of(&self, slot: Option<u64>) -> u32 {
        self.reconstructed
            .iter()
            .find(|(held, _)| slot.is_some() && *held == slot)
            .map_or(VA_INVALID_SURFACE, |(_, surface)| surface.id())
    }

    fn packed(
        &self,
        picture: &mut Picture<cros_libva::PictureNew, &Surface<D>>,
        kind: EncPackedHeaderType,
        data: Vec<u8>,
    ) -> Result<(), Error> {
        let bits = u32::try_from(data.len() * 8)?;
        let parameter = EncPackedHeaderParameter::new(kind, bits, false);
        picture.add_buffer(
            self.context
                .create_buffer(BufferType::EncPackedHeaderParameter(parameter))?,
        );
        picture.add_buffer(
            self.context
                .create_buffer(BufferType::EncPackedHeaderData(data))?,
        );
        Ok(())
    }
}

impl VaEncoder<Dmabuf> {
    /// Imports an NV12 DMA-BUF of the encoder's size as an input surface.
    pub fn import(&self, picture: Dmabuf) -> Result<Surface<Dmabuf>, Error> {
        if picture.size != (self.width, self.height) {
            return Err("the picture and the encoder differ in size".into());
        }
        let mut surfaces = self.display.create_surfaces(
            VA_RT_FORMAT_YUV420,
            Some(VA_FOURCC_NV12),
            u32::from(self.width),
            u32::from(self.height),
            Some(UsageHint::USAGE_HINT_ENCODER),
            vec![picture],
        )?;
        surfaces
            .pop()
            .ok_or_else(|| "the driver returned no surface".into())
    }
}

/// One NV12 picture in a DMA-BUF that GStreamer owns for the frame's lifetime.
pub struct Dmabuf {
    fd: RawFd,
    bytes: u32,
    modifier: u64,
    size: (u16, u16),
    offsets: [u32; 2],
    pitches: [u32; 2],
}

impl Dmabuf {
    /// `None` unless both planes lie within the buffer.
    pub fn new(
        fd: RawFd,
        bytes: u32,
        modifier: u64,
        size: (u16, u16),
        offsets: [u32; 2],
        pitches: [u32; 2],
    ) -> Option<Self> {
        let (width, height) = (u64::from(size.0), u64::from(size.1));
        let fits = |plane: usize, rows: u64| {
            u64::from(pitches[plane]) >= width
                && u64::from(offsets[plane]) + u64::from(pitches[plane]) * rows <= u64::from(bytes)
        };
        (fd >= 0 && fits(0, height) && fits(1, height.div_ceil(2))).then_some(Self {
            fd,
            bytes,
            modifier,
            size,
            offsets,
            pitches,
        })
    }
}

impl ExternalBufferDescriptor for Dmabuf {
    const MEMORY_TYPE: MemoryType = MemoryType::DrmPrime2;
    type DescriptorAttribute = VADRMPRIMESurfaceDescriptor;

    fn va_surface_attribute(&mut self) -> VADRMPRIMESurfaceDescriptor {
        let mut descriptor = VADRMPRIMESurfaceDescriptor {
            fourcc: VA_FOURCC_NV12,
            width: u32::from(self.size.0),
            height: u32::from(self.size.1),
            num_objects: 1,
            num_layers: 1,
            ..Default::default()
        };
        descriptor.objects[0] = VADRMPRIMESurfaceDescriptorObject {
            fd: self.fd,
            size: self.bytes,
            drm_format_modifier: self.modifier,
        };
        descriptor.layers[0] = VADRMPRIMESurfaceDescriptorLayer {
            drm_format: VA_FOURCC_NV12,
            num_planes: 2,
            object_index: [0; 4],
            offset: [self.offsets[0], self.offsets[1], 0, 0],
            pitch: [self.pitches[0], self.pitches[1], 0, 0],
        };
        descriptor
    }
}

impl<D: SurfaceMemoryDescriptor> Backend for VaEncoder<D> {
    type Input = Surface<D>;
    type Error = Error;

    fn encode(&mut self, input: &Surface<D>, frame: &Frame<'_>) -> Result<Vec<u8>, Error> {
        let (sequence, header) = (frame.sequence, frame.header);
        if (sequence.width, sequence.height) != (self.width, self.height) {
            return Err("the sequence and the encoder differ in size".into());
        }
        let reconstructed = self.reconstruct(frame.id, &frame.slots)?;
        let references = frame.slots.map(|slot| self.surface_of(slot));
        let mut picture = Picture::new(
            u64::from(header.order_hint),
            Rc::clone(&self.context),
            input,
        );
        if header.key {
            let fields = AV1EncSeqFields::new(
                false, false, false, false, false, false, false, false, true, false, false, false,
                true, false, 0, true, true, false,
            );
            let parameters =
                EncSequenceParameterBufferAV1::new(0, sequence.level, 0, 0, 0, 1, 0, &fields, 7);
            picture.add_buffer(
                self.context
                    .create_buffer(BufferType::EncSequenceParameter(EncSequenceParameter::AV1(
                        parameters,
                    )))?,
            );
        }
        self.packed(
            &mut picture,
            EncPackedHeaderType::RawData,
            temporal_delimiter(),
        )?;
        if header.key {
            self.packed(&mut picture, EncPackedHeaderType::Sequence, sequence.obu())?;
        }
        self.packed(
            &mut picture,
            EncPackedHeaderType::Picture,
            header.obu(sequence, None),
        )?;
        let layered = sequence.layers > 1;
        let flags = AV1EncPictureFlags::new(
            if header.key { 0 } else { 1 },
            false,
            header.disable_cdf_update,
            false,
            false,
            false,
            false,
            false,
            true,
            false,
            false,
            false,
            header.screen_content,
            header.screen_content,
            false,
        );
        let last = RefFrameCtrlAV1::new(u32::from(!header.key), 0, 0, 0, 0, 0, 0);
        let none = RefFrameCtrlAV1::new(0, 0, 0, 0, 0, 0, 0);
        let mut columns = [0; 63];
        let mut rows = [0; 63];
        columns[0] = self.width.div_ceil(64) - 1;
        rows[0] = self.height.div_ceil(64) - 1;
        let [vertical, horizontal, u, v] = header.loop_filter;
        let parameters = EncPictureParameterBufferAV1::new(
            self.width - 1,
            self.height - 1,
            self.reconstructed[reconstructed].1.id(),
            self.coded.id(),
            references,
            [header.reference; 7],
            0,
            if header.key { PRIMARY_REF_NONE } else { 0 },
            header.order_hint,
            if header.key { 0xff } else { header.refresh },
            &last,
            &none,
            &flags,
            0,
            0,
            header.temporal_id,
            [vertical, horizontal],
            u,
            v,
            &AV1EncLoopFilterFlags::new(0, false, false),
            0,
            0,
            [0; 8],
            [0; 2],
            header.qindex,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            &AV1EncQMatrixFlags::new(false, 0, 0, 0),
            &AV1EncModeControlFlags::new(false, 0, false, 0, false, TX_MODE_SELECT, 0, false),
            &EncSegParamAV1::new(
                &EncSegParamFlagsAV1::new(false, false, false),
                0,
                [[0; 8]; 8],
                [0; 8],
            ),
            1,
            1,
            columns,
            rows,
            0,
            0,
            0,
            [0; 8],
            [0; 8],
            &AV1EncLoopRestorationFlags::new(0, 0, 0, 0, false),
            [EncWarpedMotionParamsAV1::default(); 7],
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            &AV1EncTileGroupObuHdrInfo::new(layered, true, header.temporal_id, 0),
            0,
            0,
        );
        picture.add_buffer(self.context.create_buffer(BufferType::EncPictureParameter(
            EncPictureParameter::AV1(parameters),
        ))?);
        picture.add_buffer(self.context.create_buffer(BufferType::EncSliceParameter(
            EncSliceParameter::AV1(EncTileGroupBufferAV1::new(0, 0)),
        ))?);
        let picture = picture
            .begin()?
            .render()?
            .end()?
            .sync()
            .map_err(|(error, _)| error)?;
        drop(picture);
        let mapped = MappedCodedBuffer::new(&self.coded)?;
        let mut unit = Vec::new();
        for segment in mapped.iter() {
            if segment.status & VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK != 0
                || unit.len() + segment.buf.len() > self.coded_bytes
            {
                return Err("the frame overflowed the coded buffer".into());
            }
            unit.extend_from_slice(segment.buf);
        }
        Ok(unit)
    }
}

fn attribute(type_: VAConfigAttribType::Type, value: u32) -> VAConfigAttrib {
    VAConfigAttrib { type_, value }
}

#[cfg(test)]
mod tests;
