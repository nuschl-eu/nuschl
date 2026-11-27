// SPDX-License-Identifier: EUPL-1.2
//! Plugin data (Mumble's `PluginDataTransmission`; what servers forward and
//! the wire rules are in `nuschl_protocol::plugin`): opaque bytes between
//! clients for an embedder's own features. The client never interprets,
//! executes or decrypts them, and the reserved `nuschl/` data IDs are neither
//! sent nor delivered here; call cards (`super::call`) are the one reserved
//! ID the client sends and reads itself.
//!
//! Connections decode plugin frames for call cards. Delivering other plugin
//! data is off until
//! [`Client::accept_plugin_data`]. What arrives waits in one queue
//! for all servers, at most [`MAX_PLUGIN_QUEUE`] items and
//! [`MAX_PLUGIN_QUEUE_BYTES`]; when it is full, new items are dropped and
//! counted ([`Client::plugin_data_dropped`]). Each item names the connection
//! generation it came from and its sender as the session knew it when the
//! packet arrived (session, presence instance, registration, certificate
//! hash), never as whoever holds that session number later. When a
//! connection ends, reconnects or is replaced, its queued items are
//! discarded, and nothing is handed out for a connection that is not current.
//! The certificate hash is the server's claim: on an honest server it shows
//! that the same key is back, not who that is, and it authorises nothing.
//!
//! Upstream of the queue a packet passes bounded buffers: up to 32 frames
//! read ahead of the session (plugin frames there at most 4,951 bytes; larger
//! ones are read past and dropped), a per-connection receive budget (64
//! items and 64 KiB a second), the session's event channel (128) and the
//! driver's (512). Plugin data never waits for room in those channels; it is
//! dropped instead. With delivery off only call cards are retained.
//!
//! Sending ([`Client::send_plugin_data`]) names each receiver by session and
//! presence instance. The connection checks them again right before the
//! message leaves, so a send waiting in a queue is refused rather than
//! reaching someone who took over a departed receiver's session number. Its
//! outcome comes back as [`PluginDataEvent::Admission`]: admitted means
//! handed to the connection within the local rate (two a second, a burst of
//! ten); the server may still drop it, and nothing confirms delivery.
use super::Client;
use crate::{input::ServerEvent, platform::Request, server::Status};
use nuschl_protocol::{
    nuschl::capability,
    plugin::{self, Payload, Refusal},
};
use std::collections::{BTreeSet, VecDeque};

/// Receivers one send may name (the session's `MAX_ACTION_TARGETS`).
pub const MAX_PLUGIN_RECEIVERS: usize = 128;
/// The queue of received plugin data and admissions, for all servers.
pub const MAX_PLUGIN_QUEUE: usize = 256;
pub const MAX_PLUGIN_QUEUE_BYTES: usize = 256 * 1024;

/// Plugin data another client sent us, with its sender as the session knew
/// it when the packet arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceivedPluginData {
    pub sender: u32,
    /// The sender's presence: reply with it, never with the session number alone.
    pub sender_instance: u64,
    pub sender_user_id: Option<u32>,
    /// What the server said; key continuity on an honest server, not identity.
    pub sender_certificate_hash: Option<String>,
    pub payload: Payload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginDataEvent {
    Received {
        server: String,
        generation: u64,
        data: ReceivedPluginData,
    },
    /// Our send `request` was admitted (handed to the connection, not
    /// delivered) or refused.
    Admission {
        server: String,
        generation: u64,
        request: u64,
        result: Result<(), Refusal>,
    },
}

impl PluginDataEvent {
    fn origin(&self) -> (&str, u64) {
        match self {
            Self::Received {
                server, generation, ..
            }
            | Self::Admission {
                server, generation, ..
            } => (server, *generation),
        }
    }
    /// What the item holds, for the queue's byte budget.
    fn cost(&self) -> usize {
        64 + match self {
            Self::Received { server, data, .. } => {
                server.len()
                    + data.payload.size()
                    + data.sender_certificate_hash.as_ref().map_or(0, String::len)
            }
            Self::Admission { server, .. } => server.len(),
        }
    }
}

/// Kit presses whose admission has not arrived; a press is one small frame.
const MAX_KIT_REQUESTS: usize = 64;

#[derive(Default)]
pub(super) struct PluginQueue {
    accept: bool,
    items: VecDeque<PluginDataEvent>,
    bytes: usize,
    dropped: u64,
    /// Requests of our own kit presses, whose admissions belong to no caller of
    /// the generic interface.
    kit_requests: BTreeSet<u64>,
}

impl PluginQueue {
    /// Remembers a kit press until its admission arrives, so the admission is
    /// dropped rather than queued for the generic interface.
    pub(super) fn kit_request(&mut self, request: u64) {
        self.kit_requests.insert(request);
        while self.kit_requests.len() > MAX_KIT_REQUESTS {
            self.kit_requests.pop_first();
        }
    }

    /// Takes a kit press's admission, if this request is one.
    fn kit_admission(&mut self, request: u64) -> bool {
        self.kit_requests.remove(&request)
    }

    fn push(&mut self, item: PluginDataEvent) {
        let cost = item.cost();
        if self.items.len() >= MAX_PLUGIN_QUEUE || self.bytes + cost > MAX_PLUGIN_QUEUE_BYTES {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        self.bytes += cost;
        self.items.push_back(item);
    }
    fn retain(&mut self, keep: impl Fn(&PluginDataEvent) -> bool) {
        self.items.retain(|item| keep(item));
        self.bytes = self.items.iter().map(PluginDataEvent::cost).sum();
    }
}

impl Client {
    /// Deliver plugin data from every connection (off by default). Turning it
    /// off discards what is queued.
    pub fn accept_plugin_data(&mut self, accept: bool) {
        self.plugin_data.accept = accept;
        if !accept {
            self.plugin_data
                .retain(|item| matches!(item, PluginDataEvent::Admission { .. }));
        }
    }

