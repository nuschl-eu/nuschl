// SPDX-License-Identifier: EUPL-1.2
//! Live audio and all codec/DSP state stay in this process, without server secrets.
//!
//! The device callbacks only move samples through lock-free single-producer
//! rings; they never lock, allocate or block. This thread decodes, buffers,
//! mixes, cancels echo, encodes and talks to the supervisor.
use crate::{
    MAX_PACKET_BYTES,
    adaptive::{Controller, WINDOW},
    capture::CaptureConverter,
    capture_queue::CaptureQueue,
    dsp::{CapturePipeline, FRAME},
    mixer::{Mixer, Voice},
    playback::PlaybackConverter,
    probe::EchoProbe,
    reference::{self, Clock, Played, ReferenceConverter, Stamper, Step, Timeline},
    sounds::Effects,
    timing::Timing,
};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use nuschl_core::audio::{AudioDevice, AudioSettings, EchoMode, EchoSource};
use nuschl_ipc::{Envelope, LiveMediaEvent, LiveMediaRequest, read_frame, write_frame};
use opus_rs::OpusEncoder;
use rtrb::{Consumer, Producer, RingBuffer};
use std::{
    collections::VecDeque,
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant},
};

/// Mixed audio is kept ready in the playback ring; the adaptive controller
/// sizes it (device buffering is extra). The ring must hold the largest fill.
const RING: usize = 20 * FRAME;
/// The canceller's initial delay guess: device buffering on both sides plus
/// the acoustic path. Far-end audio reaches it when the device takes it from
/// Nuschl's ring, so the ring's own fill is not part of the delay.
const ECHO_DELAY_HINT_MS: i32 = 30;

const LEVEL_EVERY: u32 = 10;

/// Call a stream builder with the sample type of `format`.
macro_rules! by_format {
    ($format:expr, $build:ident($($argument:expr),* $(,)?)) => {
        match $format {
            SampleFormat::F32 => $build::<f32>($($argument),*),
            SampleFormat::I16 => $build::<i16>($($argument),*),
            SampleFormat::U16 => $build::<u16>($($argument),*),
            SampleFormat::I32 => $build::<i32>($($argument),*),
            format => Err(format!("unsupported sample format {format}")),
        }
    };
}

enum Event {
    Request(LiveMediaRequest),
    /// A stream failed; `microphone` is the number of the microphone it
    /// belongs to (see [`Microphone`]), `None` for the output.
    DeviceError {
        message: String,
        microphone: Option<u64>,
    },
    /// A microphone opened on its own thread (or why not), with the source
    /// it borrowed.
    Opened(Box<(Result<Microphone, String>, MicrophoneSource)>),
    /// A microphone closed on its own thread, for the mute of this epoch.
    MicrophoneClosed(u64),
    Closed,
    /// Windows: time to service the open devices (see [`Ticker`]).
    #[cfg(windows)]
    Tick,
}

struct Devices {
    _output: cpal::Stream,
    playback: Producer<[f64; 2]>,
    /// Actual negotiated device layout, fixed for this output stream.
    output_channels: usize,
    /// Output callbacks that found the ring empty; written by the device thread.
    underruns: Arc<AtomicU32>,
    clock: Clock,
    /// What the output callback hands to the device: the canceller's
    /// reference where the output cannot be recorded.
    played: Reference,
    /// The microphone, open while the supervisor says so (see `SetState`).
    microphone: Option<Microphone>,
    /// What opens the microphone again; `None` while a thread opens it.
    source: Option<MicrophoneSource>,
    route: Route,
}

/// The open microphone. Muting closes it in this process; playback goes on.
struct Microphone {
    /// Its errors carry this number, so a closing microphone's are ignored.
    number: u64,
    _input: cpal::Stream,
    samples: Consumer<f32>,
    timeline: Timeline,
    /// The recording of the output device for the canceller, while the
    /// microphone is open.
    recording: Option<(cpal::Stream, Reference)>,
    /// The canceller's reference is `Devices::played` (no recording).
    played: bool,
    /// What `recording` records, for the route report.
    reference_name: Option<String>,
}

/// What a microphone opens from, kept with the devices.
struct MicrophoneSource {
    host: cpal::Host,
    all: Vec<cpal::Device>,
    input: Option<cpal::Device>,
    output: cpal::Device,
    output_id: Option<String>,
    /// Nuschl cancels the echo: the output is recorded as its reference.
    echo: bool,
    clock: Clock,
    labels: &'static Labels,
    timing: Timing,
    tx: SyncSender<Event>,
}

/// The canceller's reference stream and how far it has been fed.
struct Reference {
    samples: Consumer<f32>,
    timeline: Timeline,
    channels: usize,
    index: u64,
}

impl Reference {
    /// Discard what is queued: it played before the microphone opened.
    fn skip(&mut self) {
        let frames = self.samples.slots() / self.channels;
        if let Ok(chunk) = self.samples.read_chunk(frames * self.channels) {
            chunk.commit_all();
        }
        self.index += frames as u64;
        let _ = self.timeline.time_of(self.index);
    }
}

struct Transmit {
    encoder: OpusEncoder,
    queue: CaptureQueue,
    packet: [u8; MAX_PACKET_BYTES],
    sequence: u64,
    active: bool,
}

impl Transmit {
    /// Encode exactly 20 ms of already-processed, currently authorized PCM.
    fn encode(&mut self, samples: &[f32; 2 * FRAME]) -> Option<Vec<u8>> {
        // The destination's per-frame allowance: every sender admits frames
        // against this same bound (nuschl_core::bitrate).
        let max_bytes = nuschl_core::bitrate::max_voice_frame_bytes(
            u32::try_from(self.encoder.bitrate_bps).unwrap_or(0),
        )
        .min(self.packet.len());
        let count = self
            .encoder
            .encode(samples, 2 * FRAME, &mut self.packet[..max_bytes])
            .ok()?;
        Some(self.packet[..count].to_vec())
    }
}

