// SPDX-License-Identifier: EUPL-1.2
//! Bounded asynchronous adapters for persistent worker processes.
use crate::Error;
use nuschl_core::{ChatMessage, HistoryPage, SearchCursor, SearchPage, ServerRecord};
use nuschl_ipc::{
    Envelope, LiveMediaEvent, LiveMediaRequest, MAX_FRAME_BYTES, StorageRequest, StorageResponse,
    TransferEvent, TransferRequest, read_frame_async, write_frame,
};
use nuschl_sandbox::{Role, Worker};
use serde::Serialize;
use std::path::Path;
use tokio::{
    io::AsyncWriteExt,
    process::ChildStdin,
    sync::mpsc,
    time::{Duration, timeout},
};

fn failed(error: nuschl_sandbox::Error) -> Error {
    Error::Worker(error.to_string())
}

/// The sibling executable `name`, started confined as `role`.
async fn start(name: &str, role: Role<'_>) -> Result<Worker, Error> {
    let command = nuschl_sandbox::command(&nuschl_sandbox::sibling(name)?, role).map_err(failed)?;
    nuschl_sandbox::spawn(command).await.map_err(failed)
}

/// How long the first request after a start may take: the worker opens its
/// database before it reads requests, and its pool allows 60 s for that.
const STORAGE_OPENING: Duration = Duration::from_secs(65);
/// How long any later request may take.
const STORAGE_ANSWER: Duration = Duration::from_secs(5);