    /// Send plugin data on server `index` to these receivers, each named by
    /// session and presence instance (as a [`User`](crate::model::User) or a
    /// [`ReceivedPluginData`] names them). Returns the request its
    /// [`PluginDataEvent::Admission`] answers; refused at once when it cannot
    /// be sent as asked.
    pub fn send_plugin_data(
        &mut self,
        index: usize,
        receivers: &[(u32, u64)],
        data_id: &str,
        data: &[u8],
    ) -> Result<u64, Refusal> {
        if plugin::is_reserved(data_id) {
            return Err(Refusal::Reserved);
        }
        let payload = Payload::new(data_id.to_owned(), data.to_vec()).map_err(Refusal::Payload)?;
        self.send_payload(index, receivers, payload)
    }

    /// [`Self::send_plugin_data`] without the reserved namespace's check, for
    /// Nuschl's own features.
    pub(super) fn send_payload(
        &mut self,
        index: usize,
        receivers: &[(u32, u64)],
        payload: Payload,
    ) -> Result<u64, Refusal> {
        let server = self.servers.get(index).ok_or(Refusal::NotConnected)?;
        if server.status != Status::Connected {
            return Err(Refusal::NotConnected);
        }
        if server.has(capability::E2EE) {
            return Err(Refusal::EndToEnd);
        }
        if receivers.is_empty() || receivers.len() > MAX_PLUGIN_RECEIVERS {
            return Err(Refusal::Receivers(MAX_PLUGIN_RECEIVERS));
        }
        let own = server
            .own_session
            .and_then(|session| server.users.get(&session))
            .filter(|own| own.instance != 0)
            .ok_or(Refusal::NotConnected)?;
        let mut users = receivers.to_vec();
        users.sort_unstable();
        users.dedup();
        if users.iter().any(|&(session, instance)| {
            instance == 0
                || server
                    .users
                    .get(&session)
                    .is_none_or(|user| user.instance != instance)
        }) {
            return Err(Refusal::ReceiverChanged);
        }
        let request = super::request_id();
        let authority = nuschl_core::model::MetadataAuthority {
            generation: server.generation,
            own: (own.session, own.instance),
            users: users.clone(),
            channels: Vec::new(),
        };
        let id = server.id.clone();
        self.platform
            .request_metadata(
                &id,
                authority,
                Request::PluginData {
                    request,
                    receivers: users,
                    payload,
                },
            )
            .map_err(Refusal::Unavailable)?;
        Ok(request)
    }

    /// What arrived since the last call, oldest first: only items of
    /// connections that are still current. Received data also requires a
    /// connected server with our current presence; admissions remain visible
    /// for the current generation, including a refusal while connecting.
    pub fn take_plugin_data(&mut self) -> Vec<PluginDataEvent> {
        let items = std::mem::take(&mut self.plugin_data.items);
        self.plugin_data.bytes = 0;
        items
            .into_iter()
            .filter(|item| {
                let (server, generation) = item.origin();
                self.current(server, generation).is_some_and(|index| {
                    let server = &self.servers[index];
                    !matches!(item, PluginDataEvent::Received { .. })
                        || (server.status == Status::Connected
                            && server
                                .own_session
                                .and_then(|session| server.users.get(&session))
                                .is_some_and(|own| own.instance != 0))
                })
            })
            .collect()
    }

    /// Items dropped because the queue was full, since the client started.
    pub fn plugin_data_dropped(&self) -> u64 {
        self.plugin_data.dropped
    }

    /// A plugin event of server `index`'s current connection.
    pub(super) fn plugin_event(&mut self, index: usize, event: ServerEvent) {
        let server = &self.servers[index];
        let (id, generation) = (server.id.clone(), server.generation);
        let connected = server.status == Status::Connected
            && server
                .own_session
                .and_then(|session| server.users.get(&session))
                .is_some_and(|own| own.instance != 0);
        match event {
            ServerEvent::PluginData(data)
                if connected && data.payload.data_id() == nuschl_core::integration::KIT_DATA_ID =>
            {
                self.kit_surface(index, &data);
            }
            ServerEvent::PluginData(data)
                if connected && data.payload.data_id() == nuschl_core::call::CARD_DATA_ID =>
            {
                self.call_card(index, &data);
            }
            ServerEvent::PluginData(data)
                if connected
                    && data.payload.data_id() == nuschl_core::integration::KIT_LINK_DATA_ID =>
            {
                self.kit_link(index, &data);
            }
            ServerEvent::PluginDataAdmission { request, result } if self.call_request(request) => {
                if result.is_err() {
                    self.call_refused(request);
                }
            }
            ServerEvent::PluginData(data)
                if connected && self.plugin_data.accept && !data.payload.is_reserved() =>
            {
                self.plugin_data.push(PluginDataEvent::Received {
                    server: id,
                    generation,
                    data,
                });
            }
            ServerEvent::PluginDataAdmission { request, .. }
                if self.plugin_data.kit_admission(request) => {}
            ServerEvent::PluginDataAdmission { request, result } => {
                self.plugin_data.push(PluginDataEvent::Admission {
                    server: id,
                    generation,
                    request,
                    result,
                });
            }
            _ => {}
        }
    }

