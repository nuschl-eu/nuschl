// SPDX-License-Identifier: EUPL-1.2
//! Moving someone else to another channel against a stock Murmur on
//! loopback: the SuperUser holds Move and the move happens; a plain user
//! lacks it and Murmur refuses.
use nuschl_session::{
    config::{Profile, ServerBookmark},
    connection::{self, Command, ConnectOptions, MessageRate, SessionEvent},
    model::UserAction,
};
use nuschl_testkit::murmur::Murmur;
use std::{collections::BTreeMap, path::Path, time::Duration};
use tokio::sync::mpsc;

const PASSWORD: &str = "move-test";
const WITHIN: Duration = Duration::from_secs(60);

fn options(port: u16, name: &str, pin: Option<String>, profile: &Path) -> ConnectOptions {
    let mut server = ServerBookmark::new(name, "127.0.0.1", port, name).unwrap();
    server.certificate_sha256 = pin;
    ConnectOptions {
        voice_route: Default::default(),
        server,
        identity: Profile::open(Some(profile.to_owned()))
            .unwrap()
            .identity()
            .unwrap(),
        password: (name == "SuperUser").then(|| PASSWORD.into()),
        force_tcp: true,
        relay_only: false,
        muted: true,
        deafened: false,
        channel: None,
        resend: Vec::new(),
        message_rate: MessageRate::default(),
        required: Default::default(),
        crypto: None,
    }
}

struct Session {
    name: &'static str,
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<SessionEvent>,
    session: u32,
    /// Each user's channel as this session last heard it.
    channels: BTreeMap<u32, u32>,
    denials: Vec<String>,
    task: tokio::task::JoinHandle<Result<(), nuschl_session::Error>>,
}

impl Session {
    async fn join(port: u16, name: &'static str, pin: &str, profiles: &Path) -> Self {
        let (commands, receiver) = mpsc::channel(64);
        let (sender, events) = mpsc::channel(1024);
        let task = tokio::spawn(connection::run(
            options(port, name, Some(pin.into()), &profiles.join(name)),
            receiver,
            sender,
        ));
        let mut session = Self {
            name,
            commands,
            events,
            session: 0,
            channels: BTreeMap::new(),
            denials: Vec::new(),
            task,
        };
        session.session = session
            .until(|event| match event {
                SessionEvent::Connected { session, .. } => Some(*session),
                _ => None,
            })
            .await;
        session
    }

    /// The first event `wanted` picks, keeping track of channels and denials.
    async fn until<T>(&mut self, mut wanted: impl FnMut(&SessionEvent) -> Option<T>) -> T {
        loop {
            let event = match tokio::time::timeout(WITHIN, self.events.recv()).await {
                Ok(Some(event)) => event,
                other => panic!("{}: no expected event: {other:?}", self.name),
            };
            match &event {
                SessionEvent::User(user) => {
                    self.channels.insert(user.session, user.channel);
                }
                SessionEvent::PermissionDenied(text) => self.denials.push(text.clone()),
                _ => {}
            }
            if let Some(found) = wanted(&event) {
                return found;
            }
        }
    }

    async fn send(&self, command: Command) {
        self.commands.send(command).await.unwrap();
    }

    fn move_user(session: u32, channel: u32) -> Command {
        Command::User {
            session,
            action: UserAction::Move(channel),
            reason: None,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_takes_effect_with_move_and_is_refused_without() {
    let murmur = Murmur::start_with_superuser(Some(PASSWORD));
    let profiles = nuschl_testkit::temp_dir();
    let (_commands, receiver) = mpsc::channel(4);
    let (events, _events) = mpsc::channel(256);
    let pin = match connection::run(
        options(murmur.port, "Probe", None, &profiles.path().join("Probe")),
        receiver,
        events,
    )
    .await
    {
        Err(nuschl_session::Error::Certificate { sha256, .. }) => sha256,
        other => panic!(
            "expected the certificate question, got {other:?}: {}",
            murmur.log()
        ),
    };
    let mut admin = Session::join(murmur.port, "SuperUser", &pin, profiles.path()).await;
    admin
        .send(Command::CreateChannel {
            parent: 0,
            name: "Elsewhere".into(),
            temporary: false,
            rules: Vec::new(),
        })
        .await;
    let elsewhere = admin
        .until(|event| match event {
            SessionEvent::Channel(channel) if channel.name == "Elsewhere" => Some(channel.id),
            _ => None,
        })
        .await;
    let target = Session::join(murmur.port, "Target", &pin, profiles.path()).await;
    let mut bystander = Session::join(murmur.port, "Bystander", &pin, profiles.path()).await;
    let moved = |session: u32, channel: u32| {
        move |event: &SessionEvent| match event {
            SessionEvent::User(user) if user.session == session && user.channel == channel => {
                Some(())
            }
            _ => None,
        }
    };
    if bystander.channels.get(&target.session) != Some(&0) {
        bystander.until(moved(target.session, 0)).await;
    }

    // Without Move, Murmur refuses and Target stays in the root.
    bystander
        .send(Session::move_user(target.session, elsewhere))
        .await;
    let denial = bystander
        .until(|event| match event {
            SessionEvent::PermissionDenied(text) => Some(text.clone()),
            _ => None,
        })
        .await;
    assert_eq!(denial, "Permission denied: Move", "{}", murmur.log());
    assert_eq!(bystander.channels.get(&target.session), Some(&0));

    // The SuperUser holds Move: everyone sees Target in Elsewhere.
    admin
        .send(Session::move_user(target.session, elsewhere))
        .await;
    bystander.until(moved(target.session, elsewhere)).await;
    assert!(admin.denials.is_empty(), "{:?}", admin.denials);
    assert_eq!(bystander.denials, [denial]);
}
