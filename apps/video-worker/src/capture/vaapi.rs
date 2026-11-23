// SPDX-License-Identifier: EUPL-1.2
//! Hardware AV1 from the authorized PipeWire stream. GStreamer imports
//! DMA-BUFs supported by the VA driver and uses VA surfaces through encode.

use super::{Cadence, Control, Failure, PREVIEW_FPS};
use gstreamer::{self as gst, prelude::*};
use gstreamer_app::AppSink;
use nuschl_ipc::video::{
    CaptureFailure, CaptureLimits, Kind, PREVIEW_HEIGHT, PREVIEW_WIDTH, Settings, Source,
    write_capture_failure, write_capture_preview,
};
use std::{
    io,
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const QUEUE: &str = "max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream";

const QVBR_QINDEX: u32 = 112;
// Finest quantizers for a screen whose keyframes approach the frame limit, up
// to 3440x1440 and above. Each keeps such a keyframe of a static screen full
// of text below the 1 MiB limit and the encoder's coded buffer.
const FLOOR_QINDEX: u32 = 128;
const LARGE_FLOOR_QINDEX: u32 = 144;

pub fn prepare(
    source: Source,
    node: Option<u32>,
    bitrate: u32,
    preview: bool,
) -> Result<gst::Pipeline, Failure> {
    let plugins = option_env!("NUSCHL_VIDEO_GST_PLUGINS")
        .ok_or("the VA-API GStreamer plugin list is missing")?;
    gst::init()?;
    for path in plugins.split(':') {
        gst::Plugin::load_file(path)?;
    }
    // Element registration probes the render node before the source starts.
    if gst::ElementFactory::find("vaav1enc").is_none() {
        return Err("the selected render node cannot encode AV1".into());
    }

    let target = node.map_or_else(String::new, |node| format!("path={node}"));
    let (width, height) = source.max_dimensions();
    // Screens send keyframes when a receiver asks for one. 1024 frames is
    // vaav1enc's longest keyframe interval.
    let key_interval = match source {
        Source::Screen => 1024,
        Source::Camera => 60,
    };
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
            "caps=\"video/x-raw(memory:VAMemory),format=NV12,width=[16,{width}],height=[16,{height}]\" ! ",
            "vaav1enc name=encoder bitrate={bitrate} target-percentage=100 hierarchical-level=1 ",
            "ref-frames=1 key-int-max={key_interval} ! ",
            "av1parse ! video/x-av1,stream-format=obu-stream,alignment=tu ! ",
            "appsink name=encoded max-buffers=1 sync=false{preview_path}",
        ),
        target = target,
        tee = tee,
        queue = QUEUE,
        width = width,
        height = height,
        bitrate = bitrate / 1000,
        key_interval = key_interval,
        preview_path = preview_path,
    );
    let pipeline = gst::parse::launch(&description)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "invalid video pipeline")?;
    let encoder = pipeline.by_name("encoder").ok_or("missing video encoder")?;
    // Screens use quality-defined VBR where the driver offers it. The mode is
    // fixed for the life of the encoder.
    let modes: &[&str] = match source {
        Source::Screen => &["qvbr", "vbr", "cbr"],
        Source::Camera => &["cbr"],
    };
    let rates = encoder
        .find_property("rate-control")
        .and_then(|property| gst::glib::EnumClass::with_type(property.value_type()))
        .ok_or("missing video rate control")?;
    let (mode, rate) = modes
        .iter()
        .find_map(|mode| Some((*mode, rates.to_value_by_nick(mode)?)))
        .ok_or("the render node has no supported video rate control")?;
    // The target percentage makes the negotiated bitrate the codec's peak
    // rate budget. QVBR drivers can still overshoot it. Other modes ignore
    // the quality factor.
    encoder.set_property_from_value("rate-control", &rate);
    encoder.set_property("qp", QVBR_QINDEX);
    // VBR keyframes of a static screen grow from one to the next until one
    // overflows the coded buffer, so VBR and CBR screens start floored.
    if source == Source::Screen && mode != "qvbr" {
        encoder.set_property("min-qp", FLOOR_QINDEX);
    }
    Ok(pipeline)
}

pub fn run(
    pipeline: gst::Pipeline,
    source: Source,
    control: Arc<Control>,
    preview: Option<UnixStream>,
) -> Result<(), Failure> {
    let sink = pipeline
        .by_name("encoded")
        .ok_or("missing video sink")?
        .downcast::<AppSink>()
        .map_err(|_| "invalid video sink")?;
    let encoder = pipeline.by_name("encoder").ok_or("missing video encoder")?;
    let mut scale = Scale::new(&pipeline, &encoder)?;
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
        let result = write_frames(&pipeline, &sink, &encoder, &mut scale, source, &control);
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
    encoder: gst::Pad,
    size: Option<(u16, u16)>,
}

impl Scale {
    fn new(pipeline: &gst::Pipeline, encoder: &gst::Element) -> Result<Self, Failure> {
        let input = pipeline
            .by_name("scale")
            .and_then(|element| element.static_pad("sink"))
            .ok_or("missing video scaler input")?;
        let filter = pipeline
            .by_name("limit")
            .ok_or("missing video caps filter")?;
        let encoder = encoder
            .static_pad("sink")
            .ok_or("missing video encoder input")?;
        Ok(Self {
            input,
            filter,
            encoder,
            size: None,
        })
    }