pub struct Storage {
    worker: Worker,
    request_id: u64,
    failed: bool,
}
impl Storage {
    pub async fn start(directory: &Path) -> Result<Self, Error> {
        Ok(Self::new(
            start("nuschl-storage-worker", Role::Storage { directory }).await?,
        ))
    }
    /// A storage worker its caller started (a trusted test's own).
    pub fn new(worker: Worker) -> Self {
        Self {
            worker,
            request_id: 0,
            failed: false,
        }
    }
    /// Whether the worker was killed: it broke the protocol, exited or took
    /// too long. A request it rejected leaves it running.
    pub fn failed(&self) -> bool {
        self.failed
    }
    async fn request(&mut self, payload: StorageRequest) -> Result<StorageResponse, Error> {
        if self.failed {
            return Err(Error::Worker("Local history is unavailable.".into()));
        }
        self.request_id += 1;
        let id = self.request_id;
        let exchange = async {
            // Bounded with the reply: an opening worker reads nothing yet.
            let mut bytes = Vec::new();
            write_frame(&mut bytes, &Envelope::new(id, payload))?;
            self.worker.input.write_all(&bytes).await?;
            let response: Envelope<StorageResponse> =
                nuschl_ipc::read_reply(&mut self.worker.output, MAX_FRAME_BYTES, id).await?;
            Ok(response.payload)
        };
        let limit = if id == 1 {
            STORAGE_OPENING
        } else {
            STORAGE_ANSWER
        };
        match timeout(limit, exchange).await {
            Ok(Ok(StorageResponse::Rejected(error))) => Err(Error::Worker(error)),
            Ok(Ok(response)) => Ok(response),
            result => {
                self.failed = true;
                let _ = self.worker.child.kill().await;
                match result {
                    Ok(Err(error)) => Err(error),
                    _ => Err(Error::Timeout("waiting for local history")),
                }
            }
        }
    }
    pub async fn append(&mut self, message: ChatMessage) -> Result<i64, Error> {
        match self.request(StorageRequest::Append { message }).await? {
            StorageResponse::Appended(id) => Ok(id),
            _ => Err(Error::Worker("unexpected storage response".into())),
        }
    }
    /// Store a Nuschl server's messages by seq, once each (a page, or a
    /// live message): row IDs in order, 0 for one beyond the limits. Sent in
    /// batches that stay well inside the IPC frame limit.
    pub async fn store_server(&mut self, records: Vec<ServerRecord>) -> Result<Vec<i64>, Error> {
        /// Text per request; a single larger record goes alone.
        const BATCH_BYTES: usize = 2 * 1024 * 1024;
        const BATCH_RECORDS: usize = 100;
        let mut ids = Vec::with_capacity(records.len());
        let mut batch = Vec::new();
        let mut bytes = 0;
        let mut records = records.into_iter().peekable();
        while let Some(record) = records.next() {
            bytes += record.message.body.len() + record.message.author.len();
            batch.push(record);
            let next = records.peek().map_or(0, |next| {
                next.message.body.len() + next.message.author.len()
            });
            if records.peek().is_none()
                || batch.len() >= BATCH_RECORDS
                || bytes + next > BATCH_BYTES
            {
                let records = std::mem::take(&mut batch);
                bytes = 0;
                match self
                    .request(StorageRequest::StoreServer { records })
                    .await?
                {
                    StorageResponse::Stored(stored) => ids.extend(stored),
                    _ => return Err(Error::Worker("unexpected storage response".into())),
                }
            }
        }
        Ok(ids)
    }
    /// Our own pending message (row `id`) was acknowledged as `seq`.
    pub async fn set_seq(&mut self, id: i64, seq: i64) -> Result<(), Error> {
        self.updated(StorageRequest::SetSeq { id, seq })
            .await
            .map(drop)
    }
    /// The cached message `seq` was edited; whether it was cached.
    pub async fn edit(
        &mut self,
        server: String,
        channel: String,
        seq: i64,
        body: String,
        edited_ms: i64,
    ) -> Result<bool, Error> {
        self.updated(StorageRequest::Edit {
            server,
            channel,
            seq,
            body,
            edited_ms,
        })
        .await
    }
    /// The cached message `seq` was deleted (a tombstone without text);
    /// whether it was cached.
    pub async fn delete(
        &mut self,
        server: String,
        channel: String,
        seq: i64,
        deleted_ms: i64,
    ) -> Result<bool, Error> {
        self.updated(StorageRequest::Delete {
            server,
            channel,
            seq,
            deleted_ms,
        })
        .await
    }
    async fn updated(&mut self, request: StorageRequest) -> Result<bool, Error> {
        match self.request(request).await? {
            StorageResponse::Updated(found) => Ok(found),
            _ => Err(Error::Worker("unexpected storage response".into())),
        }
    }
    pub async fn page(
        &mut self,
        server: String,
        channel: String,
        before: Option<i64>,
    ) -> Result<HistoryPage, Error> {
        self.history(StorageRequest::HistoryPage {
            server,
            channel,
            before,
            limit: 40,
        })
        .await
    }
    /// A page around message `id`, to show it in its conversation.
    pub async fn page_around(
        &mut self,
        server: String,
        channel: String,
        id: i64,
    ) -> Result<HistoryPage, Error> {
        self.history(StorageRequest::HistoryAround {
            server,
            channel,
            id,
            limit: 40,
        })
        .await
    }
    pub async fn page_after(
        &mut self,
        server: String,
        channel: String,
        after: i64,
    ) -> Result<HistoryPage, Error> {
        self.history(StorageRequest::HistoryAfter {
            server,
            channel,
            after,
            limit: 40,
        })
        .await
    }
    async fn history(&mut self, request: StorageRequest) -> Result<HistoryPage, Error> {
        match self.request(request).await? {
            StorageResponse::HistoryPage(page) => Ok(page),
            _ => Err(Error::Worker("unexpected storage response".into())),
        }
    }
    /// Full-text search of these servers' history, or of one conversation
    /// of a single server.
    pub async fn search(
        &mut self,
        servers: Vec<String>,
        channel: Option<String>,
        query: String,
        after: Option<SearchCursor>,
    ) -> Result<SearchPage, Error> {
        match self
            .request(StorageRequest::Search {
                servers,
                channel,
                query,
                after,
                limit: 50,
            })
            .await?
        {
            StorageResponse::SearchResults(page) => Ok(page),
            _ => Err(Error::Worker("unexpected storage response".into())),
        }
    }
    pub async fn stop(mut self) {
        let _ = self.worker.child.kill().await;
    }
}

