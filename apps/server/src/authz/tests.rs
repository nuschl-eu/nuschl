// SPDX-License-Identifier: EUPL-1.2
//! `State::authorize` as a table: every action against each kind of actor
//! (SuperUser, an administrator, a channel's owner, a registered user an ACL
//! names, a registered user without one, a guest with a certificate, a guest
//! without), on Murmur's default root ACLs; then the inconsistencies fixed
//! when the checks moved into authz.rs, and a guard that keeps them there.
use super::{Action, Denial, SUPERUSER};
use crate::{
    acl::{AclEntry, Group, perm},
    auth::new_user,
    config::Config,
    conversations::Id,
    extension::{Extension, server_capabilities},
    kt::{KtHandle, Reply},
    routing::Datagrams,
    state::{Channel, ClientInfo, Counters, Link, Peer, Registered, State},
    storage::StorageHandle,
};
use bytes::Bytes;
use nuschl_ipc::server::HistoryVisibility;
use nuschl_protocol::{
    ControlFrame, control as proto,
    crypt::CryptState,
    kind,
    nuschl::{self, v1},
};
use proto::permission_denied::DenyType;
use std::{collections::HashMap, net::SocketAddr, path::Path, sync::Arc, time::Instant};
use tokio::sync::mpsc;

const LOBBY: u32 = 1;
/// Created by the owner; denies the administrator Write (a creator locking
/// an administrator out).
const OWNED: u32 = 2;
/// Enter, Speak, Whisper, Text and Listen denied to all, granted to the member
/// by name; the token `secret` grants Enter and Speak.
const PRIVATE: u32 = 3;
/// A temporary channel the owner made.
const TEMP: u32 = i32::MAX as u32;
/// A Private channel ([`Fixture::add_secret`]) and its subchannel.
const SECRET: u32 = 5;
const INNER: u32 = 6;
/// A channel with Mumble's password `letmein` ([`Fixture::add_door`]).
const DOOR: u32 = 7;

const ADMIN: u32 = 2;
const OWNER: u32 = 3;
const MEMBER: u32 = 4;
const REGISTERED: u32 = 5;

const ARCHIVE: Id = [0xaa; 16];
const DIRECT: Id = [0xdd; 16];
const UNKNOWN: Id = [0x99; 16];

#[test]
fn knocking_requires_both_moderator_permissions_and_rechecks_decisions() {
    let mut f = Fixture::new(Config::default());
    let requester = f.actor("registered");
    let moderator = f.actor("admin");
    let member = f.actor("member");
    assert!(f.state.allowed(member, Action::Enter { channel: PRIVATE }));
    assert!(
        !f.state
            .allowed(member, Action::ModerateKnock { channel: PRIVATE })
    );
    let id = f.state.knock_request(requester, PRIVATE).unwrap();
    assert!(
        f.received(moderator)
            .iter()
            .any(|(k, _)| *k == nuschl::kind::KNOCK_NOTICE)
    );
    assert!(
        !f.received(member)
            .iter()
            .any(|(k, _)| *k == nuschl::kind::KNOCK_NOTICE)
    );
    assert_eq!(f.state.knock_request(requester, PRIVATE), Ok(id));
    assert!(f.state.knock_decide(member, id, false).is_err());
    // Retain Move while removing Write (which itself implies Enter) and Enter.
    // Changes after the notice affect BOTH Admit and Decline.
    f.state
        .channels
        .get_mut(PRIVATE)
        .unwrap()
        .node
        .acls
        .push(entry(
            "",
            Some(ADMIN),
            perm::MOVE,
            perm::WRITE | perm::ENTER,
        ));
    f.state.invalidate();
    assert!(
        !f.state
            .allowed(moderator, Action::ModerateKnock { channel: PRIVATE })
    );
    assert!(f.state.knock_decide(moderator, id, true).is_err());
    assert!(f.state.knock_decide(moderator, id, false).is_err());
    assert_eq!(f.state.users[&requester].channel, LOBBY);
    let anonymous = f.actor("anonymous");
    f.state.knock_request(anonymous, PRIVATE).unwrap();
    assert!(
        !f.received(moderator)
            .iter()
            .any(|(k, _)| *k == nuschl::kind::KNOCK_NOTICE)
    );
}

#[test]
fn admission_has_exact_channel_permissions_history_floor_and_lifetime() {
    let mut f = Fixture::new(Config::default());
    let requester = f.actor("registered");
    let moderator = f.actor("admin");
    let conversation = f.conversation(PRIVATE);
    let child = Channel::new(4, Some(PRIVATE), "Child".into());
    f.state.channels.0.insert(4, child);
    f.state.channels.get_mut(PRIVATE).unwrap().children.push(4);
    f.state.conversations.set_head(conversation, 20);
    let id = f.state.knock_request(requester, PRIVATE).unwrap();
    f.state.knock_decide(moderator, id, true).unwrap();
    f.state.finish();
    assert_eq!(f.state.users[&requester].channel, PRIVATE);
    assert!(!f.state.users[&requester].suppress);
    for action in [
        Action::Enter { channel: 4 },
        Action::Speak { channel: 4 },
        Action::TextChannel { channel: 4 },
    ] {
        assert!(
            !f.state.allowed(requester, action),
            "grant leaked to child: {action:?}"
        );
    }
    for action in [
        Action::Enter { channel: PRIVATE },
        Action::Speak { channel: PRIVATE },
        Action::TextChannel { channel: PRIVATE },
        Action::ReadHistory(&conversation),
    ] {
        assert!(f.state.allowed(requester, action), "{action:?}");
    }
    assert!(
        !f.state
            .allowed(requester, Action::Whisper { channel: PRIVATE })
    );
    assert!(!f.state.allowed(
        requester,
        Action::Listen {
            user: requester,
            channel: PRIVATE
        }
    ));
    assert_eq!(
        f.state.visible_ranges(requester, &conversation),
        (vec![(21, u64::MAX)], true)
    );
    assert!(!f.state.allowed(
        requester,
        Action::SeeLive {
            conversation: &conversation,
            seq: 20
        }
    ));
    assert!(f.state.allowed(
        requester,
        Action::SeeLive {
            conversation: &conversation,
            seq: 21
        }
    ));
    assert_ne!(
        f.state.registry.users[&REGISTERED].last_channel,
        Some(PRIVATE)
    );
    f.handle(
        requester,
        kind::USER_STATE,
        &proto::UserState {
            channel_id: Some(LOBBY),
            ..Default::default()
        },
    );
    assert_eq!(f.state.admission_floor(requester, PRIVATE), None);
    assert!(
        !f.state
            .allowed(requester, Action::Enter { channel: PRIVATE })
    );
    assert!(
        !f.state
            .allowed(requester, Action::ReadHistory(&conversation))
    );
}

#[test]
fn stock_denied_enter_knocks_and_a_stock_moderator_move_admits() {
    use prost::Message as _;
    let mut f = Fixture::new(Config::default());
    let requester = f.actor("anonymous");
    let moderator = f.actor("admin");
    f.state.users.get_mut(&requester).unwrap().extension = None;
    f.state.users.get_mut(&moderator).unwrap().extension = None;
    f.handle(
        requester,
        kind::USER_STATE,
        &proto::UserState {
            channel_id: Some(PRIVATE),
            ..Default::default()
        },
    );
    assert_eq!(f.state.users[&requester].channel, LOBBY);
    assert!(f.received(moderator).iter().any(|(k, b)| {
        *k == kind::TEXT_MESSAGE
            && proto::TextMessage::decode(b.clone())
                .unwrap()
                .message
                .contains("knocks")
    }));
    f.handle(
        moderator,
        kind::USER_STATE,
        &proto::UserState {
            session: Some(requester),
            channel_id: Some(PRIVATE),
            ..Default::default()
        },
    );
    assert_eq!(f.state.users[&requester].channel, PRIVATE);
    assert!(
        f.state
            .allowed(requester, Action::Speak { channel: PRIVATE })
    );
    // Guests receive read during admission even under history.guests=none.
    assert!(
        f.state
            .allowed(requester, Action::ReadHistory(&f.conversation(PRIVATE)))
    );
    f.state.disconnect(requester, false);
    assert_eq!(f.state.admission_floor(requester, PRIVATE), None);
}

#[test]
fn admission_fences_pending_pages_and_live_edits_of_old_messages() {
    use nuschl_ipc::server::{Anchor, PageQuery, StoredMessage, StoredPage};
    use prost::Message as _;
    let mut f = Fixture::new(Config::default());
    let requester = f.actor("registered");
    let moderator = f.actor("admin");
    let conversation = f.conversation(PRIVATE);
    let old_epoch = f.state.authz_epoch;
    f.state.conversations.set_head(conversation, 10);
    let id = f.state.knock_request(requester, PRIVATE).unwrap();
    f.state.knock_decide(moderator, id, true).unwrap();
    f.received(requester);
    // A reader started under a previous authz epoch with an unrestricted query.
    f.state.page_read(
        crate::history::Read {
            session: requester,
            request_id: 44,
            conversation,
            query: PageQuery {
                conversation: hex::encode(conversation),
                anchor: Anchor::Latest,
                limit: 10,
                ranges: vec![(1, u64::MAX)],
                max_bytes: 4096,
            },
            epoch: old_epoch,
            clipped: false,
        },
        Some(StoredPage {
            rows: vec![
                StoredMessage {
                    seq: 9,
                    content: "secret".into(),
                    ..Default::default()
                },
                StoredMessage {
                    seq: 11,
                    content: "new".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }),
    );
    let frames = f.received(requester);
    let page = frames
        .iter()
        .find(|(k, _)| *k == nuschl::kind::HISTORY_PAGE)
        .unwrap();
    let page = v1::HistoryPage::decode(page.1.clone()).unwrap();
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].seq, 11);
    assert!(page.restricted);
    for (target, expected) in [(9, false), (11, true)] {
        f.state.deliver(
            &conversation,
            crate::history::Delivery::Event {
                message: v1::Message {
                    seq: 12,
                    kind: v1::message::Kind::Edit as i32,
                    target,
                    content: "edited".into(),
                    ..Default::default()
                },
            },
            moderator,
        );
        assert_eq!(
            f.received(requester)
                .iter()
                .any(|(k, _)| *k == nuschl::kind::CHAT_EVENT),
            expected
        );
    }
    // A stock recipient must not get an earlier message whose storage write
    // completed after admission, on either the successful or failed path.
    let extension = f.state.users.get_mut(&requester).unwrap().extension.take();
    let old = v1::Message {
        seq: 9,
        content: "secret".into(),
        ..Default::default()
    };
    f.state.deliver(
        &conversation,
        crate::history::Delivery::Message {
            message: old.clone(),
            stock: Some(crate::history::Stock {
                actor: moderator,
                html: "secret".into(),
            }),
        },
        moderator,
    );
    assert!(
        !f.received(requester)
            .iter()
            .any(|(k, _)| *k == kind::TEXT_MESSAGE)
    );
    f.state.deliver(
        &conversation,
        crate::history::Delivery::Message {
            message: v1::Message {
                seq: 11,
                ..old.clone()
            },
            stock: Some(crate::history::Stock {
                actor: moderator,
                html: "new".into(),
            }),
        },
        moderator,
    );
    assert!(
        f.received(requester)
            .iter()
            .any(|(k, _)| *k == kind::TEXT_MESSAGE)
    );
    // Failed stock writes fall back to stock frames for HISTORY clients.
    f.state.users.get_mut(&requester).unwrap().extension = extension;
    f.state.stored(
        crate::history::Done::Stock {
            source: moderator,
            conversation,
            message: old,
            fallback: proto::TextMessage {
                message: "secret".into(),
                ..Default::default()
            },
        },
        None,
    );
    assert!(
        !f.received(requester)
            .iter()
            .any(|(k, _)| *k == kind::TEXT_MESSAGE)
    );
}

