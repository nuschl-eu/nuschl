// SPDX-License-Identifier: EUPL-1.2
//! Native admission and key signaling to the configured call host over WSS.

use crate::identity::CryptoHandle;
use futures_util::{SinkExt, StreamExt};
use nuschl_core::network::PeerPolicy;
use nuschl_ipc::crypto::call::{Action, Outcome, Request};
use nuschl_protocol::sfu::{self, ClientMessage, Codec, ServerMessage, Welcome};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpStream, time::timeout};
use tokio_rustls::{TlsConnector, client::TlsStream};
use tokio_tungstenite::{
    WebSocketStream, client_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

#[cfg(any(target_os = "macos", windows))]
mod keys;
mod media;
mod turn;
pub use media::{Media, MediaEvent, MediaOptions, Purpose, Reason, Route};
pub use nuschl_stream_quality::{Kind as VideoKind, Quality};

const HANDSHAKE: Duration = Duration::from_secs(10);
type Socket = WebSocketStream<TlsStream<TcpStream>>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Worker(#[from] crate::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("call signaling: {0}")]
    Protocol(&'static str),
    #[error("call host refused admission ({0})")]
    Refused(String),
    #[error("call signaling timed out")]
    Timeout,
    #[error(transparent)]
    Rtc(#[from] nuschl_ipc::rtc::WorkerError),
}

pub struct Connect<'a> {
    pub generation: u64,
    /// The configured SFU hostname, with an optional port.
    pub host: &'a str,
    pub link: &'a str,
    pub codecs: &'a [Codec],
    /// The configured SFU remains available in relay-only mode. The media
    /// adapter applies this policy before gathering any candidates.
    pub policy: PeerPolicy,
}

pub struct Connection {
    welcome: Welcome,
    socket: Socket,
    video_limits: bool,
    scope: CallScope,
    policy: PeerPolicy,
    pending_offer: Option<ServerMessage>,
}

struct CallScope {
    #[cfg(target_os = "linux")]
    crypto: CryptoHandle,
    generation: u64,
    #[cfg(target_os = "linux")]
    cleanup: Option<tokio::runtime::Handle>,
    #[cfg(not(target_os = "linux"))]
    keys: keys::Keys,
    #[cfg(not(target_os = "linux"))]
    epochs: Option<std::io::PipeReader>,
}

impl CallScope {
    async fn start_media(
        &mut self,
        executable: std::path::PathBuf,
    ) -> Result<nuschl_ipc::rtc::Worker, Error> {
        #[cfg(target_os = "linux")]
        {
            let (epochs, reader) = std::os::unix::net::UnixStream::pair()?;
            let worker =
                nuschl_ipc::rtc::Worker::start(executable, self.generation, reader.into()).await?;
            if let Err(error) = self
                .crypto
                .attach_media(self.generation, epochs.into())
                .await
            {
                let _ = worker.stop().await;
                return Err(error.into());
            }
            Ok(worker)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let reader = self
                .epochs
                .take()
                .ok_or(Error::Protocol("call media already started"))?;
            Ok(nuschl_ipc::rtc::Worker::start(executable, self.generation, reader).await?)
        }
    }

    fn new(crypto: CryptoHandle, generation: u64) -> Result<Self, Error> {
        #[cfg(target_os = "linux")]
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| Error::Protocol("call signaling requires a Tokio runtime"))?;
        crypto.reserve_call_generation(generation)?;
        #[cfg(not(target_os = "linux"))]
        let (keys, epochs) = keys::Keys::start()?;
        Ok(Self {
            #[cfg(target_os = "linux")]
            crypto,
            generation,
            #[cfg(target_os = "linux")]
            cleanup: Some(runtime),
            #[cfg(not(target_os = "linux"))]
            keys,
            #[cfg(not(target_os = "linux"))]
            epochs: Some(epochs),
        })
    }

    async fn request(&self, action: Action) -> Result<Outcome, Error> {
        let request = Request {
            generation: self.generation,
            action,
        };
        #[cfg(target_os = "linux")]
        let reply = self.crypto.call_signal_reserved(request).await?;
        #[cfg(not(target_os = "linux"))]
        let reply = self.keys.request(request).await?;
        Ok(reply.outcome)
    }

    #[cfg(target_os = "linux")]
    async fn close(mut self) -> Result<(), Error> {
        let result = self.request(Action::Close).await.map(|_| ());
        self.cleanup = None;
        result
    }

    #[cfg(not(target_os = "linux"))]
    async fn close(self) -> Result<(), Error> {
        let result = self.request(Action::Close).await;
        result.and(self.keys.stop().await)
    }
}

#[cfg(target_os = "linux")]
impl Drop for CallScope {
    fn drop(&mut self) {
        if let Some(runtime) = self.cleanup.take() {
            let crypto = self.crypto.clone();
            let generation = self.generation;
            runtime.spawn(async move {
                let _ = crypto
                    .call_signal_reserved(Request {
                        generation,
                        action: Action::Close,
                    })
                    .await;
            });
        }
    }
}

impl Connection {
    pub async fn connect(crypto: CryptoHandle, options: Connect<'_>) -> Result<Self, Error> {
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Self::connect_with_roots(crypto, options, roots).await
    }

    async fn connect_with_roots(
        crypto: CryptoHandle,
        options: Connect<'_>,
        roots: rustls::RootCertStore,
    ) -> Result<Self, Error> {
        if options.codecs.is_empty() || options.codecs.len() > 2 {
            return Err(Error::Protocol("invalid decoder list"));
        }
        // Reserve before queueing Open so cleanup also covers an in-flight reply.
        let scope = CallScope::new(crypto, options.generation)?;
        let Outcome::Opened { room, verifier } = scope
            .request(Action::Open {
                host: options.host.to_owned(),
                link: options.link.to_owned(),
                relay_only: options.policy.relay_only,
            })
            .await?
        else {
            return Err(Error::Protocol("invalid room metadata"));
        };
        let (socket, welcome, video_limits) = timeout(HANDSHAKE, async {
            let mut socket = open(options.host, roots).await?;
            let ServerMessage::Hello {
                nonce,
                video_limits,
                screen_rate,
            } = read(&mut socket).await?
            else {
                return Err(Error::Protocol("expected a challenge"));
            };
            let nonce = sfu::from_hex(&nonce).ok_or(Error::Protocol("invalid challenge"))?;
            let Outcome::Admitted {
                proof,
                host_proof,
                create,
            } = scope.request(Action::Admit { nonce }).await?
            else {
                return Err(Error::Protocol("invalid admission proof"));
            };
            write(
                &mut socket,
                &ClientMessage::Join {
                    room: hex::encode(room),
                    verifier: hex::encode(verifier),
                    proof: hex::encode(proof),
                    host_proof: host_proof.map(hex::encode),
                    create: create.map(|keys| sfu::Create {
                        admit_key: hex::encode(keys.admit_key),
                        host_key: hex::encode(keys.host_key),
                    }),
                    codecs: options.codecs.to_vec(),
                    video_limits,
                    screen_rate,
                },
            )
            .await?;
            match read(&mut socket).await? {
                ServerMessage::Welcome(welcome)
                    if welcome.peer > 0
                        && welcome.peers.len() <= 256
                        && welcome.peers.iter().enumerate().all(|(index, peer)| {
                            peer.id != 0
                                && peer.id != welcome.peer
                                && welcome.peers[..index]
                                    .iter()
                                    .all(|previous| previous.id != peer.id)
                        })
                        && options.codecs.contains(&welcome.codec) =>
                {
                    Ok((socket, welcome, video_limits))
                }
                ServerMessage::Error { reason }
                    if reason.len() <= 32
                        && reason.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
                {
                    Err(Error::Refused(reason.into_owned()))
                }
                _ => Err(Error::Protocol("invalid welcome")),
            }
        })
        .await
        .map_err(|_| Error::Timeout)??;
        Ok(Self {
            welcome,
            socket,
            video_limits,
            scope,
            policy: options.policy,
            pending_offer: None,
        })
    }

    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    pub async fn next(&mut self) -> Result<ServerMessage, Error> {
        if let Some(offer) = self.pending_offer.take() {
            return Ok(offer);
        }
        let mut message = read(&mut self.socket).await?;
        if !self.video_limits {
            match &mut message {
                ServerMessage::Quality { limit, .. } => *limit = None,
                ServerMessage::VideoLimit { .. } => {
                    return Err(Error::Protocol("unexpected video limit report"));
                }
                _ => {}
            }
        }
        Ok(message)
    }

    /// Keep control signaling current while the owner chooses a video source.
    pub async fn poll_idle(&mut self) -> Result<(), Error> {
        match read(&mut self.socket).await? {
            ServerMessage::Joined { peer, role } => {
                if self.welcome.peers.len() >= 256 || peer == 0 || peer == self.welcome.peer {
                    return Err(Error::Protocol("invalid call roster"));
                }
                if !self.welcome.peers.iter().any(|entry| entry.id == peer) {
                    self.welcome.peers.push(sfu::PeerInfo { id: peer, role });
                }
            }
            ServerMessage::Left { peer } => self.welcome.peers.retain(|entry| entry.id != peer),
            ServerMessage::Codec { codec } if codec != self.welcome.codec => {
                return Err(Error::Protocol("native call codec changed"));
            }
            offer @ ServerMessage::Offer { .. } => self.pending_offer = Some(offer),
            ServerMessage::Error { reason } => return Err(Error::Refused(reason.into_owned())),
            _ => {}
        }
        Ok(())
    }

    /// Exchange opaque call envelopes with the configured SFU. The worker
    /// owns the call generation, independent peer keys and relay policy.
    pub async fn signal(&mut self, action: Action) -> Result<Outcome, Error> {
        if !matches!(
            action,
            Action::Publish { .. }
                | Action::Subscribe { .. }
                | Action::Left { .. }
                | Action::Tick
                | Action::Key { .. }
                | Action::Seal { .. }
        ) {
            return Err(Error::Protocol("invalid key action"));
        }
        let outcome = self.scope.request(action).await?;
        match &outcome {
            Outcome::Send { peer, data, .. } => self.send_key(*peer, data).await?,
            Outcome::KeyBatch { messages } => {
                for message in messages {
                    self.send_key(message.peer, &message.data).await?;
                }
            }
            _ => {}
        }
        Ok(outcome)
    }

    async fn send_key(&mut self, peer: u64, data: &[u8]) -> Result<(), Error> {
        write(
            &mut self.socket,
            &ClientMessage::Key {
                to: peer,
                data: hex::encode(data),
            },
        )
        .await
    }

    pub async fn close(mut self) -> Result<(), Error> {
        let _ = timeout(HANDSHAKE, self.socket.close(None)).await;
        self.scope.close().await
    }
}

async fn open(host: &str, roots: rustls::RootCertStore) -> Result<Socket, Error> {
    let (name, port) = match host.split_once(':') {
        Some((name, port)) => (
            name,
            port.parse().map_err(|_| Error::Protocol("invalid port"))?,
        ),
        None => (host, 443),
    };
    let name = rustls::pki_types::ServerName::try_from(name.to_owned())
        .map_err(|_| Error::Protocol("invalid host"))?;
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| Error::Protocol("cannot configure TLS"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let stream = TcpStream::connect((name.to_str().as_ref(), port)).await?;
    let tls = TlsConnector::from(Arc::new(config))
        .connect(name, stream)
        .await?;
    let limits = WebSocketConfig::default()
        .max_message_size(Some(sfu::MAX_MESSAGE_BYTES))
        .max_frame_size(Some(sfu::MAX_MESSAGE_BYTES));
    let (socket, _) = client_async_with_config(format!("wss://{host}/sfu"), tls, Some(limits))
        .await
        .map_err(|_| Error::Protocol("WebSocket handshake failed"))?;
    Ok(socket)
}

async fn write(socket: &mut Socket, message: &ClientMessage) -> Result<(), Error> {
    let message = serde_json::to_string(message).map_err(|_| Error::Protocol("invalid request"))?;
    timeout(HANDSHAKE, socket.send(Message::text(message)))
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(|_| Error::Protocol("cannot send request"))
}

async fn read(socket: &mut Socket) -> Result<ServerMessage, Error> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => {
                let message =
                    serde_json::from_str(&text).map_err(|_| Error::Protocol("invalid response"))?;
                if let ServerMessage::Key { from, data } = &message
                    && (*from == 0 || !sfu::valid_key_message(data))
                {
                    return Err(Error::Protocol("invalid key message"));
                }
                return Ok(message);
            }
            Some(Ok(Message::Ping(_))) => socket
                .flush()
                .await
                .map_err(|_| Error::Protocol("closed"))?,
            Some(Ok(Message::Pong(_))) => {}
            _ => return Err(Error::Protocol("closed")),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
