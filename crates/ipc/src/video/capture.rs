// SPDX-License-Identifier: EUPL-1.2

#[cfg(target_os = "linux")]
use super::preview::{read_capture_preview, send_capture_start};
use super::{
    CAPTURE_DETAIL_BYTES, CaptureFailure, CaptureLimits, DecodedFrame, Error, HEADER_BYTES, Header,
    Kind, Settings, Source, invalid,
    preview::read_preview_pixels,
    supervisor::{DEADLINE, SOURCES},
};
use crate::host::preview_lifecycle::Retirement;
use nuschl_sandbox::Role;
use std::{
    future::Future,
    path::PathBuf,
    time::{Duration, Instant},
};
#[cfg(target_os = "linux")]
use std::{
    os::{fd::OwnedFd, unix::net::UnixStream as Socket},
    process::Stdio,
};
#[cfg(target_os = "linux")]
use tokio::net::UnixStream;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    process::Child,
    sync::{mpsc, watch},
    task::JoinHandle,
    time::timeout,
};

pub struct VideoFrame {
    pub width: u16,
    pub height: u16,
    /// The negotiated encoding cadence. Quiet sources can emit fewer frames.
    pub fps: u16,
    /// Microseconds since this capture began, on the worker's monotonic clock.
    pub capture_time_us: u64,
    pub av1: Vec<u8>,
    pub keyframe: bool,
    pub hardware: bool,
    pub bitrate_bps: u32,
    received: Instant,
}

pub struct Capture {
    frames: mpsc::Receiver<Result<VideoFrame, Error>>,
    task: JoinHandle<()>,
    control: watch::Sender<Control>,
    source: Source,
    waiting_for_key: bool,
    #[cfg(all(target_os = "linux", feature = "video-portal"))]
    grant: Option<ashpd::zbus::Connection>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Control {
    limits: CaptureLimits,
    key_request: u64,
    preview: bool,
}

const MAX_AGE: Duration = Duration::from_millis(250);

impl Capture {
    /// The remote is the connected PipeWire FD granted by the Linux portal.
    #[cfg(target_os = "linux")]
    pub fn start(
        executable: PathBuf,
        remote: OwnedFd,
        source: Source,
        node_id: Option<u32>,
    ) -> Result<Self, Error> {
        Self::start_inner(
            executable,
            remote,
            source,
            node_id,
            false,
            CaptureLimits::for_source(source, 2_000_000),
            None,
        )
    }

    /// Prefer the isolated VA-API path when its fixed render device exists.
    #[cfg(target_os = "linux")]
    pub fn start_vaapi(
        executable: PathBuf,
        remote: OwnedFd,
        source: Source,
        node_id: Option<u32>,
    ) -> Result<Self, Error> {
        Self::start_inner(
            executable,
            remote,
            source,
            node_id,
            true,
            CaptureLimits::for_source(source, 20_000_000),
            None,
        )
    }

    /// Send local preview pixels over a dedicated socket from the capture worker.
    #[cfg(target_os = "linux")]
    pub fn start_with_preview(
        executable: PathBuf,
        remote: OwnedFd,
        source: Source,
        node_id: Option<u32>,
        limits: CaptureLimits,
        preview: mpsc::Sender<DecodedFrame>,
    ) -> Result<Self, Error> {
        Self::start_inner(
            executable,
            remote,
            source,
            node_id,
            true,
            limits,
            Some(preview),
        )
    }

    #[cfg(target_os = "linux")]
    pub(super) fn start_inner(
        executable: PathBuf,
        remote: OwnedFd,
        source: Source,
        node_id: Option<u32>,
        vaapi: bool,
        limits: CaptureLimits,
        preview: Option<mpsc::Sender<DecodedFrame>>,
    ) -> Result<Self, Error> {
        if (source == Source::Screen) != node_id.is_some() || matches!(node_id, Some(0 | u32::MAX))
        {
            return Err(invalid().into());
        }
        Self::supervise(
            source,
            limits,
            move |sender, updates, requests| async move {
                capture(
                    executable,
                    remote,
                    source,
                    node_id,
                    vaapi,
                    &sender,
                    (updates, requests, preview),
                )
                .await
            },
        )
    }

    /// Start the selected native source; the worker runs the OS consent UI.
    #[cfg(any(target_os = "macos", windows))]
    pub fn start_native(
        executable: PathBuf,
        source: Source,
        limits: CaptureLimits,
        preview: mpsc::Sender<DecodedFrame>,
    ) -> Result<Self, Error> {
        Self::supervise(
            source,
            limits,
            move |sender, updates, requests| async move {
                capture_native(executable, source, &sender, updates, requests, &preview).await
            },
        )
    }

