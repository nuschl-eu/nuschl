// SPDX-License-Identifier: EUPL-1.2
//! The calls into libaom: one unsafe operation per block, each with the
//! contract it relies on from libaom's headers (aom_encoder.h, aomcx.h).

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code,
    unreachable_pub,
    clippy::all,
    clippy::pedantic,
    clippy::as_conversions,
    clippy::ptr_as_ptr,
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block,
    clippy::missing_safety_doc
)]
mod sys {
    include!(concat!(env!("OUT_DIR"), "/aom.rs"));
}

use crate::{Config, Error, Frame};
use std::{
    ffi::{c_int, c_uint},
    ptr,
};
use sys::aome_enc_control_id::{self as control, Type as Control};

/// libaom's real-time speed: the fastest presets keep a 1080p screen within a
/// frame interval on one core.
const SPEED: c_int = 10;

/// An initialized libaom encoder context, destroyed on drop.
pub struct Encoder {
    /// Boxed so its address stays fixed while libaom holds its private state.
    context: Box<sys::aom_codec_ctx_t>,
    width: u16,
    height: u16,
    pts: i64,
}

fn check(call: &'static str, code: sys::aom_codec_err_t::Type) -> Result<(), Error> {
    if code == sys::aom_codec_err_t::AOM_CODEC_OK {
        Ok(())
    } else {
        Err(Error { call, code })
    }
}

fn invalid(call: &'static str) -> Error {
    Error {
        call,
        code: sys::aom_codec_err_t::AOM_CODEC_INVALID_PARAM,
    }
}

impl Encoder {
    pub fn new(config: &Config) -> Result<Self, Error> {
        if config.width == 0
            || config.height == 0
            || !config.width.is_multiple_of(2)
            || !config.height.is_multiple_of(2)
            || config.fps == 0
            || !(1..=3).contains(&config.layers)
        {
            return Err(invalid("Encoder::new"));
        }
        // SAFETY: aom_codec_av1_cx takes no arguments and returns libaom's
        // static AV1 encoder interface.
        let interface = unsafe { sys::aom_codec_av1_cx() };
        let mut settings = sys::aom_codec_enc_cfg_t::default();
        // SAFETY: `settings` is writable; libaom fills it with the
        // interface's real-time defaults.
        check("aom_codec_enc_config_default", unsafe {
            sys::aom_codec_enc_config_default(interface, &raw mut settings, sys::AOM_USAGE_REALTIME)
        })?;
        settings.g_w = c_uint::from(config.width);
        settings.g_h = c_uint::from(config.height);
        settings.g_timebase = sys::aom_rational {
            num: 1,
            den: c_int::from(config.fps),
        };
        settings.g_threads = c_uint::from(config.threads.max(1));
        settings.g_lag_in_frames = 0;
        // The caller sets each frame's quantizer, decides drops and asks
        // for keyframes.
        settings.rc_end_usage = sys::aom_rc_mode::AOM_CBR;
        settings.rc_dropframe_thresh = 0;
        settings.rc_min_quantizer = 1;
        settings.rc_max_quantizer = 63;
        settings.kf_mode = sys::aom_kf_mode::AOM_KF_DISABLED;
        let abi = c_int::try_from(sys::AOM_ENCODER_ABI_VERSION)
            .map_err(|_| invalid("aom_codec_enc_init_ver"))?;
        let mut context = Box::new(sys::aom_codec_ctx_t::default());
        // SAFETY: `context` is a zeroed context that the encoder owns from
        // here on, `settings` is complete and libaom copies it, and `abi` is
        // the version of the headers the bindings come from. A failed init
        // releases what it allocated.
        check("aom_codec_enc_init_ver", unsafe {
            sys::aom_codec_enc_init_ver(&raw mut *context, interface, &raw const settings, 0, abi)
        })?;
        let mut encoder = Self {
            context,
            width: config.width,
            height: config.height,
            pts: 0,
        };
        let tiles = u32::from(config.threads)
            .min(u32::from(config.width) / 320)
            .max(1)
            .ilog2();
        encoder.control(control::AOME_SET_CPUUSED, SPEED)?;
        encoder.control(control::AV1E_SET_RTC_EXTERNAL_RC, 1)?;
        encoder.control(control::AV1E_SET_AQ_MODE, 0)?;
        encoder.control(control::AV1E_SET_ROW_MT, 1)?;
        // Without order hints a frame's header follows from the slots its
        // operating point decodes: references the frame names but does not
        // predict from may hold frames of higher layers.
        encoder.control(control::AV1E_SET_ENABLE_ORDER_HINT, 0)?;
        encoder.control(
            control::AV1E_SET_TILE_COLUMNS,
            c_int::try_from(tiles).map_err(|_| invalid("AV1E_SET_TILE_COLUMNS"))?,
        )?;
        if config.screen {
            let screen = c_int::try_from(sys::aom_tune_content::AOM_CONTENT_SCREEN)
                .map_err(|_| invalid("AV1E_SET_TUNE_CONTENT"))?;
            encoder.control(control::AV1E_SET_TUNE_CONTENT, screen)?;
            encoder.control(control::AV1E_SET_ENABLE_PALETTE, 1)?;
        }
        let layers = c_int::from(config.layers);
        let mut svc = sys::aom_svc_params_t {
            number_spatial_layers: 1,
            number_temporal_layers: layers,
            ..Default::default()
        };
        svc.scaling_factor_num[0] = 1;
        svc.scaling_factor_den[0] = 1;
        for layer in 0..config.layers {
            let index = usize::from(layer);
            svc.max_quantizers[index] = 63;
            svc.min_quantizers[index] = 1;
            svc.framerate_factor[index] = 1 << c_int::from(config.layers - 1 - layer);
            // libaom's own rate control stays off; it only needs targets.
            svc.layer_target_bitrate[index] = 1000 * (c_int::from(layer) + 1);
        }
        // SAFETY: AV1E_SET_SVC_PARAMS takes a pointer to aom_svc_params_t.
        unsafe { encoder.control_pointer(control::AV1E_SET_SVC_PARAMS, &mut svc) }?;
        Ok(encoder)
    }

