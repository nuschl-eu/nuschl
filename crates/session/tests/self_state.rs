// SPDX-License-Identifier: EUPL-1.2
//! Our self-mute against a stock Murmur on loopback. Murmur drops a user's
//! own UserState beyond its rate limit (a burst of 5, then 1 a second) and
//! answers nothing, so rapid toggles can leave the server showing another
//! state than the one the user chose last.
use nuschl_session::{
    config::{Profile, ServerBookmark},
    connection::{self, Command, ConnectOptions, MessageRate, SessionEvent},
};
use nuschl_testkit::murmur::Murmur;
use std::{collections::BTreeMap, path::Path, time::Duration};
use tokio::sync::mpsc;

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
        password: None,
        force_tcp: true,
        relay_only: false,
        muted: false,
        deafened: false,
        channel: None,
        resend: Vec::new(),
        message_rate: MessageRate::default(),
        required: Default::default(),
        crypto: None,
    }
}

struct Session {
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<SessionEvent>,
    session: u32,
    /// Each user's self-mute as this session last heard it.
    self_muted: BTreeMap<u32, bool>,
    task: tokio::task::JoinHandle<Result<(), nuschl_session::Error>>,
}

impl Session {
    async fn join(port: u16, name: &str, pin: &str, profile: &Path) -> Self {
        let (commands, receiver) = mpsc::channel(64);
        let (sender, mut events) = mpsc::channel(1024);
        let task = tokio::spawn(connection::run(
            options(port, name, Some(pin.into()), profile),
            receiver,
            sender,
        ));
        let mut self_muted = BTreeMap::new();
        let session = loop {
            match tokio::time::timeout(Duration::from_secs(60), events.recv()).await {
                Ok(Some(SessionEvent::Connected { session, .. })) => break session,
                Ok(Some(SessionEvent::User(user))) => {
                    self_muted.insert(user.session, user.self_muted);
                }
                Ok(Some(_)) => {}
                other => panic!("{name} did not connect: {other:?}"),
            }
        };
        Self {
            commands,
            events,
            session,
            self_muted,
            task,
        }
    }

    async fn set_muted(&self, muted: bool) {
        let command = Command::SetSelf {
            muted,
            deafened: false,
        };
        self.commands.send(command).await.unwrap();
    }

    /// `session`'s self-mute as this session hears it, once it is
    /// `expected` after `settle`, or after `within`.
    async fn self_muted(
        &mut self,
        session: u32,
        expected: bool,
        (settle, within): (Duration, Duration),
    ) -> Option<bool> {
        let start = tokio::time::Instant::now();
        while (start.elapsed() < settle || self.self_muted.get(&session) != Some(&expected))
            && let Ok(Some(event)) =
                tokio::time::timeout_at(start + within, self.events.recv()).await
        {
            if let SessionEvent::User(user) = event {
                self.self_muted.insert(user.session, user.self_muted);
            }
        }
        self.self_muted.get(&session).copied()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Lets Murmur's bucket for our messages run empty (it drains whole tokens).
const DRAIN: Duration = Duration::from_secs(6);
/// Long enough for the echoes of the messages Murmur took to arrive.
const SETTLE: Duration = Duration::from_secs(1);
const WITHIN: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread")]
async fn the_server_ends_on_the_self_mute_chosen_last_despite_its_rate_limit() {
    let murmur = Murmur::start();
    let profiles = nuschl_testkit::temp_dir();
    let (talker_profile, observer_profile) = (
        profiles.path().join("talker"),
        profiles.path().join("observer"),
    );
    let (_commands, receiver) = mpsc::channel(4);
    let (events, _events) = mpsc::channel(256);
    let pin = match connection::run(
        options(murmur.port, "Probe", None, &talker_profile),
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
    let talker = Session::join(murmur.port, "Talker", &pin, &talker_profile).await;
    let mut observer = Session::join(murmur.port, "Observer", &pin, &observer_profile).await;
    let shown = observer
        .self_muted(talker.session, false, (Duration::ZERO, WITHIN))
        .await;
    assert_eq!(shown, Some(false), "the talker joined unmuted");
    // The microphone stops and mutes, the user unmutes, and toggles twice
    // more to bring it back: the last Unmute is the sixth message, beyond
    // the burst.
    tokio::time::sleep(DRAIN).await;
    for muted in [true, false, true, false, true, false] {
        talker.set_muted(muted).await;
    }
    let shown = observer
        .self_muted(talker.session, false, (SETTLE, WITHIN))
        .await;
    assert_eq!(
        shown,
        Some(false),
        "the server shows the talker muted after the Unmute"
    );
    // The same with a Mute last: the server must not show the talker open.
    tokio::time::sleep(DRAIN).await;
    for muted in [false, true, false, true, false, true] {
        talker.set_muted(muted).await;
    }
    let shown = observer
        .self_muted(talker.session, true, (SETTLE, WITHIN))
        .await;
    assert_eq!(
        shown,
        Some(true),
        "the server shows the talker unmuted after the Mute"
    );
}