pub struct Media {
    pub commands: mpsc::Sender<LiveMediaRequest>,
    pub events: mpsc::Receiver<LiveMediaEvent>,
    task: tokio::task::JoinHandle<()>,
    /// Dropped with this handle: the worker is killed at once (see `stop`).
    _stop: tokio::sync::oneshot::Sender<()>,
}
/// Requests queued for one media worker. One worker plays every server's
/// voice: room for a burst of packets from several talkers.
const MEDIA_REQUESTS: usize = 256;
const MEDIA_EVENTS: usize = 32;
impl Media {
    /// A test double for the supervisor: a worker reached in memory, with a
    /// real worker's queue sizes. It returns the requests the worker is sent
    /// and where its events go. Once stopped, it has exited when `exit`
    /// completes.
    #[doc(hidden)]
    pub fn in_memory(
        exit: impl Future<Output = ()> + Send + 'static,
    ) -> (
        Self,
        mpsc::Receiver<LiveMediaRequest>,
        mpsc::Sender<LiveMediaEvent>,
    ) {
        let (commands, requests) = mpsc::channel(MEDIA_REQUESTS);
        let (replies, events) = mpsc::channel(MEDIA_EVENTS);
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = stopped.await;
            exit.await;
        });
        (
            Self {
                commands,
                events,
                task,
                _stop: stop,
            },
            requests,
            replies,
        )
    }

    /// Kill the worker at once; the handle completes once it has exited and
    /// been reaped, on Linux once its whole sandbox has exited.
    pub fn stop(self) -> tokio::task::JoinHandle<()> {
        self.task
    }

    pub async fn start() -> Result<Self, Error> {
        let Worker {
            mut child,
            mut input,
            mut output,
            containment,
        } = nuschl_sandbox::spawn_receipted(
            &nuschl_sandbox::sibling("nuschl-media-worker")?,
            Role::Media,
        )
        .await
        .map_err(failed)?;
        // The receipt names the sandbox's original init before any input.
        #[cfg(target_os = "linux")]
        let retirement = {
            let mut retirement = nuschl_ipc::host::preview_lifecycle::Retirement::new(&mut child);
            if let Err(error) = retirement.ready().await {
                let _ = child.start_kill();
                retirement.retire(&mut child, &containment).await;
                return Err(Error::Worker(error.to_string()));
            }
            retirement
        };
        let (tx, mut rx) = mpsc::channel(MEDIA_REQUESTS);
        let (events_tx, events) = mpsc::channel(MEDIA_EVENTS);
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            // The worker lives exactly as long as this task holds its containment.
            let _containment = containment;
            let mut reader = tokio::spawn(async move {
                while let Ok(frame) =
                    read_frame_async::<LiveMediaEvent>(&mut output, MAX_FRAME_BYTES).await
                {
                    if events_tx.send(frame.payload).await.is_err() {
                        break;
                    }
                }
            });
            let mut reader_finished = false;
            let exchange = async {
                loop {
                    tokio::select! {
                        _ = &mut reader => { reader_finished = true; break; },
                        payload = rx.recv() => {
                            let Some(payload) = payload else { break; };
                            let shutdown = matches!(payload, LiveMediaRequest::Shutdown);
                            if send(&mut input, &Envelope::new(0, payload)).await.is_err() || shutdown { break; }
                        },
                    }
                }
            };
            // Stopped, it is killed at once, even during a write.
            tokio::select! {
                () = exchange => {},
                _ = stopped => {},
            }
            if !reader_finished {
                reader.abort();
                let _ = reader.await;
            }
            let _ = child.start_kill();
            // On Linux, wait for the wrapper and the sandbox's namespace init.
            #[cfg(target_os = "linux")]
            retirement.retire(&mut child, &_containment).await;
            #[cfg(not(target_os = "linux"))]
            nuschl_sandbox::reap(&mut child).await;
        });
        Ok(Self {
            commands: tx,
            events,
            task,
            _stop: stop,
        })
    }
}