    /// Codes one I420 picture of the configured size as `frame` plans it
    /// and returns its temporal unit.
    pub fn encode(&mut self, i420: &[u8], frame: &Frame) -> Result<Vec<u8>, Error> {
        let luma = usize::from(self.width) * usize::from(self.height);
        if i420.len() != luma + luma / 2 || frame.reference > 7 {
            return Err(invalid("Encoder::encode"));
        }
        let mut layer = sys::aom_svc_layer_id_t {
            spatial_layer_id: 0,
            temporal_layer_id: c_int::from(frame.temporal_id),
        };
        // SAFETY: AV1E_SET_SVC_LAYER_ID takes a pointer to aom_svc_layer_id_t.
        unsafe { self.control_pointer(control::AV1E_SET_SVC_LAYER_ID, &mut layer) }?;
        // LAST predicts from the planned slot. libaom replaces only slots a
        // reference names, so the other six name the replaced slots first.
        let replaced = |slot: u8| frame.key || (frame.refresh >> slot) & 1 == 1;
        let mut others: Vec<u8> = (0..8).filter(|&slot| slot != frame.reference).collect();
        others.sort_by_key(|&slot| !replaced(slot));
        if !frame.key && replaced(others[6]) {
            return Err(invalid("Encoder::encode"));
        }
        let mut references = sys::aom_svc_ref_frame_config_t::default();
        references.reference[0] = c_int::from(!frame.key);
        for (index, slot) in references
            .ref_idx
            .iter_mut()
            .zip(std::iter::once(frame.reference).chain(others))
        {
            *index = c_int::from(slot);
        }
        for (slot, refresh) in (0..).zip(&mut references.refresh) {
            *refresh = c_int::from(replaced(slot));
        }
        // SAFETY: AV1E_SET_SVC_REF_FRAME_CONFIG takes a pointer to
        // aom_svc_ref_frame_config_t.
        unsafe { self.control_pointer(control::AV1E_SET_SVC_REF_FRAME_CONFIG, &mut references) }?;
        let quantizer = c_int::from(frame.qindex.div_ceil(4).clamp(1, 63));
        self.control(control::AV1E_SET_QUANTIZER_ONE_PASS, quantizer)?;

        let mut image = sys::aom_image_t::default();
        // SAFETY: `i420` holds a width × height luma plane and two quarter
        // size chroma planes, the storage AOM_IMG_FMT_I420 at stride
        // alignment 1 needs for these even dimensions. libaom stores pointers
        // into it in `image` and writes nothing else.
        let wrapped = unsafe {
            sys::aom_img_wrap(
                &raw mut image,
                sys::aom_img_fmt::AOM_IMG_FMT_I420,
                c_uint::from(self.width),
                c_uint::from(self.height),
                1,
                i420.as_ptr().cast_mut(),
            )
        };
        if wrapped.is_null() {
            return Err(invalid("aom_img_wrap"));
        }
        // `aom_enc_frame_flags_t` is a C long, 32 bits on Windows.
        #[allow(clippy::unnecessary_fallible_conversions)]
        let flags = if frame.key {
            sys::aom_enc_frame_flags_t::try_from(sys::AOM_EFLAG_FORCE_KF)
                .map_err(|_| invalid("aom_codec_encode"))?
        } else {
            0
        };
        self.pts += 1;
        // SAFETY: the context is initialized and `image` describes `i420`,
        // which outlives the call. With no lag libaom copies the picture into
        // its own buffers during the call, and it writes to the picture only
        // when its denoiser is on, which it is not here.
        check("aom_codec_encode", unsafe {
            sys::aom_codec_encode(&raw mut *self.context, &raw const image, self.pts, 1, flags)
        })?;
        let mut data = Vec::new();
        let mut iterator: sys::aom_codec_iter_t = ptr::null();
        loop {
            // SAFETY: the context is initialized and `iterator` is this
            // loop's position in libaom's packet list.
            let packet =
                unsafe { sys::aom_codec_get_cx_data(&raw mut *self.context, &raw mut iterator) };
            // SAFETY: libaom returns null at the end of the list, else a
            // packet that stays valid until the next call into the context,
            // and this loop reads it before that.
            let Some(packet) = (unsafe { packet.as_ref() }) else {
                break;
            };
            if packet.kind != sys::aom_codec_cx_pkt_kind::AOM_CODEC_CX_FRAME_PKT {
                continue;
            }
            // SAFETY: a frame packet's data is its `frame` member.
            let coded = unsafe { packet.data.frame };
            if coded.buf.is_null() || coded.sz == 0 {
                continue;
            }
            // SAFETY: a frame packet points at `sz` bytes of coded data that
            // stay valid until the next call into the context.
            data.extend_from_slice(unsafe {
                std::slice::from_raw_parts(coded.buf.cast::<u8>(), coded.sz)
            });
        }
        Ok(data)
    }