    /// The connection of `server` ended, reconnects or was replaced: what it
    /// left in the queue is obsolete.
    pub(super) fn forget_plugin_data(&mut self, server: &str) {
        self.plugin_data.retain(|item| item.origin().0 != server);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HistoryTicket, input::Input, model::User, platform::Platform, timeline::Page};
    use nuschl_core::{audio::Sound, bookmark::ServerBookmark, model::MetadataAuthority};
    use nuschl_core::{
        call::{CARD_DATA_ID, Card},
        model::TextTarget,
    };
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Recorder {
        sent: Mutex<Vec<(String, MetadataAuthority, Request)>>,
        accept: Mutex<Vec<bool>>,
        generation: Mutex<u64>,
    }
    impl Platform for Recorder {
        fn connect(&self, _: ServerBookmark, _: Option<String>, _: bool) -> Result<u64, String> {
            let mut generation = self.generation.lock().unwrap();
            *generation += 1;
            Ok(*generation)
        }
        fn disconnect(&self, _: &str) {}
        fn request(&self, _: &str, _: Request) -> Result<(), String> {
            Err("plugin data needs authority".into())
        }
        fn request_metadata(
            &self,
            server: &str,
            authority: MetadataAuthority,
            request: Request,
        ) -> Result<(), String> {
            self.sent
                .lock()
                .unwrap()
                .push((server.into(), authority, request));
            Ok(())
        }
        fn local_history(
            &self,
            _: String,
            _: String,
            _: String,
            _: HistoryTicket,
            _: Page,
        ) -> Result<(), String> {
            Ok(())
        }
        fn server_history(
            &self,
            _: String,
            _: [u8; 16],
            _: HistoryTicket,
            _: Option<crate::platform::CacheKey>,
            _: Page,
        ) -> Result<(), String> {
            Ok(())
        }
        fn change_message(&self, _: crate::platform::ChangeRequest) -> Result<(), String> {
            Ok(())
        }
        fn render_html(&self, _: String, _: u64, _: crate::platform::RenderKey, _: String) {}
        fn decode_avatar(&self, _: String, _: u64, _: String, _: Vec<u8>) {}
        fn play_sound(&self, _: &str, _: Sound) {}
        fn chime(&self, _: Sound) {}
        fn search(
            &self,
            _: u64,
            _: (Vec<String>, Option<String>),
            _: String,
            _: Option<nuschl_core::SearchCursor>,
        ) -> Result<(), String> {
            Ok(())
        }
        fn accept_plugin_data(&self, accept: bool) {
            self.accept.lock().unwrap().push(accept);
        }
    }

    fn user(session: u32, instance: u64) -> User {
        User {
            session,
            instance,
            name: format!("User {session}"),
            ..User::default()
        }
    }

    fn server(name: &str) -> ServerBookmark {
        ServerBookmark::new(name, "localhost", 64738, "tester").unwrap()
    }

    /// A client connected to `a` and `b` as session 1 with peers 2 and 3.
    fn connected() -> (Client, Arc<Recorder>) {
        let recorder = Arc::new(Recorder::default());
        let mut client = Client::new(
            Box::new(recorder.clone()),
            vec![server("a"), server("b")],
            Default::default(),
        );
        for index in 0..2 {
            client.connect(index, None, false);
            for event in [
                ServerEvent::User(user(1, 11 + index as u64 * 100)),
                ServerEvent::User(user(2, 22 + index as u64 * 100)),
                ServerEvent::User(user(3, 33 + index as u64 * 100)),
                ServerEvent::Connected { session: 1 },
            ] {
                client.handle(input(&client, index, event));
            }
        }
        (client, recorder)
    }

    fn input(client: &Client, index: usize, event: ServerEvent) -> Input {
        Input::Server {
            server: client.servers[index].id.clone(),
            generation: client.servers[index].generation,
            event,
        }
    }

    fn data(sender: u32, instance: u64, bytes: &[u8]) -> ServerEvent {
        ServerEvent::PluginData(ReceivedPluginData {
            sender,
            sender_instance: instance,
            sender_user_id: None,
            sender_certificate_hash: Some("cafe".into()),
            payload: Payload::new("game".into(), bytes.to_vec()).unwrap(),
        })
    }

    #[test]
    fn sends_name_presences_and_are_refused_when_they_cannot_be_kept() {
        let (mut client, recorder) = connected();
        let request = client
            .send_plugin_data(0, &[(3, 33), (2, 22), (3, 33)], "game", b"\x01")
            .unwrap();
        let sent = recorder.sent.lock().unwrap().pop().unwrap();
        let (
            server,
            authority,
            Request::PluginData {
                request: sent_request,
                receivers,
                payload,
            },
        ) = sent
        else {
            panic!("a plugin data request");
        };
        assert_eq!((&server, sent_request), (&client.servers[0].id, request));
        assert_eq!(receivers, [(2, 22), (3, 33)]);
        assert_eq!(authority.users, [(2, 22), (3, 33)]);
        assert_eq!(authority.own, (1, 11));
        assert_eq!(authority.generation, client.servers[0].generation);
        assert_eq!(payload.data(), b"\x01");
        // A presence that is not (or no longer) the session's occupant.
        assert_eq!(
            client.send_plugin_data(0, &[(2, 122)], "game", b""),
            Err(Refusal::ReceiverChanged)
        );
        client.handle(input(&client, 0, ServerEvent::User(user(2, 23))));
        assert_eq!(
            client.send_plugin_data(0, &[(2, 22)], "game", b""),
            Err(Refusal::ReceiverChanged)
        );
        let many: Vec<(u32, u64)> = (0..=128).map(|session| (session, 1)).collect();
        for (receivers, data_id, data, refusal) in [
            (&[][..], "game", &b""[..], Refusal::Receivers(128)),
            (&many[..], "game", &b""[..], Refusal::Receivers(128)),
            (&[(2, 23)][..], "nuschl/rtc", &b""[..], Refusal::Reserved),
            (
                &[(2, 23)][..],
                "game",
                &[0; 1001][..],
                Refusal::Payload(plugin::Error::DataTooLarge),
            ),
        ] {
            assert_eq!(
                client.send_plugin_data(0, receivers, data_id, data),
                Err(refusal)
            );
        }
        client.disconnect(1);
        assert_eq!(
            client.send_plugin_data(1, &[(2, 122)], "game", b""),
            Err(Refusal::NotConnected)
        );
        assert_eq!(recorder.sent.lock().unwrap().len(), 0);
    }

