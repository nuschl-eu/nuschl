// SPDX-License-Identifier: EUPL-1.2
//! Hardware AV1 from the authorized PipeWire stream. GStreamer imports the
//! PipeWire DMA-BUFs into the VA driver, scales them and converts them to NV12
//! DMA-BUFs; the encoder core imports those into its own VA display and codes
//! them without a CPU copy.

use super::{Cadence, Control, Failure, PREVIEW_FPS};
use crate::va_encoder::{Dmabuf, VaEncoder};
use cros_libva::Surface;
use gstreamer::{self as gst, prelude::*};
use gstreamer_app::AppSink;
use gstreamer_video::{VideoInfoDmaDrm, VideoMeta};
use nuschl_ipc::video::{
    CaptureFailure, CaptureLimits, Kind, PREVIEW_HEIGHT, PREVIEW_WIDTH, Settings, Source,
    write_capture_failure, write_capture_preview,
};
use nuschl_video_encode::{Config, Encoder, Layering};
use std::{
    io,
    net::Shutdown,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const QUEUE: &str = "max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream";
/// The largest frame as time at the bitrate, the SFU's queue bound.
const WINDOW: Duration = Duration::from_millis(300);

pub fn prepare(
    source: Source,
    node: Option<u32>,
    preview: bool,
) -> Result<(gst::Pipeline, String), Failure> {
    let plugins = option_env!("NUSCHL_VIDEO_GST_PLUGINS")
        .ok_or("the VA-API GStreamer plugin list is missing")?;
    gst::init()?;
    for path in plugins.split(':') {
        gst::Plugin::load_file(path)?;
    }
    // Element registration probes the render node before the source starts.
    let nv12 = nv12_format().ok_or("the render node cannot convert to NV12 DMA-BUFs")?;
    let (min_width, min_height) = encoder_minimum().ok_or("the render node cannot encode AV1")?;

    let target = node.map_or_else(String::new, |node| format!("path={node}"));
    let (width, height) = source.max_dimensions();
    // Fix square pixels before the branches negotiate their output dimensions.
    let tee = if preview {
        "video/x-raw(ANY),pixel-aspect-ratio=1/1 ! tee name=split split. ! "
    } else {
        ""
    };
    let preview_path = if preview {
        format!(
            concat!(
                " split. ! queue name=preview_queue {queue} ! vapostproc ! ",
                "video/x-raw,format=BGRA,width=[1,{width}],height=[1,{height}],pixel-aspect-ratio=1/1 ! ",
                "appsink name=preview max-buffers=1 leaky-type=downstream sync=false async=false ",
                "enable-last-sample=false wait-on-eos=false",
            ),
            queue = QUEUE,
            width = PREVIEW_WIDTH,
            height = PREVIEW_HEIGHT,
        )
    } else {
        String::new()
    };
    let description = format!(
        concat!(
            "pipewiresrc name=capture fd=0 {target} ! ",
            "{tee}queue name=encode_queue {queue} ! ",
            "vapostproc name=scale ! capsfilter name=limit ",
            "caps=\"video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={nv12},width=[{min_width},{width}],height=[{min_height},{height}]\" ! ",
            "appsink name=frames max-buffers=1 sync=false{preview_path}",
        ),
        target = target,
        tee = tee,
        queue = QUEUE,
        nv12 = nv12,
        min_width = min_width,
        min_height = min_height,
        width = width,
        height = height,
        preview_path = preview_path,
    );
    let pipeline = gst::parse::launch(&description)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "invalid video pipeline")?;
    Ok((pipeline, nv12))
}

/// The smallest picture the render node encodes in AV1, as GStreamer's VA
/// plugin registered its encoder from the driver.
pub fn encoder_minimum() -> Option<(u16, u16)> {
    let factory = gst::ElementFactory::find("vaav1enc")?;
    let template = factory
        .static_pad_templates()
        .into_iter()
        .find(|template| template.direction() == gst::PadDirection::Sink)?;
    let caps = template.caps();
    let format = caps.structure(0)?;
    let minimum = |field| {
        let range = format.get::<gst::IntRange<i32>>(field).ok()?;
        u16::try_from(range.min()).ok()
    };
    Some((minimum("width")?, minimum("height")?))
}

