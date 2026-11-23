// SPDX-License-Identifier: EUPL-1.2

use super::{CaptureFailure, HEADER_BYTES, Header, Kind, Settings, invalid};
use crate::host::preview_lifecycle::Retirement;
use nuschl_sandbox::{Role, Worker};
use std::{io, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    sync::{Semaphore, mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};

pub(super) const DEADLINE: Duration = Duration::from_secs(5);
const HEALTHY: Duration = Duration::from_secs(30);
pub(super) static SOURCES: Semaphore = Semaphore::const_new(2);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Sandbox(#[from] nuschl_sandbox::Error),
    #[error("both video encoder slots are occupied")]
    Busy,
    #[error("video encoder could not start in confinement")]
    Confinement,
    #[error("video encoder exceeded its deadline")]
    Timeout,
    #[error("video encoder has stopped")]
    Stopped,
    /// The worker's reason, with the pipeline's message when it sent one.
    #[error("{0}")]
    Capture(CaptureFailure, Option<String>),
    #[error("The video capture connection closed.")]
    CaptureClosed,
    #[error("The video capture worker exited ({0}).")]
    CaptureExit(std::process::ExitStatus),
    #[error("a Tokio runtime is required to start a video source")]
    Runtime,
}

impl From<CaptureFailure> for Error {
    fn from(reason: CaptureFailure) -> Self {
        Self::Capture(reason, None)
    }
}

impl Error {
    /// The capture pipeline's message for a failed capture.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Capture(_, detail) => detail.as_deref(),
            _ => None,
        }
    }
}

pub enum Output {
    /// One independent AV1 temporal unit with its sequence header.
    Keyframe(Vec<u8>),
    /// A frame exceeded its size limit. The next frame is also a keyframe.
    Dropped,
}

struct Request {
    frame: Vec<u8>,
    reply: oneshot::Sender<Result<Output, Error>>,
}

/// A local video source. At most one frame is in the encoder and one frame
/// waits in the channel, including if its caller cancels an encode.
pub struct Encoder {
    requests: mpsc::Sender<Request>,
    task: JoinHandle<()>,
    frame_bytes: usize,
}

impl Encoder {
    /// The executable path comes from the trusted caller's installed directory.
    pub fn start(executable: PathBuf, settings: Settings) -> Result<Self, Error> {
        let frame_bytes = settings.frame_bytes()?;
        let permit = SOURCES.try_acquire().map_err(|_| Error::Busy)?;
        let (requests, queue) = mpsc::channel(1);
        let task = tokio::runtime::Handle::try_current()
            .map_err(|_| Error::Runtime)?
            .spawn(async move {
                let _permit = permit;
                supervise(executable, settings, queue).await;
            });
        Ok(Self {
            requests,
            task,
            frame_bytes,
        })
    }

    /// Encode one complete I420 frame with tightly packed Y, U and V planes.
    /// A failed frame is discarded. A later frame may start a new worker.
    pub async fn encode(&mut self, frame: Vec<u8>) -> Result<Output, Error> {
        if frame.len() != self.frame_bytes {
            return Err(invalid().into());
        }
        let (reply, result) = oneshot::channel();
        self.requests
            .try_send(Request { frame, reply })
            .map_err(|err| match err {
                mpsc::error::TrySendError::Full(_) => Error::Busy,
                mpsc::error::TrySendError::Closed(_) => Error::Stopped,
            })?;
        result.await.map_err(|_| Error::Stopped)?
    }

    /// Stop accepting frames and wait until the source slot can be reused.
    pub async fn stop(self) -> Result<(), Error> {
        drop(self.requests);
        self.task.await.map_err(|_| Error::Stopped)
    }
}

