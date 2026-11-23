// SPDX-License-Identifier: EUPL-1.2
//! A call's shared audio (`--call-audio`, docs/screen-share-audio.md).
//!
//! Sharing records every application's playback except Nuschl's own into a
//! PipeWire node of this worker's (the [`policy`] picks the streams) and
//! encodes it as 20 ms stereo Opus on its own 48 kHz clock. Shared audio
//! received from others is mixed per publisher and plays as a stream of its
//! own, "Shared audio", each publisher's at the volume this viewer chose.

#[cfg(target_os = "linux")]
mod pipewire;
mod policy;

use crate::{
    dsp::FRAME,
    mixer::{Mixer, Voice},
    playback::PlaybackConverter,
};
use cpal::{
    FromSample, SampleFormat, SizedSample,
    traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _},
};
use nuschl_ipc::{
    Envelope,
    call_audio::{Event, MAX_FRAME, Request},
    read_frame_within, write_frame_within,
};
use opus_rs::{Application, OpusEncoder};
use rtrb::{Consumer, Producer, RingBuffer};
use std::{
    collections::BTreeMap,
    io::{self, Write as _},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

/// Shared audio is encoded in 20 ms stereo packets, at a bitrate for music.
const PACKET: usize = 2 * FRAME;
const BITRATE: i32 = 128_000;
/// Below the mixer's packet limit, and with room for SFrame's overhead.
const MAX_PACKET: usize = 1000;
/// After a pause this long in the graph, the share's clock catches up.
const PAUSE: Duration = Duration::from_millis(100);
/// Received audio queued ahead of the device, and the queue's size.
const AHEAD: usize = 4 * FRAME;
const RING: usize = 20 * FRAME;
/// Publishers whose shared audio plays at once, and whose volumes are kept.
const MAX_PUBLISHERS: usize = 16;
const MAX_VOLUMES: usize = 256;
/// Opus timestamps tick at 48 kHz; the jitter buffer counts 10 ms frames.
const TICKS_PER_FRAME: u64 = FRAME as u64;

enum Input {
    Request(Request),
    /// The share of this number stopped by itself.
    Stopped(u64, String),
    End,
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let (inputs, requests) = mpsc::sync_channel(64);
    let reader = inputs.clone();
    thread::Builder::new()
        .name("requests".into())
        .spawn(move || {
            let mut input = io::stdin().lock();
            while let Ok(Some(frame)) = read_frame_within::<Request>(&mut input, MAX_FRAME) {
                if !frame.payload.valid() || reader.send(Input::Request(frame.payload)).is_err() {
                    break;
                }
            }
            let _ = reader.send(Input::End);
        })?;
    let (events, outgoing) = mpsc::sync_channel::<Event>(32);
    let writer = thread::Builder::new()
        .name("events".into())
        .spawn(move || {
            let mut output = io::stdout().lock();
            for event in outgoing {
                if write_frame_within(&mut output, &Envelope::new(0, event), MAX_FRAME).is_err()
                    || output.flush().is_err()
                {
                    break;
                }
            }
        })?;
    let mut share: Option<Share> = None;
    let mut shares = 0_u64;
    let mut playback = Playback::default();
    loop {
        // The device's queue is topped up every few milliseconds while it plays.
        let input = if playback.output.is_some() {
            requests.recv_timeout(Duration::from_millis(5))
        } else {
            requests.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        match input {
            Ok(Input::Request(Request::Share { active })) => {
                share = None;
                if active {
                    shares += 1;
                    match Share::start(shares, &inputs, &events) {
                        Ok(started) => share = Some(started),
                        Err(reason) => {
                            let _ = events.send(Event::Stopped { reason });
                        }
                    }
                }
            }
            Ok(Input::Request(Request::Play {
                publisher,
                timestamp,
                opus,
            })) => playback.push(publisher, timestamp, opus),
            Ok(Input::Request(Request::End { publisher })) => playback.end(publisher),
            Ok(Input::Request(Request::Volume { publisher, volume })) => {
                playback.volume(publisher, volume);
            }
            Ok(Input::Stopped(number, reason)) => {
                if share.as_ref().is_some_and(|share| share.number == number) {
                    share = None;
                    let _ = events.send(Event::Stopped { reason });
                }
            }
            Ok(Input::End) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        playback.fill();
    }
    drop(share);
    drop(events);
    let _ = writer.join();
    Ok(())
}

/// A running desktop audio share. Dropping it stops the capture and encoder.
struct Share {
    number: u64,
    running: Arc<AtomicBool>,
    #[cfg(target_os = "linux")]
    _capture: pipewire::Capture,
    encoder: Option<thread::JoinHandle<()>>,
}

impl Share {
    #[cfg(not(target_os = "linux"))]
    fn start(_: u64, _: &SyncSender<Input>, _: &SyncSender<Event>) -> Result<Self, String> {
        Err("Audio sharing unavailable".into())
    }

    #[cfg(target_os = "linux")]
    fn start(
        number: u64,
        inputs: &SyncSender<Input>,
        events: &SyncSender<Event>,
    ) -> Result<Self, String> {
        // A second of stereo audio between the graph and the encoder.
        let (samples, recorded) = RingBuffer::<f32>::new(2 * 48_000);
        let failed = inputs.clone();
        let capture = pipewire::Capture::start(samples, move |reason| {
            let _ = failed.send(Input::Stopped(number, reason));
        })?;
        let running = Arc::new(AtomicBool::new(true));
        let encoder = thread::Builder::new()
            .name("share-encoder".into())
            .spawn({
                let running = running.clone();
                let events = events.clone();
                let failed = inputs.clone();
                move || {
                    if let Err(reason) = encode(recorded, &running, &events) {
                        let _ = failed.send(Input::Stopped(number, reason.into()));
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            number,
            running,
            _capture: capture,
            encoder: Some(encoder),
        })
    }
}

impl Drop for Share {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(encoder) = self.encoder.take() {
            let _ = encoder.join();
        }
    }
}

/// Encode the recorded audio until the share stops. Packets the event writer
/// has no room for are dropped.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn encode(
    mut recorded: Consumer<f32>,
    running: &AtomicBool,
    events: &SyncSender<Event>,
) -> Result<(), &'static str> {
    let mut encoder = OpusEncoder::new(48_000, 2, Application::Audio)?;
    encoder.bitrate_bps = BITRATE;
    encoder.set_vbr_constraint(true);
    let started = Instant::now();
    let mut last = started;
    let mut timestamp = 0_u64;
    let mut pcm = [0_f32; 2 * PACKET];
    let mut packet = [0_u8; MAX_PACKET];
    while running.load(Ordering::Relaxed) {
        let Ok(chunk) = recorded.read_chunk(pcm.len()) else {
            thread::sleep(Duration::from_millis(5));
            continue;
        };
        let (first, second) = chunk.as_slices();
        pcm[..first.len()].copy_from_slice(first);
        pcm[first.len()..].copy_from_slice(second);
        chunk.commit_all();
        for sample in &mut pcm {
            *sample = if sample.is_finite() {
                sample.clamp(-1., 1.)
            } else {
                0.
            };
        }
        let now = Instant::now();
        if now - last > PAUSE {
            let elapsed = now.duration_since(started).as_micros() as u64 * 48 / 1000;
            timestamp = timestamp.max(elapsed / PACKET as u64 * PACKET as u64);
        }
        last = now;
        let size = encoder.encode(&pcm, PACKET, &mut packet)?;
        if size > 0 {
            let _ = events.try_send(Event::Packet {
                timestamp,
                opus: packet[..size].to_vec(),
            });
        }
        timestamp += PACKET as u64;
    }
    Ok(())
}

struct Output {
    ring: Producer<[f64; 2]>,
    _stream: cpal::Stream,
}

/// One publisher's received shared audio and its latest 10 ms sequence number.
struct Publisher {
    mixer: Mixer,
    latest: u64,
}

/// Received shared audio, each publisher's at the volume this viewer chose.
struct Playback {
    publishers: BTreeMap<u64, Publisher>,
    /// Kept for the rest of the call, also while a publisher's audio is off.
    volumes: BTreeMap<u64, f32>,
    output: Option<Output>,
    /// An output that failed to open is tried again after a pause.
    retry: Instant,
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            publishers: BTreeMap::new(),
            volumes: BTreeMap::new(),
            output: None,
            retry: Instant::now(),
        }
    }
}