    /// Sets a control whose argument aomcx.h types as `int`.
    fn control(&mut self, id: Control, value: c_int) -> Result<(), Error> {
        let id = c_int::try_from(id).map_err(|_| invalid("aom_codec_control"))?;
        // SAFETY: the context is initialized, and every caller passes a
        // control that takes an int, which the variadic call passes as c_int.
        check("aom_codec_control", unsafe {
            sys::aom_codec_control(&raw mut *self.context, id, value)
        })
    }

    /// Sets a control whose argument is a pointer to `T`; libaom copies
    /// `value` during the call.
    ///
    /// # Safety
    ///
    /// aomcx.h must type `id`'s argument as a pointer to `T`.
    unsafe fn control_pointer<T>(&mut self, id: Control, value: &mut T) -> Result<(), Error> {
        let id = c_int::try_from(id).map_err(|_| invalid("aom_codec_control"))?;
        // SAFETY: the context is initialized, the caller pairs `id` with
        // its argument type, and libaom reads `value` during the call only.
        check("aom_codec_control", unsafe {
            sys::aom_codec_control(&raw mut *self.context, id, ptr::from_mut(value))
        })
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: `new` initialized the context, and this is its only
        // destruction.
        unsafe { sys::aom_codec_destroy(&raw mut *self.context) };
    }
}