/// The NV12 DMA-BUF format, with the driver's modifier, that vapostproc writes.
fn nv12_format() -> Option<String> {
    let factory = gst::ElementFactory::find("vapostproc")?;
    factory
        .static_pad_templates()
        .into_iter()
        .find_map(|template| {
            if template.direction() != gst::PadDirection::Src {
                return None;
            }
            template.caps().iter().find_map(|format| {
                let formats = format.get::<gst::List>("drm-format").ok()?;
                formats
                    .iter()
                    .filter_map(|value| value.get::<String>().ok())
                    .find(|name| name.starts_with("NV12:"))
            })
        })
}

pub fn run(
    (pipeline, nv12): (gst::Pipeline, String),
    source: Source,
    control: Arc<Control>,
    preview: Option<UnixStream>,
) -> Result<(), Failure> {
    let sink = pipeline
        .by_name("frames")
        .ok_or("missing video sink")?
        .downcast::<AppSink>()
        .map_err(|_| "invalid video sink")?;
    // vapostproc writes DMA-BUFs only for a consumer that reads their layout
    // from the video metadata.
    sink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .propose_allocation(|_, query| {
                query.add_allocation_meta::<VideoMeta>(None);
                true
            })
            .build(),
    );
    let mut scale = Scale::new(&pipeline, nv12)?;
    let timing = Mutex::new(Timing::default());
    let timing_control = control.clone();
    // Keep encoder caps changes downstream of the authorized PipeWire link.
    let reconfigure = scale
        .input
        .add_probe(gst::PadProbeType::EVENT_UPSTREAM, |_, info| {
            if info
                .event()
                .is_some_and(|event| event.type_() == gst::EventType::Reconfigure)
            {
                gst::PadProbeReturn::Drop
            } else {
                gst::PadProbeReturn::Ok
            }
        })
        .ok_or("could not isolate capture caps")?;
    let input = branch_input(&pipeline, "encode_queue")?;
    let frames = input
        .add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
            move |pad, info| timing_probe(pad, info, &timing, timing_control.limits().fps),
        )
        .ok_or("could not install the capture cadence limit")?;
    let preview_running = Arc::new(AtomicBool::new(preview.is_some()));
    let preview = preview
        .map(|stream| {
            stream.set_write_timeout(Some(Duration::from_millis(200)))?;
            let sink = preview_sink(&pipeline, control.clone(), preview_running.clone())?;
            Ok::<_, Failure>((stream.try_clone()?, stream, sink))
        })
        .transpose()?;
    pipeline.set_state(gst::State::Playing)?;
    std::thread::scope(|scope| {
        let stop_preview = preview.map(|(shutdown, stream, sink)| {
            let running = &preview_running;
            let control = &control;
            scope.spawn(move || write_previews(stream, &sink, control, running));
            shutdown
        });
        let result = write_frames(&pipeline, &sink, &mut scale, source, &control);
        preview_running.store(false, Ordering::Relaxed);
        if let Some(stream) = stop_preview {
            let _ = stream.shutdown(Shutdown::Write);
        }
        input.remove_probe(frames);
        scale.input.remove_probe(reconfigure);
        let _ = pipeline.set_state(gst::State::Null);
        result
    })
}

fn branch_input(pipeline: &gst::Pipeline, queue: &str) -> Result<gst::Pad, Failure> {
    pipeline
        .by_name(queue)
        .and_then(|element| element.static_pad("sink"))
        .and_then(|pad| pad.peer())
        .ok_or_else(|| "missing capture branch input".into())
}

