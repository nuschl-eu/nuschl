// SPDX-License-Identifier: EUPL-1.2

use super::{
    Header, Kind, Source, invalid,
    supervisor::{DEADLINE, Session},
};
use nuschl_sandbox::Role;
use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};

const MAX_FAILURES: u32 = 3;
static SOURCES: Semaphore = Semaphore::const_new(16);

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Sandbox(#[from] nuschl_sandbox::Error),
    #[error("all video decoder slots are occupied")]
    Busy,
    #[error("video decoder could not start in confinement")]
    Confinement,
    #[error("video decoder exceeded its deadline")]
    Timeout,
    #[error("video decoder has stopped")]
    Stopped,
    #[error("a Tokio runtime is required to start a video decoder")]
    Runtime,
}

/// Tightly packed BGRA pixels from the confined decoder, local to the viewer.
pub struct DecodedFrame {
    pub width: u16,
    pub height: u16,
    pub bgra: Vec<u8>,
}

pub fn decode_packet_limit(source: Source, keyframe: bool) -> usize {
    match source {
        Source::Camera if keyframe => 256 * 1024,
        Source::Camera => 128 * 1024,
        Source::Screen => super::SCREEN_FRAME_BYTES,
    }
}

pub fn decode_frame_bytes(source: Source, width: u16, height: u16) -> io::Result<usize> {
    let (long, short) = source.max_dimensions();
    if width.min(height) == 0 || width.min(height) > short || width.max(height) > long {
        return Err(invalid());
    }
    Ok(usize::from(width) * usize::from(height) * 4)
}

pub fn source_to_byte(source: Source) -> u8 {
    match source {
        Source::Camera => 0,
        Source::Screen => 1,
    }
}

pub fn source_from_byte(value: u8) -> io::Result<Source> {
    match value {
        0 => Ok(Source::Camera),
        1 => Ok(Source::Screen),
        _ => Err(invalid()),
    }
}

struct Request {
    data: Vec<u8>,
    keyframe: bool,
    reset: bool,
    reply: oneshot::Sender<Result<Option<DecodedFrame>, DecodeError>>,
}

/// Decodes a single remote source in a confined worker. One request can be
/// active and one queued, including if a caller cancels its decode future.
pub struct Decoder {
    requests: mpsc::Sender<Request>,
    task: JoinHandle<()>,
    source: Source,
    reset: bool,
}

impl Decoder {
    /// The executable path comes from the installed directory.
    pub fn start(executable: PathBuf, source: Source) -> Result<Self, DecodeError> {
        let permit = SOURCES.try_acquire().map_err(|_| DecodeError::Busy)?;
        let (requests, queue) = mpsc::channel(1);
        let task = tokio::runtime::Handle::try_current()
            .map_err(|_| DecodeError::Runtime)?
            .spawn(async move {
                let _permit = permit;
                supervise(&executable, source, queue).await;
            });
        Ok(Self {
            requests,
            task,
            source,
            reset: false,
        })
    }

    /// Wait for a fresh keyframe after the source loses continuity.
    pub fn reset(&mut self) {
        self.reset = true;
    }

    /// Submit one AV1 temporal unit. An empty reply means the decoder is
    /// waiting for a keyframe or this temporal unit produced no picture.
    pub async fn decode(
        &mut self,
        keyframe: bool,
        data: Vec<u8>,
    ) -> Result<Option<DecodedFrame>, DecodeError> {
        if data.is_empty() || data.len() > decode_packet_limit(self.source, keyframe) {
            self.reset();
            return Err(invalid().into());
        }
        let (reply, result) = oneshot::channel();
        if let Err(error) = self.requests.try_send(Request {
            data,
            keyframe,
            reset: self.reset,
            reply,
        }) {
            self.reset();
            return Err(match error {
                mpsc::error::TrySendError::Full(_) => DecodeError::Busy,
                mpsc::error::TrySendError::Closed(_) => DecodeError::Stopped,
            });
        }
        self.reset = false;
        result.await.map_err(|_| DecodeError::Stopped)?
    }

    /// Stop accepting frames and wait for the sandbox to exit.
    pub async fn stop(self) -> Result<(), DecodeError> {
        drop(self.requests);
        self.task.await.map_err(|_| DecodeError::Stopped)
    }
}

async fn supervise(executable: &Path, source: Source, mut queue: mpsc::Receiver<Request>) {
    let mut session = None;
    let mut vaapi = super::VAAPI_AVAILABLE;
    let mut synchronized = false;
    let mut failures = 0;
    let mut healthy_since = Instant::now();
    let mut id = 0u64;
    while let Some(request) = queue.recv().await {
        if request.reset || request.reply.is_closed() {
            synchronized = false;
        }
        if request.reply.is_closed() || (!synchronized && !request.keyframe) {
            let _ = request.reply.send(Ok(None));
            continue;
        }
        id = id.wrapping_add(1);
        let decode = async {
            if session.is_none() {
                healthy_since = Instant::now();
                configure(executable, source, &mut vaapi, &mut session).await?;
            }
            let session = session.as_mut().ok_or(DecodeError::Stopped)?;
            let packet = async {
                if !synchronized {
                    session
                        .write(Header::empty(Kind::ResetDecoder, 0), &[])
                        .await?;
                }
                decode_one(session, request.keyframe, &request.data, id, source).await
            };
            timeout(DEADLINE, packet)
                .await
                .unwrap_or(Err(DecodeError::Timeout))
        };
        let result = decode.await;
        synchronized = result.is_ok();
        let _ = request.reply.send(result);
        if !synchronized {
            if let Some(session) = session.take() {
                session.retire().await;
            }
            vaapi = false;
            if healthy_since.elapsed() >= Duration::from_secs(30) {
                failures = 0;
            }
            failures += 1;
            if failures == MAX_FAILURES {
                return;
            }
            sleep(Duration::from_millis(250 * u64::from(failures))).await;
        }
    }
    if let Some(session) = session {
        session.retire().await;
    }
}