    #[test]
    fn payload_byte_limits_refuse_without_sending_or_changing_the_queue() {
        let (mut client, recorder) = connected();
        let exact_id = "é".repeat(50);
        let long_ascii_id = "i".repeat(101);
        let long_utf8_id = "é".repeat(51);
        let before = (
            client.servers[0].status,
            client.servers[0].generation,
            client.servers[0].own_session,
            client.servers[0].users.len(),
            client.updates.len(),
        );
        for (id, bytes, error) in [
            ("game", &[0; 1001][..], plugin::Error::DataTooLarge),
            (
                long_ascii_id.as_str(),
                &b""[..],
                plugin::Error::DataIdTooLong,
            ),
            (
                long_utf8_id.as_str(),
                &b""[..],
                plugin::Error::DataIdTooLong,
            ),
        ] {
            assert_eq!(
                client.send_plugin_data(0, &[(2, 22)], id, bytes),
                Err(Refusal::Payload(error))
            );
            assert!(recorder.sent.lock().unwrap().is_empty());
            assert!(client.plugin_data.items.is_empty());
            assert_eq!(client.plugin_data.bytes, 0);
            assert_eq!(client.plugin_data.dropped, 0);
            assert_eq!(
                (
                    client.servers[0].status,
                    client.servers[0].generation,
                    client.servers[0].own_session,
                    client.servers[0].users.len(),
                    client.updates.len(),
                ),
                before
            );
        }
        client
            .send_plugin_data(0, &[(2, 22)], &exact_id, &[0; 1000])
            .unwrap();
        let sent = recorder.sent.lock().unwrap();
        let [(_, _, Request::PluginData { payload, .. })] = sent.as_slice() else {
            panic!("one send at the exact byte limits");
        };
        assert_eq!(payload.data_id(), exact_id);
        assert_eq!(payload.data().len(), 1000);
    }

    #[test]
    fn late_data_is_refused_after_disconnect_and_while_reconnecting() {
        let (mut client, _) = connected();
        client.accept_plugin_data(true);
        let generation = client.servers[0].generation;
        let late = input(&client, 0, data(2, 22, b"late"));
        client.handle(input(&client, 0, data(2, 22, b"queued")));
        client.disconnect(0);
        assert_eq!(client.servers[0].generation, generation);
        client.handle(late);
        assert!(client.take_plugin_data().is_empty());

        let generation = client.servers[1].generation;
        let late = input(&client, 1, data(2, 122, b"late"));
        client.handle(input(&client, 1, data(2, 122, b"queued")));
        client.handle(Input::Reconnecting {
            server: client.servers[1].id.clone(),
            generation,
            attempt: 1,
            delay_secs: 1,
            reason: "test connection loss".into(),
        });
        assert_eq!(client.servers[1].generation, generation);
        client.handle(late);
        assert!(client.take_plugin_data().is_empty());
        // A connected status without our own known presence cannot accept
        // data, either before it is queued or when it is taken.
        client.servers[1].status = Status::Connected;
        client.handle(input(&client, 1, data(2, 122, b"no own session")));
        assert!(client.take_plugin_data().is_empty());
    }

    #[test]
    fn queued_data_requires_our_presence_when_taken() {
        let (mut client, _) = connected();
        client.accept_plugin_data(true);
        for unavailable in 0..4 {
            client.handle(input(&client, 0, data(2, 22, b"queued")));
            let own = client.servers[0].users.get(&1).unwrap().clone();
            match unavailable {
                0 => client.servers[0].status = Status::Disconnected,
                1 => client.servers[0].own_session = None,
                2 => {
                    client.servers[0].users.remove(&1);
                }
                _ => client.servers[0].users.get_mut(&1).unwrap().instance = 0,
            }
            assert!(client.take_plugin_data().is_empty());
            client.handle(input(&client, 0, data(2, 22, b"late")));
            assert!(client.take_plugin_data().is_empty());
            client.servers[0].status = Status::Connected;
            client.servers[0].own_session = Some(1);
            client.servers[0].users.insert(1, own);
        }
        // Admissions are connection-generation results, not incoming data.
        client.disconnect(0);
        client.handle(input(
            &client,
            0,
            ServerEvent::PluginDataAdmission {
                request: 8,
                result: Err(Refusal::NotConnected),
            },
        ));
        assert!(matches!(
            client.take_plugin_data().as_slice(),
            [PluginDataEvent::Admission { request: 8, .. }]
        ));
    }

