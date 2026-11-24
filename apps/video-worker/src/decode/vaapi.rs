// SPDX-License-Identifier: EUPL-1.2

use super::Error;
use gstreamer::{self as gst, prelude::*};
use gstreamer_app::{AppSink, AppSrc};
use nuschl_ipc::video::{Source, decode_frame_bytes};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_millis(200);

pub struct Pipeline {
    pipeline: gst::Pipeline,
    source: AppSrc,
    sink: AppSink,
    bus: gst::Bus,
}

impl Pipeline {
    pub fn start() -> Result<Self, Error> {
        let plugins = option_env!("NUSCHL_VIDEO_GST_PLUGINS")
            .ok_or("the VA-API GStreamer plugin list is missing")?;
        gst::init()?;
        for path in plugins.split(':') {
            gst::Plugin::load_file(path)?;
        }
        let pipeline = gst::parse::launch(concat!(
            "appsrc name=encoded format=time is-live=true max-buffers=2 ",
            "caps=\"video/x-av1,stream-format=obu-stream,alignment=tu\" ! ",
            "av1parse ! vaav1dec ! vapostproc ! ",
            "video/x-raw,format=BGRA,width=[1,3840],height=[1,3840] ! ",
            "appsink name=decoded sync=false max-buffers=2 leaky-type=downstream",
        ))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "invalid VA-API decoder pipeline")?;
        let source = pipeline
            .by_name("encoded")
            .and_then(|element| element.downcast::<AppSrc>().ok())
            .ok_or("VA-API decoder source is missing")?;
        let sink = pipeline
            .by_name("decoded")
            .and_then(|element| element.downcast::<AppSink>().ok())
            .ok_or("VA-API decoder sink is missing")?;
        let bus = pipeline.bus().ok_or("VA-API decoder bus is missing")?;
        let decoder = Self {
            pipeline,
            source,
            sink,
            bus,
        };
        decoder.pipeline.set_state(gst::State::Playing)?;
        Ok(decoder)
    }

    pub fn reset(&mut self) -> Result<(), Error> {
        self.pipeline.set_state(gst::State::Null)?;
        self.pipeline.set_state(gst::State::Playing)?;
        Ok(())
    }

    pub fn decode(
        &mut self,
        encoded: Vec<u8>,
        id: u64,
        source: Source,
        render: Option<(u16, u16)>,
        write: impl FnOnce(u16, u16, &[u8]) -> Result<(), Error>,
    ) -> Result<bool, Error> {
        let mut buffer = gst::Buffer::from_mut_slice(encoded);
        let timestamp = gst::ClockTime::from_nseconds(id);
        buffer
            .get_mut()
            .ok_or("shared encoded decoder input")?
            .set_pts(timestamp);
        self.source.push_buffer(buffer)?;

        let boundary = Instant::now() + WAIT;
        while let Some(left) = boundary.checked_duration_since(Instant::now()) {
            let limit = gst::ClockTime::from_nseconds(u64::try_from(left.as_nanos())?);
            let Some(sample) = self.sink.try_pull_sample(limit) else {
                break;
            };
            let buffer = sample.buffer().ok_or("missing VA-API decoded frame")?;
            if buffer.pts() != Some(timestamp) {
                continue;
            }
            let format = sample
                .caps()
                .and_then(|caps| caps.structure(0))
                .ok_or("missing decoded video dimensions")?;
            let frame = (
                u16::try_from(format.get::<i32>("width")?)?,
                u16::try_from(format.get::<i32>("height")?)?,
            );
            let (width, height) = super::shown(frame, render);
            let length = decode_frame_bytes(source, width, height)?;
            let pixels = buffer.map_readable()?;
            let row = usize::from(frame.0) * 4;
            if pixels.size() != row * usize::from(frame.1) {
                return Err("invalid BGRA layout from VA-API".into());
            }
            if width == frame.0 {
                write(width, height, &pixels[..length])?;
            } else {
                let mut picture = Vec::with_capacity(length);
                for line in pixels.chunks_exact(row).take(height.into()) {
                    picture.extend_from_slice(&line[..usize::from(width) * 4]);
                }
                write(width, height, &picture)?;
            }
            return Ok(true);
        }
        if let Some(message) = self
            .bus
            .pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos])
        {
            return match message.view() {
                gst::MessageView::Error(error) => Err(error.error().into()),
                _ => Err("VA-API decode has ended".into()),
            };
        }
        Ok(false)
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