impl Playback {
    fn push(&mut self, publisher: u64, timestamp: u64, opus: Vec<u8>) {
        let sequence = timestamp / TICKS_PER_FRAME;
        if !self.publishers.contains_key(&publisher) && self.publishers.len() == MAX_PUBLISHERS {
            return;
        }
        let received = self
            .publishers
            .entry(publisher)
            .or_insert_with(|| Publisher {
                mixer: Mixer::default(),
                latest: sequence,
            });
        // A jump of more than a second (a new path or clock) starts afresh.
        if received.latest.abs_diff(sequence) > 100 {
            received.mixer.clear();
        }
        received.latest = sequence;
        let voice = Voice {
            stream: 0,
            sender: 0,
            sequence,
            opus,
            end: false,
            volume: 1.,
        };
        let _ = received.mixer.push(voice, Instant::now());
        if self.output.is_none() && Instant::now() >= self.retry {
            match open() {
                Ok(output) => self.output = Some(output),
                Err(error) => {
                    eprintln!("shared audio output unavailable: {error}");
                    self.retry = Instant::now() + Duration::from_secs(3);
                }
            }
        }
    }

    fn end(&mut self, publisher: u64) {
        self.publishers.remove(&publisher);
        if self.publishers.is_empty() {
            self.output = None;
        }
    }

