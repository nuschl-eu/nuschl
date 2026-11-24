// SPDX-License-Identifier: EUPL-1.2
//! Decode at most two selected remote feeds, independently of publication.

use super::{
    Display, Feed, Latest, Selection, Stream, TILES,
    gallery::{REMOTE_SOURCES, REMOTE_TILES},
    stats, text,
};
use nuschl_client::native::sfu::{Media, MediaEvent, VideoKind};
use nuschl_ipc::video::{CaptureLimits, DecodeError, Decoder};
use nuschl_protocol::sfu::VideoLimit;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

struct Encoded {
    stream: Stream,
    contiguous: bool,
    keyframe: bool,
    data: Vec<u8>,
}

pub(super) struct Decoded {
    stream: Stream,
    result: Result<Option<(u16, u16)>, DecodeError>,
    elapsed: Duration,
}

struct Lane {
    target: watch::Sender<Option<Stream>>,
    input: mpsc::Sender<Encoded>,
    task: JoinHandle<()>,
    lost: bool,
    failed: bool,
    last_keyframe: Option<Instant>,
    stats: stats::Stats,
}

impl Lane {
    fn new(
        executable: PathBuf,
        latest: Arc<Latest>,
        output: mpsc::Sender<Decoded>,
        relay_only: bool,
    ) -> Self {
        let (target, selection) = watch::channel(None);
        let (input, frames) = mpsc::channel(1);
        let task = tokio::spawn(decode(executable, selection, frames, latest, output));
        Self {
            target,
            input,
            task,
            lost: false,
            failed: false,
            last_keyframe: None,
            stats: stats::Stats::new(relay_only, Instant::now()),
        }
    }

    fn stream(&self) -> Option<Stream> {
        *self.target.borrow()
    }

    fn select(&mut self, stream: Option<Stream>, limit: Option<VideoLimit>) {
        self.target.send_replace(stream);
        self.lost = true;
        self.failed = false;
        self.last_keyframe = None;
        self.stats.clear(Instant::now());
        self.stats.limit(limit);
    }

    fn request_keyframe(&mut self, media: &Media, stream: Stream) -> Result<(), String> {
        let now = Instant::now();
        if let Some(publisher) = stream.feed.publisher
            && self
                .last_keyframe
                .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(500))
        {
            media
                .request_source_keyframe(publisher, stream.feed.purpose())
                .map_err(text)?;
            self.last_keyframe = Some(now);
        }
        Ok(())
    }

    fn receive(&mut self, media: &Media, mut encoded: Encoded) -> Result<(), String> {
        self.stats.encoded(encoded.data.len());
        if self.failed {
            return Ok(());
        }
        encoded.contiguous &= !self.lost;
        let stream = encoded.stream;
        self.lost =
            (!encoded.keyframe && !encoded.contiguous) || self.input.try_send(encoded).is_err();
        if self.lost {
            self.request_keyframe(media, stream)?;
        }
        Ok(())
    }
}

pub(super) struct Viewer {
    sources: [Option<Stream>; REMOTE_SOURCES],
    limits: [Option<VideoLimit>; REMOTE_SOURCES],
    serial: u64,
    lanes: [Lane; REMOTE_TILES],
    tiles: [Option<Stream>; TILES],
    selection: Selection,
    pub(super) decoded: mpsc::Receiver<Decoded>,
}

impl Viewer {
    pub(super) fn new(executable: PathBuf, latest: Arc<Latest>, relay_only: bool) -> Self {
        let (output, decoded) = mpsc::channel(REMOTE_TILES);
        let lanes = std::array::from_fn(|_| {
            Lane::new(
                executable.clone(),
                latest.clone(),
                output.clone(),
                relay_only,
            )
        });
        Self {
            sources: [None; REMOTE_SOURCES],
            limits: [None; REMOTE_SOURCES],
            serial: 0,
            lanes,
            tiles: [None; TILES],
            selection: Selection::default(),
            decoded,
        }
    }

    pub(super) fn sources(&self) -> [Option<Stream>; REMOTE_SOURCES] {
        self.sources
    }

    fn position(&self, feed: Feed) -> Option<usize> {
        self.sources
            .iter()
            .position(|slot| slot.is_some_and(|stream| stream.feed == feed))
    }

