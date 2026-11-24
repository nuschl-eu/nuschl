// SPDX-License-Identifier: EUPL-1.2
//! Optional native peer legs: a publisher sends to up to two viewers, a viewer
//! receives from one publisher. The SFU transports sealed signaling.

use super::{
    Command, Connection, Description, Error, Instant, Leg, LegRole, Network, Purpose, Reason,
    Route, Track, Worker, describe, done,
};
use crate::sfu::turn::turn_address;
use futures_util::{FutureExt, future::BoxFuture};
use nuschl_ipc::{
    crypto::call::Action,
    rtc::{Codec, FIRST_DIRECT_LEG, MAX_DIRECT_SEND_LEGS},
};
use serde::{Deserialize, Serialize};
use std::{
    future::{pending, poll_fn},
    net::{IpAddr, SocketAddr},
    task::Poll,
    time::Duration,
};
use tokio::{
    io::ReadBuf,
    net::{UdpSocket, lookup_host},
    time::timeout,
};
use turn_client_proto::stun::types::{
    attribute::XorMappedAddress,
    message::{BINDING, Message, MessageClass, MessageWrite, MessageWriteVec},
};

const SETUP: Duration = Duration::from_secs(10);
const COOLDOWN: Duration = Duration::from_secs(15);
const SAMPLE_AGE: Duration = Duration::from_secs(5);
const DISCOVERY: Duration = Duration::from_millis(900);
// str0m emits ICE checks at least every three seconds on an idle live leg.
const SILENCE: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Candidates {
    host: SocketAddr,
    reflexive: Option<SocketAddr>,
}

impl Candidates {
    pub(super) fn contains(self, address: SocketAddr) -> bool {
        address == self.host || self.reflexive == Some(address)
    }

    fn valid(self, interface: IpAddr) -> bool {
        allowed(interface, self.host)
            && self
                .reflexive
                .is_none_or(|address| address != self.host && allowed(interface, address))
    }
}

pub(super) enum Setup {
    Offer(Vec<Purpose>),
    Answer {
        description: Description,
        remote: Candidates,
    },
}

pub(super) struct Prepared {
    peer: u64,
    socket: UdpSocket,
    local: Candidates,
    setup: Setup,
}

pub(super) type Discovery = BoxFuture<'static, Result<Prepared, Error>>;