async fn configure(
    executable: &Path,
    source: Source,
    vaapi: &mut bool,
    active: &mut Option<Session>,
) -> Result<(), DecodeError> {
    loop {
        *active = Some(Session::spawn(executable, Role::VideoDecode { vaapi: *vaapi }).await?);
        let session = active.as_mut().ok_or(DecodeError::Stopped)?;
        let exchange = async {
            session
                .ready()
                .await
                .map_err(|_| DecodeError::Confinement)?;
            session.grow_output_pipe();
            let header = Header {
                bytes: 1,
                ..Header::empty(Kind::Configure, 0)
            };
            let status = session.exchange(header, &[source_to_byte(source)]).await?;
            if status == Header::empty(Kind::Ready, 0) {
                Ok(true)
            } else if *vaapi && status == Header::empty(Kind::Dropped, 0) {
                Ok(false)
            } else {
                Err(DecodeError::from(invalid()))
            }
        };
        if timeout(DEADLINE, exchange)
            .await
            .unwrap_or(Err(DecodeError::Timeout))?
        {
            return Ok(());
        }
        if let Some(session) = active.take() {
            session.retire().await;
        }
        *vaapi = false;
    }
}

async fn decode_one(
    session: &mut Session,
    keyframe: bool,
    data: &[u8],
    id: u64,
    source: Source,
) -> Result<Option<DecodedFrame>, DecodeError> {
    let request = Header {
        key: keyframe,
        bytes: u32::try_from(data.len()).map_err(|_| invalid())?,
        ..Header::empty(Kind::Decode, id)
    };
    let reply = session.exchange(request, data).await?;
    if reply == Header::empty(Kind::Dropped, id) {
        return Ok(None);
    }
    let (long, short) = source.max_dimensions();
    let max = usize::from(long) * usize::from(short) * 4 + 4;
    if reply.kind != Kind::Decoded || reply.id != id || !(4..=max).contains(&(reply.bytes as usize))
    {
        return Err(invalid().into());
    }
    let mut dimensions = [0; 4];
    session.read(&mut dimensions).await?;
    let [w_hi, w_lo, h_hi, h_lo] = dimensions;
    let width = u16::from_be_bytes([w_hi, w_lo]);
    let height = u16::from_be_bytes([h_hi, h_lo]);
    let required = decode_frame_bytes(source, width, height)?;
    if reply.bytes as usize != 4 + required {
        return Err(invalid().into());
    }
    let bgra = session.read_pixels(required).await?;
    Ok(Some(DecodedFrame {
        width,
        height,
        bgra,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_limits_follow_source_and_accept_portrait() {
        assert_eq!(decode_packet_limit(Source::Camera, true), 256 * 1024);
        assert_eq!(decode_packet_limit(Source::Camera, false), 128 * 1024);
        assert_eq!(decode_packet_limit(Source::Screen, false), 8 * 1024 * 1024);
        assert_eq!(
            decode_frame_bytes(Source::Camera, 1080, 1920).ok(),
            Some(1920 * 1080 * 4)
        );
        assert!(decode_frame_bytes(Source::Camera, 1920, 1081).is_err());
        assert!(decode_frame_bytes(Source::Screen, 1, u16::MAX).is_err());
        assert!(decode_frame_bytes(Source::Screen, 0, 1).is_err());
    }

    #[tokio::test]
    async fn canceling_decode_keeps_the_request_queued_until_reclaimed() {
        use std::{
            future::{Future, poll_fn},
            pin::pin,
            task::Poll,
        };

        let (requests, mut queue) = mpsc::channel(1);
        let mut decoder = Decoder {
            requests,
            task: tokio::spawn(async {}),
            source: Source::Screen,
            reset: false,
        };
        {
            let mut decode = pin!(decoder.decode(true, vec![1]));
            let state = poll_fn(|cx| Poll::Ready(decode.as_mut().poll(cx))).await;
            assert!(state.is_pending());
        }
        assert!(matches!(
            decoder.decode(false, vec![2]).await,
            Err(DecodeError::Busy)
        ));
        assert!(decoder.reset);
        assert!(
            queue
                .try_recv()
                .is_ok_and(|request| request.reply.is_closed())
        );
        assert!(queue.is_empty());
        decoder.stop().await.unwrap();
    }
}