/// `--probe-route`: open the default (or configured) devices once, as a call
/// would, and print what Nuschl found upstream of the microphone. A
/// diagnostic for users: it opens the microphone briefly and sends nothing.
pub fn probe_route() -> Result<(), Box<dyn std::error::Error>> {
    let (tx, _rx) = mpsc::sync_channel(64);
    let devices = open_devices(
        tx,
        true,
        &AudioSettings::default(),
        None,
        &CALL,
        &Timing::new(),
        0,
    )?;
    let route = &devices.route;
    let reference = devices
        .microphone
        .as_ref()
        .and_then(|microphone| microphone.reference_name.as_ref());
    let report = serde_json::json!({
        "input": route.input,
        "output": route.output,
        "echo_cancellation": route.echo,
        "noise_suppression_upstream": route.noise,
        "echo_reference": reference,
        "inputs": route.inputs,
        "outputs": route.outputs,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    // Dropping native streams can block; the OS closes them with the process.
    std::process::exit(0)
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let timing = Timing::new();
    timing.mark("worker_start");
    let mut stats = crate::stats::Stats::new(timing.enabled());
    let (mut first_processed, mut first_transmit, mut first_packet) = (true, true, true);
    let (tx, rx) = mpsc::sync_channel(256);
    let input_tx = tx.clone();
    std::thread::spawn(move || forward_requests(io::stdin().lock(), &input_tx));
    // Windows rounds the receive timeout below up to its 15.6 ms timer tick;
    // this ticker wakes the loop on time instead (see `Ticker`). Without it
    // (the thread could not start) the loop keeps its own timeout.
    #[cfg(windows)]
    let mut ticker = Ticker::start(tx.clone(), || Event::Tick, SERVICE_PERIOD);
    let mut output = io::stdout().lock();
    let mut settings = AudioSettings::default();
    let encoder = crate::encoder::live()?;
    let mut transmit = Transmit {
        encoder,
        queue: CaptureQueue::default(),
        packet: [0; MAX_PACKET_BYTES],
        sequence: 0,
        active: false,
    };
    let mut muted = true;
    let mut deafened = false;
    // Devices open only after the supervisor states mute/deafen: dropping a live
    // native stream can block, so streams are never reopened for a state change.
    let mut state_known = false;
    let mut epoch = 0;
    let mut push_to_talk = false;
    // Every server's voice, mixed (see mixer.rs).
    let mut mixer = Mixer::default();
    let mut devices: Option<Devices> = None;
    let mut pipeline: Option<CapturePipeline> = None;
    // The reference channels `pipeline` was built for.
    let mut chain_channels = 0;
    let mut microphones = 0_u64;
    let mut retry_at = Instant::now();
    let mut last_cleanup = Instant::now();
    let mut frames_seen = 0_u32;
    let mut only_host: Option<String> = None;
    let mut controller = Controller::default();
    let mut last_window = Instant::now();
    // Start-up underruns (the device starts before the ring is filled) are not a signal.
    let mut skip_window = true;
    let mut capture_frame = [0.; FRAME];
    let mut mix = [[0.; 2]; FRAME];
    let mut wide_mix = [[0_f64; 2]; FRAME];
    let mut probe_frame = [0_f32; FRAME];
    let mut mono_effects = [0.; FRAME];
    // Local test mode (Settings): capture and process, but never encode or send.
    let mut preview = false;
    let mut loopback = false;
    let mut looped = VecDeque::<f32>::with_capacity(RING);
    let mut probe: Option<EchoProbe> = None;
    // Microphone frames processed since the microphone opened, and a buffer for
    // one reference frame (up to two channels).
    let mut mic_index = 0_u64;
    let mut reference_frame = [0_f32; 2 * FRAME];
    let mut far_mono = [0_f32; FRAME];
    // A noise suppressor upstream of the microphone (e.g. RNNoise) replaces ours.
    let mut upstream_noise = false;
    let mut effects = Effects::new();
    loop {
        if devices.is_none() && state_known && !deafened && Instant::now() >= retry_at {
            let labels = if preview { &CHECK } else { &CALL };
            microphones += 1;
            match open_devices(
                tx.clone(),
                !muted,
                &settings,
                only_host.as_deref(),
                labels,
                &timing,
                microphones,
            ) {
                Ok(mut opened) => {
                    upstream_noise = opened.route.noise.is_some();
                    // New devices: the canceller starts over.
                    pipeline = None;
                    timing.mark("pipeline_ready");
                    timing.report_capture();
                    let microphone = opened.microphone.take();
                    let open = devices.insert(opened);
                    match microphone {
                        Some(microphone) => {
                            mic_index = 0;
                            install(
                                open,
                                microphone,
                                (&mut pipeline, &mut chain_channels),
                                &settings,
                                upstream_noise,
                                &mut output,
                            )?;
                        }
                        None => {
                            announce(open, &mut output)?;
                            report(open, muted, &mut output)?;
                        }
                    }
                    last_window = Instant::now();
                    skip_window = true;
                }
                Err(error) => {
                    emit(
                        &mut output,
                        LiveMediaEvent::Status {
                            ready: false,
                            message: format!("Audio reconnecting: {error}"),
                        },
                    )?;
                    retry_at = Instant::now() + Duration::from_secs(2);
                }
            }
        }
        #[cfg(windows)]
        if let Some(ticker) = ticker.as_mut() {
            ticker.run_while(devices.is_some());
        }
        // Open devices are serviced every few milliseconds; without them the
        // worker waits for the supervisor or its next attempt to open them.
        let timeout = if devices.is_some() {
            Duration::from_millis(4)
        } else if state_known && !deafened {
            retry_at.saturating_duration_since(Instant::now())
        } else {
            Duration::MAX
        };
        let event = match rx.recv_timeout(timeout) {
            Ok(event) => Some(event),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(_) => break,
        };
        match event {
            Some(Event::Closed | Event::Request(LiveMediaRequest::Shutdown)) => break,
            // A closing microphone's streams may report their own ending.
            Some(Event::DeviceError {
                microphone: Some(number),
                ..
            }) if devices
                .as_ref()
                .and_then(|open| open.microphone.as_ref())
                .is_none_or(|mic| mic.number != number) => {}
            Some(Event::DeviceError { message, .. }) => {
                transmit.queue.reset();
                if transmit.active {
                    finish(&mut output, &mut transmit, epoch)?;
                }
                devices.take();
                retry_at = Instant::now() + Duration::from_secs(1);
                emit(
                    &mut output,
                    LiveMediaEvent::Status {
                        ready: false,
                        message: format!("Audio reconnecting: {message}"),
                    },
                )?;
            }
            Some(Event::Request(LiveMediaRequest::SetState {
                epoch: new_epoch,
                muted: mute,
                deafened: deaf,
            })) => {
                timing.mark(if mute || deaf {
                    "set_state_muted"
                } else {
                    "set_state_unmuted"
                });
                let was_transmitting = transmit.active;
                if new_epoch != epoch || mute || deaf {
                    transmit.queue.reset();
                }
                let moved = new_epoch != epoch;
                muted = mute || deaf;
                epoch = new_epoch;
                deafened = deaf;
                state_known = true;
                if deaf {
                    mixer.clear();
                }
                if was_transmitting && muted {
                    // Audio captured before this request arrived may already
                    // be after the user muted: the terminator is silence.
                    transmit.queue.reset();
                    finish(&mut output, &mut transmit, epoch)?;
                }
                match devices.as_mut() {
                    // Nothing captured from here on is processed. The streams
                    // close on their own thread: one that blocks in its backend
                    // holds up neither playback nor this loop, and the
                    // supervisor replaces this process if it is not confirmed
                    // closed soon.
                    Some(open) if muted => {
                        if let Some(microphone) = open.microphone.take() {
                            close(microphone, tx.clone(), epoch);
                            open.route.reference = None;
                            report(open, muted, &mut output)?;
                        } else if open.source.is_some() {
                            emit(&mut output, LiveMediaEvent::MicrophoneClosed { epoch })?;
                        }
                        // A microphone being opened is closed once it is.
                    }
                    Some(open) => match open.microphone.as_mut() {
                        // A new destination gets none of the audio captured
                        // before it (the epoch changes with the destinations).
                        Some(microphone) if moved => {
                            let queued = microphone.samples.slots();
                            if let Ok(chunk) = microphone.samples.read_chunk(queued) {
                                chunk.commit_all();
                            }
                            mic_index += queued as u64;
                        }
                        Some(_) => {}
                        None => {
                            if let Some(mut source) = open.source.take() {
                                microphones += 1;
                                let number = microphones;
                                let tx = tx.clone();
                                std::thread::spawn(move || {
                                    let opened = open_microphone(&mut source, number);
                                    let _ = tx.send(Event::Opened(Box::new((opened, source))));
                                });
                            }
                        }
                    },
                    None if muted => emit(&mut output, LiveMediaEvent::MicrophoneClosed { epoch })?,
                    None => {}
                }
            }
            Some(Event::Opened(lent)) => match (*lent, devices.as_mut()) {
                // These devices lent the source: they take it back.
                ((opened, source), Some(open)) if open.source.is_none() => {
                    open.source = Some(source);
                    match opened {
                        Ok(microphone) if !muted => {
                            mic_index = 0;
                            install(
                                open,
                                microphone,
                                (&mut pipeline, &mut chain_channels),
                                &settings,
                                upstream_noise,
                                &mut output,
                            )?;
                        }
                        Ok(microphone) => close(microphone, tx.clone(), epoch),
                        Err(error) => {
                            eprintln!("microphone unavailable: {error}");
                            if muted {
                                emit(&mut output, LiveMediaEvent::MicrophoneClosed { epoch })?;
                            }
                            report(open, muted, &mut output)?;
                        }
                    }
                }
                ((opened, _), _) => {
                    if let Ok(microphone) = opened {
                        close(microphone, tx.clone(), epoch);
                    }
                }
            },
            Some(Event::MicrophoneClosed(closed)) => {
                emit(
                    &mut output,
                    LiveMediaEvent::MicrophoneClosed { epoch: closed },
                )?;
            }
            Some(Event::Request(LiveMediaRequest::Configure {
                bitrate,
                packet_loss_percent,
                settings: requested,
                host,
            })) => {
                timing.mark("configure");
                only_host = host;
                transmit.encoder.bitrate_bps = bitrate.clamp(
                    nuschl_core::audio::MIN_VOICE_BITRATE,
                    nuschl_core::bitrate::MAX_VOICE_BITRATE,
                ) as i32;
                transmit.encoder.packet_loss_perc = i32::from(packet_loss_percent.clamp(1, 30));
                let requested = requested.sanitized();
                if !requested.same_route(&settings) && devices.is_some() {
                    // A new route needs new streams; the supervisor restarts this process.
                    emit(
                        &mut output,
                        LiveMediaEvent::Status {
                            ready: false,
                            message: "Switching audio devices…".into(),
                        },
                    )?;
                    std::process::exit(0);
                }
                if requested != settings {
                    transmit.queue.reset();
                    if transmit.active {
                        finish(&mut output, &mut transmit, epoch)?;
                    }
                }
                if requested != settings
                    && let Some(chain) = pipeline.as_mut()
                {
                    // The canceller and filters keep what they learned.
                    chain.configure(&chain_settings(&requested, upstream_noise));
                }
                settings = requested;
            }
            Some(Event::Request(LiveMediaRequest::Preview {
                capture,
                loopback: play_back,
            })) => {
                if state_known && capture == muted && devices.is_some() {
                    // Opening or closing the microphone needs a new process (see SetState).
                    emit(
                        &mut output,
                        LiveMediaEvent::Status {
                            ready: false,
                            message: "Restarting audio…".into(),
                        },
                    )?;
                    std::process::exit(0);
                }
                transmit.queue.reset();
                preview = true;
                muted = !capture;
                deafened = false;
                state_known = true;
                loopback = play_back;
                if !loopback {
                    looped.clear();
                }
            }
            Some(Event::Request(LiveMediaRequest::EchoTest)) => {
                if devices
                    .as_ref()
                    .is_some_and(|open| open.microphone.is_some())
                {
                    probe = Some(EchoProbe::new());
                }
            }
            Some(Event::Request(LiveMediaRequest::PushToTalk { active })) => push_to_talk = active,
            Some(Event::Request(LiveMediaRequest::PlaySound { sound, volume })) => {
                effects.play(sound, volume, Instant::now());
            }
            Some(Event::Request(LiveMediaRequest::Playback {
                stream,
                sender,
                sequence,
                opus,
                end,
                volume,
            })) => {
                if deafened {
                    continue;
                }
                let voice = Voice {
                    stream,
                    sender,
                    sequence,
                    opus,
                    end,
                    volume,
                };
                mixer.push(voice, Instant::now())?;
            }
            Some(Event::Request(LiveMediaRequest::EndStream { stream })) => {
                mixer.end_stream(stream)
            }
            // Serviced below, exactly as after the timeout.
            #[cfg(windows)]
            Some(Event::Tick) => {
                if let Some(ticker) = &ticker {
                    ticker.received();
                }
            }
            None => {}
        }
        let Some(open) = devices.as_mut() else {
            continue;
        };
        // Playback: keep a short, steady amount of mixed audio queued.
        while open.playback.slots() >= FRAME
            && RING - open.playback.slots() < controller.playback_samples()
        {
            wide_mix.fill([0.; 2]);
            mono_effects.fill(0.);
            mixer.mix(&mut wide_mix);
            if loopback {
                for (out, sample) in mono_effects
                    .iter_mut()
                    .zip(looped.drain(..looped.len().min(FRAME)))
                {
                    *out += f64::from(sample);
                }
            }
            if let Some(probe) = probe.as_mut() {
                probe_frame.fill(0.);
                probe.render(&mut probe_frame);
                for (out, sample) in mono_effects.iter_mut().zip(&probe_frame) {
                    *out += f64::from(*sample);
                }
            }
            // Effects join the output here, so the canceller's reference (a
            // recording of the device, or what this ring hands it) has them.
            effects.mix(&mut mono_effects, Instant::now());
            crate::mixer::finish(open.output_channels, &wide_mix, &mono_effects, &mut mix);
            let (pushed, _) = open.playback.push_partial_slice(&mix);
            if pushed.len() < FRAME {
                break;
            }
            // With a reference recording, far-end activity is judged from it
            // (all applications) when it reaches the canceller, below.
            if !open
                .microphone
                .as_ref()
                .is_some_and(|mic| mic.recording.is_some() || mic.played)
            {
                // Activity detection must not cancel antiphase LR; this is not
                // the AEC reference, which keeps the actual two channels below.
                for (mono, frame) in far_mono.iter_mut().zip(&mix) {
                    *mono = if frame[0].abs() >= frame[1].abs() {
                        frame[0]
                    } else {
                        frame[1]
                    }
                    .clamp(-1., 1.) as f32;
                }
                controller.observe_render(&far_mono);
            }
        }
        if last_window.elapsed() >= WINDOW {
            last_window = Instant::now();
            let underruns = open.underruns.swap(0, Ordering::Relaxed);
            let underruns = if std::mem::take(&mut skip_window) {
                0
            } else {
                underruns
            };
            stats.underruns(underruns);
            let residual = pipeline
                .as_ref()
                .filter(|chain| chain.echo_cancellation())
                .and_then(|chain| chain.echo_metrics().residual_likelihood);
            if let Some(health) = controller.window(residual, underruns) {
                emit(
                    &mut output,
                    LiveMediaEvent::Health {
                        clipping: health.clipping,
                        echo_guard: health.echo_guard,
                        double_talk: health.double_talk,
                        underruns: health.underruns,
                        playback_ms: health.playback_ms,
                    },
                )?;
            }
        }
        // Capture: process every complete 10 ms frame, each after the
        // reference frames that sounded up to it (see reference.rs).
        let Devices {
            microphone, played, ..
        } = open;
        let Some(microphone) = microphone.as_mut() else {
            continue;
        };
        let capture = &mut microphone.samples;
        while capture.slots() >= FRAME {
            let Some(chain) = pipeline.as_mut() else {
                break;
            };
            let mic_time = microphone.timeline.time_of(mic_index);
            let reference = match &mut microphone.recording {
                Some((_, recorded)) => Some(recorded),
                None if microphone.played => Some(&mut *played),
                None => None,
            };
            if let (Some(reference), Some(mic_time)) = (reference, mic_time) {
                let width = FRAME * reference.channels;
                let mut hold = false;
                loop {
                    let available = reference.samples.slots() >= width;
                    let time = reference.timeline.time_of(reference.index);
                    match reference::step(time, available, mic_time) {
                        decision @ (Step::Feed | Step::Drop) => {
                            let Ok(chunk) = reference.samples.read_chunk(width) else {
                                break;
                            };
                            let (first, second) = chunk.as_slices();
                            reference_frame[..first.len()].copy_from_slice(first);
                            reference_frame[first.len()..width].copy_from_slice(second);
                            chunk.commit_all();
                            reference.index += FRAME as u64;
                            if decision == Step::Feed {
                                let frame = &reference_frame[..width];
                                chain.render(frame);
                                for (mono, samples) in far_mono
                                    .iter_mut()
                                    .zip(frame.chunks_exact(reference.channels))
                                {
                                    // Activity is not an acoustic downmix: opposite
                                    // LR phases must not conceal far-end playback.
                                    *mono = samples
                                        .iter()
                                        .copied()
                                        .max_by(|a, b| a.abs().total_cmp(&b.abs()))
                                        .unwrap_or(0.);
                                }
                                controller.observe_render(&far_mono);
                            }
                        }
                        Step::Later => break,
                        Step::Wait => {
                            // The reference lags: hold the microphone briefly
                            // rather than give the canceller its echo first.
                            hold = capture.slots() < reference::MAX_HOLD_FRAMES * FRAME;
                            break;
                        }
                    }
                }
                if hold {
                    break;
                }
            }
            if capture.pop_entire_slice(&mut capture_frame).is_err() {
                break;
            }
            stats.capture(mic_index, mic_time);
            mic_index += FRAME as u64;
            timing.report_capture();
            if first_processed {
                timing.mark("first_processed_pcm");
                first_processed = false;
            }
            let raw = capture_frame;
            chain.set_far_end(controller.far_active());
            let result = chain.process(&mut capture_frame, push_to_talk, controller.guard_gate());
            controller.observe_capture(&raw, result.voice);
            frames_seen = frames_seen.wrapping_add(1);
            // Only Settings shows a meter.
            if preview && frames_seen.is_multiple_of(LEVEL_EVERY) {
                emit(
                    &mut output,
                    LiveMediaEvent::Level {
                        level_db: result.level_db,
                        voice: result.voice,
                        transmitting: result.transmit,
                    },
                )?;
            }
            if let Some(report) = probe
                .as_mut()
                .and_then(|probe| probe.observe(&raw, &capture_frame, result.transmit))
            {
                probe = None;
                emit(
                    &mut output,
                    LiveMediaEvent::EchoTest {
                        echo_db: report.echo_db,
                        reduction_db: report.reduction_db,
                        transmitted: report.transmitted,
                    },
                )?;
            }
            if loopback && result.transmit {
                // Play back what others would hear; never more than 200 ms behind.
                looped.extend(capture_frame);
                let excess = looped.len().saturating_sub(RING / 2);
                looped.drain(..excess);
            }
            if frames_seen.is_multiple_of(200) && chain.echo_cancellation() {
                let metrics = chain.echo_metrics();
                emit(
                    &mut output,
                    LiveMediaEvent::Echo {
                        erle_db: metrics.erle_db,
                        residual_likelihood: metrics.residual_likelihood,
                        delay_ms: metrics.delay_ms,
                    },
                )?;
            }
            if muted || preview {
                continue;
            }
            if result.transmit && first_transmit {
                timing.mark("first_open_gate");
                first_transmit = false;
            }
            transmit.queue.push(
                capture_frame,
                result.transmit,
                settings.transmit == nuschl_core::audio::TransmitMode::VoiceActivity
                    && !controller.guard_gate(),
            );
        }
        if let Some(packet) = transmit.queue.due(Instant::now()) {
            if let Some(opus) = transmit.encode(&packet.samples) {
                if first_packet {
                    timing.mark("first_packet");
                    first_packet = false;
                }
                // An end packet's frames after the gate closed are not queued.
                if !packet.end {
                    stats.send(
                        open.clock.now_shifted(Duration::ZERO, Duration::ZERO),
                        transmit.queue.queued(),
                        transmit.queue.late,
                    );
                }
                emit(
                    &mut output,
                    LiveMediaEvent::Packet {
                        epoch,
                        sequence: transmit.sequence,
                        opus,
                        end: packet.end,
                    },
                )?;
                transmit.active = !packet.end;
                if packet.end {
                    transmit.encoder.reset();
                }
            }
            transmit.sequence += 2;
        }
        if last_cleanup.elapsed() > Duration::from_secs(2) {
            mixer.cleanup(Instant::now());
            last_cleanup = Instant::now();
        }
    }
    stats.print();
    output.flush()?;
    // Stop and join the ticker before exiting (dropping it does both).
    #[cfg(windows)]
    drop(ticker);
    if devices.is_some() {
        // Dropping native streams can block; the OS closes them with the process.
        std::process::exit(0);
    }
    Ok(())
}

/// Hand the supervisor's requests to the main loop until its pipe closes.
/// Voice and sound effects are dropped while the loop is behind, so a
/// control request after them is still read at once; control requests wait
/// for room in the loop's queue and keep their order.
fn forward_requests(mut input: impl io::Read, tx: &SyncSender<Event>) {
    while let Ok(Some(frame)) = read_frame::<LiveMediaRequest>(&mut input) {
        let request = frame.payload;
        let queued = if matches!(
            request,
            LiveMediaRequest::Playback { .. } | LiveMediaRequest::PlaySound { .. }
        ) {
            !matches!(
                tx.try_send(Event::Request(request)),
                Err(mpsc::TrySendError::Disconnected(_))
            )
        } else {
            tx.send(Event::Request(request)).is_ok()
        };
        if !queued {
            return;
        }
    }
    let _ = tx.send(Event::Closed);
}

/// Close a transmission with a terminator packet so receivers stop promptly.
fn finish(
    output: &mut impl io::Write,
    transmit: &mut Transmit,
    epoch: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    transmit.queue.reset();
    emit(
        output,
        LiveMediaEvent::Packet {
            epoch,
            sequence: transmit.sequence,
            opus: nuschl_core::audio::SILENT_OPUS.to_vec(),
            end: true,
        },
    )?;
    transmit.sequence += 2;
    transmit.active = false;
    transmit.encoder.reset();
    Ok(())
}

fn emit(
    output: &mut impl io::Write,
    event: LiveMediaEvent,
) -> Result<(), Box<dyn std::error::Error>> {
    write_frame(output, &Envelope::new(0, event))?;
    Ok(())
}

#[derive(Clone)]
struct Route {
    inputs: Vec<AudioDevice>,
    outputs: Vec<AudioDevice>,
    input: Option<String>,
    output: Option<String>,
    echo: EchoSource,
    noise: Option<String>,
    /// The output recorded as the canceller's reference; `None` when Nuschl
    /// cancels only its own playback (or does not cancel at all).
    reference: Option<String>,
}

/// The processing Nuschl applies itself: a noise suppressor upstream of the
/// microphone (e.g. an RNNoise filter-chain) replaces Nuschl's own.
fn chain_settings(settings: &AudioSettings, upstream_noise: bool) -> AudioSettings {
    let mut settings = settings.clone();
    settings.noise_suppression &= !upstream_noise;
    settings
}

/// What volume mixers (pavucontrol, GNOME Settings, Helvum) call the streams.
struct Labels {
    microphone: &'static str,
    playback: &'static str,
}

const CALL: Labels = Labels {
    microphone: "Microphone",
    playback: "Voice chat",
};
/// The audio check in Settings: echo test and the processed voice played back.
const CHECK: Labels = Labels {
    microphone: "Microphone test",
    playback: "Audio test",
};

fn open_devices(
    tx: SyncSender<Event>,
    capture: bool,
    settings: &AudioSettings,
    only_host: Option<&str>,
    labels: &'static Labels,
    timing: &Timing,
    microphone: u64,
) -> Result<Devices, String> {
    let mut hosts = vec![];
    #[cfg(target_os = "linux")]
    {
        let forced = std::env::var("NUSCHL_AUDIO_BACKEND").ok();
        match forced.as_deref().or(only_host) {
            Some("pulse" | "pulseaudio") => hosts.push(cpal::HostId::PulseAudio),
            Some("pipewire") => hosts.push(cpal::HostId::PipeWire),
            Some("alsa") => hosts.push(cpal::HostId::Alsa),
            _ => {
                hosts.push(cpal::HostId::PipeWire);
                hosts.push(cpal::HostId::PulseAudio);
                hosts.push(cpal::HostId::Alsa);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = only_host;
        hosts.push(cpal::default_host().id());
    }
    let mut errors = Vec::new();
    for id in hosts {
        timing.mark("host_connect_begin");
        match connect(id).and_then(|host| {
            timing.mark("host_connected");
            open_host(
                host,
                tx.clone(),
                capture,
                settings,
                labels,
                timing,
                microphone,
            )
        }) {
            Ok(opened) => return Ok(opened),
            Err(error) => {
                // Diagnostics only: stderr never carries protocol frames.
                eprintln!("audio host {id:?} unavailable: {error}");
                errors.push(format!("{id:?}: {error}"));
            }
        }
    }
    Err(errors.join("; "))
}

/// Connect to an audio server as Nuschl, the application mixers show.
fn connect(id: cpal::HostId) -> Result<cpal::Host, String> {
    #[cfg(target_os = "linux")]
    let host = {
        let application = || cpal::ApplicationInfo {
            name: "Nuschl".into(),
            icon_name: Some("org.nuschl.Nuschl".into()),
        };
        match id {
            cpal::HostId::PipeWire => {
                cpal::platform::PipeWireHost::with_application(application()).map(cpal::Host::from)
            }
            cpal::HostId::PulseAudio => {
                cpal::platform::PulseAudioHost::with_application(application())
                    .map(cpal::Host::from)
            }
            id => cpal::host_from_id(id),
        }
    };
    #[cfg(not(target_os = "linux"))]
    let host = cpal::host_from_id(id);
    host.map_err(|error| error.to_string())
}

/// Name the streams built next from `host`'s devices. PipeWire also gets the
/// communication role, as stock Mumble sets; PulseAudio's equivalent
/// ("phone") would pause music for as long as Nuschl is in a call
/// (module-role-cork), so it is left out there.
fn label(host: &mut cpal::Host, name: &str) {
    #[cfg(target_os = "linux")]
    {
        let metadata = |role| cpal::StreamMetadata {
            name: Some(name.into()),
            role,
        };
        // Names come from Nuschl's constants and device names: a NUL byte
        // (refused) only costs the label.
        let _ = match host.as_inner_mut() {
            cpal::platform::HostInner::PipeWire(host) => {
                host.set_stream_metadata(metadata(Some(cpal::StreamRole::Communication)))
            }
            cpal::platform::HostInner::PulseAudio(host) => host.set_stream_metadata(metadata(None)),
            _ => Ok(()),
        };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (host, name);
}

fn device_id(device: &cpal::Device) -> Option<String> {
    device.id().ok().map(|id| id.to_string())
}

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|description| description.name().to_string())
        .unwrap_or_else(|_| device_id(device).unwrap_or_else(|| "Unknown device".into()))
}

/// The backend-local part of a device identifier, e.g. a PulseAudio name.
fn backend_name(id: &str) -> &str {
    id.split_once(':').map_or(id, |(_, name)| name)
}

/// Recognise PipeWire's and PulseAudio's echo-cancel nodes by name. Anything
/// else is unknown: missing metadata is not proof that no canceller exists.
pub fn is_echo_cancelled(id: &str, name: &str) -> bool {
    let text = format!("{} {}", backend_name(id), name).to_lowercase();
    ["echo-cancel", "echo_cancel", "echocancel", "echo cancel"]
        .iter()
        .any(|needle| text.contains(needle))
}

/// The playback node feeding an echo-cancel source's reference, if any.
pub fn paired_reference<'a>(source_id: &str, sinks: &'a [AudioDevice]) -> Option<&'a AudioDevice> {
    let source = backend_name(source_id);
    let swapped = source.replace("source", "sink").replace("input", "output");
    sinks
        .iter()
        .find(|sink| backend_name(&sink.id) == swapped)
        .or_else(|| {
            let candidates: Vec<_> = sinks
                .iter()
                .filter(|sink| is_echo_cancelled(&sink.id, &sink.name))
                .collect();
            (candidates.len() == 1).then(|| candidates[0])
        })
}

fn open_host(
    mut host: cpal::Host,
    tx: SyncSender<Event>,
    capture: bool,
    settings: &AudioSettings,
    labels: &'static Labels,
    timing: &Timing,
    number: u64,
) -> Result<Devices, String> {
    let all: Vec<cpal::Device> = host
        .devices()
        .map(|devices| devices.collect())
        .unwrap_or_default();
    // PipeWire's node list also contains other applications' streams, sinks
    // offered as (monitor) inputs, and cpal's virtual default entries. The
    // PulseAudio API (pipewire-pulse) knows which nodes are real sinks and
    // sources, so the PipeWire lists keep only those when it is available.
    #[cfg(target_os = "linux")]
    let pipewire = host.id() == cpal::HostId::PipeWire;
    #[cfg(not(target_os = "linux"))]
    let pipewire = false;
    #[cfg(target_os = "linux")]
    let pulse_nodes = pipewire.then(pulse_device_names).flatten();
    #[cfg(not(target_os = "linux"))]
    let pulse_nodes: Option<(Vec<String>, Vec<String>)> = None;
    let listed = |input: bool| -> Vec<AudioDevice> {
        all.iter()
            .filter(|device| {
                if input {
                    device.supports_input()
                } else {
                    device.supports_output()
                }
            })
            .filter_map(|device| {
                let id = device_id(device)?;
                let name = backend_name(&id);
                // Sink monitors are listed as sources by PulseAudio; they are not microphones.
                let keep = if !pipewire {
                    !(input && name.ends_with(".monitor"))
                } else if let Some((sources, sinks)) = &pulse_nodes {
                    let real = if input { sources } else { sinks };
                    real.iter().any(|node| node == name)
                } else {
                    let duplex = device.supports_input() && device.supports_output();
                    !matches!(name, "input_default" | "output_default" | "sink_default")
                        && !(input && duplex)
                };
                keep.then(|| AudioDevice {
                    name: device_name(device),
                    id,
                })
            })
            .take(128)
            .collect()
    };
    timing.mark("devices_listed");
    let inputs = listed(true);
    let outputs = listed(false);
    let find = |id: &Option<String>, input: bool| {
        id.as_ref().and_then(|wanted| {
            all.iter().find(|device| {
                (if input {
                    device.supports_input()
                } else {
                    device.supports_output()
                }) && device_id(device).as_deref() == Some(wanted.as_str())
            })
        })
    };
    let input_device = find(&settings.input_device, true)
        .cloned()
        .or_else(|| host.default_input_device());
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut input_id = input_device.as_ref().and_then(device_id);
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut input_name = input_device.as_ref().map(device_name).unwrap_or_default();
    // The microphone opens first: a filter-chain's capture (as in PipeWire's
    // RNNoise configuration) is passive and is linked to its own source only
    // once someone records from the chain.
    // Every callback stamps its samples on this clock (see reference.rs).
    let clock = Clock::new();
    let mut microphone = capture
        .then(|| {
            let tx = tx.clone();
            open_input(
                &mut host,
                input_device.as_ref(),
                labels,
                tx,
                clock,
                timing,
                number,
            )
            .ok()
        })
        .flatten();
    // Processing that already happens before Nuschl: follow PipeWire's graph
    // upstream (mic → echo-cancel → RNNoise → default source), falling back to
    // recognising an echo-cancel source by name (PulseAudio, other systems).
    #[cfg(target_os = "linux")]
    let (found, resolved_input) = input_id
        .as_ref()
        .map(|id| upstream_processing(backend_name(id), microphone.is_some(), pipewire))
        .unwrap_or_default();
    // The virtual CPAL default follows native PipeWire metadata. Resolve it
    // while inspecting the existing graph, without a second startup query or
    // a dependency on the optional pipewire-pulse compatibility service.
    #[cfg(target_os = "linux")]
    if pipewire && let Some((name, label)) = resolved_input {
        input_id = Some(format!("pipewire:{name}"));
        input_name = label;
    }
    #[cfg(not(target_os = "linux"))]
    let found = crate::graph::Upstream::default();
    timing.mark("graph_inspected");
    let upstream_echo = found.echo.clone().or_else(|| {
        input_id
            .as_ref()
            .is_some_and(|id| is_echo_cancelled(id, &input_name))
            .then(|| input_name.clone())
    });
    let mode = input_id
        .as_ref()
        .map_or(settings.echo_cancellation, |id| settings.echo_for(id));
    // Reported even without capture, so Settings can explain the route before a test.
    let echo = match (mode, &upstream_echo) {
        (EchoMode::Off, _) => EchoSource::None,
        (EchoMode::On, _) => EchoSource::Nuschl,
        (EchoMode::Auto, Some(name)) => EchoSource::Upstream(name.clone()),
        (EchoMode::Auto, None) => EchoSource::Nuschl,
    };
    // With a system canceller, Nuschl's own playback must reach its reference.
    // Only Nuschl's stream is routed; system defaults and other apps are untouched.
    let reference = if settings.output_device.is_none() && upstream_echo.is_some() {
        found
            .reference
            .as_deref()
            .and_then(|name| {
                outputs
                    .iter()
                    .find(|device| backend_name(&device.id) == name)
            })
            .or_else(|| {
                input_id
                    .as_ref()
                    .and_then(|id| paired_reference(id, &outputs))
            })
            .map(|device| device.id.clone())
    } else {
        None
    };
    let output_device = find(&settings.output_device, false)
        .or_else(|| find(&reference, false))
        .cloned()
        .or_else(|| host.default_output_device())
        .ok_or("no default output device")?;
    let output_id = device_id(&output_device);
    let config = output_device
        .default_output_config()
        .map_err(|error| error.to_string())?;
    let output_channels = usize::from(config.channels());
    let (playback, playback_consumer) = RingBuffer::<[f64; 2]>::new(RING);
    let underruns = Arc::new(AtomicU32::new(0));
    // What the output callback hands to the device: the reference where the
    // output device cannot be recorded.
    let (played_samples, played_consumer) = RingBuffer::<f32>::new(2 * reference::RING_FRAMES);
    let (played_stamper, played_timeline) = reference::stamped();
    let played = (
        Played {
            samples: played_samples,
            stamper: played_stamper,
        },
        clock,
    );
    timing.mark("playback_create_begin");
    label(&mut host, labels.playback);
    let stream = by_format!(
        config.sample_format(),
        output_stream(
            &output_device,
            stream_config(&host, config),
            tx.clone(),
            (playback_consumer, underruns.clone()),
            played
        )
    )?;
    timing.mark("playback_created");
    stream.play().map_err(|error| error.to_string())?;
    timing.mark("playback_play_ready");
    let mut source = MicrophoneSource {
        host,
        all,
        input: input_device,
        output: output_device,
        output_id: output_id.clone(),
        echo: matches!(echo, EchoSource::Nuschl),
        clock,
        labels,
        timing: timing.clone(),
        tx,
    };
    if let Some(microphone) = microphone.as_mut() {
        record_reference(&mut source, microphone);
    }
    Ok(Devices {
        _output: stream,
        playback,
        output_channels,
        underruns,
        clock,
        played: Reference {
            samples: played_consumer,
            timeline: played_timeline,
            channels: 2,
            index: 0,
        },
        microphone,
        source: Some(source),
        route: Route {
            inputs,
            outputs,
            input: input_id,
            output: output_id,
            echo,
            noise: found.noise,
            reference: None,
        },
    })
}

/// Open the microphone from `source` (the input stream, then the canceller's
/// recording of the output).
fn open_microphone(source: &mut MicrophoneSource, number: u64) -> Result<Microphone, String> {
    // PulseAudio: a connection of its own, so opening and closing the
    // microphone leaves the playback stream's connection alone, and closing
    // ends the record stream with its connection.
    #[cfg(target_os = "linux")]
    let mut own = match (source.host.id(), &source.input) {
        (cpal::HostId::PulseAudio, Some(input)) => {
            let own = connect(cpal::HostId::PulseAudio)?;
            let id = device_id(input);
            let found = own
                .devices()
                .map_err(|error| error.to_string())?
                .find(|device| device.supports_input() && device_id(device) == id)
                .ok_or("the microphone is gone")?;
            Some((own, found))
        }
        _ => None,
    };
    #[cfg(not(target_os = "linux"))]
    let mut own: Option<(cpal::Host, cpal::Device)> = None;
    let (host, input) = match own.as_mut() {
        Some((own, found)) => (own, Some(&*found)),
        None => (&mut source.host, source.input.as_ref()),
    };
    let mut microphone = open_input(
        host,
        input,
        source.labels,
        source.tx.clone(),
        source.clock,
        &source.timing,
        number,
    )?;
    record_reference(source, &mut microphone);
    Ok(microphone)
}

/// The input stream of `input`, stamped on `clock` (see reference.rs).
fn open_input(
    host: &mut cpal::Host,
    input: Option<&cpal::Device>,
    labels: &Labels,
    tx: SyncSender<Event>,
    clock: Clock,
    timing: &Timing,
    number: u64,
) -> Result<Microphone, String> {
    let input = input.ok_or("no microphone")?;
    label(host, labels.microphone);
    timing.mark("capture_create_begin");
    let config = input
        .default_input_config()
        .map_err(|error| error.to_string())?;
    let (producer, samples) = RingBuffer::<f32>::new(RING);
    let (stamper, timeline) = reference::stamped();
    let stream = by_format!(
        config.sample_format(),
        input_stream(
            input,
            stream_config(host, config),
            (tx, number),
            producer,
            (stamper, clock),
            timing.clone()
        )
    )?;
    timing.mark("capture_created");
    stream.play().map_err(|error| error.to_string())?;
    timing.mark("capture_play_ready");
    Ok(Microphone {
        number,
        _input: stream,
        samples,
        timeline,
        recording: None,
        played: false,
        reference_name: None,
    })
}

/// Nuschl's canceller needs to know everything the speakers play: record
/// the output device (all applications) where the system allows it, else
/// Nuschl's own playback. Only while the microphone is open.
fn record_reference(source: &mut MicrophoneSource, microphone: &mut Microphone) {
    if !source.echo {
        return;
    }
    source.timing.mark("reference_create_begin");
    match record_output(
        &mut source.host,
        &source.all,
        &source.output,
        source.output_id.as_deref(),
        (source.tx.clone(), microphone.number),
        source.clock,
    ) {
        Ok((stream, reference, name)) => {
            source.timing.mark("reference_play_ready");
            microphone.recording = Some((stream, reference));
            microphone.reference_name = Some(name);
        }
        Err(error) => {
            eprintln!(
                "output recording unavailable, using Nuschl's playback as the echo reference: {error}"
            );
            microphone.played = true;
        }
    }
}

/// Close `microphone` on its own thread, then report the mute of `epoch` done.
fn close(microphone: Microphone, tx: SyncSender<Event>, epoch: u64) {
    std::thread::spawn(move || {
        drop(microphone);
        let _ = tx.send(Event::MicrophoneClosed(epoch));
    });
}

/// Start processing `microphone` and report the route. The capture chain
/// that ran before is kept when it fits: its canceller keeps what it
/// learned about the room across a mute.
fn install(
    open: &mut Devices,
    mut microphone: Microphone,
    (chain, chain_channels): (&mut Option<CapturePipeline>, &mut usize),
    settings: &AudioSettings,
    upstream_noise: bool,
    output: &mut impl io::Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let echo = matches!(open.route.echo, EchoSource::Nuschl);
    let channels = match &microphone.recording {
        Some((_, reference)) => reference.channels,
        None if microphone.played => {
            open.played.skip();
            open.played.channels
        }
        None => 1,
    };
    match chain.as_mut() {
        Some(kept) if kept.echo_cancellation() == echo && *chain_channels == channels => {
            kept.configure(&chain_settings(settings, upstream_noise));
        }
        _ => {
            let mut fresh = CapturePipeline::with_reference(
                &chain_settings(settings, upstream_noise),
                echo,
                channels,
            );
            fresh.set_delay_hint(ECHO_DELAY_HINT_MS);
            *chain = Some(fresh);
            *chain_channels = channels;
        }
    }
    open.route.reference = microphone.reference_name.take();
    open.microphone = Some(microphone);
    announce(open, output)?;
    report(open, false, output)
}

/// Report the devices and the route.
fn announce(open: &Devices, output: &mut impl io::Write) -> Result<(), Box<dyn std::error::Error>> {
    let route = open.route.clone();
    emit(
        output,
        LiveMediaEvent::Devices {
            inputs: route.inputs,
            outputs: route.outputs,
            input: route.input,
            output: route.output,
            echo: route.echo,
            noise: route.noise,
            reference: route.reference,
        },
    )
}

/// Report that the devices are ready, and what the microphone does.
fn report(
    open: &Devices,
    muted: bool,
    output: &mut impl io::Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let message = if muted {
        "Audio ready · microphone muted".to_string()
    } else if open.microphone.is_some() {
        match (&open.route.echo, &open.route.noise) {
            (EchoSource::Upstream(name), _) => {
                format!("Audio ready · system echo cancellation ({name})")
            }
            (_, Some(name)) => format!("Audio ready · system noise suppression ({name})"),
            (EchoSource::Nuschl, None) => "Audio ready · echo cancellation on".into(),
            (EchoSource::None, None) => "Audio ready".into(),
        }
    } else if open.source.is_none() {
        // The microphone is still opening.
        return Ok(());
    } else {
        "Playback ready; microphone unavailable".into()
    };
    emit(
        output,
        LiveMediaEvent::Status {
            ready: true,
            message,
        },
    )
}

/// Walk PipeWire's graph upstream from `source`. While our own capture has
/// just started, passive filter links may still be on their way: wait for
/// them briefly rather than miss a canceller behind a noise suppressor.
#[cfg(target_os = "linux")]
fn upstream_processing(
    source: &str,
    capturing: bool,
    pipewire: bool,
) -> (crate::graph::Upstream, Option<(String, String)>) {
    let mut found = crate::graph::Upstream::default();
    let default = pipewire && crate::graph::default_source_alias(source);
    let mut resolved = None;
    // Query the compatibility API at most once, and only if the native
    // effective default is absent. Explicit device selections stay pinned.
    let mut pulse_fallback = None;
    for attempt in 0..8 {
        let graph = crate::graph::snapshot();
        if default {
            resolved = graph
                .as_ref()
                .and_then(|graph| graph.default_source_identity());
            if resolved.is_none()
                && graph
                    .as_ref()
                    .is_none_or(|graph| graph.default_source.is_none())
            {
                resolved = pulse_fallback
                    .get_or_insert_with(|| {
                        resolve_pulse_default(true)
                            .map(|(id, label)| (backend_name(&id).to_owned(), label))
                    })
                    .clone();
            }
        }
        let Some(graph) = graph else {
            break;
        };
        let source = resolved.as_ref().map_or(source, |(name, _)| name);
        found = graph.upstream(source);
        if !(capturing && found.dangling) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50 + attempt * 25));
    }
    (found, resolved)
}