    #[test]
    fn received_data_is_opt_in_bound_to_its_connection_and_bounded() {
        let (mut client, _) = connected();
        client.handle(input(&client, 0, data(2, 22, b"early")));
        assert!(client.take_plugin_data().is_empty(), "off by default");
        client.accept_plugin_data(true);
        client.handle(input(&client, 0, data(2, 22, b"one")));
        client.handle(input(&client, 1, data(3, 133, b"two")));
        let items = client.take_plugin_data();
        assert_eq!(items.len(), 2);
        let PluginDataEvent::Received {
            server,
            data: first,
            ..
        } = &items[0]
        else {
            panic!("received data");
        };
        assert_eq!(
            (server, first.sender, first.sender_instance),
            (&client.servers[0].id, 2, 22)
        );
        // Session 2 is taken over by someone else: a queued item keeps its
        // first occupant, and replying to that presence is refused.
        client.handle(input(&client, 0, data(2, 22, b"queued")));
        client.handle(input(&client, 0, ServerEvent::User(user(2, 24))));
        let items = client.take_plugin_data();
        let [PluginDataEvent::Received { data: queued, .. }] = items.as_slice() else {
            panic!("{items:?}");
        };
        assert_eq!(queued.sender_instance, 22);
        assert_eq!(
            client.send_plugin_data(0, &[(queued.sender, queued.sender_instance)], "game", b""),
            Err(Refusal::ReceiverChanged)
        );
        // A reconnect discards what the old connection left, and late events
        // of the old generation never arrive.
        client.handle(input(&client, 0, data(2, 24, b"before reconnect")));
        let old = input(&client, 0, data(2, 24, b"late"));
        client.connect(0, None, false);
        client.handle(old);
        assert!(client.take_plugin_data().is_empty());
        // Admissions of the current connection always arrive; the reserved
        // namespace never does.
        client.handle(input(
            &client,
            1,
            ServerEvent::PluginDataAdmission {
                request: 7,
                result: Err(Refusal::RateLimited {
                    retry_after_ms: 250,
                }),
            },
        ));
        client.handle(input(
            &client,
            1,
            ServerEvent::PluginData(ReceivedPluginData {
                payload: Payload::new("nuschl/x".into(), vec![]).unwrap(),
                ..match data(3, 133, b"") {
                    ServerEvent::PluginData(data) => data,
                    _ => unreachable!(),
                }
            }),
        ));
        assert_eq!(
            client.take_plugin_data(),
            [PluginDataEvent::Admission {
                server: client.servers[1].id.clone(),
                generation: client.servers[1].generation,
                request: 7,
                result: Err(Refusal::RateLimited {
                    retry_after_ms: 250
                }),
            }]
        );
        // One queue for every server: items and bytes are bounded together.
        // Use connected servers: the reconnect above left server A connecting.
        let (mut client, recorder) = connected();
        client.accept_plugin_data(true);
        for index in 0..MAX_PLUGIN_QUEUE + 10 {
            client.handle(input(
                &client,
                index % 2,
                data(3, 33 + (index % 2) as u64 * 100, b""),
            ));
        }
        assert_eq!(client.plugin_data_dropped(), 10);
        let big = [0; plugin::MAX_DATA_BYTES];
        assert_eq!(client.take_plugin_data().len(), MAX_PLUGIN_QUEUE);
        for _ in 0..MAX_PLUGIN_QUEUE {
            client.handle(input(&client, 1, data(3, 133, &big)));
        }
        let kept = client.take_plugin_data().len();
        assert!(
            kept < MAX_PLUGIN_QUEUE && kept * 1000 <= MAX_PLUGIN_QUEUE_BYTES,
            "{kept}"
        );
        // Turning it off drops what waits and tells every connection.
        client.handle(input(&client, 1, data(3, 133, b"waiting")));
        client.accept_plugin_data(false);
        assert!(client.take_plugin_data().is_empty());
        // Connections keep decoding plugin data, for call cards.
        assert_eq!(*recorder.accept.lock().unwrap(), [true]);
    }

    /// A Join link.
    fn link(seed: char) -> String {
        let value = seed.to_string().repeat(43);
        format!("https://stream.example/call#v=1&k={value}&h={value}")
    }

    /// A sent call card: its request, receivers and card.
    type Sent = (u64, Vec<(u32, u64)>, Card);

    /// The call cards sent since the last call.
    fn cards(recorder: &Recorder) -> Vec<Sent> {
        recorder
            .sent
            .lock()
            .unwrap()
            .drain(..)
            .filter_map(|(_, _, request)| match request {
                Request::PluginData {
                    request,
                    receivers,
                    payload,
                } if payload.data_id() == CARD_DATA_ID => {
                    Some((request, receivers, Card::decode(payload.data()).unwrap()))
                }
                _ => None,
            })
            .collect()
    }

    /// The one call card sent since the last call.
    fn one(recorder: &Recorder) -> Sent {
        let mut sent = cards(recorder);
        assert_eq!(sent.len(), 1, "{sent:?}");
        sent.remove(0)
    }

    fn card(sender: u32, instance: u64, card: &Card) -> ServerEvent {
        ServerEvent::PluginData(ReceivedPluginData {
            payload: Payload::new(CARD_DATA_ID.into(), card.encode()).unwrap(),
            ..match data(sender, instance, b"") {
                ServerEvent::PluginData(data) => data,
                _ => unreachable!(),
            }
        })
    }

