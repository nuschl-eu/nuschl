// SPDX-License-Identifier: EUPL-1.2
//! Call signaling on platforms with a dedicated, inherited key pipe.

use super::Error;
use nuschl_ipc::{
    Envelope,
    crypto::call::{MAX_REPLY_FRAME_BYTES, Reply, Request},
    host::preview_lifecycle::Retirement,
    write_frame_within,
};
use std::{io, process::Stdio, time::Duration};
use tokio::{
    io::AsyncWriteExt as _,
    sync::{Semaphore, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};

static WORKERS: Semaphore = Semaphore::const_new(2);
const EXCHANGE: Duration = Duration::from_secs(5);
type Result<T> = std::result::Result<T, Error>;

pub(super) struct Keys {
    requests: mpsc::Sender<(Request, oneshot::Sender<Result<Reply>>)>,
    task: JoinHandle<()>,
}

impl Keys {
    pub(super) fn start() -> Result<(Self, io::PipeReader)> {
        let executable = nuschl_sandbox::sibling("nuschl-crypto-worker")?;
        let permit = WORKERS
            .try_acquire()
            .map_err(|_| Error::Protocol("call key workers are busy"))?;
        let (reader, writer) = io::pipe()?;
        let (requests, mut queue) = mpsc::channel::<(Request, oneshot::Sender<Result<Reply>>)>(1);
        let task = tokio::spawn(async move {
            let _permit = permit;
            let Ok(mut worker) = nuschl_sandbox::spawn_call_worker(
                &executable,
                nuschl_sandbox::Role::CallKeys,
                Stdio::from(writer),
            )
            .await
            else {
                return;
            };
            let mut retirement = Retirement::new(&mut worker.child);
            if retirement.ready().await.is_ok() {
                let mut sequence = 1..=u64::MAX;
                while let Some((request, answer)) = queue.recv().await {
                    let Some(id) = sequence.next() else { break };
                    let result = timeout(EXCHANGE, async {
                        let mut data = Vec::new();
                        let request = Envelope::new(id, request);
                        write_frame_within(&mut data, &request, MAX_REPLY_FRAME_BYTES)
                            .map_err(io::Error::from)?;
                        worker.input.write_all(&data).await?;
                        let reply = nuschl_ipc::read_reply::<Option<Reply>>(
                            &mut worker.output,
                            MAX_REPLY_FRAME_BYTES,
                            id,
                        )
                        .await
                        .map_err(io::Error::from)?;
                        if reply
                            .payload
                            .as_ref()
                            .is_some_and(|reply| !reply.matches(&request.payload))
                        {
                            return Err(Error::Protocol("invalid call key reply"));
                        }
                        Ok(reply.payload)
                    })
                    .await
                    .unwrap_or(Err(Error::Timeout));
                    let failed = result.is_err();
                    let _ = answer.send(result.and_then(|reply| {
                        reply.ok_or(Error::Protocol("the call key worker rejected the request"))
                    }));
                    if failed {
                        break;
                    }
                }
            }
            queue.close();
            drop(queue);
            drop(worker.input);
            drop(worker.output);
            retirement
                .retire(&mut worker.child, &worker.containment)
                .await;
        });
        Ok((Self { requests, task }, reader))
    }

    pub(super) async fn request(&self, request: Request) -> Result<Reply> {
        request.check().map_err(Error::Protocol)?;
        let (send, receive) = oneshot::channel();
        self.requests
            .send((request, send))
            .await
            .map_err(|_| stopped())?;
        receive.await.map_err(|_| stopped())?
    }

    pub(super) async fn stop(self) -> Result<()> {
        drop(self.requests);
        self.task.await.map_err(|_| stopped())
    }
}

fn stopped() -> Error {
    Error::Protocol("the call key worker stopped")
}