/// Whether a PulseAudio server (native or pipewire-pulse) may be listening,
/// checked before cpal waits for a connection.
#[cfg(target_os = "linux")]
fn pulse_running() -> bool {
    std::env::var_os("PULSE_SERVER").is_some_and(|server| !server.is_empty())
        || std::env::var_os("XDG_RUNTIME_DIR")
            .is_some_and(|runtime| std::path::Path::new(&runtime).join("pulse/native").exists())
}

/// Real sources (without monitors) and sinks by node name, from the PulseAudio
/// API (pipewire-pulse) when it is running.
#[cfg(target_os = "linux")]
fn pulse_device_names() -> Option<(Vec<String>, Vec<String>)> {
    if !pulse_running() {
        return None;
    }
    let host = connect(cpal::HostId::PulseAudio).ok()?;
    let (mut sources, mut sinks) = (Vec::new(), Vec::new());
    for device in host.devices().ok()? {
        let Some(id) = device_id(&device) else {
            continue;
        };
        let name = backend_name(&id).to_string();
        if device.supports_input() && !name.ends_with(".monitor") {
            sources.push(name.clone());
        }
        if device.supports_output() {
            sinks.push(name);
        }
    }
    Some((sources, sinks))
}

/// Ask the PulseAudio API (native or pipewire-pulse) for the default node.
#[cfg(target_os = "linux")]
fn resolve_pulse_default(input: bool) -> Option<(String, String)> {
    if !pulse_running() {
        return None;
    }
    let host = connect(cpal::HostId::PulseAudio).ok()?;
    let device = if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    }?;
    Some((device_id(&device)?, device_name(&device)))
}