#[test]
fn knock_protocol_negotiation_cancel_and_e2ee_fail_closed() {
    use prost::Message as _;
    let mut f = Fixture::new(Config::default());
    let requester = f.actor("registered");
    let moderator = f.actor("admin");
    f.handle(
        requester,
        nuschl::kind::KNOCK_REQUEST,
        &v1::KnockRequest {
            request_id: 55,
            channel: PRIVATE,
        },
    );
    let frames = f.received(requester);
    let frame = frames
        .iter()
        .find(|(k, _)| *k == nuschl::kind::KNOCK_PENDING)
        .unwrap();
    let id = v1::KnockPending::decode(frame.1.clone()).unwrap().knock_id;
    f.handle(
        moderator,
        nuschl::kind::KNOCK_CANCEL,
        &v1::KnockCancel {
            request_id: 56,
            knock_id: id,
        },
    );
    assert!(
        f.received(moderator)
            .iter()
            .any(|(k, _)| *k == nuschl::kind::ERROR)
    );
    f.handle(
        requester,
        nuschl::kind::KNOCK_CANCEL,
        &v1::KnockCancel {
            request_id: 57,
            knock_id: id,
        },
    );
    assert!(f.state.knock_decide(moderator, id, true).is_err());
    // Existing requests must not bypass a mode change either.
    f.state.mls = Some(crate::mls::MlsHandle::detached(
        f.state.settings.config.mls.clone(),
    ));
    assert_eq!(
        f.state.knock_request(requester, PRIVATE),
        Err((v1::error::Code::Unsupported, 0))
    );
    assert_eq!(f.state.admission_floor(requester, PRIVATE), None);
    assert!(
        !crate::extension::server_capabilities(true, true, true).has(nuschl::capability::KNOCK)
    );
}

#[test]
fn e2ee_forced_moves_cannot_bypass_unsupported_no_history_admission() {
    let mut f = Fixture::new(Config::default());
    let requester = f.actor("registered");
    let moderator = f.actor("admin");
    let member = f.actor("member");
    // No pending knock exists: forcing someone into the channel would still
    // make them an MLS reader and trigger an ordinary retained-key grant.
    f.state.mls = Some(crate::mls::MlsHandle::detached(
        f.state.settings.config.mls.clone(),
    ));
    assert!(!f.state.allowed(
        moderator,
        Action::Move {
            user: requester,
            to: PRIVATE
        }
    ));
    f.handle(
        moderator,
        kind::USER_STATE,
        &proto::UserState {
            session: Some(requester),
            channel_id: Some(PRIVATE),
            ..Default::default()
        },
    );
    assert_eq!(f.state.users[&requester].channel, LOBBY);
    // Move OUT of a channel, or between channels the target may enter, works.
    assert!(f.state.allowed(
        moderator,
        Action::Move {
            user: member,
            to: LOBBY
        }
    ));
    f.handle(
        moderator,
        kind::USER_STATE,
        &proto::UserState {
            session: Some(member),
            channel_id: Some(LOBBY),
            ..Default::default()
        },
    );
    assert_eq!(f.state.users[&member].channel, LOBBY);
}

fn account(id: u32) -> Id {
    [0x10 + id as u8; 16]
}

fn entry(group: &str, user_id: Option<u32>, grant: u32, deny: u32) -> AclEntry {
    AclEntry {
        apply_here: true,
        apply_subs: true,
        user_id,
        group: group.into(),
        grant,
        deny,
    }
}

struct Fixture {
    state: State,
    /// In the order of [`KINDS`].
    actors: [u32; 7],
    frames: HashMap<u32, mpsc::UnboundedReceiver<Bytes>>,
}

/// The actor kinds, in table order.
const KINDS: [&str; 7] = [
    "superuser",
    "admin",
    "owner",
    "member",
    "registered",
    "guest",
    "anonymous",
];

impl Fixture {
    fn new(config: Config) -> Self {
        let settings = Arc::new(config.settings().expect("valid settings"));
        let mut state = State::new(
            settings,
            StorageHandle::detached(),
            Arc::new(Counters::default()),
        );
        state.initialise_root();
        state.kt = Some(KtHandle::detached(vec![0; 71], nuschl_kt::LogId([7; 32])));
        state
            .channels
            .get_mut(0)
            .and_then(|root| root.node.groups.get_mut("admin"))
            .expect("Murmur's admin group")
            .add
            .insert(ADMIN);
        for (id, name) in [
            (ADMIN, "Admin"),
            (OWNER, "Owner"),
            (MEMBER, "Member"),
            (REGISTERED, "Registered"),
        ] {
            state.registry.insert(Registered {
                id,
                account: account(id),
                name: name.into(),
                password_hash: None,
                certificate_hash: Some(format!("{id:02x}").repeat(20)),
                last_channel: None,
                last_active: None,
            });
        }
        let owners = || {
            let mut group = Group::new();
            group.add.insert(OWNER);
            group
        };
        let mut add = |id: u32, name: &str, edit: &dyn Fn(&mut Channel)| {
            let mut channel = Channel::new(id, Some(0), name.into());
            edit(&mut channel);
            state.conversations.add_channel(channel.conversation_id, id);
            state.channels.0.insert(id, channel);
            if let Some(root) = state.channels.get_mut(0) {
                root.children.push(id);
            }
        };
        add(LOBBY, "Lobby", &|channel| {
            channel.node.acls = vec![entry("all", None, perm::MAKE_TEMP_CHANNEL, 0)];
        });
        add(OWNED, "Owned", &|channel| {
            channel.node.groups.insert("admin".into(), owners());
            let mut locked = entry("", Some(ADMIN), 0, perm::WRITE);
            locked.apply_subs = false;
            channel.node.acls = vec![locked];
        });
        let private = perm::ENTER | perm::SPEAK | perm::WHISPER | perm::TEXT_MESSAGE | perm::LISTEN;
        add(PRIVATE, "Private", &|channel| {
            channel.node.acls = vec![
                entry("all", None, 0, private),
                entry("", Some(MEMBER), private, 0),
                entry("#secret", None, perm::ENTER | perm::SPEAK, 0),
            ];
        });
        add(TEMP, "Temporary", &|channel| {
            channel.temporary = true;
            channel.node.groups.insert("admin".into(), owners());
        });
        state.conversations.add_archived(ARCHIVE);
        state
            .conversations
            .add_direct(DIRECT, account(MEMBER), account(REGISTERED));
        let mut fixture = Self {
            state,
            actors: [0; 7],
            frames: HashMap::new(),
        };
        let people = [
            (Some(SUPERUSER), 0),
            (Some(ADMIN), 0),
            (Some(OWNER), OWNED),
            (Some(MEMBER), PRIVATE),
            (Some(REGISTERED), LOBBY),
            (None, TEMP),
            (None, LOBBY),
        ];
        for (index, (user_id, channel)) in people.into_iter().enumerate() {
            let hash = match (user_id, index) {
                (Some(id), _) => Some(format!("{id:02x}").repeat(20)),
                (None, 5) => Some("ab".repeat(20)),
                (None, _) => None,
            };
            fixture.actors[index] = fixture.connect(KINDS[index], user_id, hash, channel);
        }
        fixture.state.invalidate();
        fixture
    }

    /// A Nuschl connection in `channel`.
    fn connect(
        &mut self,
        name: &str,
        user_id: Option<u32>,
        hash: Option<String>,
        channel: u32,
    ) -> u32 {
        let session = self.state.allocate_session().expect("a session");
        let (link, frames, _) = Link::new(1 << 20);
        self.frames.insert(session, frames);
        let peer = Peer {
            address: SocketAddr::from(([10, 0, 0, session as u8], 50_000)),
            certificates: Vec::new(),
            certificate_hash: hash,
            strong: false,
            negotiation: Default::default(),
            info: ClientInfo {
                version: (1 << 48) | (5 << 32),
                ..Default::default()
            },
            link,
        };
        let (crypt, _) = CryptState::generate_server([session as u8; 48]);
        let config = self.state.settings.config.clone();
        let mut user = new_user(
            &config,
            session,
            name.into(),
            user_id,
            &peer,
            &proto::Authenticate::default(),
            crypt,
            Instant::now(),
        );
        user.extension = Some(Box::new(Extension::new(
            server_capabilities(true, true, false),
            &config,
            Instant::now(),
        )));
        user.account = user_id
            .and_then(|id| self.state.registry.users.get(&id))
            .map(|registered| registered.account);
        user.channel = channel;
        self.state.users.insert(session, user);
        if let Some(channel) = self.state.channels.get_mut(channel) {
            channel.users.push(session);
        }
        self.state.invalidate();
        session
    }

    fn actor(&self, kind: &str) -> u32 {
        self.actors[KINDS
            .iter()
            .position(|known| *known == kind)
            .expect("a kind")]
    }

    fn conversation(&self, channel: u32) -> Id {
        self.state
            .channels
            .get(channel)
            .expect("a channel")
            .conversation_id
    }

    /// Frames sent to `session` since last asked: (type, payload).
    fn received(&mut self, session: u32) -> Vec<(u16, Bytes)> {
        let mut out = Vec::new();
        if let Some(frames) = self.frames.get_mut(&session) {
            while let Ok(frame) = frames.try_recv() {
                let kind = u16::from_be_bytes([frame[0], frame[1]]);
                out.push((kind, frame.slice(6..)));
            }
        }
        out
    }

    fn handle(&mut self, session: u32, kind: u16, message: &impl prost::Message) {
        let frame = ControlFrame {
            kind,
            payload: Bytes::from(message.encode_to_vec()),
        };
        self.state.handle(session, frame, &mut Datagrams::default());
    }
}