    fn update(&mut self, source: Source, limits: CaptureLimits) -> Result<bool, Failure> {
        // Measure the unscaled PipeWire format upstream of the capsfilter.
        let Some(input) = self.input.current_caps() else {
            return Ok(false);
        };
        let size = dimensions(input.structure(0).ok_or("missing PipeWire video format")?)?;
        let Some((width, height)) = limits.dimensions(source, size.0, size.1) else {
            // Some PipeWire nodes produce dimensions below the VA encoder's
            // minimum. Retain the legal format chosen by the existing filter.
            return Ok(self.size.take().is_some());
        };
        if self.size == Some((width, height)) {
            return Ok(false);
        }
        let caps =
            format!("video/x-raw(memory:VAMemory),format=NV12,width={width},height={height}")
                .parse::<gst::Caps>()?;
        if !caps.can_intersect(&self.encoder.pad_template_caps()) {
            return Ok(self.size.take().is_some());
        }
        self.filter.set_property("caps", caps);
        self.size = Some((width, height));
        // Request a keyframe when the encoder's input size changes.
        let encoding = self
            .encoder
            .current_caps()
            .and_then(|caps| caps.structure(0).and_then(|format| dimensions(format).ok()));
        Ok(encoding != self.size)
    }
}

fn dimensions(format: &gst::StructureRef) -> Result<(u16, u16), Failure> {
    Ok((
        u16::try_from(format.get::<i32>("width")?)?,
        u16::try_from(format.get::<i32>("height")?)?,
    ))
}

fn write_frames(
    pipeline: &gst::Pipeline,
    sink: &AppSink,
    encoder: &gst::Element,
    scale: &mut Scale,
    source: Source,
    control: &Control,
) -> Result<(), Failure> {
    let bus = pipeline.bus().ok_or("missing pipeline bus")?;
    let clock = Instant::now();
    let mut stdout = io::stdout().lock();
    let mut bitrate = encoder.property::<u32>("bitrate");
    let mut previous = 0;
    let mut waiting_for_key = true;
    let mut requested_at: Option<Instant> = None;
    let mut floored = false;
    loop {
        let limits = control.limits();
        if scale.update(source, limits)? {
            waiting_for_key = true;
            requested_at = None;
        }
        let wanted = limits.bitrate / 1000;
        if wanted != bitrate {
            encoder.set_property("bitrate", wanted);
            bitrate = encoder.property::<u32>("bitrate");
        }
        if requested_at.is_none_or(|time| time.elapsed() >= Duration::from_millis(150))
            && (control.keyframe.swap(false, Ordering::Relaxed) || waiting_for_key)
        {
            force_keyframe(encoder)?;
            requested_at = Some(Instant::now());
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
            .ok_or("missing AV1 format")?;
        let fps = format
            .get::<gst::Fraction>("framerate")
            .ok()
            .map_or(30, |rate| {
                (rate.numer() / rate.denom().max(1)).clamp(1, 120)
            });
        let (width, height) = dimensions(format)?;
        if scale.size.is_some_and(|wanted| wanted != (width, height))
            || width.min(height) > limits.height
        {
            continue;
        }
        let settings = Settings {
            source,
            width,
            height,
            fps: u16::try_from(fps)?.min(limits.fps),
            bitrate: bitrate * 1000,
        };
        settings.frame_bytes()?;

        let buffer = sample.buffer().ok_or("missing AV1 packet")?;
        let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
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
        let size = buffer.size();
        if size == 0 || buffer.flags().contains(gst::BufferFlags::CORRUPTED) || stale {
            waiting_for_key = true;
            continue;
        }
        // The next keyframe after one within an eighth of the limit can exceed
        // the limit or the coded buffer. Setting the quality factor makes the
        // encoder reconfigure with the floor, for the rest of the capture.
        if keyframe
            && source == Source::Screen
            && !floored
            && size > settings.packet_limit(true) / 8 * 7
        {
            floored = true;
            let large = u32::from(width) * u32::from(height) > 3440 * 1440;
            let floor = if large {
                LARGE_FLOOR_QINDEX
            } else {
                FLOOR_QINDEX
            };
            encoder.set_property("min-qp", floor);
            encoder.set_property("qp", QVBR_QINDEX);
        }
        if size > settings.packet_limit(keyframe) {
            waiting_for_key = true;
            continue;
        }
        if waiting_for_key && !keyframe {
            continue;
        }
        waiting_for_key = false;
        let bytes = buffer.map_readable()?;
        let timestamp = captured
            .map_or_else(
                || u64::try_from(clock.elapsed().as_micros()),
                |time| Ok(time.useconds()),
            )?
            .max(previous + 1);
        previous = timestamp;
        super::write_frame(
            &mut stdout,
            Kind::HardwareFrame,
            settings,
            timestamp,
            keyframe,
            bytes.as_slice(),
        )?;
    }
}

fn force_keyframe(encoder: &gst::Element) -> Result<(), Failure> {
    let source = encoder
        .static_pad("src")
        .ok_or("missing video encoder output")?;
    // The GstVideoEncoder upstream key-unit event takes a running time in ns.
    let event = gst::event::CustomUpstream::new(
        gst::Structure::builder("GstForceKeyUnit")
            .field("running-time", u64::MAX)
            .field("all-headers", true)
            .field("count", 0u32)
            .build(),
    );
    // The first request can race the pipeline's transition to Playing.
    source.send_event(event);
    Ok(())
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