/// The channel count and rate of a device format the converters handle.
fn convertible(config: &cpal::StreamConfig) -> Result<(usize, u32), String> {
    let channels = usize::from(config.channels);
    let rate = config.sample_rate;
    if channels == 0 || channels > 32 || !(8000..=384_000).contains(&rate) {
        return Err(format!(
            "unsupported device format: {channels} channels at {rate} Hz"
        ));
    }
    Ok((channels, rate))
}

/// Report errors that end a stream. A stream that follows the system default
/// (`DeviceChanged`: the audio server moved it to the new default device, as
/// when Bluetooth headphones connect) keeps running and needs no reopening.
/// `microphone` numbers a microphone's errors; `stream` names the stream in
/// the message.
fn error_callback(
    tx: SyncSender<Event>,
    microphone: Option<u64>,
    stream: &'static str,
) -> impl FnMut(cpal::Error) + Send + 'static {
    move |error| {
        if !matches!(
            error.kind(),
            cpal::ErrorKind::Xrun
                | cpal::ErrorKind::RealtimeDenied
                | cpal::ErrorKind::DeviceChanged
        ) {
            let _ = tx.try_send(Event::DeviceError {
                message: format!("{stream}: {error}"),
                microphone,
            });
        }
    }
}

/// Downmix and resample to 48 kHz mono into the capture ring. Runs on the
/// device thread: no locks, allocation or blocking.
fn input_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    (tx, microphone): (SyncSender<Event>, u64),
    mut ring: Producer<f32>,
    (mut stamper, clock): (Stamper, Clock),
    timing: Timing,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let (channels, rate) = convertible(&config)?;
    let mut converter = CaptureConverter::new(channels, rate);
    device
        .build_input_stream(
            config,
            move |data: &[T], info: &cpal::InputCallbackInfo| {
                if !data.is_empty() {
                    timing.capture();
                }
                let timestamp = info.timestamp();
                let latency = timestamp
                    .callback
                    .saturating_duration_since(timestamp.capture);
                stamper.anchor(converter.capture_time(clock.now_shifted(latency, Duration::ZERO)));
                stamper.advance(converter.push(data, &mut ring));
            },
            error_callback(tx, Some(microphone), "microphone"),
            None,
        )
        .map_err(|error| error.to_string())
}