    fn supervise<F>(
        source: Source,
        limits: CaptureLimits,
        run: impl FnOnce(
            mpsc::Sender<Result<VideoFrame, Error>>,
            watch::Receiver<Control>,
            watch::Sender<Control>,
        ) -> F
        + Send
        + 'static,
    ) -> Result<Self, Error>
    where
        F: Future<Output = Result<(), Error>> + Send + 'static,
    {
        limits.validate(source)?;
        let permit = SOURCES.try_acquire().map_err(|_| Error::Busy)?;
        let (sender, frames) = mpsc::channel(1);
        let (control, updates) = watch::channel(Control {
            limits,
            key_request: 0,
            preview: false,
        });
        let requests = control.clone();
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| Error::Runtime)?;
        let task = runtime.spawn(async move {
            let _permit = permit;
            if let Err(error) = run(sender.clone(), updates, requests).await {
                let _ = sender.send(Err(error)).await;
            }
        });
        Ok(Self {
            frames,
            task,
            control,
            source,
            waiting_for_key: true,
            #[cfg(all(target_os = "linux", feature = "video-portal"))]
            grant: None,
        })
    }

    #[cfg(all(target_os = "linux", feature = "video-portal"))]
    pub(super) fn retain_grant(&mut self, connection: ashpd::zbus::Connection) {
        self.grant = Some(connection);
    }

    /// Update the target bitrate. The latest request replaces pending requests.
    pub fn set_bitrate(&self, bitrate_bps: u32) -> Result<(), Error> {
        if !Settings::valid_bitrate(bitrate_bps) {
            return Err(invalid().into());
        }
        if self.control.is_closed() {
            return Err(Error::Stopped);
        }
        self.control.send_if_modified(|control| {
            std::mem::replace(&mut control.limits.bitrate, bitrate_bps) != bitrate_bps
        });
        Ok(())
    }

    /// Apply current height, cadence and bitrate caps. Updates coalesce to
    /// the latest target while the capture retains the existing portal grant.
    pub fn set_limits(&self, limits: CaptureLimits) -> Result<(), Error> {
        limits.validate(self.source)?;
        if self.control.is_closed() {
            return Err(Error::Stopped);
        }
        self.control
            .send_if_modified(|control| std::mem::replace(&mut control.limits, limits) != limits);
        Ok(())
    }

    /// Request a new independently decodable frame after a receiver loss report.
    pub fn request_keyframe(&self) {
        request_keyframe(&self.control);
    }

    pub fn set_preview(&self, visible: bool) {
        self.control.send_if_modified(|control| {
            std::mem::replace(&mut control.preview, visible) != visible
        });
    }

    pub async fn next(&mut self) -> Result<VideoFrame, Error> {
        loop {
            let frame = self.frames.recv().await.ok_or(Error::Stopped)??;
            if frame.received.elapsed() > MAX_AGE {
                self.waiting_for_key = true;
                self.request_keyframe();
            } else if frame.keyframe || !self.waiting_for_key {
                self.waiting_for_key = false;
                return Ok(frame);
            }
        }
    }

    pub async fn stop(self) -> Result<(), Error> {
        drop(self.frames);
        #[cfg(all(target_os = "linux", feature = "video-portal"))]
        drop(self.grant);
        self.task.await.map_err(|_| Error::Stopped)
    }
}

fn request_keyframe(control: &watch::Sender<Control>) {
    control.send_modify(|value| value.key_request = value.key_request.wrapping_add(1));
}

#[cfg(target_os = "linux")]
async fn capture(
    executable: PathBuf,
    remote: OwnedFd,
    source: Source,
    node: Option<u32>,
    vaapi: bool,
    sender: &mpsc::Sender<Result<VideoFrame, Error>>,
    (mut updates, requests, preview): (
        watch::Receiver<Control>,
        watch::Sender<Control>,
        Option<mpsc::Sender<DecodedFrame>>,
    ),
) -> Result<(), Error> {
    let (supervisor, worker) = Socket::pair()?;
    supervisor.set_nonblocking(true)?;
    let mut supervisor = UnixStream::from_std(supervisor)?;
    let role = Role::VideoCapture {
        node,
        vaapi: vaapi && super::VAAPI_AVAILABLE,
    };
    let mut child = nuschl_sandbox::command(&executable, role)?
        .stdin(Stdio::from(remote))
        .stdout(Stdio::from(OwnedFd::from(worker)))
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let containment = nuschl_sandbox::contain(&child)?;
    let mut retirement = Retirement::new(&mut child);
    let result = async {
        retirement.ready().await.map_err(|_| Error::Confinement)?;
        let initial = *updates.borrow_and_update();
        let (preview_socket, descriptor) = if preview.is_some() {
            let (reader, writer) = Socket::pair()?;
            reader.set_nonblocking(true)?;
            (Some(UnixStream::from_std(reader)?), Some(writer.into()))
        } else {
            (None, None)
        };
        timeout(
            DEADLINE,
            send_capture_start(&mut supervisor, initial.limits, descriptor),
        )
        .await
        .map_err(|_| Error::Timeout)??;
        let (mut output, mut control) = supervisor.into_split();
        let preview = async move {
            if let Some((mut socket, sender)) = preview_socket.zip(preview) {
                // A failed preview closes its own socket. The AV1 stream
                // continues on the capture socket.
                let _ = receive_previews(&mut socket, &sender).await;
            }
            std::future::pending::<Result<(), Error>>().await
        };
        tokio::select! {
            _ = sender.closed() => Ok(()),
            result = receive_frames(&mut output, source, sender, &requests, None, Duration::from_secs(15)) => result,
            result = send_updates(&mut control, &mut updates, initial) => result,
            result = preview => result,
        }
    }
    .await;
    let result = match result {
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(capture_exit(&mut child).await)
        }
        result => result,
    };
    let report = async {
        if let Err(error) = result {
            let _ = sender.send(Err(error)).await;
        }
    };
    tokio::join!(retirement.retire(&mut child, &containment), report);
    Ok(())
}