    /// The volume takes effect with the next mixed frame.
    fn volume(&mut self, publisher: u64, volume: f32) {
        if self.volumes.len() < MAX_VOLUMES || self.volumes.contains_key(&publisher) {
            self.volumes.insert(publisher, volume);
        }
    }

    /// Mix 10 ms frames until the device has enough queued.
    fn fill(&mut self) {
        let Some(output) = &mut self.output else {
            return;
        };
        let now = Instant::now();
        while RING - output.ring.slots() < AHEAD {
            let mut mix = [[0.; 2]; FRAME];
            for (publisher, received) in &mut self.publishers {
                received.mixer.cleanup(now);
                let mut own = [[0.; 2]; FRAME];
                received.mixer.mix(&mut own);
                let gain = f64::from(self.volumes.get(publisher).copied().unwrap_or(1.));
                for (total, frame) in mix.iter_mut().zip(own) {
                    total[0] += frame[0] * gain;
                    total[1] += frame[1] * gain;
                }
            }
            for frame in mix {
                let _ = output.ring.push(frame);
            }
        }
    }
}

/// Open the default output as Nuschl's "Shared audio" stream.
fn open() -> Result<Output, String> {
    #[cfg(target_os = "linux")]
    let hosts = [
        cpal::HostId::PipeWire,
        cpal::HostId::PulseAudio,
        cpal::HostId::Alsa,
    ];
    #[cfg(not(target_os = "linux"))]
    let hosts = [cpal::default_host().id()];
    let mut errors = Vec::new();
    for id in hosts {
        match crate::live::connect(id).and_then(|mut host| {
            name(&mut host);
            let device = host
                .default_output_device()
                .ok_or("no default output device")?;
            let supported = device
                .default_output_config()
                .map_err(|error| error.to_string())?;
            let format = supported.sample_format();
            let config = crate::live::stream_config(&host, supported);
            let (ring, queued) = RingBuffer::<[f64; 2]>::new(RING);
            let stream = match format {
                SampleFormat::F32 => output::<f32>(&device, config, queued),
                SampleFormat::I16 => output::<i16>(&device, config, queued),
                SampleFormat::U16 => output::<u16>(&device, config, queued),
                SampleFormat::I32 => output::<i32>(&device, config, queued),
                format => Err(format!("unsupported sample format {format}")),
            }?;
            stream.play().map_err(|error| error.to_string())?;
            Ok(Output {
                ring,
                _stream: stream,
            })
        }) {
            Ok(output) => return Ok(output),
            Err(error) => errors.push(format!("{id:?}: {error}")),
        }
    }
    Err(errors.join("; "))
}

/// Name the stream, which volume mixers show with its own volume.
fn name(host: &mut cpal::Host) {
    #[cfg(target_os = "linux")]
    {
        let metadata = cpal::StreamMetadata {
            name: Some("Shared audio".into()),
            role: None,
        };
        let _ = match host.as_inner_mut() {
            cpal::platform::HostInner::PipeWire(host) => host.set_stream_metadata(metadata),
            cpal::platform::HostInner::PulseAudio(host) => host.set_stream_metadata(metadata),
            _ => Ok(()),
        };
    }
    #[cfg(not(target_os = "linux"))]
    let _ = host;
}

fn output<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut queued: Consumer<[f64; 2]>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
    f32: FromSample<T>,
{
    let (channels, rate) = crate::live::convertible(&config)?;
    let mut converter = PlaybackConverter::new(channels, rate);
    // No echo reference: the media worker records what the device plays.
    let (mut reference, _) = RingBuffer::<f32>::new(1);
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                converter.pull(data, &mut queued, &mut reference);
            },
            |error| eprintln!("shared audio output: {error}"),
            None,
        )
        .map_err(|error| error.to_string())
}