/// Record what an output device plays (all applications), as the echo
/// canceller's reference: PipeWire captures the sink, PulseAudio records its
/// monitor source, WASAPI and Core Audio (process tap) record in loopback.
fn record_output(
    host: &mut cpal::Host,
    all: &[cpal::Device],
    output: &cpal::Device,
    output_id: Option<&str>,
    tx: (SyncSender<Event>, u64),
    clock: Clock,
) -> Result<(cpal::Stream, Reference, String), String> {
    #[cfg(target_os = "linux")]
    if host.id() == cpal::HostId::Alsa {
        return Err("ALSA cannot record an output device".into());
    }
    // A readable name for what is recorded; the default sink's own name where
    // cpal only offers a virtual "default" entry.
    #[cfg(target_os = "linux")]
    let name = (output_id
        .map(backend_name)
        .is_none_or(|name| name == "output_default"))
    .then(|| resolve_pulse_default(false).map(|(_, name)| name))
    .flatten()
    .unwrap_or_else(|| device_name(output));
    #[cfg(not(target_os = "linux"))]
    let name = device_name(output);
    let description = format!("Echo reference (what {name} plays)");
    label(host, &description);
    #[cfg(target_os = "linux")]
    let pulse_monitor = (host.id() == cpal::HostId::PulseAudio)
        .then(|| {
            let sink = output_id
                .map(backend_name)
                .map(str::to_string)
                .or_else(|| {
                    resolve_pulse_default(false).map(|(id, _)| backend_name(&id).to_string())
                })?;
            let monitor = format!("{sink}.monitor");
            // A connection of its own: cpal's PulseAudio client stops a stream
            // playing into a sink while the same connection records its monitor.
            let mut separate = connect(cpal::HostId::PulseAudio).ok()?;
            label(&mut separate, &description);
            let devices: Vec<cpal::Device> = separate.devices().ok()?.collect();
            devices.into_iter().find(|device| {
                device.supports_input()
                    && device_id(device).as_deref().map(backend_name) == Some(monitor.as_str())
            })
        })
        .flatten();
    // PipeWire: a chosen sink is recorded directly (cpal sets
    // stream.capture.sink); the default output through cpal's "sink_default".
    #[cfg(target_os = "linux")]
    let pipewire_sink = (host.id() == cpal::HostId::PipeWire
        && output_id
            .map(backend_name)
            .is_none_or(|name| name == "output_default"))
    .then(|| {
        all.iter()
            .find(|device| device_id(device).as_deref().map(backend_name) == Some("sink_default"))
            .cloned()
    })
    .flatten();
    #[cfg(not(target_os = "linux"))]
    let (pulse_monitor, pipewire_sink): (Option<cpal::Device>, Option<cpal::Device>) = {
        let _ = (all, output_id);
        (None, None)
    };
    let device = pulse_monitor
        .or(pipewire_sink)
        .unwrap_or_else(|| output.clone());
    // On Linux only a sink (or its monitor) may be recorded here: never risk
    // treating a microphone as the speakers. Windows and macOS record output
    // devices in loopback.
    #[cfg(target_os = "linux")]
    let config = {
        let id = device_id(&device).unwrap_or_default();
        let sink_like = backend_name(&id).ends_with(".monitor")
            || backend_name(&id) == "sink_default"
            || device.supports_output();
        if !sink_like || !device.supports_input() {
            return Err(format!("{id} cannot be recorded as an output"));
        }
        device
            .default_input_config()
            .map_err(|error| error.to_string())?
    };
    #[cfg(not(target_os = "linux"))]
    let config = device
        .default_input_config()
        .or_else(|_| output.default_output_config())
        .map_err(|error| error.to_string())?;
    let format = config.sample_format();
    let stream_config = stream_config(host, config);
    let (channels, rate) = convertible(&stream_config)?;
    let mut converter = ReferenceConverter::new(channels, rate);
    let outputs = converter.outputs();
    let (mut ring, samples) = RingBuffer::<f32>::new(reference::RING_FRAMES * outputs);
    let (mut stamper, timeline) = reference::stamped();
    let callback = move |values: &mut dyn Iterator<Item = f32>, info: &cpal::InputCallbackInfo| {
        let timestamp = info.timestamp();
        let latency = timestamp
            .callback
            .saturating_duration_since(timestamp.capture);
        // Filtered reference samples describe earlier capture instants;
        // unlike playback delay, compensate toward the past.
        stamper.anchor(clock.now_shifted(latency + converter.timestamp_delay(), Duration::ZERO));
        stamper.advance(converter.push(values, &mut ring));
    };
    let stream = by_format!(
        format,
        build_recording(&device, stream_config, tx, callback)
    )?;
    stream.play().map_err(|error| error.to_string())?;
    Ok((
        stream,
        Reference {
            samples,
            timeline,
            channels: outputs,
            index: 0,
        },
        name,
    ))
}