fn preview_sink(
    pipeline: &gst::Pipeline,
    control: Arc<Control>,
    running: Arc<AtomicBool>,
) -> Result<AppSink, Failure> {
    let sink = pipeline
        .by_name("preview")
        .and_then(|element| element.downcast::<AppSink>().ok())
        .ok_or("missing video preview sink")?;
    let clock = Instant::now();
    let cadence = Mutex::new(Cadence::default());
    branch_input(pipeline, "preview_queue")?
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if !running.load(Ordering::Relaxed)
                || !control.preview.load(Ordering::Relaxed)
                || !cadence
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .accept(clock.elapsed(), PREVIEW_FPS)
            {
                return gst::PadProbeReturn::Drop;
            }
            if let Some(buffer) = info.buffer_mut()
                && buffer
                    .iter_memories()
                    .any(|memory| memory.is_type("dmabuf"))
            {
                if !buffer
                    .iter_memories()
                    .all(|memory| memory.is_type("dmabuf"))
                {
                    return gst::PadProbeReturn::Drop;
                }
                let Some(preview) = separate_preview_memory(buffer) else {
                    return gst::PadProbeReturn::Drop;
                };
                *buffer = preview;
            }
            gst::PadProbeReturn::Ok
        })
        .ok_or("could not install the preview cadence limit")?;
    Ok(sink)
}

fn separate_preview_memory(source: &gst::Buffer) -> Option<gst::Buffer> {
    if source
        .iter_memories()
        .any(|memory| memory.size() == 0 || memory.flags().contains(gst::MemoryFlags::NO_SHARE))
    {
        return None;
    }
    // Each VA importer attaches its surface to the memory header. A separate
    // header lets the preview and encoder keep their own surfaces for each
    // read-only DMA-BUF. The parent retains the PipeWire buffer pool lease.
    let mut preview = source.copy_region(gst::BUFFER_COPY_ALL, ..).ok()?;
    let buffer = preview.get_mut()?;
    for (index, memory) in source.iter_memories().enumerate() {
        buffer.replace_memory(index, memory.share(..));
    }
    if !buffer
        .iter_meta::<gst::ParentBufferMeta>()
        .any(|meta| std::ptr::eq(meta.parent(), source.as_ref()))
    {
        gst::ParentBufferMeta::add(buffer, source);
    }
    Some(preview)
}

fn write_previews(mut stream: UnixStream, sink: &AppSink, control: &Control, running: &AtomicBool) {
    while running.load(Ordering::Relaxed) {
        let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) else {
            continue;
        };
        if control.preview.load(Ordering::Relaxed) && write_preview(&mut stream, &sample).is_err() {
            running.store(false, Ordering::Relaxed);
        }
    }
    let _ = stream.shutdown(Shutdown::Write);
}

fn write_preview(stream: &mut UnixStream, sample: &gst::Sample) -> Result<(), Failure> {
    let format = sample
        .caps()
        .and_then(|caps| caps.structure(0))
        .ok_or("missing preview format")?;
    let (width, height) = dimensions(format)?;
    let buffer = sample.buffer().ok_or("missing preview video frame")?;
    let pixels = buffer.map_readable()?;
    write_capture_preview(stream, width, height, pixels.as_slice())?;
    Ok(())
}

#[derive(Default)]
struct Timing {
    cadence: Cadence,
    source_caps: Option<gst::Caps>,
    applied_fps: u16,
    our_event: Option<gst::Seqnum>,
}