/// An expected answer, relative to the actor asking.
#[derive(Clone, Copy, Debug)]
enum W {
    /// Allowed.
    Y,
    /// The actor lacks `permission` in `channel`.
    P(u32, u32),
    /// The kind of actor named lacks `permission` in `channel`.
    Of(&'static str, u32, u32),
    T(DenyType),
    /// The actor has no certificate.
    Cert,
    CertOf(&'static str),
    /// Ignored silently.
    I,
    /// The extension's `not_found`.
    N,
}
use W::{Cert, CertOf, I, N, Of, P, T, Y};

impl Fixture {
    fn expected(&self, actor: u32, want: W) -> Result<(), Denial> {
        match want {
            Y => Ok(()),
            P(permission, channel) => Err(Denial::Permission {
                session: actor,
                channel,
                permission,
            }),
            Of(kind, permission, channel) => Err(Denial::Permission {
                session: self.actor(kind),
                channel,
                permission,
            }),
            T(deny) => Err(Denial::Type(deny)),
            Cert => Err(Denial::MissingCertificate { session: actor }),
            CertOf(kind) => Err(Denial::MissingCertificate {
                session: self.actor(kind),
            }),
            I => Err(Denial::Ignored),
            N => Err(Denial::NotFound),
        }
    }

    /// One row: the action (built per actor) and what each kind gets.
    fn row<'a>(&self, name: &str, action: impl Fn(u32) -> Action<'a>, wants: [W; 7]) {
        for (index, want) in wants.into_iter().enumerate() {
            let actor = self.actors[index];
            let action = action(actor);
            assert_eq!(
                self.state.authorize(actor, action),
                self.expected(actor, want),
                "{name}: {} doing {action:?}",
                KINDS[index]
            );
        }
    }
}

#[test]
fn presence_voice_and_text() {
    let f = Fixture::new(Config::default());
    let [_, _, _, member, registered, guest, _] = f.actors;
    let (enter, speak, whisper, text, listen, moves, write) = (
        perm::ENTER,
        perm::SPEAK,
        perm::WHISPER,
        perm::TEXT_MESSAGE,
        perm::LISTEN,
        perm::MOVE,
        perm::WRITE,
    );
    f.row(
        "enter a restricted channel",
        |_| Action::Enter { channel: PRIVATE },
        [
            Y,
            Y,
            P(enter, PRIVATE),
            Y,
            P(enter, PRIVATE),
            P(enter, PRIVATE),
            P(enter, PRIVATE),
        ],
    );
    f.row(
        "enter an open channel",
        |_| Action::Enter { channel: LOBBY },
        [Y; 7],
    );
    f.row(
        "move the registered user into the restricted channel",
        |_| Action::Move {
            user: registered,
            to: PRIVATE,
        },
        [
            Y,
            Y,
            P(moves, LOBBY),
            P(moves, LOBBY),
            P(enter, PRIVATE),
            P(moves, LOBBY),
            P(moves, LOBBY),
        ],
    );
    f.row(
        "move the guest into the restricted channel (denial names the guest)",
        |_| Action::Move {
            user: guest,
            to: PRIVATE,
        },
        [
            Y,
            Y,
            Of("guest", enter, PRIVATE),
            P(moves, TEMP),
            P(moves, TEMP),
            P(enter, PRIVATE),
            P(moves, TEMP),
        ],
    );
    f.row(
        "join a full channel",
        |_| Action::IgnoreUserLimit { channel: OWNED },
        [
            Y,
            P(write, OWNED),
            Y,
            P(write, OWNED),
            P(write, OWNED),
            P(write, OWNED),
            P(write, OWNED),
        ],
    );
    f.row(
        "listen to the restricted channel",
        |actor| Action::Listen {
            user: actor,
            channel: PRIVATE,
        },
        [
            Y,
            Y,
            P(listen, PRIVATE),
            Y,
            P(listen, PRIVATE),
            P(listen, PRIVATE),
            P(listen, PRIVATE),
        ],
    );
    f.row(
        "listen is evaluated for the listener, not the asker",
        |_| Action::Listen {
            user: registered,
            channel: PRIVATE,
        },
        [Of("registered", listen, PRIVATE); 7],
    );
    // SuperUser has every permission but Speak and Whisper; Write does not
    // imply them either.
    f.row(
        "speak in the restricted channel",
        |_| Action::Speak { channel: PRIVATE },
        [
            P(speak, PRIVATE),
            P(speak, PRIVATE),
            P(speak, PRIVATE),
            Y,
            P(speak, PRIVATE),
            P(speak, PRIVATE),
            P(speak, PRIVATE),
        ],
    );
    f.row(
        "speak in an open channel",
        |_| Action::Speak { channel: LOBBY },
        [P(speak, LOBBY), Y, Y, Y, Y, Y, Y],
    );
    f.row(
        "whisper into the restricted channel",
        |_| Action::Whisper { channel: PRIVATE },
        [
            P(whisper, PRIVATE),
            P(whisper, PRIVATE),
            P(whisper, PRIVATE),
            Y,
            P(whisper, PRIVATE),
            P(whisper, PRIVATE),
            P(whisper, PRIVATE),
        ],
    );
    f.row(
        "whisper to the member (in the restricted channel)",
        |_| Action::WhisperUser { user: member },
        [
            P(whisper, PRIVATE),
            P(whisper, PRIVATE),
            P(whisper, PRIVATE),
            Y,
            P(whisper, PRIVATE),
            P(whisper, PRIVATE),
            P(whisper, PRIVATE),
        ],
    );
    f.row(
        "text the restricted channel",
        |_| Action::TextChannel { channel: PRIVATE },
        [
            Y,
            Y,
            P(text, PRIVATE),
            Y,
            P(text, PRIVATE),
            P(text, PRIVATE),
            P(text, PRIVATE),
        ],
    );
    f.row(
        "text the member (TextMessage in their channel)",
        |_| Action::TextUser { user: member },
        [
            Y,
            Y,
            P(text, PRIVATE),
            Y,
            P(text, PRIVATE),
            P(text, PRIVATE),
            P(text, PRIVATE),
        ],
    );
}

#[test]
fn users() {
    let f = Fixture::new(Config::default());
    let [superuser, _, _, _, registered, guest, anonymous] = f.actors;
    let (mute, reset, register, self_register) = (
        perm::MUTE_DEAFEN,
        perm::RESET_USER_CONTENT,
        perm::REGISTER,
        perm::SELF_REGISTER,
    );
    let (kick, ban) = (perm::BAN | perm::KICK, perm::BAN);
    f.row(
        "change SuperUser",
        |_| Action::ChangeUser { user: superuser },
        [
            Y,
            T(DenyType::SuperUser),
            T(DenyType::SuperUser),
            T(DenyType::SuperUser),
            T(DenyType::SuperUser),
            T(DenyType::SuperUser),
            T(DenyType::SuperUser),
        ],
    );
    f.row(
        "change a registered user",
        |_| Action::ChangeUser { user: registered },
        [Y; 7],
    );
    f.row(
        "rename anyone",
        |actor| Action::RenameUser { user: actor },
        [T(DenyType::UserName); 7],
    );
    let muting = |user| {
        move |_| Action::SpeakState {
            user,
            suppress: false,
            muting: true,
        }
    };
    f.row(
        "mute the registered user",
        muting(registered),
        [
            Y,
            Y,
            P(mute, LOBBY),
            P(mute, LOBBY),
            P(mute, LOBBY),
            P(mute, LOBBY),
            P(mute, LOBBY),
        ],
    );
    f.row(
        "mute SuperUser (not even SuperUser itself)",
        muting(superuser),
        [T(DenyType::SuperUser); 7],
    );
    f.row(
        "suppress is never set by a client",
        |_| Action::SpeakState {
            user: registered,
            suppress: true,
            muting: true,
        },
        [P(mute, LOBBY); 7],
    );
    f.row(
        "mute in a temporary channel needs Mute/Deafen in its permanent parent",
        muting(guest),
        [
            Y,
            Y,
            T(DenyType::TemporaryChannel),
            P(mute, TEMP),
            P(mute, TEMP),
            P(mute, TEMP),
            P(mute, TEMP),
        ],
    );
    f.row(
        "priority speaker in a temporary channel needs only the channel's",
        |_| Action::SpeakState {
            user: guest,
            suppress: false,
            muting: false,
        },
        [
            Y,
            Y,
            Y,
            P(mute, TEMP),
            P(mute, TEMP),
            P(mute, TEMP),
            P(mute, TEMP),
        ],
    );
    f.row(
        "clear the registered user's comment",
        |_| Action::SetComment {
            user: registered,
            clearing: true,
        },
        [Y, Y, P(reset, 0), P(reset, 0), Y, P(reset, 0), P(reset, 0)],
    );
    f.row(
        "set the registered user's comment",
        |_| Action::SetComment {
            user: registered,
            clearing: false,
        },
        [
            T(DenyType::TextTooLong),
            T(DenyType::TextTooLong),
            P(reset, 0),
            P(reset, 0),
            Y,
            P(reset, 0),
            P(reset, 0),
        ],
    );
    f.row(
        "clear the registered user's avatar",
        |_| Action::SetTexture {
            user: registered,
            clearing: true,
        },
        [Y, Y, P(reset, 0), P(reset, 0), Y, P(reset, 0), P(reset, 0)],
    );
    f.row(
        "register oneself",
        |actor| Action::Register { user: actor },
        [
            P(self_register, 0),
            P(self_register, 0),
            P(self_register, 0),
            P(self_register, 0),
            P(self_register, 0),
            Y,
            Cert,
        ],
    );
    f.row(
        "register the guest",
        |_| Action::Register { user: guest },
        [
            Y,
            Y,
            P(register, 0),
            P(register, 0),
            P(register, 0),
            Y,
            P(register, 0),
        ],
    );
    f.row(
        "register the guest without a certificate",
        |_| Action::Register { user: anonymous },
        [
            CertOf("anonymous"),
            CertOf("anonymous"),
            P(register, 0),
            P(register, 0),
            P(register, 0),
            P(register, 0),
            Cert,
        ],
    );
    f.row(
        "self-mute, plugin data, recording and listeners of the registered user",
        |_| Action::SelfState { user: registered },
        [I, I, I, I, Y, I, I],
    );
    f.row(
        "kick the registered user",
        |_| Action::Kick { user: registered },
        [
            Y,
            Y,
            P(kick, 0),
            P(kick, 0),
            P(kick, 0),
            P(kick, 0),
            P(kick, 0),
        ],
    );
    f.row(
        "kick SuperUser",
        |_| Action::Kick { user: superuser },
        [P(kick, 0); 7],
    );
    f.row(
        "ban the registered user",
        |_| Action::Ban { user: registered },
        [Y, Y, P(ban, 0), P(ban, 0), P(ban, 0), P(ban, 0), P(ban, 0)],
    );
    f.row(
        "the registered user list",
        |_| Action::ManageRegistrations,
        [
            Y,
            Y,
            P(register, 0),
            P(register, 0),
            P(register, 0),
            P(register, 0),
            P(register, 0),
        ],
    );
    f.row(
        "rename or unregister SuperUser through it",
        |_| Action::EditRegistration { user_id: SUPERUSER },
        [I; 7],
    );
    f.row(
        "rename or unregister someone else through it",
        |_| Action::EditRegistration {
            user_id: REGISTERED,
        },
        [Y; 7],
    );
    f.row(
        "the ban list",
        |_| Action::ManageBans,
        [Y, Y, P(ban, 0), P(ban, 0), P(ban, 0), P(ban, 0), P(ban, 0)],
    );
    f.row(
        "look up registered names (Write somewhere)",
        |_| Action::QueryUsers,
        [Y, Y, Y, I, I, I, I],
    );
    let member = f.actor("member");
    f.row(
        "statistics of the member (in the restricted channel)",
        |_| Action::UserStats { user: member },
        [
            Y,
            Y,
            P(perm::ENTER, PRIVATE),
            Y,
            P(perm::ENTER, PRIVATE),
            P(perm::ENTER, PRIVATE),
            P(perm::ENTER, PRIVATE),
        ],
    );
    f.row(
        "certificates and address of the registered user",
        |_| Action::UserStatsDetails { user: registered },
        [Y, Y, I, I, Y, I, I],
    );
    f.row(
        "connection statistics of the registered user (same channel or details)",
        |_| Action::UserStatsLocal { user: registered },
        [Y, Y, I, I, Y, I, Y],
    );
}

#[test]
fn channels() {
    let f = Fixture::new(Config::default());
    let (write, make, make_temporary, link) = (
        perm::WRITE,
        perm::MAKE_CHANNEL,
        perm::MAKE_TEMP_CHANNEL,
        perm::LINK_CHANNEL,
    );
    f.row(
        "make a channel",
        |_| Action::MakeChannel {
            parent: LOBBY,
            temporary: false,
        },
        [
            Y,
            Y,
            P(make, LOBBY),
            P(make, LOBBY),
            P(make, LOBBY),
            P(make, LOBBY),
            P(make, LOBBY),
        ],
    );
    f.row(
        "make a temporary channel (granted to all in the lobby)",
        |_| Action::MakeChannel {
            parent: LOBBY,
            temporary: true,
        },
        [Y, Y, Y, Y, Y, Y, Cert],
    );
    let temporary_parent = T(DenyType::TemporaryChannel);
    f.row(
        "make a channel in a temporary one",
        |_| Action::MakeChannel {
            parent: TEMP,
            temporary: true,
        },
        [
            temporary_parent,
            temporary_parent,
            temporary_parent,
            temporary_parent,
            temporary_parent,
            P(make_temporary, TEMP),
            P(make_temporary, TEMP),
        ],
    );
    let locked = [
        Y,
        P(write, OWNED),
        Y,
        P(write, OWNED),
        P(write, OWNED),
        P(write, OWNED),
        P(write, OWNED),
    ];
    f.row(
        "edit the owned channel",
        |_| Action::EditChannel { channel: OWNED },
        locked,
    );
    f.row(
        "rename the owned channel",
        |_| Action::RenameChannel { channel: OWNED },
        locked,
    );
    f.row(
        "its ACL: Write at the root overrides the lock-out",
        |_| Action::EditAcl { channel: OWNED },
        [
            Y,
            Y,
            Y,
            P(write, OWNED),
            P(write, OWNED),
            P(write, OWNED),
            P(write, OWNED),
        ],
    );
    f.row(
        "rename the root",
        |_| Action::RenameChannel { channel: 0 },
        [P(write, 0); 7],
    );
    f.row(
        "remove the root",
        |_| Action::RemoveChannel { channel: 0 },
        [P(write, 0); 7],
    );
    f.row(
        "remove the temporary channel",
        |_| Action::RemoveChannel { channel: TEMP },
        [
            Y,
            Y,
            Y,
            P(write, TEMP),
            P(write, TEMP),
            P(write, TEMP),
            P(write, TEMP),
        ],
    );
    f.row(
        "move the owned channel into the lobby",
        |_| Action::MoveChannel {
            channel: OWNED,
            to: LOBBY,
        },
        [
            Y,
            P(write, OWNED),
            P(make, LOBBY),
            P(write, OWNED),
            P(write, OWNED),
            P(write, OWNED),
            P(write, OWNED),
        ],
    );
    f.row(
        "move a channel into a temporary one",
        |_| Action::MoveChannel {
            channel: OWNED,
            to: TEMP,
        },
        [T(DenyType::TemporaryChannel); 7],
    );
    f.row(
        "move the temporary channel (Make temporary on the new parent)",
        |_| Action::MoveChannel {
            channel: TEMP,
            to: LOBBY,
        },
        [
            Y,
            Y,
            Y,
            P(write, TEMP),
            P(write, TEMP),
            P(write, TEMP),
            P(write, TEMP),
        ],
    );
    f.row(
        "link the owned channel",
        |_| Action::LinkChannel { channel: OWNED },
        [
            Y,
            P(link, OWNED),
            Y,
            P(link, OWNED),
            P(link, OWNED),
            P(link, OWNED),
            P(link, OWNED),
        ],
    );
}

#[test]
fn conversations() {
    let mut f = Fixture::new(Config::default());
    let (lobby, private, owned) = (
        f.conversation(LOBBY),
        f.conversation(PRIVATE),
        f.conversation(OWNED),
    );
    let registered_only = [Y, Y, Y, Y, Y, N, N];
    f.row(
        "read the lobby (guests read no history by default)",
        |_| Action::ReadHistory(&lobby),
        registered_only,
    );
    f.row(
        "read the restricted channel",
        |_| Action::ReadHistory(&private),
        [Y, Y, N, Y, N, N, N],
    );
    f.row(
        "read the direct conversation",
        |_| Action::ReadHistory(&DIRECT),
        [N, N, N, Y, Y, N, N],
    );
    f.row(
        "read a deleted channel's archive",
        |_| Action::ReadHistory(&ARCHIVE),
        [Y, N, N, N, N, N, N],
    );
    f.row(
        "read an unknown conversation",
        |_| Action::ReadHistory(&UNKNOWN),
        [N; 7],
    );
    f.row("send to the lobby", |_| Action::Send(&lobby), [Y; 7]);
    f.row(
        "send to the restricted channel",
        |_| Action::Send(&private),
        [Y, Y, N, Y, N, N, N],
    );
    f.row(
        "type in the restricted channel",
        |_| Action::Typing(&private),
        [Y, Y, N, Y, N, N, N],
    );
    // The member may text the registered user's channel; not the other way.
    f.row(
        "send to the direct conversation",
        |_| Action::Send(&DIRECT),
        [N, N, N, Y, N, N, N],
    );
    f.row("send to an archive", |_| Action::Send(&ARCHIVE), [N; 7]);
    let member_account = account(MEMBER);
    f.row(
        "message the member's account",
        |_| Action::SendDirect {
            peer: &member_account,
        },
        [Y, Y, N, N, N, N, N],
    );
    f.row(
        "edit in the lobby",
        |_| Action::Edit(&lobby),
        registered_only,
    );
    f.row(
        "delete one's own message in the lobby",
        |_| Action::DeleteOwn(&lobby),
        registered_only,
    );
    f.row(
        "delete one's own message where no conversation is",
        |_| Action::DeleteOwn(&UNKNOWN),
        [N; 7],
    );
    f.row(
        "moderate the owned channel",
        |_| Action::Moderate(&owned),
        [Y, N, Y, N, N, N, N],
    );
    f.row(
        "moderate the lobby",
        |_| Action::Moderate(&lobby),
        [Y, Y, N, N, N, N, N],
    );
    f.row(
        "moderate a direct conversation",
        |_| Action::Moderate(&DIRECT),
        [N; 7],
    );
    f.row(
        "moderate an archive",
        |_| Action::Moderate(&ARCHIVE),
        [N; 7],
    );
    f.row(
        "the owned channel's settings",
        |_| Action::ChannelSettings { channel: OWNED },
        [Y, N, Y, N, N, N, N],
    );
    f.row(
        "an unknown channel's settings",
        |_| Action::ChannelSettings { channel: 999 },
        [N; 7],
    );
    f.row(
        "a read marker in the lobby",
        |_| Action::ReadMarker(&lobby),
        registered_only,
    );
    f.row(
        "a read marker in the restricted channel",
        |_| Action::ReadMarker(&private),
        [Y, Y, N, Y, N, N, N],
    );
    let registered_account = account(REGISTERED);
    f.row(
        "open the direct conversation with the registered user",
        |_| Action::OpenDirect {
            peer: &registered_account,
        },
        [Y, Y, Y, Y, N, N, N],
    );
    f.row(
        "open a direct conversation with nobody",
        |_| Action::OpenDirect { peer: &UNKNOWN },
        [N; 7],
    );
    f.row(
        "list direct conversations",
        |_| Action::ListDirects,
        registered_only,
    );

    // Moderation off: authors only.
    let mut config = Config::default();
    config.history.moderators_delete = false;
    let quiet = Fixture::new(config);
    let quiet_owned = quiet.conversation(OWNED);
    quiet.row(
        "moderate with moderators_delete off",
        |_| Action::Moderate(&quiet_owned),
        [N; 7],
    );
    // Guests read with history.guests = "full".
    let mut config = Config::default();
    config.history.guests = crate::config::GuestHistory::Full;
    let open = Fixture::new(config);
    let open_lobby = open.conversation(LOBBY);
    open.row(
        "read the lobby with guests = full",
        |_| Action::ReadHistory(&open_lobby),
        [Y; 7],
    );

    // `since_access`: outside the channel, only what was said while one could read.
    if let Some(channel) = f.state.channels.get_mut(OWNED) {
        channel.history_visibility = HistoryVisibility::SinceAccess;
    }
    f.state
        .conversations
        .access
        .entry(owned)
        .or_default()
        .insert(account(MEMBER), vec![(2, Some(4))]);
    f.row(
        "see message 3 of the owned channel live",
        |_| Action::SeeLive {
            conversation: &owned,
            seq: 3,
        },
        [I, I, Y, Y, I, I, I],
    );
    f.row(
        "see message 5 of the owned channel live",
        |_| Action::SeeLive {
            conversation: &owned,
            seq: 5,
        },
        [I, I, Y, I, I, I, I],
    );
    f.row(
        "see a message of the lobby live",
        |_| Action::SeeLive {
            conversation: &lobby,
            seq: 99,
        },
        [Y; 7],
    );
}

#[test]
fn key_transparency() {
    let f = Fixture::new(Config::default());
    let (registered_account, member_account) = (account(REGISTERED), account(MEMBER));
    for action in [Action::KtStatus, Action::KtAudit, Action::KtHead] {
        f.row("status, audits and heads", |_| action, [Y; 7]);
    }
    f.row(
        "publish one's own binding",
        |_| Action::KtPublish,
        [Y, Y, Y, Y, Y, N, N],
    );
    f.row(
        "look up a registered account",
        |_| Action::KtLookup {
            account: &registered_account,
        },
        [Y; 7],
    );
    f.row(
        "look up an unknown account",
        |_| Action::KtLookup { account: &UNKNOWN },
        [N; 7],
    );
    // Contacts: the owner, a direct conversation, or the same channel now.
    f.row(
        "the member's binding history",
        |_| Action::KtHistory {
            account: &member_account,
        },
        [N, N, N, Y, Y, N, N],
    );
}

#[test]
fn reported_permissions_and_groups() {
    let f = Fixture::new(Config::default());
    let [superuser, admin, owner, _, registered, _, _] = f.actors;
    assert_eq!(f.state.sync_permissions(superuser), perm::ALL);
    assert_eq!(
        f.state.sync_permissions(registered),
        perm::DEFAULT | perm::MAKE_TEMP_CHANNEL | perm::SELF_REGISTER
    );
    assert_ne!(f.state.sync_permissions(admin) & perm::BAN, 0);
    assert!(f.state.in_group(owner, OWNED, "admin"));
    assert!(f.state.in_group(admin, OWNED, "admin"));
    assert!(!f.state.in_group(registered, OWNED, "admin"));
    assert!(f.state.in_group(registered, LOBBY, "auth"));
    assert!(!f.state.in_group(f.actor("guest"), LOBBY, "auth"));
}

/// A strong certificate (tls::Authorities) matches the ACL group `strong`.
#[test]
fn strong_certificates_match_the_strong_group() {
    let mut f = Fixture::new(Config::default());
    if let Some(lobby) = f.state.channels.get_mut(LOBBY) {
        lobby
            .node
            .acls
            .push(entry("strong", None, perm::MAKE_CHANNEL, 0));
    }
    f.state.invalidate();
    let guest = f.actor("guest");
    let make = Action::MakeChannel {
        parent: LOBBY,
        temporary: false,
    };
    assert!(!f.state.allowed(guest, make));
    assert!(!f.state.in_group(guest, LOBBY, "strong"));
    if let Some(user) = f.state.users.get_mut(&guest) {
        user.strong = true;
    }
    f.state.invalidate();
    assert!(f.state.allowed(guest, make));
    assert!(f.state.in_group(guest, LOBBY, "strong"));
    assert!(!f.state.allowed(f.actor("registered"), make));
}

// ---- inconsistencies fixed when the checks moved here ----

/// A temporary access token counts for one `UserState` only. Before, the
/// token's grants were dropped from the cache when the message was done, but
/// suppression (and the client's permission cache) were not re-evaluated as
/// after every other credential change: a token that granted Speak left the
/// user unsuppressed. Murmur's `TemporaryAccessTokenHelper` clears the ACL
/// cache with its suppression update on both ends.
#[test]
fn temporary_tokens_do_not_outlive_their_message() {
    let mut f = Fixture::new(Config::default());
    let registered = f.actor("registered");
    f.received(registered);
    f.handle(
        registered,
        kind::USER_STATE,
        &proto::UserState {
            channel_id: Some(PRIVATE),
            temporary_access_tokens: vec!["secret".into()],
            ..Default::default()
        },
    );
    let user = &f.state.users[&registered];
    assert_eq!(user.channel, PRIVATE, "the token let them in");
    assert!(user.tokens.is_empty(), "the token is gone");
    assert!(
        user.suppress,
        "Speak came only from the token, so the user is suppressed again"
    );
    assert!(
        !f.state
            .allowed(registered, Action::Speak { channel: PRIVATE })
    );
    // The client's permission cache was flushed with the token-free value.
    let flushed = f
        .received(registered)
        .into_iter()
        .filter(|(kind, _)| *kind == kind::PERMISSION_QUERY)
        .filter_map(|(_, payload)| <proto::PermissionQuery as prost::Message>::decode(payload).ok())
        .filter(|query| query.flush() && query.channel_id == Some(PRIVATE))
        .last()
        .expect("a flushed PermissionQuery");
    assert_eq!(flushed.permissions() & perm::SPEAK, 0);
}

/// A stock client's direct message is archived in the accounts' direct
/// conversation only if the sender may write to it as a Nuschl client would:
/// TextMessage in the channel of each of the peer's connections. Before, the
/// stock path checked only the connection it named and then delivered the
/// stored message to all of the account's connections.
#[test]
fn stock_direct_messages_are_archived_only_where_a_nuschl_send_would_be() {
    let mut config = Config::default();
    config.history.archive_stock_direct_messages = true;
    let mut f = Fixture::new(config);
    let registered = f.actor("registered");
    // The member is online twice: in the lobby and in the restricted channel.
    let lobby_member = f.connect("member", Some(MEMBER), None, LOBBY);
    f.state.conversations = Default::default();
    for channel in f.state.channels.0.values() {
        f.state
            .conversations
            .add_channel(channel.conversation_id, channel.id);
    }
    let text = proto::TextMessage {
        session: vec![lobby_member],
        message: "hello".into(),
        ..Default::default()
    };
    f.received(lobby_member);
    f.handle(registered, kind::TEXT_MESSAGE, &text);
    assert!(
        f.state
            .conversations
            .direct(account(REGISTERED), account(MEMBER))
            .is_none(),
        "not archived: the sender may not text the member's other connection"
    );
    assert!(
        f.received(lobby_member)
            .iter()
            .any(|(kind, _)| *kind == kind::TEXT_MESSAGE),
        "the named connection still gets it live, as from Murmur"
    );
    // Once the member's other connection is somewhere the sender may text,
    // the same message is archived.
    let member = f.actor("member");
    let mut moved = proto::UserState {
        session: Some(member),
        channel_id: Some(LOBBY),
        ..Default::default()
    };
    f.state.enter_channel(member, LOBBY, &mut moved);
    f.handle(registered, kind::TEXT_MESSAGE, &text);
    assert!(
        f.state
            .conversations
            .direct(account(REGISTERED), account(MEMBER))
            .is_some()
    );
}

/// A binding history is authorised again when its answer comes back from the
/// key-transparency task, as history pages are re-checked: someone who stopped
/// being a contact in between gets `not_found`.
#[test]
fn key_transparency_histories_are_checked_again_when_answered() {
    let mut f = Fixture::new(Config::default());
    let (owner, registered) = (f.actor("owner"), f.actor("registered"));
    let owner_account = account(OWNER);
    let history = Action::KtHistory {
        account: &owner_account,
    };
    let answer = |request_id| {
        Ok(Reply::History(v1::KtHistoryReply {
            witnessed: Vec::new(),
            label: 0,
            request_id,
            account: owner_account.to_vec(),
            response: vec![1, 2, 3],
        }))
    };
    let mut moved = proto::UserState::default();
    f.state.enter_channel(owner, LOBBY, &mut moved);
    assert!(
        f.state.allowed(registered, history),
        "same channel: a contact"
    );
    f.received(registered);
    f.state.kt_answer(registered, 7, answer(7));
    assert!(
        f.received(registered)
            .iter()
            .any(|(kind, _)| *kind == nuschl::kind::KT_HISTORY_REPLY)
    );
    // Authorised, then the owner leaves before the answer arrives.
    f.state.enter_channel(owner, OWNED, &mut moved);
    assert!(!f.state.allowed(registered, history));
    f.state.kt_answer(registered, 8, answer(8));
    let frames = f.received(registered);
    assert!(
        !frames
            .iter()
            .any(|(kind, _)| *kind == nuschl::kind::KT_HISTORY_REPLY)
    );
    let error = frames
        .iter()
        .find(|(kind, _)| *kind == nuschl::kind::ERROR)
        .and_then(|(_, payload)| <v1::Error as prost::Message>::decode(payload.clone()).ok())
        .expect("an error");
    assert_eq!(error.request_id, 8);
    assert_eq!(error.code(), v1::error::Code::NotFound);
}

// ---- listeners follow Listen (the user's decision on review D3.4) ----

fn listening(f: &Fixture, session: u32) -> Vec<u32> {
    f.state.users[&session].listening.keys().copied().collect()
}

/// What `session` was told about its listeners: the channels a `UserState`
/// about it removed, and the channels of `PermissionDenied`s for Listen.
fn told(f: &mut Fixture, viewer: u32, session: u32) -> (Vec<u32>, Vec<u32>) {
    let (mut removed, mut denied) = (Vec::new(), Vec::new());
    for (kind, payload) in f.received(viewer) {
        if kind == kind::USER_STATE
            && let Ok(state) = <proto::UserState as prost::Message>::decode(payload.clone())
            && state.session == Some(session)
        {
            removed.extend(state.listening_channel_remove);
        } else if kind == kind::PERMISSION_DENIED
            && let Ok(message) = <proto::PermissionDenied as prost::Message>::decode(payload)
            && message.permission == Some(perm::LISTEN)
        {
            denied.extend(message.channel_id);
        }
    }
    (removed, denied)
}

/// A listener that only a temporary access token allowed ends with the
/// message that carried the token (Murmur keeps it).
#[test]
fn a_listener_a_temporary_token_allowed_ends_with_its_message() {
    let mut f = Fixture::new(Config::default());
    if let Some(private) = f.state.channels.get_mut(PRIVATE) {
        private
            .node
            .acls
            .push(entry("#secret", None, perm::LISTEN, 0));
    }
    f.state.invalidate();
    let (registered, member) = (f.actor("registered"), f.actor("member"));
    f.handle(
        member,
        kind::USER_STATE,
        &proto::UserState {
            listening_channel_add: vec![LOBBY],
            ..Default::default()
        },
    );
    assert_eq!(listening(&f, member), [LOBBY]);
    f.received(registered);
    f.handle(
        registered,
        kind::USER_STATE,
        &proto::UserState {
            listening_channel_add: vec![PRIVATE],
            temporary_access_tokens: vec!["secret".into()],
            ..Default::default()
        },
    );
    assert!(listening(&f, registered).is_empty());
    assert!(
        !f.state
            .channels
            .get(PRIVATE)
            .is_some_and(|private| private.listeners.contains(&registered))
    );
    assert_eq!(
        told(&mut f, registered, registered),
        (vec![PRIVATE], vec![PRIVATE])
    );
    assert_eq!(listening(&f, member), [LOBBY], "another listener stays");
}

/// Unregistering takes the account's ACL entries with it; a listener only
/// such an entry allowed ends, and everyone is told.
#[test]
fn a_listener_ends_when_unregistering_takes_its_listen_away() {
    let mut f = Fixture::new(Config::default());
    let (superuser, registered, guest) = (
        f.actor("superuser"),
        f.actor("registered"),
        f.actor("guest"),
    );
    if let Some(private) = f.state.channels.get_mut(PRIVATE) {
        private
            .node
            .acls
            .push(entry("", Some(REGISTERED), perm::LISTEN, 0));
    }
    f.state.invalidate();
    f.handle(
        registered,
        kind::USER_STATE,
        &proto::UserState {
            listening_channel_add: vec![PRIVATE],
            ..Default::default()
        },
    );
    assert_eq!(listening(&f, registered), [PRIVATE]);
    f.received(guest);
    f.handle(
        superuser,
        kind::USER_LIST,
        &proto::UserList {
            users: vec![proto::user_list::User {
                user_id: REGISTERED,
                ..Default::default()
            }],
        },
    );
    assert!(!f.state.registry.users.contains_key(&REGISTERED));
    assert!(listening(&f, registered).is_empty());
    assert_eq!(told(&mut f, guest, registered), (vec![PRIVATE], vec![]));
}

// ---- what each client sees (#341) ----

impl Fixture {
    /// A Private channel below the root, as the channel type stores it
    /// (`@all` denied and `@admin` allowed Traverse, Enter, Listen, Speak,
    /// Whisper, Text message, here and in subchannels), with the member's row
    /// and Write for the owner, and a subchannel; each comes with its rules,
    /// as a create that carries a type's rules (`hidden-channels/1`) makes it.
    fn add_secret(&mut self) {
        let access = perm::TRAVERSE
            | perm::ENTER
            | perm::LISTEN
            | perm::SPEAK
            | perm::WHISPER
            | perm::TEXT_MESSAGE;
        let mut secret = Channel::new(SECRET, Some(0), "Secret".into());
        secret.node.acls = vec![
            entry("all", None, 0, access),
            entry("admin", None, access, 0),
            entry("", Some(MEMBER), access, 0),
            entry("", Some(OWNER), perm::WRITE, 0),
        ];
        secret.children.push(INNER);
        let inner = Channel::new(INNER, Some(SECRET), "Inner".into());
        for channel in [secret, inner] {
            self.state
                .conversations
                .add_channel(channel.conversation_id, channel.id);
            self.state.channels.0.insert(channel.id, channel);
        }
        if let Some(root) = self.state.channels.get_mut(0) {
            root.children.push(SECRET);
        }
        self.state.invalidate();
        self.state.channel_added(SECRET);
        self.state.channel_added(INNER);
    }