pub(super) async fn next_prepared(active: &mut Option<Discovery>) -> Result<Prepared, Error> {
    match active {
        Some(wake) => wake.await,
        None => pending().await,
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "native_direct", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Signal {
    Ready,
    Offer {
        candidates: Candidates,
        description: Description,
    },
    Answer {
        candidates: Candidates,
        description: Description,
    },
    Connected {
        id: [u8; 16],
    },
    Close {
        id: [u8; 16],
    },
}

/// The direct legs an endpoint may use. Video in both directions keeps the SFU.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Role {
    /// Every other participant receives this publisher's video.
    Send(Vec<u64>),
    /// This viewer receives video from one publisher.
    Receive(u64),
}

pub(super) fn role(peers: &[u64], publishing: bool, subscribed: &[Track]) -> Result<Role, Reason> {
    match (publishing, subscribed) {
        (true, []) if peers.is_empty() => Err(Reason::Viewer),
        (true, []) if peers.len() > MAX_DIRECT_SEND_LEGS => Err(Reason::Viewers),
        (true, []) => Ok(Role::Send(peers.to_vec())),
        (true, _) => Err(Reason::TwoWay),
        (false, []) => Err(Reason::Sender),
        (false, [first, rest @ ..]) => match first.publisher {
            Some(publisher) if rest.iter().all(|track| track.publisher == Some(publisher)) => {
                Ok(Role::Receive(publisher))
            }
            _ => Err(Reason::Senders),
        },
    }
}

pub(super) struct Direct {
    interface: IpAddr,
    stun: Option<(String, u16)>,
    next_leg: u32,
    /// Viewers that decoded this publisher's video through the SFU.
    pub(super) ready: Vec<u64>,
    pub(super) asked: bool,
    /// All legs of one role; a publisher selects them together.
    pub(super) legs: Vec<Attempt>,
    retry: Instant,
    /// Why the last attempt closed, until the next one starts.
    closed: Option<Reason>,
}

pub(super) struct Attempt {
    pub(super) peer: u64,
    pub(super) transport: Leg,
    pub(super) id: [u8; 16],
    deadline: Instant,
    tracks: Vec<Track>,
    pub(super) answered: bool,
    pub(super) confirmed: bool,
}

impl Direct {
    pub(super) fn new(interface: IpAddr, urls: &[String]) -> Self {
        Self {
            interface,
            stun: urls
                .iter()
                .take(6)
                .find_map(|url| turn_address(url))
                .map(|(host, port)| (host.to_owned(), port)),
            next_leg: FIRST_DIRECT_LEG,
            ready: Vec::new(),
            asked: false,
            legs: Vec::new(),
            retry: Instant::now(),
            closed: None,
        }
    }

    /// `upload` tells whether the publisher's probed upload carries the
    /// stream to every viewer.
    pub(super) fn route(&self, role: &Result<Role, Reason>, upload: Option<bool>) -> Route {
        let reason = match role {
            Err(reason) => *reason,
            Ok(_) if self.selected() => return Route::Direct,
            Ok(Role::Send(viewers))
                if !viewers.iter().all(|viewer| self.ready.contains(viewer)) =>
            {
                Reason::Viewer
            }
            Ok(Role::Send(_)) if upload.is_none() => Reason::Measuring,
            Ok(Role::Send(_)) if upload == Some(false) => Reason::Upload,
            Ok(_) if !self.legs.is_empty() => Reason::Connecting,
            Ok(_) if let Some(reason) = self.closed => reason,
            Ok(Role::Send(_)) => Reason::Connecting,
            Ok(Role::Receive(_)) => Reason::Sender,
        };
        Route::Sfu(reason)
    }

    pub(super) fn can_offer(&self) -> bool {
        Instant::now() >= self.retry
    }

    /// The publisher's cooldown paces retries.
    pub(super) fn can_accept(&self) -> bool {
        self.asked && self.legs.is_empty()
    }

    pub(super) fn selected(&self) -> bool {
        self.legs.iter().any(|leg| leg.transport.selected)
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.legs
            .iter()
            .map(|leg| {
                if leg.transport.selected {
                    leg.transport
                        .last_packet
                        .map_or(leg.deadline, |last| last + SILENCE)
                } else {
                    leg.deadline
                }
            })
            .min()
    }

    pub(super) fn wake(&self) -> Option<Instant> {
        self.legs
            .iter()
            .filter(|leg| leg.answered)
            .filter_map(|leg| leg.transport.wake)
            .min()
    }

    pub(super) fn discover(&self, peer: u64, setup: Setup) -> Discovery {
        let interface = self.interface;
        let stun = self.stun.clone();
        async move {
            let socket = UdpSocket::bind(SocketAddr::new(interface, 0)).await?;
            let host = socket.local_addr()?;
            let reflexive = match stun {
                Some((server, port)) => {
                    timeout(DISCOVERY, binding(&socket, &server, port, interface))
                        .await
                        .ok()
                        .flatten()
                        .filter(|&address| address != host && allowed(interface, address))
                }
                None => None,
            };
            Ok(Prepared {
                peer,
                socket,
                local: Candidates { host, reflexive },
                setup,
            })
        }
        .boxed()
    }

    pub(super) fn accept(
        &self,
        peer: u64,
        candidates: Candidates,
        mut description: Description,
        subscribed: &[Track],
    ) -> Option<Discovery> {
        if !(1..=2).contains(&description.tracks.len())
            || description.tracks.iter().any(|track| {
                track.publisher.is_some()
                    || !subscribed.iter().any(|known| {
                        known.publisher == Some(peer) && known.purpose == track.purpose
                    })
            })
            || !valid_sdp(self.interface, candidates, &description.sdp)
        {
            return None;
        }
        for track in &mut description.tracks {
            track.publisher = Some(peer);
        }
        Some(self.discover(
            peer,
            Setup::Answer {
                description,
                remote: candidates,
            },
        ))
    }

    pub(super) async fn complete(
        &mut self,
        worker: &mut Worker,
        codec: Codec,
        Prepared {
            peer,
            socket,
            mut local,
            setup,
        }: Prepared,
    ) -> Result<(u64, Signal), Error> {
        let leg = self.next_leg;
        self.next_leg = leg
            .checked_add(1)
            .ok_or(Error::Protocol("direct leg limit"))?;
        let id = match &setup {
            Setup::Offer(_) => *uuid::Uuid::new_v4().as_bytes(),
            Setup::Answer { description, .. } => description.id,
        };
        let transport = Leg::start(
            worker,
            leg,
            Network::Direct {
                socket,
                remote: None,
            },
            codec,
            LegRole::Direct,
        )
        .await?;
        self.legs.push(Attempt {
            peer,
            transport,
            id,
            deadline: Instant::now() + SETUP,
            tracks: Vec::new(),
            answered: false,
            confirmed: false,
        });
        let attempt = self.legs.last_mut().ok_or(Error::Protocol("direct leg"))?;
        if let Some(address) = local.reflexive
            && !matches!(
                worker
                    .exchange(Command::AddReflexive { leg, address }, Vec::new())
                    .await?
                    .kind,
                nuschl_ipc::rtc::Reply::Done
            )
        {
            local.reflexive = None;
        }
        match setup {
            Setup::Offer(purposes) => {
                let description = describe(worker, Command::Offer { leg, id, purposes }).await?;
                attempt.tracks = description.tracks.clone();
                Ok((
                    peer,
                    Signal::Offer {
                        candidates: local,
                        description,
                    },
                ))
            }
            Setup::Answer {
                description,
                remote,
            } => {
                attempt.transport.set_video_tracks(&description.tracks);
                let description =
                    describe(worker, Command::AcceptOffer { leg, description }).await?;
                attempt.connect(remote);
                Ok((
                    peer,
                    Signal::Answer {
                        candidates: local,
                        description,
                    },
                ))
            }
        }
    }

    pub(super) async fn answer(
        &mut self,
        worker: &mut Worker,
        peer: u64,
        candidates: Candidates,
        mut description: Description,
    ) -> Result<(), Error> {
        let Some(attempt) = self
            .legs
            .iter_mut()
            .find(|leg| leg.peer == peer && leg.id == description.id && !leg.answered)
        else {
            return Ok(());
        };
        if description.tracks != attempt.tracks
            || !valid_sdp(self.interface, candidates, &description.sdp)
        {
            return Err(Error::Protocol("invalid native direct answer"));
        }
        for track in &mut description.tracks {
            track.publisher = Some(peer);
        }
        done(
            worker,
            Command::AcceptAnswer {
                leg: attempt.transport.id,
                description,
            },
        )
        .await?;
        attempt.connect(candidates);
        Ok(())
    }

    /// Close every leg for `cause`, `None` for a role change, and return each
    /// peer with its attempt.
    pub(super) async fn close(
        &mut self,
        worker: &mut Worker,
        publish: Option<&mut Leg>,
        cause: Option<Reason>,
    ) -> Result<Vec<(u64, [u8; 16])>, Error> {
        self.retry = Instant::now() + COOLDOWN;
        let legs = std::mem::take(&mut self.legs);
        let connected = legs.iter().all(|attempt| attempt.transport.connected);
        self.closed = cause.map(|cause| if connected { cause } else { Reason::Failed });
        let retired = match publish.filter(|_| legs.iter().any(|leg| leg.transport.selected)) {
            Some(publish) => {
                // Switching retires the selected direct send legs in the worker.
                done(
                    worker,
                    Command::SetSendLegs {
                        legs: vec![publish.id],
                    },
                )
                .await?;
                publish.bandwidth = None;
                publish.wake = Some(Instant::now());
                true
            }
            None => false,
        };
        for attempt in legs
            .iter()
            .filter(|attempt| !(retired && attempt.transport.selected))
        {
            done(
                worker,
                Command::CloseLeg {
                    leg: attempt.transport.id,
                },
            )
            .await?;
        }
        Ok(legs
            .into_iter()
            .map(|attempt| (attempt.peer, attempt.id))
            .collect())
    }
}

/// The next datagram on an answered leg, with the leg's index.
pub(super) async fn receive(
    legs: &[Attempt],
    buffer: &mut [u8],
) -> (usize, std::io::Result<(usize, SocketAddr)>) {
    poll_fn(|context| {
        for (index, attempt) in legs.iter().enumerate() {
            if let (true, Network::Direct { socket, .. }) =
                (attempt.answered, &attempt.transport.network)
            {
                let mut read = ReadBuf::new(&mut *buffer);
                if let Poll::Ready(result) = socket.poll_recv_from(context, &mut read) {
                    let size = read.filled().len();
                    return Poll::Ready((index, result.map(|source| (size, source))));
                }
            }
        }
        Poll::Pending
    })
    .await
}

impl Attempt {
    fn connect(&mut self, candidates: Candidates) {
        if let Network::Direct { remote, .. } = &mut self.transport.network {
            *remote = Some(candidates);
        }
        self.transport.wake = Some(Instant::now());
        self.answered = true;
    }
}

pub(super) async fn send(
    connection: &mut Connection,
    peer: u64,
    publisher: bool,
    signal: &Signal,
) -> Result<(), Error> {
    let data = serde_json::to_vec(signal).map_err(|_| Error::Protocol("invalid direct signal"))?;
    connection
        .signal(Action::Seal {
            peer,
            publisher,
            data,
        })
        .await?;
    Ok(())
}

/// 80 % of a probed rate carries `desired`.
pub(super) fn sustains(rate: u64, desired: u64) -> bool {
    rate.saturating_mul(4) / 5 >= desired
}

/// Whether a fresh probe of the upload sustains the stream for every viewer;
/// `None` without a fresh probe.
pub(super) fn upload(sample: Option<(Instant, u64)>, desired: u64, viewers: usize) -> Option<bool> {
    sample
        .filter(|(observed, _)| observed.elapsed() <= SAMPLE_AGE)
        .map(|(_, rate)| sustains(rate, desired.saturating_mul(viewers as u64)))
}

async fn binding(
    socket: &UdpSocket,
    host: &str,
    port: u16,
    interface: IpAddr,
) -> Option<SocketAddr> {
    let server = lookup_host((host, port)).await.ok()?.find(|address| {
        address.is_ipv4() == interface.is_ipv4()
            && !address.ip().is_unspecified()
            && !address.ip().is_multicast()
    })?;
    let request = Message::builder_request(BINDING, MessageWriteVec::new()).finish();
    let id = Message::from_bytes(&request).ok()?.transaction_id();
    socket.send_to(&request, server).await.ok()?;
    let mut packet = [0; nuschl_ipc::rtc::MAX_DATAGRAM + 1];
    loop {
        let (size, source) = socket.recv_from(&mut packet).await.ok()?;
        if source != server || size == packet.len() {
            continue;
        }
        let Ok(reply) = Message::from_bytes(&packet[..size]) else {
            continue;
        };
        if reply.transaction_id() == id
            && reply.method() == BINDING
            && reply.class() == MessageClass::Success
        {
            return Some(reply.attribute::<XorMappedAddress>().ok()?.addr(id));
        }
    }
}

fn valid_sdp(interface: IpAddr, candidates: Candidates, sdp: &str) -> bool {
    if !candidates.valid(interface) {
        return false;
    }
    let mut host = false;
    let mut reflexive = false;
    for line in sdp
        .lines()
        .filter_map(|line| line.strip_prefix("a=candidate:"))
    {
        let mut fields = line.split_ascii_whitespace();
        if fields.nth(1) != Some("1")
            || !fields
                .next()
                .is_some_and(|transport| transport.eq_ignore_ascii_case("udp"))
        {
            return false;
        }
        let Some(ip) = fields.nth(1).and_then(|field| field.parse().ok()) else {
            return false;
        };
        let Some(port) = fields.next().and_then(|field| field.parse().ok()) else {
            return false;
        };
        if fields.next() != Some("typ") {
            return false;
        }
        let endpoint = SocketAddr::new(ip, port);
        match fields.next() {
            Some("host") if endpoint == candidates.host && !host => host = true,
            Some("srflx") if Some(endpoint) == candidates.reflexive && !reflexive => {
                reflexive = true
            }
            _ => return false,
        }
    }
    host && (candidates.reflexive.is_some() == reflexive)
}

fn allowed(interface: IpAddr, endpoint: SocketAddr) -> bool {
    if endpoint.port() < 1024
        || endpoint.ip().is_loopback() != interface.is_loopback()
        || endpoint.ip() == interface && !interface.is_loopback()
    {
        return false;
    }
    match (interface, endpoint) {
        (IpAddr::V4(_), SocketAddr::V4(remote)) => {
            let ip = remote.ip();
            !ip.is_unspecified() && !ip.is_broadcast() && !ip.is_multicast() && !ip.is_link_local()
        }
        (IpAddr::V6(_), SocketAddr::V6(remote)) => {
            let ip = remote.ip();
            !ip.is_unspecified()
                && !ip.is_multicast()
                && !ip.is_unicast_link_local()
                && ip.to_ipv4_mapped().is_none()
                && remote.scope_id() == 0
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use turn_client_proto::stun::types::message::MessageWriteExt;

    #[test]
    fn a_publisher_sends_one_way_to_at_most_two_viewers() {
        let screen = |publisher| Track {
            mid: format!("{publisher}"),
            purpose: Purpose::ScreenVideo,
            publisher: Some(publisher),
        };
        assert_eq!(role(&[2], true, &[]), Ok(Role::Send(vec![2])));
        assert_eq!(role(&[2, 3], true, &[]), Ok(Role::Send(vec![2, 3])));
        assert_eq!(role(&[2, 3, 4], true, &[]), Err(Reason::Viewers));
        assert_eq!(role(&[], true, &[]), Err(Reason::Viewer));
        assert_eq!(role(&[2], true, &[screen(2)]), Err(Reason::TwoWay));
        assert_eq!(role(&[2, 3], false, &[screen(2)]), Ok(Role::Receive(2)));
        assert_eq!(
            role(&[2, 3], false, &[screen(2), screen(3)]),
            Err(Reason::Senders)
        );
        assert_eq!(role(&[2], false, &[]), Err(Reason::Sender));
    }

    #[test]
    fn the_route_names_why_video_stays_on_the_sfu() {
        let mut direct = Direct::new("127.0.0.1".parse().unwrap(), &[]);
        let send = Ok(Role::Send(vec![2, 3]));
        let receive = Ok(Role::Receive(2));
        let sfu = |reason| Route::Sfu(reason);
        let fits = Some(true);
        assert_eq!(
            direct.route(&Err(Reason::TwoWay), fits),
            sfu(Reason::TwoWay)
        );
        direct.ready.push(2);
        assert_eq!(direct.route(&send, fits), sfu(Reason::Viewer));
        direct.ready.push(3);
        assert_eq!(direct.route(&send, None), sfu(Reason::Measuring));
        assert_eq!(direct.route(&send, Some(false)), sfu(Reason::Upload));
        assert_eq!(direct.route(&send, fits), sfu(Reason::Connecting));
        direct.closed = Some(Reason::Failed);
        assert_eq!(direct.route(&send, fits), sfu(Reason::Failed));
        assert_eq!(direct.route(&receive, None), sfu(Reason::Failed));
        direct.closed = Some(Reason::Upload);
        assert_eq!(direct.route(&send, fits), sfu(Reason::Upload));
        direct.closed = None;
        assert_eq!(direct.route(&send, fits), sfu(Reason::Connecting));
        assert_eq!(direct.route(&receive, None), sfu(Reason::Sender));
    }

    #[test]
    fn eighty_percent_of_a_fresh_upload_probe_covers_every_viewer() {
        let now = Instant::now();
        let screen = 8_000_000;
        assert_eq!(upload(Some((now, 10_000_000)), screen, 1), Some(true));
        assert_eq!(upload(Some((now, 9_999_999)), screen, 1), Some(false));
        assert_eq!(upload(Some((now, 10_000_000)), screen, 2), Some(false));
        assert_eq!(upload(Some((now, 20_000_000)), screen, 2), Some(true));
        assert_eq!(upload(None, screen, 1), None);
        let stale = now.checked_sub(SAMPLE_AGE + Duration::from_millis(1));
        assert!(stale.is_some_and(|at| upload(Some((at, 20_000_000)), screen, 1).is_none()));
        assert!(!sustains(u64::MAX, u64::MAX));
        assert!(!sustains(9_999_999, screen));
    }

    #[tokio::test]
    async fn binding_observes_the_mapping_on_the_same_socket() {
        let host = "127.0.0.1";
        let socket = UdpSocket::bind((host, 0)).await.unwrap();
        let server = UdpSocket::bind((host, 0)).await.unwrap();
        let mapped = "127.0.0.2:49000".parse().unwrap();
        let request = binding(
            &socket,
            host,
            server.local_addr().unwrap().port(),
            host.parse().unwrap(),
        );
        let respond = async {
            let mut data = [0; 256];
            let (size, source) = server.recv_from(&mut data).await.unwrap();
            assert_eq!(source, socket.local_addr().unwrap());
            let incoming = Message::from_bytes(&data[..size]).unwrap();
            assert_eq!(incoming.method(), BINDING);
            let mut response = Message::builder_success(&incoming, MessageWriteVec::new());
            response
                .add_attribute(&XorMappedAddress::new(mapped, incoming.transaction_id()))
                .unwrap();
            server.send_to(&response.finish(), source).await.unwrap();
        };
        let (address, ()) = timeout(DISCOVERY, async { tokio::join!(request, respond) })
            .await
            .unwrap();
        assert_eq!(address, Some(mapped));
    }
}