fn timing_probe(
    pad: &gst::Pad,
    info: &mut gst::PadProbeInfo<'_>,
    timing: &Mutex<Timing>,
    fps: u16,
) -> gst::PadProbeReturn {
    let lock = || timing.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(event) = info.event_mut() {
        let mut timing = lock();
        // Keep the producer's original caps across our downstream updates.
        if timing.our_event != Some(event.seqnum())
            && let gst::EventView::Caps(input) = event.view()
        {
            let source = input.caps().to_owned();
            let caps = cadence_caps(&source, fps);
            *event = gst::event::Caps::builder(&caps)
                .seqnum(event.seqnum())
                .build();
            timing.source_caps = Some(source);
            timing.applied_fps = fps;
        }
        return gst::PadProbeReturn::Ok;
    }
    // Use worker ingress time for frame age and pace the stream before the queue.
    let Some(now) = pad
        .parent_element()
        .and_then(|element| element.current_running_time())
    else {
        return gst::PadProbeReturn::Drop;
    };
    let mut state = lock();
    if !state
        .cadence
        .accept(Duration::from_nanos(now.nseconds()), fps)
    {
        return gst::PadProbeReturn::Drop;
    }
    if state.applied_fps != fps
        && let Some(source) = &state.source_caps
    {
        let caps = cadence_caps(source, fps);
        let event = gst::event::Caps::new(&caps);
        state.our_event = Some(event.seqnum());
        // A downstream CAPS event reenters the probe before the next buffer.
        drop(state);
        let applied = pad.push_event(event);
        let mut state = lock();
        state.our_event = None;
        if !applied {
            return gst::PadProbeReturn::Drop;
        }
        state.applied_fps = fps;
    }
    if let Some(buffer) = info.buffer_mut() {
        buffer.make_mut().set_pts(now);
    }
    gst::PadProbeReturn::Ok
}

fn cadence_caps(source: &gst::Caps, fps: u16) -> gst::Caps {
    let mut caps = source.clone();
    if let Some(format) = caps.make_mut().structure_mut(0) {
        // VA rate control allocates bits per frame using the negotiated cadence.
        let limit = gst::Fraction::new(i32::from(fps), 1);
        let rate = format
            .get::<gst::Fraction>("framerate")
            .ok()
            .filter(|rate| rate.numer() > 0)
            .map_or(limit, |actual| actual.min(limit));
        format.set("framerate", rate);
    }
    caps
}

struct Scale {
    input: gst::Pad,
    filter: gst::Element,
    nv12: String,
    minimum: (u16, u16),
    size: Option<(u16, u16)>,
}

impl Scale {
    fn new(pipeline: &gst::Pipeline, nv12: String) -> Result<Self, Failure> {
        let input = pipeline
            .by_name("scale")
            .and_then(|element| element.static_pad("sink"))
            .ok_or("missing video scaler input")?;
        let filter = pipeline
            .by_name("limit")
            .ok_or("missing video caps filter")?;
        Ok(Self {
            input,
            filter,
            nv12,
            minimum: encoder_minimum().ok_or("the render node cannot encode AV1")?,
            size: None,
        })
    }

    /// Fits the scaler's output to `limits`; true when its size changed.
    fn update(&mut self, source: Source, limits: CaptureLimits) -> Result<bool, Failure> {
        // Measure the unscaled PipeWire format upstream of the capsfilter.
        let Some(input) = self.input.current_caps() else {
            return Ok(false);
        };
        let size = dimensions(input.structure(0).ok_or("missing PipeWire video format")?)?;
        let Some((width, height)) = limits
            .dimensions(source, size.0, size.1)
            .filter(|&(width, height)| width >= self.minimum.0 && height >= self.minimum.1)
        else {
            // Some PipeWire nodes produce dimensions below the encoder's
            // minimum. Retain the legal format chosen by the existing filter.
            return Ok(false);
        };
        if self.size == Some((width, height)) {
            return Ok(false);
        }
        let caps = format!(
            "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={},width={width},height={height}",
            self.nv12
        )
        .parse::<gst::Caps>()?;
        self.filter.set_property("caps", caps);
        self.size = Some((width, height));
        Ok(true)
    }
}

fn dimensions(format: &gst::StructureRef) -> Result<(u16, u16), Failure> {
    Ok((
        u16::try_from(format.get::<i32>("width")?)?,
        u16::try_from(format.get::<i32>("height")?)?,
    ))
}

/// The render node the worker may use: the first it can open.
fn render_node() -> Result<PathBuf, Failure> {
    let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    nodes
        .into_iter()
        .next()
        .ok_or_else(|| "no render node".into())
}