#[cfg(any(target_os = "macos", windows))]
async fn capture_native(
    executable: PathBuf,
    source: Source,
    sender: &mpsc::Sender<Result<VideoFrame, Error>>,
    mut updates: watch::Receiver<Control>,
    requests: watch::Sender<Control>,
    preview: &mpsc::Sender<DecodedFrame>,
) -> Result<(), Error> {
    let role = Role::DesktopVideoCapture {
        camera: source == Source::Camera,
    };
    let mut worker = nuschl_sandbox::spawn_receipted(&executable, role).await?;
    let mut retirement = Retirement::new(&mut worker.child);
    let result = async {
        retirement.ready().await.map_err(|_| Error::Confinement)?;
        let initial = *updates.borrow_and_update();
        send_control(&mut worker.input, Kind::CaptureStart, &initial.limits.to_bytes()).await?;
        tokio::select! {
            _ = sender.closed() => Ok(()),
            result = receive_frames(&mut worker.output, source, sender, &requests, Some(preview), Duration::from_secs(120)) => result,
            result = send_updates(&mut worker.input, &mut updates, initial) => result,
        }
    }.await;
    let result = match result {
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
            Err(capture_exit(&mut worker.child).await)
        }
        result => result,
    };
    drop(worker.input);
    drop(worker.output);
    let report = async {
        if let Err(error) = result {
            let _ = sender.send(Err(error)).await;
        }
    };
    tokio::join!(
        retirement.retire(&mut worker.child, &worker.containment),
        report
    );
    Ok(())
}

async fn capture_exit(child: &mut Child) -> Error {
    // The launcher reports the worker's exit once confinement shuts down.
    match timeout(Duration::from_millis(250), child.wait()).await {
        Ok(Ok(status)) if !status.success() => status
            .code()
            .and_then(CaptureFailure::from_exit)
            .map_or(Error::CaptureExit(status), Error::from),
        _ => Error::CaptureClosed,
    }
}