    #[test]
    fn our_call_card_reaches_the_channel_and_whoever_enters_it() {
        let (mut client, recorder) = connected();
        client
            .start_call(0, TextTarget::Channel(0), link('a'))
            .unwrap();
        let active = Card::Active(link('a'));
        let (_, receivers, sent) = one(&recorder);
        assert_eq!((receivers, &sent), (vec![(2, 22), (3, 33)], &active));
        // Someone enters: only they get it. A refused send is sent again.
        client.handle(input(&client, 0, ServerEvent::User(user(4, 44))));
        client.settle();
        let (request, receivers, _) = one(&recorder);
        assert_eq!(receivers, [(4, 44)]);
        let refused = ServerEvent::PluginDataAdmission {
            request,
            result: Err(Refusal::RateLimited { retry_after_ms: 9 }),
        };
        client.handle(input(&client, 0, refused));
        assert!(client.take_plugin_data().is_empty(), "our own admission");
        client.settle();
        assert_eq!(cards(&recorder)[0].1, [(4, 44)]);
        client.settle();
        assert!(cards(&recorder).is_empty());
        // A new link reaches everyone; ending tells everyone.
        client.rotate_call(link('b'));
        let (_, receivers, Card::Active(rotated)) = one(&recorder) else {
            panic!("a new card");
        };
        assert_eq!(receivers, [(2, 22), (3, 33), (4, 44)]);
        assert_eq!(rotated, link('b'));
        client.end_call();
        let (_, receivers, Card::Ended) = one(&recorder) else {
            panic!("an ended card");
        };
        assert_eq!(receivers, [(2, 22), (3, 33), (4, 44)]);
        assert!(client.own_call().is_none());
        // Leaving the channel ends the call.
        client
            .start_call(0, TextTarget::Channel(0), link('c'))
            .unwrap();
        let moved = User {
            channel: 5,
            ..user(1, 11)
        };
        client.handle(input(&client, 0, ServerEvent::User(moved)));
        client.settle();
        assert!(client.own_call().is_none());
        assert!(
            client
                .start_call(0, TextTarget::Channel(0), link('c'))
                .is_err()
        );
        assert!(
            client
                .start_call(0, TextTarget::Channel(5), link('c'))
                .is_ok()
        );
    }

    #[test]
    fn received_calls_follow_their_senders_presence_and_channel() {
        let (mut client, _) = connected();
        let link = link('a');
        let active = Card::Active(link.clone());
        // Delivered without opting in to generic plugin data.
        client.handle(input(&client, 0, card(2, 22, &active)));
        let calls = client.calls_in(0, &TextTarget::Channel(0));
        assert_eq!(calls.len(), 1);
        assert_eq!((calls[0].sender, &calls[0].link), (2, &link));
        assert!(client.take_plugin_data().is_empty());
        client.handle(input(&client, 0, card(2, 22, &Card::Ended)));
        assert!(client.calls_in(0, &TextTarget::Channel(0)).is_empty());
        // A stale presence, garbage and our own card are ignored.
        client.handle(input(&client, 0, card(3, 34, &active)));
        client.handle(input(&client, 0, card(1, 11, &active)));
        let mut garbage = card(3, 33, &Card::Ended);
        if let ServerEvent::PluginData(data) = &mut garbage {
            data.payload = Payload::new(CARD_DATA_ID.into(), vec![1, 1, b'x']).unwrap();
        }
        client.handle(input(&client, 0, garbage));
        assert!(client.calls_in(0, &TextTarget::Channel(0)).is_empty());
        // The card stays with the channel its sender announced it in.
        client.handle(input(&client, 0, card(3, 33, &active)));
        assert_eq!(client.calls_in(0, &TextTarget::Channel(0)).len(), 1);
        let moved = User {
            channel: 7,
            ..user(3, 33)
        };
        client.handle(input(&client, 0, ServerEvent::User(moved)));
        assert!(
            client.calls_in(0, &TextTarget::Channel(0)).is_empty()
                && client.calls_in(0, &TextTarget::Channel(7)).is_empty()
        );
        client.handle(input(&client, 0, card(3, 33, &active)));
        assert_eq!(client.calls_in(0, &TextTarget::Channel(7)).len(), 1);
        client.connect(0, None, false);
        assert!(client.servers[0].calls.is_empty());
    }

    #[test]
    fn a_direct_call_card_reaches_only_the_other_person() {
        let (mut client, recorder) = connected();
        assert!(
            client
                .start_call(0, TextTarget::Direct(1), link('d'))
                .is_err()
        );
        assert!(
            client
                .start_call(0, TextTarget::Direct(9), link('d'))
                .is_err()
        );
        assert!(
            client
                .start_call(0, TextTarget::Channel(5), link('d'))
                .is_err()
        );
        client
            .start_call(0, TextTarget::Direct(2), link('d'))
            .unwrap();
        let (_, receivers, sent) = one(&recorder);
        assert_eq!((receivers, sent), (vec![(2, 22)], Card::Direct(link('d'))));
        // Someone entering the channel, or our own move, leaves it alone.
        client.handle(input(&client, 0, ServerEvent::User(user(4, 44))));
        let moved = User {
            channel: 5,
            ..user(1, 11)
        };
        client.handle(input(&client, 0, ServerEvent::User(moved)));
        client.settle();
        assert!(cards(&recorder).is_empty());
        assert!(client.own_call().is_some());
        client.end_call();
        let (_, receivers, Card::Ended) = one(&recorder) else {
            panic!("an ended card");
        };
        assert_eq!(receivers, [(2, 22)]);
        // The other person leaving ends it.
        client
            .start_call(0, TextTarget::Direct(3), link('e'))
            .unwrap();
        one(&recorder);
        client.handle(input(&client, 0, ServerEvent::User(user(3, 34))));
        client.settle();
        assert!(client.own_call().is_none());
    }