/// The encoder core for one picture size on the render node.
fn open_encoder(
    node: &Path,
    source: Source,
    settings: Settings,
) -> Result<Encoder<VaEncoder<Dmabuf>>, Failure> {
    let backend = VaEncoder::open(node, settings.width, settings.height)?;
    let config = Config {
        width: settings.width,
        height: settings.height,
        fps: settings.fps,
        bitrate: settings.bitrate,
        // Screens send temporal layers for the SFU to pick from.
        layering: match source {
            Source::Screen => Layering::L1T3,
            Source::Camera => Layering::L1T1,
        },
        screen: source == Source::Screen,
        frame_bytes: settings.packet_limit(false),
        window: WINDOW,
        qindex: 1..=255,
    };
    Encoder::new(backend, config).ok_or_else(|| "the picture needs more than one AV1 tile".into())
}

/// The NV12 picture in a sample's DMA-BUF, as the VA driver imports it.
fn dmabuf(sample: &gst::Sample) -> Result<Dmabuf, Failure> {
    let caps = sample.caps().ok_or("missing NV12 format")?;
    let info = VideoInfoDmaDrm::from_caps(caps)?;
    let buffer = sample.buffer().ok_or("missing NV12 frame")?;
    let meta = buffer.meta::<VideoMeta>().ok_or("missing NV12 layout")?;
    if buffer.n_memory() != 1 || meta.n_planes() != 2 {
        return Err("expected NV12 in one DMA-BUF".into());
    }
    let memory = buffer.peek_memory(0);
    let dmabuf = memory
        .downcast_memory_ref::<gstreamer_allocators::DmaBufMemory>()
        .ok_or("expected a DMA-BUF")?;
    let (offsets, strides) = (meta.offset(), meta.stride());
    Dmabuf::new(
        dmabuf.fd(),
        u32::try_from(memory.maxsize())?,
        info.modifier(),
        (u16::try_from(meta.width())?, u16::try_from(meta.height())?),
        [u32::try_from(offsets[0])?, u32::try_from(offsets[1])?],
        [u32::try_from(strides[0])?, u32::try_from(strides[1])?],
    )
    .ok_or_else(|| "invalid NV12 layout".into())
}