#[cfg(target_os = "linux")]
async fn receive_previews(
    socket: &mut UnixStream,
    sender: &mpsc::Sender<DecodedFrame>,
) -> Result<(), Error> {
    loop {
        let first = match socket.read_u8().await {
            Ok(first) => first,
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let frame = timeout(DEADLINE, read_capture_preview(socket, first))
            .await
            .map_err(|_| Error::Timeout)??;
        let _ = sender.try_send(frame);
    }
}

async fn send_updates(
    socket: &mut (impl AsyncWrite + Unpin),
    updates: &mut watch::Receiver<Control>,
    mut previous: Control,
) -> Result<(), Error> {
    if previous.preview {
        send_control(socket, Kind::SetCapturePreview, &[1]).await?;
    }
    while updates.changed().await.is_ok() {
        let target = *updates.borrow_and_update();
        if target.limits != previous.limits {
            send_control(socket, Kind::SetCaptureLimits, &target.limits.to_bytes()).await?;
        }
        if target.key_request != previous.key_request {
            send_control(socket, Kind::CaptureKeyframe, &[]).await?;
        }
        if target.preview != previous.preview {
            send_control(socket, Kind::SetCapturePreview, &[u8::from(target.preview)]).await?;
        }
        previous = target;
    }
    Ok(())
}

async fn send_control(
    socket: &mut (impl AsyncWrite + Unpin),
    kind: Kind,
    body: &[u8],
) -> Result<(), Error> {
    let header = Header {
        bytes: body.len() as u32,
        ..Header::empty(kind, 0)
    };
    timeout(DEADLINE, async {
        socket.write_all(&header.to_bytes()).await?;
        socket.write_all(body).await
    })
    .await
    .map_err(|_| Error::Timeout)??;
    Ok(())
}

async fn receive_frames(
    output: &mut (impl AsyncRead + Unpin),
    source: Source,
    sender: &mpsc::Sender<Result<VideoFrame, Error>>,
    control: &watch::Sender<Control>,
    preview: Option<&mpsc::Sender<DecodedFrame>>,
    startup: Duration,
) -> Result<(), Error> {
    let mut frame = timeout(startup, read_frame(output, source, 0, preview))
        .await
        .map_err(|_| Error::Timeout)??;
    let mut waiting_for_key = true;
    loop {
        let previous = frame.capture_time_us;
        if frame.keyframe || !waiting_for_key {
            waiting_for_key = sender.try_send(Ok(frame)).is_err();
            if waiting_for_key {
                request_keyframe(control);
            }
        }
        frame = read_frame(output, source, previous, preview).await?;
    }
}

async fn read_frame(
    input: &mut (impl AsyncRead + Unpin),
    source: Source,
    previous: u64,
    preview: Option<&mpsc::Sender<DecodedFrame>>,
) -> Result<VideoFrame, Error> {
    let mut bytes = [0; HEADER_BYTES];
    loop {
        input.read_exact(&mut bytes[..1]).await?;
        let frame = timeout(DEADLINE, async {
            input.read_exact(&mut bytes[1..]).await?;
            let header = Header::from_bytes(bytes)?;
            if header.kind == Kind::CapturePreview
                && let Some(preview) = preview
            {
                let picture = read_preview_pixels(input, header).await?;
                let _ = preview.try_send(picture);
                return Ok(None);
            }
            if header.kind == Kind::CaptureFailed {
                let size = header.bytes as usize;
                if !(1..=1 + CAPTURE_DETAIL_BYTES).contains(&size) {
                    return Err(invalid().into());
                }
                let mut failure = vec![0; size];
                input.read_exact(&mut failure).await?;
                let reason = CaptureFailure::from_exit(failure[0].into()).ok_or_else(invalid)?;
                // The desktop logs this text from the confined worker on one line.
                let detail = String::from_utf8_lossy(&failure[1..]).replace(char::is_control, " ");
                return Err(Error::Capture(reason, Some(detail)));
            }
            if !matches!(header.kind, Kind::CaptureFrame | Kind::HardwareFrame)
                || header.id <= previous
            {
                return Err(invalid().into());
            }
            let mut settings = [0; Settings::BYTES];
            input.read_exact(&mut settings).await?;
            let settings = Settings::from_bytes(settings)?;
            let size = header.bytes as usize;
            if settings.source != source || size == 0 || size > settings.packet_limit(header.key) {
                return Err(invalid().into());
            }
            let mut av1 = vec![0; size];
            input.read_exact(&mut av1).await?;
            Ok(Some(VideoFrame {
                width: settings.width,
                height: settings.height,
                fps: settings.fps,
                capture_time_us: header.id,
                av1,
                keyframe: header.key,
                hardware: header.kind == Kind::HardwareFrame,
                bitrate_bps: settings.bitrate,
                received: Instant::now(),
            }))
        })
        .await
        .unwrap_or(Err(Error::Timeout))?;
        if let Some(frame) = frame {
            return Ok(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_failed_capture_reports_its_reason_and_the_pipeline_message() {
        let detail = "capture: Internal data stream error.\n".repeat(10);
        let mut bytes = Vec::new();
        super::super::write_capture_failure(&mut bytes, CaptureFailure::Hardware, &detail).unwrap();
        assert_eq!(bytes.len(), HEADER_BYTES + 1 + CAPTURE_DETAIL_BYTES);
        let error = read_frame(&mut bytes.as_slice(), Source::Screen, 0, None)
            .await
            .err()
            .unwrap();
        assert!(matches!(error, Error::Capture(CaptureFailure::Hardware, _)));
        let logged = detail[..CAPTURE_DETAIL_BYTES].replace('\n', " ");
        assert_eq!(error.detail(), Some(logged.as_str()));
        assert_eq!(error.to_string(), CaptureFailure::Hardware.to_string());

        for (reason, size) in [(0, 2), (40, 2 + CAPTURE_DETAIL_BYTES), (40, 0)] {
            let header = Header {
                bytes: size as u32,
                ..Header::empty(Kind::CaptureFailed, 0)
            };
            let mut bytes = header.to_bytes().to_vec();
            bytes.push(reason);
            bytes.resize(HEADER_BYTES + size.max(1), b'x');
            let error = read_frame(&mut bytes.as_slice(), Source::Screen, 0, None)
                .await
                .err()
                .unwrap();
            assert!(matches!(error, Error::Io(_)), "{error}");
        }
    }
}