async fn supervise(executable: PathBuf, settings: Settings, mut queue: mpsc::Receiver<Request>) {
    let mut session = None;
    let mut failures = 0;
    let mut healthy_since = Instant::now();
    let mut id = 0u64;
    while let Some(Request { frame, reply }) = queue.recv().await {
        if reply.is_closed() {
            continue;
        }
        id = id.wrapping_add(1);
        let exchange = async {
            if session.is_none() {
                session = Some(Session::spawn(&executable, Role::VideoEncode).await?);
                healthy_since = Instant::now();
                if let Some(session) = &mut session {
                    session.configure(settings).await?;
                }
            }
            let session = session.as_mut().ok_or(Error::Stopped)?;
            session.encode(id, settings, &frame).await
        };
        let result = timeout(DEADLINE, exchange)
            .await
            .unwrap_or(Err(Error::Timeout));
        let failed = result.is_err();
        let _ = reply.send(result);
        if failed {
            if let Some(session) = session.take() {
                session.retire().await;
            }
            if healthy_since.elapsed() >= HEALTHY {
                failures = 0;
            }
            failures += 1;
            if failures == 3 {
                return;
            }
            sleep(Duration::from_millis(250 * failures)).await;
        }
    }
    if let Some(session) = session {
        session.retire().await;
    }
}

pub(super) struct Session {
    worker: Worker,
    retirement: Retirement,
}

impl Session {
    pub(super) async fn spawn(
        executable: &std::path::Path,
        role: Role<'_>,
    ) -> Result<Self, nuschl_sandbox::Error> {
        let mut worker = nuschl_sandbox::spawn_receipted(executable, role).await?;
        let retirement = Retirement::new(&mut worker.child);
        Ok(Self { worker, retirement })
    }

    async fn configure(&mut self, settings: Settings) -> Result<(), Error> {
        self.ready().await.map_err(|_| Error::Confinement)?;
        let configuration = settings.to_bytes();
        let header = Header {
            bytes: Settings::BYTES as u32,
            ..Header::empty(Kind::Configure, 0)
        };
        if self.exchange(header, &configuration).await? != Header::empty(Kind::Ready, 0) {
            return Err(invalid().into());
        }
        Ok(())
    }

    async fn encode(&mut self, id: u64, settings: Settings, frame: &[u8]) -> Result<Output, Error> {
        let bytes = u32::try_from(frame.len()).map_err(|_| invalid())?;
        let header = Header {
            bytes,
            ..Header::empty(Kind::Frame, id)
        };
        let reply = self.exchange(header, frame).await?;
        if reply.id != id {
            return Err(invalid().into());
        }
        if reply == Header::empty(Kind::Dropped, id) {
            return Ok(Output::Dropped);
        }
        let size = reply.bytes as usize;
        if reply.kind != Kind::Encoded
            || !reply.key
            || size == 0
            || size > settings.packet_limit(true)
        {
            return Err(invalid().into());
        }
        let mut bytes = vec![0; size];
        self.read(&mut bytes).await?;
        Ok(Output::Keyframe(bytes))
    }

    pub(super) async fn ready(&mut self) -> Result<(), crate::host::Error> {
        self.retirement.ready().await
    }

    pub(super) async fn exchange(&mut self, header: Header, bytes: &[u8]) -> io::Result<Header> {
        self.write(header, bytes).await?;
        let mut response = [0; HEADER_BYTES];
        self.read(&mut response).await?;
        Header::from_bytes(response)
    }

    pub(super) async fn write(&mut self, header: Header, bytes: &[u8]) -> io::Result<()> {
        self.worker.input.write_all(&header.to_bytes()).await?;
        self.worker.input.write_all(bytes).await
    }

    pub(super) fn grow_output_pipe(&self) {
        // Packed video needs more pipe bandwidth. The system may grant less.
        #[cfg(target_os = "linux")]
        let _ = rustix::pipe::fcntl_setpipe_size(&self.worker.output, 1024 * 1024);
    }

    pub(super) async fn read(&mut self, bytes: &mut [u8]) -> io::Result<()> {
        self.worker.output.read_exact(bytes).await.map(|_| ())
    }

    pub(super) async fn read_pixels(&mut self, len: usize) -> io::Result<Vec<u8>> {
        let mut pixels = Vec::with_capacity(len.max(32));
        // Tokio fills the vector's spare capacity directly. The limit keeps the
        // next worker reply on the pipe even if the worker sends them together.
        let read = (&mut self.worker.output)
            .take(len as u64)
            .read_to_end(&mut pixels)
            .await?;
        if read != len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short video frame",
            ));
        }
        Ok(pixels)
    }

    pub(super) async fn retire(mut self) {
        self.retirement
            .retire(&mut self.worker.child, &self.worker.containment)
            .await;
    }
}
