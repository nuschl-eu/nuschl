// SPDX-License-Identifier: EUPL-1.2
//! A call's shared audio worker (`nuschl-media-worker --call-audio`, with the
//! media role's grants). It shares the desktop's audio, which is every
//! application's playback except Nuschl's own, as stereo Opus, and plays the
//! shared audio the call receives. Requests and events are bounded frames.

use crate::{Envelope, read_frame_async, write_frame_within};
use serde::{Deserialize, Serialize};
use std::{io, path::Path, time::Duration};
use tokio::{
    io::AsyncWriteExt as _,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

/// The largest Opus packet on a call's audio track.
pub const MAX_OPUS: usize = 1275;
/// One request or event with its JSON framing.
pub const MAX_FRAME: usize = 8 * 1024;
/// Requests queued for the worker; received packets leave room for control.
const REQUESTS: usize = 64;
const CONTROL_RESERVE: usize = 8;
const EVENTS: usize = 16;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Share the desktop's audio, or stop sharing it.
    Share { active: bool },
    /// Shared audio received from `publisher`, on its 48 kHz clock.
    Play {
        publisher: u64,
        timestamp: u64,
        opus: Vec<u8>,
    },
    /// `publisher` stopped sharing audio.
    End { publisher: u64 },
    /// `publisher`'s shared audio plays at `volume`, from 0 (muted) to 1.
    Volume { publisher: u64, volume: f32 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    /// 20 ms of the shared desktop audio as stereo Opus. The timestamp counts
    /// 48 kHz samples since the share started.
    Packet { timestamp: u64, opus: Vec<u8> },
    /// Sharing stopped by itself or could not start. Nothing more is sent.
    Stopped { reason: String },
}

impl Request {
    pub fn valid(&self) -> bool {
        match self {
            Self::Play {
                publisher, opus, ..
            } => *publisher != 0 && (1..=MAX_OPUS).contains(&opus.len()),
            Self::Volume { publisher, volume } => *publisher != 0 && (0.0..=1.0).contains(volume),
            Self::Share { .. } | Self::End { .. } => true,
        }
    }
}

impl Event {
    pub fn valid(&self) -> bool {
        match self {
            Self::Packet { opus, .. } => (1..=MAX_OPUS).contains(&opus.len()),
            Self::Stopped { reason } => reason.len() <= 256,
        }
    }
}

/// A running call audio worker. Dropping it kills the worker.
pub struct Worker {
    requests: mpsc::Sender<Request>,
    events: mpsc::Receiver<Event>,
    task: JoinHandle<()>,
    _stop: oneshot::Sender<()>,
}

impl Worker {
    /// Start the confined worker from the trusted installed `executable`.
    pub async fn start(executable: &Path) -> io::Result<Self> {
        let nuschl_sandbox::Worker {
            mut child,
            mut input,
            mut output,
            containment,
        } = nuschl_sandbox::spawn_receipted(executable, nuschl_sandbox::Role::CallAudio)
            .await
            .map_err(io::Error::other)?;
        // The receipt names the sandbox's original init before any input.
        let mut retirement = crate::host::preview_lifecycle::Retirement::new(&mut child);
        if let Err(error) = retirement.ready().await {
            let _ = child.start_kill();
            retirement.retire(&mut child, &containment).await;
            return Err(io::Error::other(error.to_string()));
        }
        let (requests, mut queue) = mpsc::channel(REQUESTS);
        let (replies, events) = mpsc::channel(EVENTS);
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut reader = tokio::spawn(async move {
                while let Ok(frame) = read_frame_async::<Event>(&mut output, MAX_FRAME).await {
                    if !frame.payload.valid() || replies.send(frame.payload).await.is_err() {
                        break;
                    }
                }
            });
            let exchange = async {
                loop {
                    tokio::select! {
                        _ = &mut reader => break,
                        request = queue.recv() => {
                            let Some(request) = request else { break };
                            let mut bytes = Vec::new();
                            if write_frame_within(&mut bytes, &Envelope::new(0, request), MAX_FRAME).is_err()
                                || !matches!(
                                    tokio::time::timeout(Duration::from_secs(3), input.write_all(&bytes)).await,
                                    Ok(Ok(()))
                                )
                            {
                                break;
                            }
                        }
                    }
                }
            };
            tokio::select! {
                () = exchange => {},
                _ = stopped => {},
            }
            reader.abort();
            let _ = reader.await;
            let _ = child.start_kill();
            retirement.retire(&mut child, &containment).await;
        });
        Ok(Self {
            requests,
            events,
            task,
            _stop: stop,
        })
    }

    /// Queue a control request. False when the worker stopped reading or ended.
    pub fn send(&self, request: Request) -> bool {
        request.valid() && self.requests.try_send(request).is_ok()
    }

    /// Queue received audio, dropped while the worker falls behind.
    pub fn play(&self, publisher: u64, timestamp: u64, opus: Vec<u8>) {
        if self.requests.capacity() > CONTROL_RESERVE {
            self.send(Request::Play {
                publisher,
                timestamp,
                opus,
            });
        }
    }

    /// The next event, or `None` once the worker has ended.
    pub async fn next(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    /// Kill the worker. Completes once it has exited and been reaped.
    pub async fn stop(self) {
        let Self { task, _stop, .. } = self;
        drop(_stop);
        let _ = task.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_events_are_bounded() {
        let packet = |length| Request::Play {
            publisher: 1,
            timestamp: 0,
            opus: vec![0; length],
        };
        assert!(packet(1).valid() && packet(MAX_OPUS).valid());
        assert!(!packet(0).valid() && !packet(MAX_OPUS + 1).valid());
        assert!(
            !Request::Play {
                publisher: 0,
                timestamp: 0,
                opus: vec![1]
            }
            .valid()
        );
        for volume in [-0.1, 1.1, f32::NAN] {
            assert!(
                !Request::Volume {
                    publisher: 1,
                    volume
                }
                .valid()
            );
        }
        assert!(
            !Request::Volume {
                publisher: 0,
                volume: 1.
            }
            .valid()
        );
        let mut bytes = Vec::new();
        write_frame_within(&mut bytes, &Envelope::new(0, packet(MAX_OPUS)), MAX_FRAME).unwrap();
        let event = Event::Packet {
            timestamp: u64::MAX,
            opus: vec![255; MAX_OPUS],
        };
        assert!(event.valid());
        write_frame_within(&mut Vec::new(), &Envelope::new(0, event), MAX_FRAME).unwrap();
        assert!(
            !Event::Stopped {
                reason: "x".repeat(257)
            }
            .valid()
        );
    }
}
