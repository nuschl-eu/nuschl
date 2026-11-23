// SPDX-License-Identifier: EUPL-1.2
#![cfg(target_os = "linux")]

use nuschl_ipc::{
    crypto::epoch::{self, Direction, Update},
    rtc::{Codec, Command, Description, Event, LegRole, MAX_DATAGRAM, Purpose, Reply, Worker},
};
use std::{os::unix::net::UnixStream, path::PathBuf, time::Duration};
use tokio::{
    net::UdpSocket,
    time::{sleep, timeout},
};

const VIDEO: &[u8] = include_bytes!("fixtures/screen.av1");
const GENERATION: u64 = 12;
// A process runs at most two RTC workers at once.
static WORKERS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn command(worker: &mut Worker, cmd: Command) -> Reply {
    worker.exchange(cmd, Vec::new()).await.unwrap().kind
}

fn description(reply: Reply, publisher: u64) -> Description {
    let Reply::Description {
        mut description, ..
    } = reply
    else {
        panic!("expected a track description");
    };
    for track in &mut description.tracks {
        track.publisher = Some(publisher);
    }
    description
}

struct Leg {
    id: u32,
    sockets: [UdpSocket; 2],
    connected: [bool; 2],
}

impl Leg {
    async fn open(id: u32, publisher: u64, workers: &mut [&mut Worker; 2]) -> Self {
        let sockets = [
            UdpSocket::bind("127.0.0.1:0").await.unwrap(),
            UdpSocket::bind("127.0.0.1:0").await.unwrap(),
        ];
        for (worker, socket) in workers.iter_mut().zip(&sockets) {
            assert!(matches!(
                command(
                    worker,
                    Command::OpenLeg {
                        relayed: false,
                        leg: id,
                        local: socket.local_addr().unwrap(),
                        codec: Codec::Av1,
                        role: LegRole::Direct,
                    }
                )
                .await,
                Reply::Done
            ));
        }
        let offer = description(
            command(
                workers[0],
                Command::Offer {
                    leg: id,
                    id: [id as u8; 16],
                    purposes: vec![Purpose::ScreenVideo],
                },
            )
            .await,
            publisher,
        );
        let answer = description(
            command(
                workers[1],
                Command::AcceptOffer {
                    leg: id,
                    description: offer,
                },
            )
            .await,
            2,
        );
        assert!(matches!(
            command(
                workers[0],
                Command::AcceptAnswer {
                    leg: id,
                    description: answer
                }
            )
            .await,
            Reply::Done
        ));

        let mut leg = Self {
            id,
            sockets,
            connected: [false; 2],
        };
        timeout(Duration::from_secs(5), async {
            while leg.connected != [true; 2] {
                assert!(leg.pump(workers).await.is_none());
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("two connected ICE legs");
        leg
    }

    async fn pump(&mut self, workers: &mut [&mut Worker; 2]) -> Option<(Event, Vec<u8>)> {
        for (index, worker) in workers.iter_mut().enumerate() {
            let other = self.sockets[1 - index].local_addr().unwrap();
            let socket = &self.sockets[index];
            let mut buffer = [0; MAX_DATAGRAM + 1];
            for _ in 0..32 {
                match socket.try_recv_from(&mut buffer) {
                    Ok((length, source)) => {
                        assert_eq!(source, other);
                        assert!(matches!(
                            worker
                                .exchange(
                                    Command::Datagram {
                                        leg: self.id,
                                        source
                                    },
                                    buffer[..length].to_vec()
                                )
                                .await
                                .unwrap()
                                .kind,
                            Reply::Done
                        ));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("{error}"),
                }
            }
            for _ in 0..32 {
                let packet = worker
                    .exchange(Command::Poll { leg: self.id }, Vec::new())
                    .await
                    .unwrap();
                match packet.kind {
                    Reply::Wake { .. } => break,
                    Reply::Datagram { destination, .. } => {
                        assert_eq!(destination, other);
                        socket.send_to(&packet.data, other).await.unwrap();
                    }
                    Reply::Event {
                        event: Event::Connected,
                        ..
                    } => self.connected[index] = true,
                    Reply::Event {
                        event: event @ Event::Frame { .. },
                        ..
                    } => {
                        assert_eq!(index, 1);
                        return Some((event, packet.data));
                    }
                    Reply::Event { .. } => {}
                    _ => panic!("unexpected RTC poll reply"),
                }
            }
        }
        None
    }

    async fn frame(&mut self, workers: &mut [&mut Worker; 2]) -> (Event, Vec<u8>) {
        timeout(Duration::from_secs(5), async {
            loop {
                if let Some(frame) = self.pump(workers).await {
                    return frame;
                }
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("decrypted AV1 frame")
    }
}

async fn start(executable: &std::path::Path) -> (Worker, UnixStream) {
    let (tx, rx) = UnixStream::pair().unwrap();
    let worker = Worker::start(executable.into(), GENERATION, rx.into())
        .await
        .unwrap();
    (worker, tx)
}

#[tokio::test]
async fn confined_workers_switch_udp_legs_without_reusing_the_sframe_counter() {
    let _workers = WORKERS.lock().await;
    nuschl_testkit::sandbox();
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_nuschl-rtc-worker"));
    let (mut left, mut left_keys) = start(&executable).await;
    let (mut right, mut right_keys) = start(&executable).await;
    let mut workers = [&mut left, &mut right];
    let mut initial = Leg::open(nuschl_ipc::rtc::FIRST_DIRECT_LEG, 1, &mut workers).await;
    assert!(matches!(
        command(
            workers[0],
            Command::SetSendLegs {
                legs: vec![initial.id]
            }
        )
        .await,
        Reply::Rejected
    ));
    for (channel, peer, direction) in [
        (&mut left_keys, 2, Direction::Send),
        (&mut right_keys, 1, Direction::Receive),
    ] {
        epoch::write(
            channel,
            &Update {
                generation: GENERATION,
                peer,
                direction,
                epoch: 1,
                key: [37; 32],
            },
        )
        .unwrap();
    }
    timeout(Duration::from_secs(1), async {
        while !matches!(
            command(
                workers[0],
                Command::SetSendLegs {
                    legs: vec![initial.id]
                }
            )
            .await,
            Reply::Done
        ) {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();

    let write = |timestamp, key_frame| Command::Write {
        purpose: Purpose::ScreenVideo,
        timestamp,
        key_frame,
    };
    assert!(matches!(
        workers[0]
            .exchange(write(9000, false), VIDEO.to_vec())
            .await
            .unwrap()
            .kind,
        Reply::Rejected
    ));
    assert!(matches!(
        workers[0]
            .exchange(write(9000, true), VIDEO.to_vec())
            .await
            .unwrap()
            .kind,
        Reply::Done
    ));
    let frame = initial.frame(&mut workers).await;
    assert!(matches!(
        frame.0,
        Event::Frame {
            publisher: 1,
            purpose: Purpose::ScreenVideo,
            timestamp: 9000,
            key_frame: true,
            ..
        }
    ));
    assert_eq!(frame.1, VIDEO);

    let mut replacement = Leg::open(initial.id + 1, 1, &mut workers).await;
    assert!(matches!(
        command(
            workers[0],
            Command::SetSendLegs {
                legs: vec![replacement.id]
            }
        )
        .await,
        Reply::Done
    ));
    assert!(matches!(
        command(workers[0], Command::Poll { leg: initial.id }).await,
        Reply::Rejected
    ));
    assert!(matches!(
        workers[0]
            .exchange(write(18000, true), VIDEO.to_vec())
            .await
            .unwrap()
            .kind,
        Reply::Done
    ));
    let frame = replacement.frame(&mut workers).await;
    assert!(matches!(
        frame.0,
        Event::Frame {
            publisher: 1,
            purpose: Purpose::ScreenVideo,
            timestamp: 18000,
            key_frame: true,
            ..
        }
    ));
    assert_eq!(frame.1, VIDEO);

    right.stop().await.unwrap();
    drop(right_keys);
    // Revocation retires the worker while the normal control stream stays open.
    drop(left_keys);
    nuschl_testkit::none_running(&executable, Duration::from_secs(3)).await;
    left.stop().await.unwrap();
}

#[tokio::test]
async fn confined_worker_sends_each_frame_once_on_every_direct_leg() {
    let _workers = WORKERS.lock().await;
    nuschl_testkit::sandbox();
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_nuschl-rtc-worker"));
    let (mut sender, mut sender_keys) = start(&executable).await;
    let (mut receiver, mut receiver_keys) = start(&executable).await;
    // The receiver binds each leg to its own publisher, so each copy passes
    // its own SFrame replay window.
    for (sender, peer, direction) in [
        (true, 1, Direction::Send),
        (false, 1, Direction::Receive),
        (false, 3, Direction::Receive),
    ] {
        epoch::write(
            if sender {
                &mut sender_keys
            } else {
                &mut receiver_keys
            },
            &Update {
                generation: GENERATION,
                peer,
                direction,
                epoch: 1,
                key: [41; 32],
            },
        )
        .unwrap();
    }
    let mut workers = [&mut sender, &mut receiver];
    let id = nuschl_ipc::rtc::FIRST_DIRECT_LEG;
    let mut legs = [
        Leg::open(id, 1, &mut workers).await,
        Leg::open(id + 1, 3, &mut workers).await,
    ];
    let set = |legs| Command::SetSendLegs { legs };
    for refused in [vec![id, id], vec![id, nuschl_ipc::rtc::SFU_PUBLISH_LEG]] {
        assert!(matches!(
            command(workers[0], set(refused)).await,
            Reply::Rejected
        ));
    }
    timeout(Duration::from_secs(1), async {
        while !matches!(
            command(workers[0], set(vec![id, id + 1])).await,
            Reply::Done
        ) {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let write = |timestamp| Command::Write {
        purpose: Purpose::ScreenVideo,
        timestamp,
        key_frame: true,
    };
    assert!(matches!(
        workers[0]
            .exchange(write(9000), VIDEO.to_vec())
            .await
            .unwrap()
            .kind,
        Reply::Done
    ));
    for (leg, publisher) in legs.iter_mut().zip([1, 3]) {
        let (event, data) = leg.frame(&mut workers).await;
        assert_eq!(
            event,
            Event::Frame {
                publisher,
                purpose: Purpose::ScreenVideo,
                timestamp: 9000,
                contiguous: true,
                key_frame: true,
            }
        );
        assert_eq!(data, VIDEO);
    }

    // Narrowing the set retires the dropped direct leg in the same step.
    assert!(matches!(
        command(workers[0], set(vec![id])).await,
        Reply::Done
    ));
    assert!(matches!(
        command(workers[0], Command::Poll { leg: id + 1 }).await,
        Reply::Rejected
    ));
    assert!(matches!(
        workers[0]
            .exchange(write(18000), VIDEO.to_vec())
            .await
            .unwrap()
            .kind,
        Reply::Done
    ));
    let (event, _) = legs[0].frame(&mut workers).await;
    assert!(matches!(
        event,
        Event::Frame {
            timestamp: 18000,
            ..
        }
    ));
    sender.stop().await.unwrap();
    receiver.stop().await.unwrap();
}