    pub(super) fn handle(&mut self, media: &Media, event: MediaEvent) -> Result<(), String> {
        match event {
            MediaEvent::SourceAvailable { publisher, purpose } => {
                if let Some(feed) = Feed::remote(publisher, purpose)
                    && self.position(feed).is_none()
                    && let Some(slot) = self.sources.iter_mut().find(|slot| slot.is_none())
                {
                    self.serial = self
                        .serial
                        .checked_add(1)
                        .ok_or("Video source limit reached")?;
                    *slot = Some(Stream {
                        feed,
                        serial: self.serial,
                        preview: true,
                    });
                }
            }
            MediaEvent::SourceEnded { publisher, purpose } => {
                if let Some(feed) = Feed::remote(publisher, purpose)
                    && let Some(index) = self.position(feed)
                {
                    self.sources[index] = None;
                    self.limits[index] = None;
                }
            }
            MediaEvent::VideoLimit {
                publisher,
                purpose,
                limit,
            } => {
                if let Some(feed) = Feed::remote(publisher, purpose)
                    && let Some(index) = self.position(feed)
                {
                    self.limits[index] = Some(limit);
                    if let Some(lane) = self
                        .lanes
                        .iter_mut()
                        .find(|lane| lane.stream() == self.sources[index])
                    {
                        lane.stats.limit(Some(limit));
                    }
                }
            }
            MediaEvent::Route(route) => {
                for lane in &mut self.lanes {
                    lane.stats.route(route);
                }
            }
            MediaEvent::Frame {
                publisher,
                purpose,
                contiguous,
                key_frame,
                data,
                ..
            } => {
                if let Some(feed) = Feed::remote(publisher, purpose)
                    && let Some((lane, stream)) = self.lanes.iter_mut().find_map(|lane| {
                        let stream = lane.stream().filter(|stream| stream.feed == feed)?;
                        Some((lane, stream))
                    })
                {
                    lane.receive(
                        media,
                        Encoded {
                            stream,
                            contiguous,
                            keyframe: key_frame,
                            data,
                        },
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn layout(
        &mut self,
        media: &Media,
        tiles: [Option<Stream>; TILES],
        selection: Selection,
    ) -> Result<(), String> {
        if self.tiles == tiles && self.selection == selection {
            return Ok(());
        }
        self.tiles = tiles;
        self.selection = selection;
        for lane in &mut self.lanes {
            if lane
                .stream()
                .is_some_and(|current| !tiles.contains(&Some(current)))
            {
                lane.select(None, None);
            }
        }
        for stream in tiles.into_iter().flatten() {
            let feed = stream.feed;
            let Some(publisher) = feed.publisher else {
                continue;
            };
            let limit = self.position(feed).and_then(|slot| self.limits[slot]);
            let lane = self
                .lanes
                .iter()
                .position(|lane| lane.stream() == Some(stream))
                .or_else(|| self.lanes.iter().position(|lane| lane.stream().is_none()))
                .and_then(|slot| self.lanes.get_mut(slot));
            let Some(lane) = lane else { continue };
            if lane.stream() != Some(stream) {
                lane.select(Some(stream), limit);
                lane.request_keyframe(media, stream)?;
            }
            let preference = selection.preference(feed);
            let pixels = if feed.kind == VideoKind::Screen {
                preference.pixels.max(1080)
            } else {
                preference.pixels
            };
            let _ = media.set_quality(publisher, feed.purpose(), preference.quality, pixels);
            let requested = feed.kind.ladder()[feed.kind.request(preference.quality, pixels)];
            lane.stats.requested(CaptureLimits {
                height: requested.height,
                fps: requested.fps.into(),
                bitrate: requested.ceiling as u32,
            });
        }
        Ok(())
    }

    pub(super) fn complete(
        &mut self,
        media: &Media,
        display: &Display<'_>,
        decoded: Decoded,
    ) -> Result<(), String> {
        let stream = decoded.stream;
        let Some(lane) = self
            .lanes
            .iter_mut()
            .find(|lane| lane.stream() == Some(stream))
        else {
            return Ok(());
        };
        match decoded.result {
            Ok(Some((width, height))) => lane.stats.frame(width, height, Some(decoded.elapsed)),
            Ok(None) => {}
            Err(DecodeError::Io(_) | DecodeError::Timeout | DecodeError::Busy) => {
                lane.lost = true;
                lane.request_keyframe(media, stream)?;
            }
            Err(error) => {
                tracing::warn!(%error, "remote video decoder stopped");
                lane.failed = true;
                display.updates.video_unavailable(stream);
            }
        }
        Ok(())
    }

    pub(super) fn report(&mut self, display: &Display<'_>, selected: Feed) {
        if let Some((lane, stream)) = self.lanes.iter_mut().find_map(|lane| {
            let stream = lane.stream().filter(|stream| stream.feed == selected)?;
            Some((lane, stream))
        }) {
            display.report(stream, lane.stats.sample(Instant::now()), None);
        }
    }

    pub(super) async fn stop(self) -> Result<(), String> {
        drop(self.decoded);
        let tasks = self.lanes.map(
            |Lane {
                 target,
                 input,
                 task,
                 ..
             }| {
                drop((target, input));
                task
            },
        );
        let [camera, screen] = tasks;
        let (camera, screen) = tokio::join!(camera, screen);
        camera.and(screen).map_err(text)
    }
}

async fn decode(
    executable: PathBuf,
    mut selected: watch::Receiver<Option<Stream>>,
    mut frames: mpsc::Receiver<Encoded>,
    latest: Arc<Latest>,
    outcome: mpsc::Sender<Decoded>,
) {
    let mut active = None;
    let mut decoder: Option<Decoder> = None;
    loop {
        let packet = tokio::select! {
            changed = selected.changed() => {
                if changed.is_err() { break; }
                None
            },
            packet = frames.recv() => {
                let Some(packet) = packet else { break };
                Some(packet)
            },
        };
        let stream = *selected.borrow_and_update();
        if active != stream {
            active = stream;
            if let Some(previous) = decoder.take() {
                let _ = previous.stop().await;
            }
        }
        let Some(packet) = packet.filter(|packet| Some(packet.stream) == active) else {
            continue;
        };
        if *selected.borrow() != active {
            continue;
        }
        let started = Instant::now();
        let result = async {
            let decoder = match &mut decoder {
                Some(decoder) => decoder,
                None => decoder.insert(Decoder::start(
                    executable.clone(),
                    packet.stream.feed.source(),
                )?),
            };
            if !packet.contiguous {
                decoder.reset();
            }
            decoder
                .decode(packet.keyframe, packet.data)
                .await
                .map(|frame| {
                    frame.map(|frame| {
                        let size = (frame.width, frame.height);
                        latest.frame(packet.stream, frame);
                        size
                    })
                })
        }
        .await;
        let event = Decoded {
            stream: packet.stream,
            result,
            elapsed: started.elapsed(),
        };
        if outcome.send(event).await.is_err() {
            break;
        }
    }
    if let Some(previous) = decoder {
        let _ = previous.stop().await;
    }
}