fn build_recording<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    (tx, microphone): (SyncSender<Event>, u64),
    mut callback: impl FnMut(&mut dyn Iterator<Item = f32>, &cpal::InputCallbackInfo) + Send + 'static,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device
        .build_input_stream(
            config,
            move |data: &[T], info: &cpal::InputCallbackInfo| {
                callback(&mut data.iter().map(|value| f32::from_sample(*value)), info);
            },
            error_callback(tx, Some(microphone), "speaker recording"),
            None,
        )
        .map_err(|error| error.to_string())
}

/// Resample the mixed 48 kHz playback ring to the device. Underruns play silence.
fn output_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    tx: SyncSender<Event>,
    (mut ring, underruns): (Consumer<[f64; 2]>, Arc<AtomicU32>),
    (mut played, clock): (Played, Clock),
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
    f32: FromSample<T>,
{
    let (channels, rate) = convertible(&config)?;
    let mut converter = PlaybackConverter::new(channels, rate);
    device
        .build_output_stream(
            config,
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                let timestamp = info.timestamp();
                let ahead = timestamp
                    .playback
                    .saturating_duration_since(timestamp.callback);
                played
                    .stamper
                    .anchor(converter.reference_time(clock.now_shifted(Duration::ZERO, ahead)));
                let (dry, handed) = converter.pull(data, &mut ring, &mut played.samples);
                played.stamper.advance(handed);
                if dry {
                    // Lock-free; the worker thread reads and resets it.
                    underruns.fetch_add(1, Ordering::Relaxed);
                }
            },
            error_callback(tx, None, "speakers"),
            None,
        )
        .map_err(|error| error.to_string())
}