    fn surface(
        sender: u32,
        instance: u64,
        surface: &nuschl_core::integration::Surface,
    ) -> ServerEvent {
        ServerEvent::PluginData(ReceivedPluginData {
            payload: Payload::new(
                nuschl_core::integration::KIT_DATA_ID.into(),
                surface.encode().unwrap(),
            )
            .unwrap(),
            ..match data(sender, instance, b"") {
                ServerEvent::PluginData(data) => data,
                _ => unreachable!(),
            }
        })
    }

    fn shown(id: u32, revision: u64, text: &str) -> nuschl_core::integration::Surface {
        nuschl_core::integration::Surface {
            id,
            revision,
            nodes: vec![nuschl_core::integration::Node::Text {
                style: nuschl_core::integration::Style::Title,
                text: text.into(),
            }],
        }
    }

    fn kit_buttons(
        sender: u32,
        instance: u64,
        buttons: Vec<nuschl_core::integration::Button>,
    ) -> ServerEvent {
        surface(
            sender,
            instance,
            &nuschl_core::integration::Surface {
                id: 1,
                revision: 1,
                nodes: vec![nuschl_core::integration::Node::Buttons(buttons)],
            },
        )
    }

    fn kit_link(sender: u32, instance: u64, url: &str) -> ServerEvent {
        ServerEvent::PluginData(ReceivedPluginData {
            payload: Payload::new(
                nuschl_core::integration::KIT_LINK_DATA_ID.into(),
                url.as_bytes().to_vec(),
            )
            .unwrap(),
            ..match data(sender, instance, b"") {
                ServerEvent::PluginData(data) => data,
                _ => unreachable!(),
            }
        })
    }

    fn web_button() -> nuschl_core::integration::Button {
        nuschl_core::integration::Button {
            id: 3,
            icon: nuschl_core::integration::Icon::Web,
            label: "Web".into(),
            enabled: true,
        }
    }