fn write_frames(
    pipeline: &gst::Pipeline,
    sink: &AppSink,
    scale: &mut Scale,
    source: Source,
    control: &Control,
) -> Result<(), Failure> {
    let bus = pipeline.bus().ok_or("missing pipeline bus")?;
    let node = render_node()?;
    let clock = Instant::now();
    let mut stdout = io::stdout().lock();
    let mut previous = 0;
    let mut current: Option<(Settings, Encoder<VaEncoder<Dmabuf>>)> = None;
    loop {
        if scale.update(source, control.limits())? {
            current = None;
        }
        let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(300)) else {
            if let Some(message) =
                bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos])
            {
                return match message.view() {
                    gst::MessageView::Error(message) => {
                        let error = message.error();
                        let reason = if error.matches(gst::CoreError::Negotiation)
                            || error.matches(gst::StreamError::Format)
                        {
                            CaptureFailure::HardwareFormat
                        } else if error.kind::<gst::ResourceError>().is_some() {
                            CaptureFailure::HardwareResource
                        } else {
                            CaptureFailure::Hardware
                        };
                        // Name the element and keep the last line of its
                        // diagnostic, such as a flow error's return value.
                        let element = message.src().map(|src| src.name());
                        let debug = message.debug();
                        let reason_line = debug.as_deref().and_then(|text| text.lines().last());
                        let detail = format!(
                            "{}: {error}{}",
                            element.as_deref().unwrap_or("pipeline"),
                            reason_line
                                .map(|line| format!(" ({line})"))
                                .unwrap_or_default(),
                        );
                        let _ = write_capture_failure(&mut stdout, reason, &detail);
                        Err(reason.into())
                    }
                    _ => Ok(()),
                };
            }
            continue;
        };
        let format = sample
            .caps()
            .and_then(|caps| caps.structure(0))
            .ok_or("missing NV12 format")?;
        let fps = format
            .get::<gst::Fraction>("framerate")
            .ok()
            .map_or(30, |rate| {
                (rate.numer() / rate.denom().max(1)).clamp(1, 120)
            });
        let (width, height) = dimensions(format)?;
        if scale.size.is_some_and(|wanted| wanted != (width, height))
            || width.min(height) > control.limits().height
        {
            continue;
        }
        let buffer = sample.buffer().ok_or("missing NV12 frame")?;
        let captured = sample.segment().and_then(|segment| {
            segment
                .downcast_ref::<gst::ClockTime>()?
                .to_running_time(buffer.pts())
        });
        let stale = captured
            .zip(pipeline.current_running_time())
            .is_some_and(|(then, now)| {
                now.saturating_sub(then) > gst::ClockTime::from_mseconds(250)
            });
        if buffer.flags().contains(gst::BufferFlags::CORRUPTED) || stale {
            continue;
        }
        // The control socket delivers new limits before a keyframe request,
        // so a requested keyframe carries the limits sent with it.
        let key = control.keyframe.swap(false, Ordering::Relaxed);
        let limits = control.limits();
        let settings = Settings {
            source,
            width,
            height,
            fps: u16::try_from(fps)?.min(limits.fps),
            bitrate: limits.bitrate,
        };
        settings.frame_bytes()?;
        if current
            .as_ref()
            .is_none_or(|(started, _)| (started.width, started.height) != (width, height))
        {
            current = Some((settings, open_encoder(&node, source, settings)?));
        }
        let Some((_, encoder)) = &mut current else {
            continue;
        };
        encoder.set_bitrate(settings.bitrate);
        if key {
            encoder.request_keyframe();
        }
        let timestamp = captured
            .map_or_else(
                || u64::try_from(clock.elapsed().as_micros()),
                |time| Ok(time.useconds()),
            )?
            .max(previous + 1);
        let surface: Surface<Dmabuf> = encoder.backend().import(dmabuf(&sample)?)?;
        let encoded = encoder.encode(&surface, Duration::from_micros(timestamp))?;
        drop(surface);
        drop(sample);
        let Some(encoded) = encoded else {
            continue;
        };
        previous = timestamp;
        super::write_frame(
            &mut stdout,
            Kind::HardwareFrame,
            settings,
            timestamp,
            encoded.keyframe,
            &encoded.data,
            &encoded.descriptor,
        )?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_keeps_the_capture_buffer_and_uses_separate_memory_headers() -> Result<(), Failure> {
        gst::init()?;
        let mut source = gst::Buffer::with_size(32)?;
        let buffer = source.get_mut().ok_or("shared fixture buffer")?;
        buffer.set_pts(gst::ClockTime::from_mseconds(42));
        buffer.append_memory(gst::Memory::with_size(16));

        let preview = separate_preview_memory(&source).ok_or("could not share fixture memory")?;
        assert_eq!(preview.pts(), source.pts());
        assert_eq!(preview.n_memory(), 2);
        assert!(
            preview
                .meta::<gst::ParentBufferMeta>()
                .is_some_and(|meta| { std::ptr::eq(meta.parent(), source.as_ref()) })
        );
        for (original, shared) in source.iter_memories().zip(preview.iter_memories()) {
            assert!(!std::ptr::eq(original, shared));
            assert!(
                shared
                    .parent()
                    .is_some_and(|parent| std::ptr::eq(parent, original))
            );
            assert_eq!(shared.sizes(), original.sizes());
        }
        Ok(())
    }

    #[test]
    fn encoder_caps_follow_the_lower_of_capture_cadence_and_native_framerate() -> Result<(), Failure>
    {
        gst::init()?;
        let source =
            "video/x-raw(memory:DMABuf),format=DMA_DRM,width=3440,height=1440,framerate=60000/1001"
                .parse::<gst::Caps>()?;
        let reduced =
            "video/x-raw(memory:DMABuf),format=DMA_DRM,width=3440,height=1440,framerate=15/1"
                .parse::<gst::Caps>()?;
        assert_eq!(cadence_caps(&source, 15), reduced);
        assert_eq!(cadence_caps(&source, 60), source);
        Ok(())
    }
}