    /// A channel below the root with Mumble's channel password rules: `@all`
    /// denied and `#letmein` granted Traverse, Enter, Speak, Whisper, Text
    /// message and Link channel, in the channel only.
    fn add_door(&mut self) {
        let password = perm::TRAVERSE
            | perm::ENTER
            | perm::SPEAK
            | perm::WHISPER
            | perm::TEXT_MESSAGE
            | perm::LINK_CHANNEL;
        let here = |group: &str, grant, deny| AclEntry {
            apply_subs: false,
            ..entry(group, None, grant, deny)
        };
        let mut door = Channel::new(DOOR, Some(0), "Door".into());
        door.node.acls = vec![here("all", 0, password), here("#letmein", password, 0)];
        self.state
            .conversations
            .add_channel(door.conversation_id, DOOR);
        self.state.channels.0.insert(DOOR, door);
        if let Some(root) = self.state.channels.get_mut(0) {
            root.children.push(DOOR);
        }
        self.state.invalidate();
        self.state.channel_added(DOOR);
    }

    fn hidden(&self, kind: &str) -> Vec<u32> {
        self.state.users[&self.actor(kind)]
            .hidden
            .iter()
            .copied()
            .collect()
    }
}

/// What a client heard about channels and people: added or changed channels,
/// removed channels, users shown, users removed (each in order).
#[derive(Debug, Default, PartialEq)]
struct Heard {
    channels: Vec<u32>,
    removed_channels: Vec<u32>,
    users: Vec<u32>,
    removed_users: Vec<u32>,
}

impl Heard {
    fn of(frames: &[(u16, Bytes)]) -> Self {
        use prost::Message as _;
        let mut heard = Self::default();
        for (kind, payload) in frames {
            match *kind {
                kind::CHANNEL_STATE => heard.channels.extend(
                    proto::ChannelState::decode(payload.clone())
                        .unwrap()
                        .channel_id,
                ),
                kind::CHANNEL_REMOVE => heard.removed_channels.push(
                    proto::ChannelRemove::decode(payload.clone())
                        .unwrap()
                        .channel_id,
                ),
                kind::USER_STATE => heard
                    .users
                    .extend(proto::UserState::decode(payload.clone()).unwrap().session),
                kind::USER_REMOVE => heard
                    .removed_users
                    .push(proto::UserRemove::decode(payload.clone()).unwrap().session),
                _ => {}
            }
        }
        heard
    }
}

#[test]
fn a_private_channel_and_its_subtree_reach_only_those_who_may_traverse_it() {
    let mut f = Fixture::new(Config::default());
    for session in f.actors {
        f.received(session);
    }
    f.add_secret();
    assert_eq!(f.state.guarded_channels(), [SECRET, INNER]);
    // SuperUser and the administrators (Write at the root), the owner (Write
    // in the channel) and the member (allowed by name) see both.
    for kind in ["superuser", "admin", "owner", "member"] {
        assert_eq!(f.hidden(kind), [] as [u32; 0], "{kind}");
        let session = f.actor(kind);
        assert_eq!(
            Heard::of(&f.received(session)).channels,
            [SECRET, INNER],
            "{kind}"
        );
    }
    // Nobody else hears of them.
    for kind in ["registered", "guest", "anonymous"] {
        assert_eq!(f.hidden(kind), [SECRET, INNER], "{kind}");
        let session = f.actor(kind);
        assert_eq!(Heard::of(&f.received(session)), Heard::default(), "{kind}");
        assert!(!f.state.sees_channel(session, SECRET));
        assert!(!f.state.sees_channel(session, INNER));
    }
    // Write at the root sees a channel that denies the administrators.
    f.state
        .channels
        .get_mut(SECRET)
        .unwrap()
        .node
        .acls
        .push(entry("admin", None, 0, perm::TRAVERSE | perm::WRITE));
    f.state.authz_changed(crate::conversations::Scope::All);
    f.state.invalidate();
    f.state.finish();
    assert_eq!(f.hidden("admin"), [] as [u32; 0]);
}

#[test]
fn a_channel_a_password_opens_stays_listed_and_only_the_password_enters() {
    let mut f = Fixture::new(Config::default());
    f.add_secret();
    for session in f.actors {
        f.received(session);
    }
    f.add_door();
    let registered = f.actor("registered");
    // Everyone sees it; the Private channel without a token rule stays hidden.
    for kind in KINDS {
        let session = f.actor(kind);
        assert_eq!(Heard::of(&f.received(session)).channels, [DOOR], "{kind}");
        assert!(!f.state.users[&session].hidden.contains(&DOOR), "{kind}");
    }
    for kind in ["registered", "guest", "anonymous"] {
        assert_eq!(f.hidden(kind), [SECRET, INNER], "{kind}");
    }
    // Seeing it is not entering it.
    assert_eq!(
        f.state
            .authorize(registered, Action::Enter { channel: DOOR }),
        Err(Denial::Permission {
            session: registered,
            channel: DOOR,
            permission: perm::ENTER
        })
    );
    // With the password, as a temporary access token, it enters.
    f.handle(
        registered,
        kind::USER_STATE,
        &proto::UserState {
            channel_id: Some(DOOR),
            temporary_access_tokens: vec!["letmein".into()],
            ..Default::default()
        },
    );
    assert_eq!(f.state.users[&registered].channel, DOOR);
    assert_eq!(f.hidden("registered"), [SECRET, INNER]);
}

#[test]
fn people_in_a_private_channel_come_and_go_as_seen_from_outside() {
    let mut f = Fixture::new(Config::default());
    f.add_secret();
    let [_, admin, _, member, registered, _, anonymous] = f.actors;
    for session in f.actors {
        f.received(session);
    }
    // The member goes in: outside, it leaves.
    f.handle(
        member,
        kind::USER_STATE,
        &proto::UserState {
            channel_id: Some(SECRET),
            ..Default::default()
        },
    );
    assert_eq!(f.state.users[&member].channel, SECRET);
    assert_eq!(Heard::of(&f.received(anonymous)).removed_users, [member]);
    assert!(!f.state.sees_user(anonymous, member));
    assert_eq!(Heard::of(&f.received(admin)).users, [member]);
    // And comes back.
    f.handle(
        member,
        kind::USER_STATE,
        &proto::UserState {
            channel_id: Some(LOBBY),
            ..Default::default()
        },
    );
    let back = f.received(anonymous);
    assert!(
        Heard::of(&back)
            .users
            .iter()
            .all(|session| *session == member)
    );
    let state = back
        .iter()
        .find(|(kind, _)| *kind == kind::USER_STATE)
        .map(|(_, payload)| {
            use prost::Message as _;
            proto::UserState::decode(payload.clone()).unwrap()
        })
        .unwrap();
    assert_eq!(
        (state.name.as_deref(), state.channel_id),
        (Some("member"), Some(LOBBY)),
        "the whole user, as for someone who connects"
    );

    // Moved in by an administrator, a registered user sees the channel it is
    // in (before it hears of the move) without its subchannel, and loses it
    // again after leaving.
    f.received(registered);
    f.handle(
        admin,
        kind::USER_STATE,
        &proto::UserState {
            session: Some(registered),
            channel_id: Some(SECRET),
            ..Default::default()
        },
    );
    assert_eq!(f.hidden("registered"), [INNER]);
    let heard = Heard::of(&f.received(registered));
    assert_eq!(heard.channels, [SECRET]);
    assert_eq!(heard.users, [registered]);
    assert_eq!(
        Heard::of(&f.received(anonymous)).removed_users,
        [registered]
    );
    f.handle(
        registered,
        kind::USER_STATE,
        &proto::UserState {
            channel_id: Some(LOBBY),
            ..Default::default()
        },
    );
    assert_eq!(f.hidden("registered"), [SECRET, INNER]);
    let heard = f.received(registered);
    let kinds: Vec<u16> = heard.iter().map(|(kind, _)| *kind).collect();
    let moved = kinds.iter().position(|kind| *kind == kind::USER_STATE);
    let removed = kinds.iter().position(|kind| *kind == kind::CHANNEL_REMOVE);
    assert!(moved < removed, "its move first, then the channel goes");
    assert_eq!(Heard::of(&heard).removed_channels, [SECRET]);
}

#[test]
fn group_membership_shows_and_hides_a_private_channel_with_its_people() {
    let mut f = Fixture::new(Config::default());
    f.add_secret();
    let [_, admin, _, member, registered, _, _] = f.actors;
    f.handle(
        member,
        kind::USER_STATE,
        &proto::UserState {
            channel_id: Some(INNER),
            ..Default::default()
        },
    );
    // SuperUser may not speak: suppressed once permissions are evaluated.
    f.state.permissions_changed(None);
    f.received(registered);
    let acl = |members: Vec<u32>| {
        let access = perm::TRAVERSE
            | perm::ENTER
            | perm::LISTEN
            | perm::SPEAK
            | perm::WHISPER
            | perm::TEXT_MESSAGE;
        let rule = |group: &str, user_id: Option<u32>, grant, deny| proto::acl::ChanAcl {
            apply_here: Some(true),
            apply_subs: Some(true),
            group: user_id.is_none().then(|| group.to_owned()),
            user_id,
            grant: Some(grant),
            deny: Some(deny),
            ..Default::default()
        };
        proto::Acl {
            channel_id: SECRET,
            inherit_acls: Some(true),
            groups: vec![proto::acl::ChanGroup {
                name: "admin".into(),
                inherit: Some(true),
                inheritable: Some(true),
                add: members,
                ..Default::default()
            }],
            acls: vec![
                rule("all", None, 0, access),
                rule("admin", None, access, 0),
                rule("", Some(MEMBER), access, 0),
            ],
            ..Default::default()
        }
    };
    // Added to the channel's admin group: the channel, its subchannel and
    // the member in it appear, parents first, then the people.
    f.handle(admin, kind::ACL, &acl(vec![REGISTERED]));
    assert_eq!(f.hidden("registered"), [] as [u32; 0]);
    let heard = Heard::of(&f.received(registered));
    assert_eq!(heard.channels, [SECRET, INNER]);
    assert_eq!(heard.users, [member]);
    // Removed: the people first, then the channels, children first.
    f.handle(admin, kind::ACL, &acl(Vec::new()));
    assert_eq!(f.hidden("registered"), [SECRET, INNER]);
    let heard = Heard::of(&f.received(registered));
    assert_eq!(heard.removed_users, [member]);
    assert_eq!(heard.removed_channels, [INNER, SECRET]);
}

/// The private ACL [`Fixture::add_secret`] stores, with `members` allowed by
/// name.
fn secret_acl(members: &[u32]) -> proto::Acl {
    let access = perm::TRAVERSE
        | perm::ENTER
        | perm::LISTEN
        | perm::SPEAK
        | perm::WHISPER
        | perm::TEXT_MESSAGE;
    let rule = |group: Option<&str>, user_id: Option<u32>, grant, deny| proto::acl::ChanAcl {
        apply_here: Some(true),
        apply_subs: Some(true),
        group: group.map(str::to_owned),
        user_id,
        grant: Some(grant),
        deny: Some(deny),
        ..Default::default()
    };
    let mut acls = vec![
        rule(Some("all"), None, 0, access),
        rule(Some("admin"), None, access, 0),
    ];
    acls.extend(members.iter().map(|id| rule(None, Some(*id), access, 0)));
    proto::Acl {
        channel_id: SECRET,
        inherit_acls: Some(true),
        acls,
        ..Default::default()
    }
}

#[test]
fn a_channel_created_with_private_rules_is_never_shown_to_others() {
    use prost::Message as _;
    let mut f =
        Fixture::new(Config::from_toml("message_limit = 1000\nmessage_burst = 1000\n").unwrap());
    let [_, admin, _, member, _, _, anonymous] = f.actors;
    for session in f.actors {
        f.received(session);
    }
    let create = |f: &mut Fixture, name: &str, rules: Vec<v1::Rule>| {
        let mut body = proto::ChannelState {
            parent: Some(0),
            name: Some(name.into()),
            ..Default::default()
        }
        .encode_to_vec();
        v1::ChannelStateExtension {
            nuschl: Some(v1::ChannelInfo {
                rules,
                ..Default::default()
            }),
        }
        .encode(&mut body)
        .unwrap();
        f.state.handle(
            admin,
            ControlFrame {
                kind: kind::CHANNEL_STATE,
                payload: Bytes::from(body),
            },
            &mut Datagrams::default(),
        );
        f.state
            .channels
            .0
            .values()
            .find(|channel| channel.name == name)
            .map(|channel| channel.id)
    };
    let rules: Vec<v1::Rule> = secret_acl(&[MEMBER])
        .acls
        .into_iter()
        .map(|acl| v1::Rule {
            apply_here: acl.apply_here(),
            apply_subs: acl.apply_subs(),
            user_id: acl.user_id,
            group: acl.group.clone().unwrap_or_default(),
            grant: acl.grant(),
            deny: acl.deny(),
        })
        .collect();
    let layoffs = create(&mut f, "Layoffs", rules.clone()).unwrap();
    assert!(!f.state.sees_channel(anonymous, layoffs));
    assert_eq!(Heard::of(&f.received(anonymous)), Heard::default());
    assert_eq!(Heard::of(&f.received(member)).channels, [layoffs]);
    // Rules past the editor's limits make no channel.
    assert_eq!(create(&mut f, "Many", vec![rules[0].clone(); 513]), None);
}

#[test]
fn a_new_channel_reaches_only_those_who_see_it() {
    let mut f = Fixture::new(Config::default());
    f.add_secret();
    let [_, admin, _, member, registered, _, anonymous] = f.actors;
    for session in f.actors {
        f.received(session);
    }
    f.handle(
        admin,
        kind::CHANNEL_STATE,
        &proto::ChannelState {
            parent: Some(INNER),
            name: Some("Deeper".into()),
            ..Default::default()
        },
    );
    let deeper = Heard::of(&f.received(member)).channels;
    assert_eq!(deeper.len(), 1);
    assert!(f.state.channels.get(deeper[0]).is_some());
    assert_eq!(Heard::of(&f.received(registered)), Heard::default());
    assert!(f.state.users[&registered].hidden.contains(&deeper[0]));
    // Removed again: only those who knew it hear of it.
    f.handle(
        admin,
        kind::CHANNEL_REMOVE,
        &proto::ChannelRemove {
            channel_id: deeper[0],
        },
    );
    assert_eq!(Heard::of(&f.received(member)).removed_channels, deeper);
    assert_eq!(Heard::of(&f.received(registered)), Heard::default());
    assert!(!f.state.users[&registered].hidden.contains(&deeper[0]));

    // A temporary channel in view: everyone who sees it sees the creator
    // move in, nobody sees it leave.
    f.received(anonymous);
    f.handle(
        registered,
        kind::CHANNEL_STATE,
        &proto::ChannelState {
            parent: Some(LOBBY),
            name: Some("Talk".into()),
            temporary: Some(true),
            ..Default::default()
        },
    );
    let talk = f.state.users[&registered].channel;
    assert!(f.state.channels.get(talk).is_some_and(|c| c.temporary));
    let heard = Heard::of(&f.received(anonymous));
    assert_eq!(
        (heard.channels, heard.users, heard.removed_users),
        (vec![talk], vec![registered], vec![])
    );
}

#[test]
fn requests_naming_a_hidden_channel_or_person_are_answered_as_for_missing_ones() {
    use prost::Message as _;
    let mut f =
        Fixture::new(Config::from_toml("message_limit = 1000\nmessage_burst = 1000\n").unwrap());
    f.add_secret();
    let [_, admin, _, member, registered, _, anonymous] = f.actors;
    f.state
        .channels
        .get_mut(SECRET)
        .unwrap()
        .set_description("behind the door".into());
    f.handle(
        member,
        kind::USER_STATE,
        &proto::UserState {
            channel_id: Some(SECRET),
            comment: Some("inside".into()),
            ..Default::default()
        },
    );
    assert!(!f.state.sees_user(registered, member));
    const MISSING_CHANNEL: u32 = 999;
    const MISSING_USER: u32 = 9999;
    type Request = (u16, Vec<u8>);
    let requests = |channel: u32, user: u32| -> Vec<Request> {
        let state = |message: proto::UserState| (kind::USER_STATE, message.encode_to_vec());
        let text = |message: proto::TextMessage| (kind::TEXT_MESSAGE, message.encode_to_vec());
        let named = |message: proto::ChannelState| (kind::CHANNEL_STATE, message.encode_to_vec());
        vec![
            (
                kind::PERMISSION_QUERY,
                proto::PermissionQuery {
                    channel_id: Some(channel),
                    ..Default::default()
                }
                .encode_to_vec(),
            ),
            (
                kind::REQUEST_BLOB,
                proto::RequestBlob {
                    channel_description: vec![channel],
                    session_comment: vec![user],
                    session_texture: vec![user],
                }
                .encode_to_vec(),
            ),
            state(proto::UserState {
                channel_id: Some(channel),
                ..Default::default()
            }),
            state(proto::UserState {
                listening_channel_add: vec![channel],
                ..Default::default()
            }),
            state(proto::UserState {
                session: Some(user),
                mute: Some(true),
                ..Default::default()
            }),
            state(proto::UserState {
                session: Some(user),
                channel_id: Some(LOBBY),
                ..Default::default()
            }),
            (
                kind::USER_STATS,
                proto::UserStats {
                    session: Some(user),
                    ..Default::default()
                }
                .encode_to_vec(),
            ),
            (
                kind::USER_REMOVE,
                proto::UserRemove {
                    session: user,
                    ..Default::default()
                }
                .encode_to_vec(),
            ),
            text(proto::TextMessage {
                channel_id: vec![channel],
                message: "hello".into(),
                ..Default::default()
            }),
            text(proto::TextMessage {
                tree_id: vec![channel],
                message: "hello".into(),
                ..Default::default()
            }),
            text(proto::TextMessage {
                session: vec![user],
                message: "hello".into(),
                ..Default::default()
            }),
            named(proto::ChannelState {
                channel_id: Some(channel),
                name: Some("Renamed".into()),
                ..Default::default()
            }),
            named(proto::ChannelState {
                parent: Some(channel),
                name: Some("Below".into()),
                temporary: Some(true),
                ..Default::default()
            }),
            named(proto::ChannelState {
                channel_id: Some(LOBBY),
                links_add: vec![channel],
                ..Default::default()
            }),
            (
                kind::CHANNEL_REMOVE,
                proto::ChannelRemove {
                    channel_id: channel,
                }
                .encode_to_vec(),
            ),
            (
                kind::ACL,
                proto::Acl {
                    channel_id: channel,
                    query: Some(true),
                    ..Default::default()
                }
                .encode_to_vec(),
            ),
        ]
    };
    let answers = |f: &mut Fixture, requests: Vec<Request>| -> Vec<Vec<(u16, Bytes)>> {
        requests
            .into_iter()
            .map(|(kind, body)| {
                f.state.handle(
                    registered,
                    ControlFrame {
                        kind,
                        payload: Bytes::from(body),
                    },
                    &mut Datagrams::default(),
                );
                f.received(registered)
            })
            .collect()
    };
    f.received(registered);
    let hidden = answers(&mut f, requests(SECRET, member));
    let missing = answers(&mut f, requests(MISSING_CHANNEL, MISSING_USER));
    assert_eq!(hidden, missing);
    assert_eq!(f.state.users[&member].channel, SECRET);
    assert!(!f.state.users[&member].mute);
    assert!(f.state.channels.get(SECRET).is_some());
    assert_eq!(
        f.state
            .authorize(registered, Action::Knock { channel: SECRET }),
        Err(Denial::NotFound)
    );

    // Whisper targets keep only what the client sees.
    f.handle(
        registered,
        kind::VOICE_TARGET,
        &proto::VoiceTarget {
            id: Some(1),
            targets: vec![
                proto::voice_target::Target {
                    session: vec![member, anonymous],
                    ..Default::default()
                },
                proto::voice_target::Target {
                    channel_id: Some(SECRET),
                    ..Default::default()
                },
            ],
        },
    );
    let target = &f.state.users[&registered].targets[&1];
    assert_eq!(
        (target.sessions.as_slice(), target.channels.len()),
        ([anonymous].as_slice(), 0)
    );
    // A message to someone seen and someone hidden reaches the one seen.
    f.received(anonymous);
    f.received(member);
    f.handle(
        registered,
        kind::TEXT_MESSAGE,
        &proto::TextMessage {
            session: vec![member, anonymous],
            message: "hello".into(),
            ..Default::default()
        },
    );
    assert!(
        f.received(anonymous)
            .iter()
            .any(|(kind, _)| *kind == kind::TEXT_MESSAGE)
    );
    assert!(f.received(member).is_empty());

    // A hidden sibling's name is taken, as Murmur has it: the same answer as
    // for a sibling in view.
    f.received(registered);
    let create = |f: &mut Fixture, name: &str| {
        f.handle(
            registered,
            kind::CHANNEL_STATE,
            &proto::ChannelState {
                parent: Some(0),
                name: Some(name.into()),
                temporary: Some(true),
                ..Default::default()
            },
        );
        f.received(registered)
    };
    let hidden = create(&mut f, "Secret");
    assert_eq!(hidden, create(&mut f, "Lobby"));
    let denied = hidden
        .into_iter()
        .find(|(kind, _)| *kind == kind::PERMISSION_DENIED)
        .map(|(_, body)| proto::PermissionDenied::decode(body).unwrap())
        .unwrap();
    assert_eq!(denied.r#type(), DenyType::ChannelName);
    assert_eq!(f.state.users[&registered].channel, LOBBY);

    // The registration list leaves out a last channel the reader does not see.
    f.state
        .registry
        .users
        .get_mut(&MEMBER)
        .unwrap()
        .last_channel = Some(SECRET);
    f.state.channels.get_mut(0).unwrap().node.acls.push(entry(
        "",
        Some(REGISTERED),
        perm::REGISTER,
        0,
    ));
    f.state.invalidate();
    let last_channel = |f: &mut Fixture, session: u32| {
        f.received(session);
        f.handle(session, kind::USER_LIST, &proto::UserList::default());
        f.received(session)
            .into_iter()
            .find(|(kind, _)| *kind == kind::USER_LIST)
            .map(|(_, body)| proto::UserList::decode(body).unwrap())
            .unwrap()
            .users
            .into_iter()
            .find(|user| user.user_id == MEMBER)
            .unwrap()
            .last_channel
    };
    assert_eq!(last_channel(&mut f, admin), Some(SECRET));
    assert_eq!(last_channel(&mut f, registered), None);
}

#[test]
fn nobody_hears_or_reads_someone_they_do_not_see() {
    use prost::Message as _;
    // Packets microseconds apart stay within the bandwidth limit.
    let mut f = Fixture::new(Config::from_toml("max_bandwidth = 1000000000\n").unwrap());
    f.add_secret();
    let [_, admin, _, member, registered, _, anonymous] = f.actors;
    let into = |channel: u32| proto::UserState {
        channel_id: Some(channel),
        ..Default::default()
    };
    f.handle(member, kind::USER_STATE, &into(SECRET));
    f.handle(admin, kind::USER_STATE, &into(LOBBY));
    for channel in [(SECRET, LOBBY), (LOBBY, SECRET)] {
        f.state
            .channels
            .get_mut(channel.0)
            .unwrap()
            .links
            .insert(channel.1);
    }
    f.state.invalidate();
    let heard = |f: &mut Fixture, session: u32, wanted: u16| {
        f.received(session)
            .into_iter()
            .filter(|(kind, _)| *kind == wanted)
            .map(|(_, body)| body)
            .collect::<Vec<_>>()
    };
    for session in f.actors {
        f.received(session);
    }

    // Voice into a linked channel, and a whisper by name, reach only those
    // who see the speaker.
    let voice = Bytes::from_static(&[0, 4 << 3, 1, (5 << 3) | 2, 3, 1, 2, 3]);
    let speak = |f: &mut Fixture, voice: &Bytes| {
        f.state.handle(
            member,
            ControlFrame {
                kind: kind::UDP_TUNNEL,
                payload: voice.clone(),
            },
            &mut Datagrams::default(),
        );
    };
    speak(&mut f, &voice);
    assert_eq!(heard(&mut f, admin, kind::UDP_TUNNEL).len(), 1);
    assert!(heard(&mut f, anonymous, kind::UDP_TUNNEL).is_empty());
    assert!(heard(&mut f, registered, kind::UDP_TUNNEL).is_empty());
    f.handle(
        member,
        kind::VOICE_TARGET,
        &proto::VoiceTarget {
            id: Some(1),
            targets: vec![proto::voice_target::Target {
                session: vec![anonymous, admin],
                ..Default::default()
            }],
        },
    );
    speak(
        &mut f,
        &Bytes::from_static(&[0, 1 << 3, 1, 4 << 3, 2, (5 << 3) | 2, 1, 9]),
    );
    assert_eq!(heard(&mut f, admin, kind::UDP_TUNNEL).len(), 1);
    assert!(heard(&mut f, anonymous, kind::UDP_TUNNEL).is_empty());

    // Typing in the lobby's conversation.
    let lobby = f.conversation(LOBBY);
    f.handle(
        member,
        nuschl::kind::TYPING,
        &v1::Typing {
            conversation: lobby.to_vec(),
            session: 0,
        },
    );
    assert_eq!(heard(&mut f, admin, nuschl::kind::TYPING).len(), 1);
    assert!(heard(&mut f, registered, nuschl::kind::TYPING).is_empty());

    // Plugin data.
    f.handle(
        member,
        kind::PLUGIN_DATA_TRANSMISSION,
        &proto::PluginDataTransmission {
            receiver_sessions: vec![anonymous, admin],
            data: Some(b"x".to_vec()),
            data_id: Some("game".into()),
            ..Default::default()
        },
    );
    assert_eq!(
        heard(&mut f, admin, kind::PLUGIN_DATA_TRANSMISSION).len(),
        1
    );
    assert!(heard(&mut f, anonymous, kind::PLUGIN_DATA_TRANSMISSION).is_empty());

    // Stock text: from someone hidden, to people seen, and naming a hidden
    // channel.
    for session in [admin, anonymous] {
        f.state.users.get_mut(&session).unwrap().extension = None;
    }
    let text = |channel_id: Vec<u32>, session: Vec<u32>| proto::TextMessage {
        channel_id,
        session,
        message: "hello".into(),
        ..Default::default()
    };
    f.handle(
        member,
        kind::TEXT_MESSAGE,
        &text(vec![LOBBY], vec![anonymous]),
    );
    assert_eq!(heard(&mut f, admin, kind::TEXT_MESSAGE).len(), 1);
    assert!(heard(&mut f, anonymous, kind::TEXT_MESSAGE).is_empty());
    f.handle(
        admin,
        kind::TEXT_MESSAGE,
        &text(vec![LOBBY, SECRET], vec![]),
    );
    let seen = heard(&mut f, anonymous, kind::TEXT_MESSAGE);
    assert_eq!(seen.len(), 1);
    let seen = proto::TextMessage::decode(seen[0].clone()).unwrap();
    assert_eq!(
        (seen.actor, seen.channel_id),
        (Some(admin), vec![LOBBY]),
        "the hidden channel is left out"
    );

    // Someone hidden who mutes someone seen is not named.
    f.state
        .channels
        .get_mut(LOBBY)
        .unwrap()
        .node
        .acls
        .push(entry("", Some(MEMBER), perm::MUTE_DEAFEN, 0));
    f.state.invalidate();
    f.handle(
        member,
        kind::USER_STATE,
        &proto::UserState {
            session: Some(registered),
            mute: Some(true),
            ..Default::default()
        },
    );
    let muted = |f: &mut Fixture, session: u32| {
        heard(f, session, kind::USER_STATE)
            .into_iter()
            .map(|body| proto::UserState::decode(body).unwrap())
            .find(|state| state.session == Some(registered))
            .map(|state| (state.mute, state.actor))
    };
    assert_eq!(muted(&mut f, admin), Some((Some(true), Some(member))));
    assert_eq!(muted(&mut f, anonymous), Some((Some(true), None)));
}

/// A direct message to someone whose only connection is in a hidden channel
/// is allowed as when they are offline: stored, and delivered to their
/// connections.
#[test]
fn a_direct_message_to_someone_in_a_hidden_channel_is_answered_as_when_they_are_offline() {
    let mut f =
        Fixture::new(Config::from_toml("message_limit = 1000\nmessage_burst = 1000\n").unwrap());
    f.add_secret();
    let [_, _, _, member, registered, _, _] = f.actors;
    f.handle(
        member,
        kind::USER_STATE,
        &proto::UserState {
            channel_id: Some(SECRET),
            ..Default::default()
        },
    );
    assert!(!f.state.sees_user(registered, member));
    let hidden = f.state.authorize(registered, Action::Send(&DIRECT));
    f.state.disconnect(member, true);
    let offline = f.state.authorize(registered, Action::Send(&DIRECT));
    assert_eq!(offline, Ok(()));
    assert_eq!(hidden, offline);
}

/// A volume adjustment for a hidden channel comes back as one for a missing
/// channel: left out of the echo.
#[test]
fn volume_adjustments_for_a_hidden_channel_are_answered_as_for_a_missing_one() {
    let mut f =
        Fixture::new(Config::from_toml("message_limit = 1000\nmessage_burst = 1000\n").unwrap());
    f.add_secret();
    let registered = f.actor("registered");
    let ask = |f: &mut Fixture, channel: u32| {
        f.received(registered);
        f.handle(
            registered,
            kind::USER_STATE,
            &proto::UserState {
                self_mute: Some(true),
                listening_volume_adjustment: vec![proto::user_state::VolumeAdjustment {
                    listening_channel: Some(channel),
                    volume_adjustment: Some(0.5),
                }],
                ..Default::default()
            },
        );
        f.received(registered)
    };
    assert!(!f.state.sees_channel(registered, SECRET));
    assert_eq!(ask(&mut f, SECRET), ask(&mut f, 999));
}

/// A requester admitted by a moderator it does not see is told it was
/// admitted, without the moderator's name.
#[test]
fn an_admission_notice_names_only_a_moderator_the_requester_sees() {
    use prost::Message as _;
    let mut f = Fixture::new(Config::default());
    f.add_secret();
    let requester = f.actor("registered");
    let moderator = f.actor("admin");
    f.handle(
        moderator,
        kind::USER_STATE,
        &proto::UserState {
            channel_id: Some(SECRET),
            ..Default::default()
        },
    );
    f.state.users.get_mut(&requester).unwrap().extension = None;
    f.state.users.get_mut(&moderator).unwrap().extension = None;
    f.handle(
        requester,
        kind::USER_STATE,
        &proto::UserState {
            channel_id: Some(PRIVATE),
            ..Default::default()
        },
    );
    f.received(requester);
    f.handle(
        moderator,
        kind::USER_STATE,
        &proto::UserState {
            session: Some(requester),
            channel_id: Some(PRIVATE),
            ..Default::default()
        },
    );
    assert_eq!(f.state.users[&requester].channel, PRIVATE);
    assert!(!f.state.sees_user(requester, moderator));
    let texts: Vec<String> = f
        .received(requester)
        .into_iter()
        .filter(|(kind, _)| *kind == kind::TEXT_MESSAGE)
        .map(|(_, body)| proto::TextMessage::decode(body).unwrap().message)
        .collect();
    assert!(
        texts.iter().any(|text| text.starts_with("Admitted;")),
        "{texts:?}"
    );
    assert!(
        !texts.iter().any(|text| text.contains("admin")),
        "{texts:?}"
    );
}

/// Someone seen who starts listening to a hidden channel: those who do not
/// see the channel hear nothing, as for a missing channel.
#[test]
fn listening_to_a_hidden_channel_sends_nothing_to_those_who_do_not_see_it() {
    let mut f =
        Fixture::new(Config::from_toml("message_limit = 1000\nmessage_burst = 1000\n").unwrap());
    f.add_secret();
    let [_, admin, _, _, registered, _, _] = f.actors;
    let listen = |f: &mut Fixture, channel: u32| {
        f.received(registered);
        f.handle(
            admin,
            kind::USER_STATE,
            &proto::UserState {
                listening_channel_add: vec![channel],
                ..Default::default()
            },
        );
        f.received(registered)
    };
    let missing = listen(&mut f, 999);
    let hidden = listen(&mut f, SECRET);
    assert!(f.state.users[&admin].listening.contains_key(&SECRET));
    assert!(f.state.sees_user(registered, admin));
    assert_eq!(hidden, missing);
}

// ---- nothing else decides ----

/// Within the server only authz evaluates ACLs (the engine is
/// `nuschl_core::acl`) and holds the cache; this also keeps the other inline
/// forms of a permission decision out of the rest of the server.
#[test]
fn nothing_outside_authz_decides_permissions() {
    let forbidden = [
        "effective_permissions",
        "group_applies",
        "self.permissions(",
        "state.permissions(",
        "can_read(",
        "can_write(",
        "kt_history_allowed",
        "user_id == Some(0)",
        "user_id != Some(0)",
        "Some(SUPERUSER)",
        "Some(crate::authz::SUPERUSER)",
    ];
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = vec![root.clone()];
    let mut offences = Vec::new();
    while let Some(path) = files.pop() {
        if path.is_dir() {
            if path == root.join("authz") {
                continue;
            }
            for entry in std::fs::read_dir(&path).expect("readable sources") {
                files.push(entry.expect("an entry").path());
            }
            continue;
        }
        if path.extension().is_none_or(|extension| extension != "rs")
            || path == root.join("authz.rs")
        {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("readable source");
        for (number, line) in text.lines().enumerate() {
            // A permission bit tested inline (`.has(session, channel, perm::…)`).
            let inline = line.contains(".has(") && line.contains("perm::");
            if !line.trim_start().starts_with("//")
                && (inline || forbidden.iter().any(|pattern| line.contains(pattern)))
            {
                offences.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }
    assert!(
        offences.is_empty(),
        "permission decisions outside authz.rs:\n{}",
        offences.join("\n")
    );
}