    fn opened(client: &mut Client) -> Vec<String> {
        client
            .take_updates()
            .into_iter()
            .filter_map(|update| match update {
                crate::Update::Link(url) => Some(url),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_web_press_opens_the_page_the_bot_sends() {
        let (mut client, _) = connected();
        client.handle(input(&client, 0, kit_buttons(2, 22, vec![web_button()])));
        client.press_surface(0, 2, 22, 1, 1, 3).unwrap();
        client.handle(input(
            &client,
            0,
            kit_link(2, 22, "https://music.example.org/u/7a/"),
        ));
        assert_eq!(opened(&mut client), ["https://music.example.org/u/7a/"]);
        // One press opens one page: the answer is not reusable.
        client.handle(input(
            &client,
            0,
            kit_link(2, 22, "https://music.example.org/u/7a/"),
        ));
        assert!(opened(&mut client).is_empty());
    }

    #[test]
    fn a_page_needs_the_press_that_asked_for_it() {
        let (mut client, _) = connected();
        client.handle(input(&client, 0, kit_buttons(2, 22, vec![web_button()])));
        // No press at all.
        client.handle(input(
            &client,
            0,
            kit_link(2, 22, "https://music.example.org/u/7a/"),
        ));
        assert!(opened(&mut client).is_empty());
        // Another presence of the same server.
        client.handle(input(&client, 0, kit_buttons(3, 33, vec![web_button()])));
        client.press_surface(0, 2, 22, 1, 1, 3).unwrap();
        client.handle(input(
            &client,
            0,
            kit_link(3, 33, "https://music.example.org/u/7a/"),
        ));
        assert!(opened(&mut client).is_empty());
    }

    #[test]
    fn only_a_web_control_asks_for_a_page() {
        let (mut client, _) = connected();
        let mut play = web_button();
        play.id = 1;
        play.icon = nuschl_core::integration::Icon::Play;
        client.handle(input(&client, 0, kit_buttons(2, 22, vec![play])));
        client.press_surface(0, 2, 22, 1, 1, 1).unwrap();
        client.handle(input(
            &client,
            0,
            kit_link(2, 22, "https://music.example.org/u/7a/"),
        ));
        assert!(opened(&mut client).is_empty());
    }

    #[test]
    fn a_link_that_is_not_a_web_page_opens_nothing() {
        let (mut client, _) = connected();
        client.handle(input(&client, 0, kit_buttons(2, 22, vec![web_button()])));
        for url in [
            "javascript:alert(1)",
            "mumble://example.org/Lobby",
            "file:///etc/passwd",
            "https://example.org/a b",
        ] {
            client.press_surface(0, 2, 22, 1, 1, 3).unwrap();
            client.handle(input(&client, 0, kit_link(2, 22, url)));
            assert!(opened(&mut client).is_empty(), "{url}");
        }
    }

    #[test]
    fn integration_surfaces_keep_the_newest_revision_and_follow_presence() {
        let (mut client, recorder) = connected();
        client.handle(input(&client, 0, surface(2, 22, &shown(1, 1, "One"))));
        assert_eq!(client.surfaces(0).len(), 1);
        assert_eq!(client.surfaces(0)[0].surface.revision, 1);
        // The same and an older revision change nothing.
        client.handle(input(&client, 0, surface(2, 22, &shown(1, 1, "Old"))));
        client.handle(input(&client, 0, surface(2, 22, &shown(1, 0, "Older"))));
        assert_eq!(client.surfaces(0)[0].surface.revision, 1);
        client.handle(input(&client, 0, surface(2, 22, &shown(1, 2, "Two"))));
        assert_eq!(client.surfaces(0)[0].surface.revision, 2);
        // A press names the revision on screen; a stale one is refused.
        client.press_surface(0, 2, 22, 1, 2, 7).unwrap();
        client.settle();
        let actions: Vec<_> = recorder
            .sent
            .lock()
            .unwrap()
            .drain(..)
            .filter_map(|(_, _, request)| match request {
                Request::PluginData {
                    receivers, payload, ..
                } if payload.data_id() == nuschl_core::integration::KIT_ACTION_DATA_ID => Some((
                    receivers,
                    nuschl_core::integration::Action::decode(payload.data()).unwrap(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].0, [(2, 22)]);
        assert_eq!(
            (
                actions[0].1.surface,
                actions[0].1.revision,
                actions[0].1.control
            ),
            (1, 2, 7)
        );
        assert!(client.press_surface(0, 2, 22, 1, 1, 7).is_err());
        // A new presence owns no revision of its predecessor's card.
        client.handle(input(&client, 0, ServerEvent::User(user(2, 23))));
        assert!(client.surfaces(0).is_empty());
        // An empty surface withdraws.
        client.handle(input(&client, 0, surface(3, 33, &shown(1, 1, "Theirs"))));
        assert_eq!(client.surfaces(0).len(), 1);
        client.handle(input(
            &client,
            0,
            surface(
                3,
                33,
                &nuschl_core::integration::Surface {
                    id: 1,
                    revision: 2,
                    nodes: Vec::new(),
                },
            ),
        ));
        assert!(client.surfaces(0).is_empty());
        // Leaving the connection forgets every card.
        client.handle(input(&client, 0, surface(3, 33, &shown(1, 3, "Again"))));
        assert_eq!(client.surfaces(0).len(), 1);
        client.disconnect(0);
        assert!(client.surfaces(0).is_empty());
    }

    #[test]
    fn a_press_names_the_sender_that_drew_the_card() {
        let (mut client, recorder) = connected();
        // Two bots, the same surface id and revision.
        client.handle(input(&client, 0, surface(2, 22, &shown(1, 1, "Theirs"))));
        client.handle(input(&client, 0, surface(3, 33, &shown(1, 1, "Mine"))));
        assert_eq!(client.surfaces(0).len(), 2);
        for (sender, instance) in [(2, 22), (3, 33)] {
            client.press_surface(0, sender, instance, 1, 1, 4).unwrap();
        }
        client.settle();
        let mut receivers: Vec<_> = recorder
            .sent
            .lock()
            .unwrap()
            .drain(..)
            .filter_map(|(_, _, request)| match request {
                Request::PluginData { receivers, .. } => Some(receivers),
                _ => None,
            })
            .collect();
        receivers.sort();
        assert_eq!(receivers, [vec![(2, 22)], vec![(3, 33)]]);
        // A press for a presence the sender no longer has is refused.
        assert!(client.press_surface(0, 2, 21, 1, 1, 4).is_err());
    }

    #[test]
    fn a_withdrawal_older_than_the_card_leaves_it() {
        let (mut client, _) = connected();
        client.handle(input(&client, 0, surface(2, 22, &shown(1, 2, "Two"))));
        let empty = nuschl_core::integration::Surface {
            id: 1,
            revision: 1,
            nodes: Vec::new(),
        };
        client.handle(input(&client, 0, surface(2, 22, &empty)));
        assert_eq!(client.surfaces(0).len(), 1, "a late withdrawal");
        client.handle(input(
            &client,
            0,
            surface(
                2,
                22,
                &nuschl_core::integration::Surface {
                    id: 1,
                    revision: 3,
                    nodes: Vec::new(),
                },
            ),
        ));
        assert!(client.surfaces(0).is_empty(), "a withdrawal after it");
    }

    #[test]
    fn a_kit_press_admission_stays_out_of_the_generic_queue() {
        let (mut client, recorder) = connected();
        client.accept_plugin_data(true);
        client.handle(input(&client, 0, surface(2, 22, &shown(1, 1, "One"))));
        client.press_surface(0, 2, 22, 1, 1, 4).unwrap();
        let request = recorder
            .sent
            .lock()
            .unwrap()
            .drain(..)
            .find_map(|(_, _, request)| match request {
                Request::PluginData { request, .. } => Some(request),
                _ => None,
            })
            .expect("the press");
        client.handle(input(
            &client,
            0,
            ServerEvent::PluginDataAdmission {
                request,
                result: Ok(()),
            },
        ));
        assert!(
            client.take_plugin_data().is_empty(),
            "a kit press's admission is nobody's"
        );
        // Any other admission still arrives.
        client.handle(input(
            &client,
            0,
            ServerEvent::PluginDataAdmission {
                request: request.wrapping_add(9_000),
                result: Ok(()),
            },
        ));
        assert_eq!(client.take_plugin_data().len(), 1);
    }

    #[test]
    fn a_received_direct_call_belongs_to_the_conversation_with_its_sender() {
        let (mut client, _) = connected();
        client.handle(input(&client, 0, card(2, 22, &Card::Direct(link('a')))));
        assert!(client.calls_in(0, &TextTarget::Channel(0)).is_empty());
        let calls = client.calls_in(0, &TextTarget::Direct(2));
        assert_eq!((calls.len(), &calls[0].link), (1, &link('a')));
        let moved = User {
            channel: 7,
            ..user(2, 22)
        };
        client.handle(input(&client, 0, ServerEvent::User(moved)));
        assert_eq!(client.calls_in(0, &TextTarget::Direct(2)).len(), 1);
        client.handle(input(&client, 0, card(2, 22, &Card::Ended)));
        assert!(client.calls_in(0, &TextTarget::Direct(2)).is_empty());
    }
}
