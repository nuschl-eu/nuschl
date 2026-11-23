// SPDX-License-Identifier: EUPL-1.2
//! Shared audio in a call: the desktop's audio while the screen is shared,
//! and the shared audio others send. One confined call audio worker
//! (`nuschl_ipc::call_audio`) records, encodes and plays it. Sharing audio
//! is its own choice and never opens the microphone.

use nuschl_client::native::sfu::{Media, MediaEvent, Purpose};
use nuschl_ipc::call_audio::{Event, Request as WorkerRequest, Worker};
use std::{
    collections::BTreeSet,
    future::pending,
    path::PathBuf,
    sync::LazyLock,
    time::{Duration, Instant},
};

/// Worker failures in one call before received audio stops playing.
const MAX_FAILURES: u8 = 3;

/// The worker's confinement is designed and tested on Linux only.
static EXECUTABLE: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
    nuschl_sandbox::sibling("nuschl-media-worker")
        .ok()
        .filter(|path| cfg!(target_os = "linux") && path.is_file())
});

/// Whether this platform can share the desktop's audio (Linux, with PipeWire
/// checked when the share starts).
pub(super) fn can_share() -> bool {
    EXECUTABLE.is_some()
}

#[derive(Clone, Copy)]
pub enum Request {
    Share(bool),
    Volume(f32),
}

/// What changed for the call's controls.
pub enum Change {
    Sharing(bool, Option<String>),
    Receiving(bool),
}

pub(super) struct Audio {
    worker: Option<Worker>,
    sharing: bool,
    /// Publishers whose shared audio this call receives.
    playing: BTreeSet<u64>,
    volume: f32,
    failures: u8,
    retry: Instant,
}

impl Audio {
    pub(super) fn new() -> Self {
        Self {
            worker: None,
            sharing: false,
            playing: BTreeSet::new(),
            volume: 1.,
            failures: 0,
            retry: Instant::now(),
        }
    }

    pub(super) fn sharing(&self) -> bool {
        self.sharing
    }

    /// A running worker, started on demand. A worker that keeps failing is
    /// started again after a growing pause, and not at all after a few.
    async fn worker(&mut self) -> Option<&mut Worker> {
        if self.worker.is_none() && self.failures < MAX_FAILURES && Instant::now() >= self.retry {
            match Worker::start(EXECUTABLE.as_ref()?).await {
                Ok(worker) => {
                    worker.send(WorkerRequest::Volume {
                        volume: self.volume,
                    });
                    self.worker = Some(worker);
                }
                Err(error) => {
                    tracing::warn!(%error, "call audio worker unavailable");
                    self.failed();
                }
            }
        }
        self.worker.as_mut()
    }

    fn failed(&mut self) {
        self.failures += 1;
        self.retry = Instant::now() + Duration::from_secs(1 << self.failures);
    }

    /// Take the shared audio events and pass the others on.
    pub(super) async fn event(&mut self, event: MediaEvent) -> Result<MediaEvent, Option<Change>> {
        match event {
            MediaEvent::SourceAvailable {
                publisher,
                purpose: Purpose::SystemAudio,
            } => {
                let first = self.playing.is_empty();
                self.playing.insert(publisher);
                Err(first.then_some(Change::Receiving(true)))
            }
            MediaEvent::SourceEnded {
                publisher,
                purpose: Purpose::SystemAudio,
            } => {
                if self.playing.remove(&publisher)
                    && let Some(worker) = &self.worker
                {
                    worker.send(WorkerRequest::End { publisher });
                }
                self.retire().await;
                Err(self.playing.is_empty().then_some(Change::Receiving(false)))
            }
            MediaEvent::Frame {
                publisher,
                purpose: Purpose::SystemAudio,
                timestamp,
                data,
                ..
            } => {
                if self.playing.contains(&publisher)
                    && let Some(worker) = self.worker().await
                {
                    worker.play(publisher, timestamp, data);
                }
                Err(None)
            }
            event => Ok(event),
        }
    }

    pub(super) async fn request(
        &mut self,
        media: &mut Media,
        request: Request,
    ) -> Result<Option<Change>, String> {
        match request {
            Request::Share(active) => self.share(media, active).await,
            Request::Volume(volume) => {
                self.volume = volume.clamp(0., 1.);
                if let Some(worker) = &self.worker {
                    worker.send(WorkerRequest::Volume {
                        volume: self.volume,
                    });
                }
                Ok(None)
            }
        }
    }

    /// Start or stop sharing the desktop's audio.
    pub(super) async fn share(
        &mut self,
        media: &mut Media,
        active: bool,
    ) -> Result<Option<Change>, String> {
        if active == self.sharing {
            return Ok(Some(Change::Sharing(active, None)));
        }
        if !active {
            return self.stop_sharing(media, None).await;
        }
        if !can_share() {
            return Ok(Some(Change::Sharing(false, Some(unavailable()))));
        }
        let started = self
            .worker()
            .await
            .is_some_and(|worker| worker.send(WorkerRequest::Share { active: true }));
        if !started {
            return Ok(Some(Change::Sharing(false, Some(unavailable()))));
        }
        media
            .set_source(Purpose::SystemAudio, true)
            .map_err(|error| error.to_string())?;
        self.sharing = true;
        Ok(Some(Change::Sharing(true, None)))
    }

    async fn stop_sharing(
        &mut self,
        media: &mut Media,
        notice: Option<String>,
    ) -> Result<Option<Change>, String> {
        let sharing = std::mem::replace(&mut self.sharing, false);
        if sharing {
            media
                .set_source(Purpose::SystemAudio, false)
                .map_err(|error| error.to_string())?;
            if let Some(worker) = &self.worker {
                worker.send(WorkerRequest::Share { active: false });
            }
        }
        self.retire().await;
        Ok(sharing.then_some(Change::Sharing(false, notice)))
    }

    /// Stop a worker with nothing to share or play.
    async fn retire(&mut self) {
        if !self.sharing
            && self.playing.is_empty()
            && let Some(worker) = self.worker.take()
        {
            worker.stop().await;
        }
    }

    /// The worker's next event, applied to the call.
    pub(super) async fn next(&mut self) -> Option<Event> {
        match &mut self.worker {
            Some(worker) => worker.next().await.or(Some(Event::Stopped {
                reason: String::new(),
            })),
            None => pending().await,
        }
    }

    /// Apply a worker event; `Event::Stopped` with no reason is a worker exit.
    pub(super) async fn handle(
        &mut self,
        media: &mut Media,
        event: Event,
    ) -> Result<Option<Change>, String> {
        match event {
            Event::Packet { timestamp, opus } => {
                if self.sharing {
                    // A full queue drops the packet; the share goes on.
                    let _ = media.send_source_frame(Purpose::SystemAudio, timestamp, false, opus);
                }
                Ok(None)
            }
            Event::Stopped { reason } if reason.is_empty() => {
                if let Some(worker) = self.worker.take() {
                    worker.stop().await;
                }
                self.failed();
                let change = self
                    .stop_sharing(media, Some("Audio sharing stopped".into()))
                    .await?;
                // After repeated failures received audio stops for this call.
                let given_up = self.failures == MAX_FAILURES && !self.playing.is_empty();
                Ok(change.or_else(|| {
                    given_up.then(|| Change::Sharing(false, Some("Shared audio stopped".into())))
                }))
            }
            Event::Stopped { reason } => self.stop_sharing(media, Some(reason)).await,
        }
    }

    pub(super) async fn stop(self) {
        if let Some(worker) = self.worker {
            worker.stop().await;
        }
    }
}

fn unavailable() -> String {
    "Audio sharing unavailable".into()
}