fn stream_config(host: &cpal::Host, supported: cpal::SupportedStreamConfig) -> cpal::StreamConfig {
    #[cfg(target_os = "linux")]
    let buffer = device_buffer(supported.buffer_size());
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut config: cpal::StreamConfig = supported.into();
    // PulseAudio's default can buffer two seconds. These Linux backends control
    // per-stream buffers, without changing a system device's global settings.
    #[cfg(target_os = "linux")]
    if matches!(host.id(), cpal::HostId::PulseAudio | cpal::HostId::PipeWire) {
        config.buffer_size = buffer;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = host;
    config
}

/// About 10 ms of device frames, within what the device accepts. PipeWire
/// refuses streams outside its quantum range, which is raised in virtual
/// machines and by users (e.g. 1024..2048); a fixed request would fail there.
fn device_buffer(supported: &cpal::SupportedBufferSize) -> cpal::BufferSize {
    const WANTED: u32 = 480;
    match supported {
        cpal::SupportedBufferSize::Range { min, max } if min <= max => {
            cpal::BufferSize::Fixed(WANTED.clamp(*min, *max))
        }
        _ => cpal::BufferSize::Fixed(WANTED),
    }
}

/// How often open devices are serviced when the platform cannot be relied
/// on to end a short receive timeout on time.
#[cfg(any(windows, test))]
const SERVICE_PERIOD: Duration = Duration::from_millis(4);

/// Wakes the worker loop every [`SERVICE_PERIOD`] while devices are open.
///
/// The loop waits with `recv_timeout`, which on Windows ends at the system
/// timer tick (15.625 ms unless a process raises the resolution): a 4 ms
/// timeout serviced the devices every 15.6 to 31 ms. `std::thread::sleep`
/// uses a high-resolution waitable timer (Windows 10 1803 and later), also
/// inside the worker's AppContainer, so this thread's sleeps end on time.
///
/// At most one wakeup is queued: `pending` stays set until the loop takes it,
/// and a full queue (which wakes the loop anyway) drops it. While no devices
/// are open the thread parks. Dropping the ticker stops and joins the thread.
#[cfg(any(windows, test))]
struct Ticker {
    state: Arc<TickerState>,
    thread: Option<std::thread::JoinHandle<()>>,
    running: bool,
}

#[cfg(any(windows, test))]
struct TickerState {
    pending: std::sync::atomic::AtomicBool,
    running: std::sync::atomic::AtomicBool,
    stop: std::sync::atomic::AtomicBool,
}

#[cfg(any(windows, test))]
impl Ticker {
    /// `None` if the thread cannot be started.
    fn start<T: Send + 'static>(
        tx: SyncSender<T>,
        tick: fn() -> T,
        period: Duration,
    ) -> Option<Self> {
        use std::sync::atomic::AtomicBool;
        let state = Arc::new(TickerState {
            pending: AtomicBool::new(false),
            running: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let shared = state.clone();
        let thread = std::thread::Builder::new()
            .name("media-service-tick".into())
            .spawn(move || {
                while !shared.stop.load(Ordering::Acquire) {
                    if !shared.running.load(Ordering::Acquire) {
                        std::thread::park();
                        continue;
                    }
                    std::thread::sleep(period);
                    if shared.pending.swap(true, Ordering::AcqRel) {
                        continue;
                    }
                    match tx.try_send(tick()) {
                        Ok(()) => {}
                        Err(mpsc::TrySendError::Full(_)) => {
                            shared.pending.store(false, Ordering::Release);
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => return,
                    }
                }
            })
            .ok()?;
        Some(Self {
            state,
            thread: Some(thread),
            running: false,
        })
    }

    /// Tick while `running` (devices are open); otherwise park the thread.
    fn run_while(&mut self, running: bool) {
        if running == self.running {
            return;
        }
        self.running = running;
        self.state.running.store(running, Ordering::Release);
        if let (true, Some(thread)) = (running, &self.thread) {
            thread.thread().unpark();
        }
    }

    /// The loop took the queued wakeup; the next one may be queued.
    fn received(&self) {
        self.state.pending.store(false, Ordering::Release);
    }
}

#[cfg(any(windows, test))]
impl Drop for Ticker {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
#[path = "output_fixture.rs"]
mod output_fixture;

#[cfg(test)]
#[path = "capture_fixture.rs"]
mod capture_fixture;

#[cfg(test)]
mod tests {
    #[test]
    fn a_stream_moved_to_the_new_default_device_keeps_its_devices() {
        let (tx, rx) = super::mpsc::sync_channel(4);
        let mut report = super::error_callback(tx, None, "speakers");
        report(cpal::ErrorKind::DeviceChanged.into());
        assert!(rx.try_recv().is_err());
        report(cpal::ErrorKind::DeviceNotAvailable.into());
        assert!(matches!(
            rx.try_recv(),
            Ok(super::Event::DeviceError { message, .. }) if message.starts_with("speakers: ")
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "explicit metadata-only PipeWire fixture; no capture or playback"]
    fn inspect_native_default_metadata_without_capture() {
        let output = std::env::var_os("NUSCHL_GRAPH_AUDIT_OUTPUT")
            .expect("explicit private metadata output path");
        let (found, resolved) = super::upstream_processing("input_default", false, true);
        let graph = crate::graph::snapshot().expect("fixture native PipeWire is available");
        let expected = graph.default_source_identity();
        assert!(expected.is_some(), "fixture publishes an effective source");
        assert_eq!(resolved, expected);
        assert_eq!(found, graph.upstream(&resolved.as_ref().unwrap().0));
        std::fs::write(
            output,
            serde_json::to_vec_pretty(&serde_json::json!({
                "resolved": resolved,
                "echo": found.echo,
                "noise": found.noise,
                "reference": found.reference,
                "dangling": found.dangling,
                "capture_opened": false,
                "playback_opened": false,
                "scope": "native read-only metadata and graph inspection only"
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn real_mono_packets_fit_default_and_reduced_live_targets_without_devices() {
        let mut encoder = OpusEncoder::new(48_000, 1, opus_rs::Application::Voip).unwrap();
        encoder.use_inband_fec = true;
        encoder.packet_loss_perc = 5;
        let mut tx = super::Transmit {
            encoder,
            queue: CaptureQueue::default(),
            packet: [0; MAX_PACKET_BYTES],
            sequence: 0,
            active: false,
        };
        let mut decoder = opus_rs::OpusDecoder::new(48_000, 1).unwrap();
        for target in [
            nuschl_core::audio::DEFAULT_VOICE_BITRATE,
            24_000,
            6_000,
            nuschl_core::bitrate::MAX_VOICE_BITRATE,
            192_000,
            128_000,
        ] {
            tx.encoder.bitrate_bps = target as i32;
            for frame in 0..8 {
                let samples = std::array::from_fn(|i| {
                    (std::f32::consts::TAU * 997. * (frame * 960 + i) as f32 / 48_000.).sin() * 0.3
                });
                let packet = tx.encode(&samples).unwrap();
                assert!(packet.len() <= (target / 400) as usize);
                assert!(nuschl_core::bitrate::voice_frame_fits(target, packet.len()));
                let mut output = vec![0.; 5760];
                assert_eq!(decoder.decode(&packet, 5760, &mut output).unwrap(), 960);
            }
        }
    }

    use super::*;
    use crate::MAX_PACKET_SAMPLES;
    use opus_rs::OpusDecoder;

    #[test]
    fn processed_prefix_marker_fixture_uses_the_real_queue_and_encoder() {
        let mut queue = CaptureQueue::default();
        let now = Instant::now();
        let tone = |frame: usize, frequency: f32| {
            std::array::from_fn(|i| {
                (std::f32::consts::TAU * frequency * (frame * FRAME + i) as f32 / 48_000.).sin()
                    * 0.2
            })
        };
        for frame in 0..8 {
            queue.push(tone(frame, 997.), false, true);
            assert!(queue.due(now).is_none(), "a prefix cannot open the gate");
        }
        let mut tx = super::Transmit {
            encoder: OpusEncoder::new(48_000, 1, opus_rs::Application::Voip).unwrap(),
            queue: CaptureQueue::default(),
            packet: [0; MAX_PACKET_BYTES],
            sequence: 0,
            active: false,
        };
        tx.encoder.bitrate_bps = nuschl_core::audio::DEFAULT_VOICE_BITRATE as i32;
        let mut packets = Vec::new();
        for frame in 8..20 {
            queue.push(tone(frame, 3_000.), true, true);
            let at = now + Duration::from_millis((frame as u64 - 8) * 10);
            if let Some(packet) = queue.due(at) {
                packets.push(tx.encode(&packet.samples).unwrap());
            }
        }
        queue.push([0.; FRAME], false, true);
        for frame in 20..40 {
            if let Some(packet) = queue.due(now + Duration::from_millis((frame - 8) * 10)) {
                packets.push(tx.encode(&packet.samples).unwrap());
            }
        }
        assert!(packets.len() >= 10);
        // An independent libopus fixture consumes these public synthetic bytes.
        // No file is produced by ordinary unit tests.
        if let Some(path) = std::env::var_os("NUSCHL_PREFIX_PROOF_OUTPUT") {
            std::fs::write(path, serde_json::to_vec(&serde_json::json!({"packets": packets, "prefix_hz":997, "following_hz":3000, "prefix_samples":8 * FRAME, "sample_rate":48000})).unwrap()).unwrap();
        }
    }

    fn device(id: &str, name: &str) -> AudioDevice {
        AudioDevice {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn device_buffer_stays_inside_the_supported_range() {
        use cpal::{BufferSize, SupportedBufferSize};
        let range = |min, max| SupportedBufferSize::Range { min, max };
        assert_eq!(device_buffer(&range(64, 8192)), BufferSize::Fixed(480));
        assert_eq!(device_buffer(&range(1024, 2048)), BufferSize::Fixed(1024));
        assert_eq!(device_buffer(&range(16, 256)), BufferSize::Fixed(256));
        assert_eq!(
            device_buffer(&SupportedBufferSize::Unknown),
            BufferSize::Fixed(480)
        );
    }

    #[test]
    fn service_ticker_queues_at_most_one_wakeup_and_parks_while_idle() {
        let (tx, rx) = mpsc::sync_channel::<u8>(16);
        let mut ticker = Ticker::start(tx, || 1, Duration::from_millis(1)).expect("ticker thread");
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(rx.try_iter().count(), 0, "no wakeups before devices open");
        ticker.run_while(true);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            rx.try_iter().count(),
            1,
            "one wakeup however long the loop is busy"
        );
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            rx.try_iter().count(),
            0,
            "a wakeup not yet taken holds back the next"
        );
        ticker.received();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(1));
        ticker.received();
        ticker.run_while(false);
        // At most the wakeup of a sleep already under way, then nothing.
        std::thread::sleep(Duration::from_millis(30));
        let _ = rx.try_iter().count();
        ticker.received();
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(rx.try_iter().count(), 0, "a parked ticker sends nothing");
        let stopping = Instant::now();
        drop(ticker);
        assert!(
            stopping.elapsed() < Duration::from_secs(1),
            "dropping stops and joins it"
        );
    }

    #[test]
    fn service_ticker_never_blocks_on_a_full_queue_and_ends_with_the_loop() {
        let (tx, rx) = mpsc::sync_channel::<u8>(1);
        tx.send(0).unwrap();
        let mut ticker = Ticker::start(tx, || 1, Duration::from_millis(1)).expect("ticker thread");
        ticker.run_while(true);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(rx.recv(), Ok(0));
        // The wakeup a full queue refused did not stay pending.
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(1));
        drop(rx);
        std::thread::sleep(Duration::from_millis(20));
        let stopping = Instant::now();
        drop(ticker);
        assert!(stopping.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn service_ticker_wakes_at_its_period_while_running() {
        let (tx, rx) = mpsc::sync_channel::<u8>(16);
        let mut ticker = Ticker::start(tx, || 1, SERVICE_PERIOD).expect("ticker thread");
        ticker.run_while(true);
        let started = Instant::now();
        let mut wakeups = 0;
        while started.elapsed() < Duration::from_millis(400) {
            if rx.recv_timeout(Duration::from_secs(1)).is_ok() {
                ticker.received();
                wakeups += 1;
            }
        }
        // 100 at exactly 4 ms; generous for loaded test machines.
        assert!((20..=101).contains(&wakeups), "{wakeups} wakeups in 400 ms");
    }

    #[test]
    fn device_callbacks_never_allocate() {
        // Typical device shapes: 44.1 kHz stereo i16 capture, 48 kHz 6-channel
        // f32 playback, and 96 kHz u16 to exercise resampling both ways.
        let (mut capture_ring, mut capture_out) = RingBuffer::<f32>::new(RING);
        let (mut playback_in, mut playback_ring) = RingBuffer::<[f64; 2]>::new(RING);
        let mut capture = CaptureConverter::new(2, 44_100);
        let mut playback = PlaybackConverter::new(6, 48_000);
        let mut resampled = PlaybackConverter::new(1, 96_000);
        let input = [1234_i16; 2 * 441];
        let mut output = [0_f32; 6 * 480];
        let mut output_u16 = [0_u16; 960];
        for _ in 0..10 {
            let _ = playback_in.push_entire_slice(&[[0.25; 2]; FRAME]);
        }
        // The reference paths: what the output callback played, and a stereo
        // recording of the output device, both stamped on the shared clock.
        let (mut played, mut played_out) = RingBuffer::<f32>::new(2 * reference::RING_FRAMES);
        let (mut recorded, mut recorded_out) = RingBuffer::<f32>::new(2 * reference::RING_FRAMES);
        let mut recording = ReferenceConverter::new(2, 44_100);
        let (mut stamper, _timeline) = reference::stamped();
        let clock = Clock::new();
        let device_frames = [0.5_f32; 2 * 441];
        let mut dry = 0;
        let counted = allocation_counter::measure(|| {
            for _ in 0..50 {
                stamper.anchor(clock.now_shifted(Duration::from_millis(5), Duration::ZERO));
                stamper.advance(capture.push(&input, &mut capture_ring));
                while capture_out.pop().is_ok() {}
                let (empty, handed) = playback.pull(&mut output, &mut playback_ring, &mut played);
                dry += usize::from(empty);
                stamper.advance(handed);
                dry += usize::from(
                    resampled
                        .pull(&mut output_u16, &mut playback_ring, &mut played)
                        .0,
                );
                while played_out.pop().is_ok() {}
                recording.push(device_frames.iter().copied(), &mut recorded);
                while recorded_out.pop().is_ok() {}
            }
        });
        assert_eq!(counted.count_total, 0, "a device callback allocated");
        let control = allocation_counter::measure(|| {
            std::hint::black_box(vec![0_u8; 64]);
        });
        assert!(
            control.count_total > 0,
            "the counting allocator is not active"
        );
        assert!(dry > 0, "the test should also cover the underrun path");
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn playback_preserves_stereo_mono_mapping_and_the_actual_fallback_reference() {
        for channels in [1, 2, 6] {
            let (mut input, mut consumer) = RingBuffer::<[f64; 2]>::new(16);
            input.push_entire_slice(&[[0.5, -0.25]; 8]).unwrap();
            let (mut reference, mut samples) = RingBuffer::<f32>::new(32);
            let mut converter = PlaybackConverter::new(channels, 48_000);
            let mut output = vec![0_f32; channels * 6];
            let (dry, handed) = converter.pull(&mut output, &mut consumer, &mut reference);
            assert!(!dry);
            assert_eq!(handed, 6, "reference timestamps count frames, not scalars");
            let last = &output[output.len() - channels..];
            if channels == 1 {
                assert_eq!(last, [0.125]);
            } else {
                assert_eq!(&last[..2], [0.5, -0.25]);
                assert!(last[2..].iter().all(|value| *value == 0.));
            }
            let expected = if channels == 1 {
                [0.125; 2]
            } else {
                [0.5, -0.25]
            };
            for _ in 0..handed {
                assert_eq!([samples.pop().unwrap(), samples.pop().unwrap()], expected);
            }
            assert!(samples.pop().is_err());
        }
    }

    #[test]
    fn a_control_request_behind_a_voice_backlog_reaches_a_busy_loop() {
        // The real reader over a pipe holding a burst of voice and then a
        // push-to-talk release; the main loop is busy and takes an event only
        // now and then.
        let mut pipe = Vec::new();
        for sequence in 0..64 {
            let voice = LiveMediaRequest::Playback {
                stream: 1,
                sender: 1,
                sequence,
                opus: vec![0; 60],
                end: false,
                volume: 1.,
            };
            write_frame(&mut pipe, &Envelope::new(0, voice)).unwrap();
        }
        let release = LiveMediaRequest::PushToTalk { active: false };
        write_frame(&mut pipe, &Envelope::new(0, release)).unwrap();
        let (tx, rx) = mpsc::sync_channel(4);
        let reader = std::thread::spawn(move || forward_requests(io::Cursor::new(pipe), &tx));
        let mut taken = 0;
        let mut released = false;
        while taken < 8 && !released {
            std::thread::sleep(Duration::from_millis(20));
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(Event::Request(LiveMediaRequest::PushToTalk { active: false, .. })) => {
                    released = true;
                }
                Ok(Event::Request(_)) => taken += 1,
                _ => break,
            }
        }
        assert!(
            released,
            "the release still waits behind voice after {taken} events"
        );
        drop(rx);
        reader.join().unwrap();
    }

    #[test]
    fn the_terminator_sent_on_mute_is_silence() {
        let mut decoder = OpusDecoder::new(48_000, 1).unwrap();
        let mut pcm = [1_f32; MAX_PACKET_SAMPLES];
        let count = decoder
            .decode(
                &nuschl_core::audio::SILENT_OPUS,
                MAX_PACKET_SAMPLES,
                &mut pcm,
            )
            .unwrap();
        assert_eq!(count, 2 * FRAME, "20 ms");
        // Far below one 24-bit step; CELT's float decoder adds a tiny offset
        // against denormals instead of returning exact zeros.
        let peak = pcm[..count]
            .iter()
            .fold(0_f32, |peak, sample| peak.max(sample.abs()));
        assert!(peak < 1e-20, "peak {peak}");
    }

    #[test]
    fn capture_resamples_to_48_khz_mono() {
        let (mut ring, mut out) = RingBuffer::<f32>::new(RING);
        let mut capture = CaptureConverter::new(2, 44_100);
        capture.push(&[0.5_f32, -0.5].repeat(4410), &mut ring);
        assert_eq!(out.slots(), 4800, "100 ms in, 100 ms out");
        assert!(std::iter::from_fn(|| out.pop().ok()).all(|sample| sample.abs() < 1e-6));
        let mut loud = CaptureConverter::new(1, 48_000);
        loud.push(&[f32::NAN, 3.0, -3.0], &mut ring);
        let values: Vec<f32> = std::iter::from_fn(|| out.pop().ok()).collect();
        assert_eq!(
            values,
            [0., 1., -1.],
            "non-finite and out-of-range input is sanitised"
        );
    }

    #[test]
    fn recognises_pipewire_and_pulseaudio_echo_cancel_nodes() {
        assert!(is_echo_cancelled(
            "pulseaudio:echo-cancel-source",
            "Echo-Cancel Source"
        ));
        assert!(is_echo_cancelled(
            "pulseaudio:alsa_input.usb-mic.echo-cancel",
            "USB Mic (echo cancelled with Speakers)"
        ));
        assert!(!is_echo_cancelled(
            "pulseaudio:alsa_input.pci",
            "Built-in Audio Analog Stereo"
        ));
    }

    #[test]
    fn routes_playback_to_the_matching_canceller_reference() {
        let sinks = vec![
            device("pulseaudio:alsa_output.pci", "Speakers"),
            device("pulseaudio:echo-cancel-sink", "Echo-Cancel Sink"),
        ];
        assert_eq!(
            paired_reference("pulseaudio:echo-cancel-source", &sinks).map(|d| d.id.as_str()),
            Some("pulseaudio:echo-cancel-sink")
        );
        let pulse = vec![
            device("pulseaudio:alsa_output.pci", "Speakers"),
            device(
                "pulseaudio:alsa_output.pci.echo-cancel",
                "Speakers (echo cancelled with Mic)",
            ),
        ];
        assert_eq!(
            paired_reference("pulseaudio:alsa_input.usb.echo-cancel", &pulse)
                .map(|d| d.id.as_str()),
            Some("pulseaudio:alsa_output.pci.echo-cancel")
        );
        assert!(paired_reference("pulseaudio:alsa_input.usb", &sinks[..1]).is_none());
    }
}