/// One magic-wormhole offer or download in its own sandboxed worker. An offer
/// exposes only `file`, read-only; a download gets no file at all. Dropping it
/// kills the worker.
pub struct Transfer {
    pub commands: mpsc::Sender<TransferRequest>,
    pub events: mpsc::Receiver<TransferEvent>,
    task: tokio::task::JoinHandle<()>,
}
impl Transfer {
    /// Revoke this worker's network/file capabilities, even when its command
    /// pipe is full or a cooperative cancellation is delayed.
    pub fn abort_handle(&self) -> tokio::task::AbortHandle {
        self.task.abort_handle()
    }

    pub async fn start(file: Option<&Path>) -> Result<Self, Error> {
        let Worker {
            mut child,
            mut input,
            mut output,
            containment,
        } = start("nuschl-transfer-worker", Role::Transfer { file }).await?;
        let (commands, mut requests) = mpsc::channel(4);
        // Few events in flight: a slow disk slows the worker, and the sender with it.
        let (events_tx, events) = mpsc::channel(8);
        let task = tokio::spawn(async move {
            let _containment = containment;
            let mut reader = tokio::spawn(async move {
                while let Ok(frame) =
                    read_frame_async::<TransferEvent>(&mut output, MAX_FRAME_BYTES).await
                {
                    if events_tx.send(frame.payload).await.is_err() {
                        break;
                    }
                }
            });
            let mut exited = false;
            loop {
                tokio::select! {
                    _ = &mut reader => { exited = true; break },
                    request = requests.recv() => {
                        let Some(request) = request else { break };
                        if send(&mut input, &Envelope::new(1, request)).await.is_err() {
                            break;
                        }
                    },
                }
            }
            // A cancelled worker gets a moment to say goodbye to its peers.
            drop(input);
            if !exited && timeout(Duration::from_secs(2), &mut reader).await.is_err() {
                reader.abort();
            }
            let _ = child.kill().await;
        });
        Ok(Self {
            commands,
            events,
            task,
        })
    }
}
impl Drop for Transfer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn send<T: Serialize>(input: &mut ChildStdin, frame: &Envelope<T>) -> Result<(), Error> {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, frame)?;
    timeout(Duration::from_secs(3), input.write_all(&bytes))
        .await
        .map_err(|_| Error::Timeout("writing to worker"))??;
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// A storage worker that waits `delay` seconds, answers `replies` to
    /// requests 1, 2, … and keeps running.
    async fn fake(delay: &str, replies: &[StorageResponse]) -> (Storage, tempfile::NamedTempFile) {
        let mut frames = Vec::new();
        for (index, reply) in replies.iter().enumerate() {
            write_frame(&mut frames, &Envelope::new(index as u64 + 1, reply.clone())).unwrap();
        }
        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, &frames).unwrap();
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", &format!("sleep {delay}; cat \"$0\"; exec sleep 60")])
            .arg(file.path());
        (
            Storage::new(nuschl_sandbox::spawn(command).await.unwrap()),
            file,
        )
    }

    fn message() -> ChatMessage {
        ChatMessage {
            server: "server".into(),
            channel: "channel".into(),
            author: "author".into(),
            observed_at_ms: 1,
            body: "body".into(),
        }
    }

    #[tokio::test]
    async fn a_rejected_request_leaves_the_worker_serving() {
        let (mut storage, _frames) = fake(
            "0",
            &[
                StorageResponse::Rejected("refused".into()),
                StorageResponse::Appended(7),
            ],
        )
        .await;
        assert!(storage.append(message()).await.is_err());
        assert!(!storage.failed());
        assert_eq!(storage.append(message()).await.unwrap(), 7);
    }

    /// Longer than a later request's limit: the worker is still opening its
    /// database when the first request arrives.
    #[tokio::test]
    async fn the_first_request_waits_for_the_database_to_open() {
        let (mut storage, _frames) = fake("6", &[StorageResponse::Appended(7)]).await;
        assert_eq!(storage.append(message()).await.unwrap(), 7);
        assert!(!storage.failed());
    }
}
