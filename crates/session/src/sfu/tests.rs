// SPDX-License-Identifier: EUPL-1.2

use super::*;
use nuschl_protocol::sfu::Role;
use tokio::net::TcpListener;

mod direct;

const CA: &str = "127.0.0.1";
const LINK: &str = "v=1&k=AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8&h=8FkoEkvVnGXqAGKF9u7ezIde49CL2ZPe5AQZJ3_d3tQ";
const OWNER: &str = "ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8";
static MEDIA_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn fixture(name: &str) -> std::path::PathBuf {
    // Cargo's workspace test builds standalone binaries beside the deps folder.
    // The Nix unit leaf supplies the same binaries on PATH.
    let current = std::env::current_exe().unwrap();
    if let Some(built) = current
        .parent()
        .and_then(|deps| deps.parent())
        .map(|bin| bin.join(name))
        && built.is_file()
    {
        return built;
    }
    nuschl_testkit::program(name)
}

/// Owned loopback TLS terminator in front of the real SFU's HTTP listener.
async fn tls_proxy(sfu: &nuschl_testkit::sfu::Sfu) -> (String, rustls::RootCertStore) {
    let upstream: std::net::SocketAddr = sfu
        .url
        .strip_prefix("ws://")
        .unwrap()
        .strip_suffix("/sfu")
        .unwrap()
        .parse()
        .unwrap();
    let listener = TcpListener::bind((CA, 0)).await.unwrap();
    let host = listener.local_addr().unwrap().to_string();
    let cert = rcgen::generate_simple_self_signed(vec![CA.into()]).unwrap();
    let der = cert.cert.der().clone();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der.clone()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![der],
        rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
    )
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(mut tls) = acceptor.accept(socket).await {
                    let mut plain = TcpStream::connect(upstream).await.unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut tls, &mut plain).await;
                }
            });
        }
    });
    (host, roots)
}

async fn next_key(connection: &mut Connection) -> Outcome {
    timeout(HANDSHAKE, async {
        loop {
            if let ServerMessage::Key { from, data } = connection.next().await.unwrap() {
                return connection
                    .signal(Action::Key {
                        peer: from,
                        data: hex::decode(data).unwrap(),
                    })
                    .await
                    .unwrap();
            }
        }
    })
    .await
    .unwrap()
}

async fn retired(worker: &CryptoHandle, generation: u64) {
    timeout(HANDSHAKE, async {
        loop {
            let response = worker
                .call_signal(Request {
                    generation,
                    action: Action::Admit { nonce: [0; 32] },
                })
                .await;
            if let Err(error) = response {
                assert!(error.to_string().contains("no open call"), "{error}");
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn two_native_peers_admit_and_signal_through_the_sfu_over_wss() {
    nuschl_testkit::sandbox();
    let worker = fixture("nuschl-crypto-worker");
    let sfu = nuschl_testkit::sfu::start(fixture("nuschl-stream-sfu"), "");
    let (host, roots) = tls_proxy(&sfu).await;
    let directory = nuschl_testkit::temp_dir();
    let alice = CryptoHandle::start(worker.clone(), directory.path().join("alice"));
    let bob_worker = CryptoHandle::start(worker, directory.path().join("bob"));
    let posted = format!("https://{host}/call#{LINK}");
    let owner = format!("{posted}&p={OWNER}");
    let connect = |generation, link| Connect {
        generation,
        host: &host,
        link,
        codecs: &[Codec::Av1, Codec::Vp9],
        policy: PeerPolicy::default(),
    };
    assert!(matches!(
        Connection::connect(alice.clone(), connect(1, &owner)).await,
        Err(Error::Io(_))
    ));
    let mut alice = Connection::connect_with_roots(alice, connect(2, &owner), roots.clone())
        .await
        .unwrap();
    let mut bob =
        Connection::connect_with_roots(bob_worker.clone(), connect(1, &posted), roots.clone())
            .await
            .unwrap();
    let (a, b) = (alice.welcome().peer, bob.welcome().peer);
    assert_eq!(alice.welcome().role, Role::Host);
    assert_eq!(bob.welcome().role, Role::Participant);

    alice.signal(Action::Publish { peer: a }).await.unwrap();
    bob.signal(Action::Subscribe { peer: a }).await.unwrap();
    assert!(matches!(next_key(&mut alice).await, Outcome::Done));
    timeout(HANDSHAKE, async {
        loop {
            if matches!(
                alice.signal(Action::Tick).await.unwrap(),
                Outcome::KeyBatch { .. }
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        matches!(next_key(&mut bob).await, Outcome::Send { peer, ready: true, .. } if peer == a)
    );
    assert!(matches!(next_key(&mut alice).await, Outcome::Ready { peer } if peer == b));

    let candidate = b"a=candidate:1 1 UDP 1 127.0.0.1 54321 typ host";
    alice
        .signal(Action::Seal {
            peer: b,
            publisher: true,
            data: candidate.to_vec(),
        })
        .await
        .unwrap();
    assert!(
        matches!(next_key(&mut bob).await, Outcome::Signal { peer, publisher: false, data } if peer == a && data == candidate)
    );
    drop(bob);
    retired(&bob_worker, 1).await;
    let bob = Connection::connect_with_roots(bob_worker.clone(), connect(2, &posted), roots)
        .await
        .unwrap();
    assert_eq!(bob.welcome().role, Role::Participant);
    {
        let _media = MEDIA_TEST.lock().await;
        let mut bob = bob
            .into_media(MediaOptions {
                executable: fixture("nuschl-rtc-worker"),
                sfu_port: sfu.media.port(),
                publish: None,
            })
            .await
            .unwrap();
        write(&mut alice.socket, &ClientMessage::End).await.unwrap();
        let event = timeout(HANDSHAKE, async {
            loop {
                match bob.next().await {
                    Some(Ok(MediaEvent::Route(_))) => {}
                    event => return event,
                }
            }
        })
        .await
        .unwrap();
        assert!(matches!(event, Some(Err(Error::Refused(reason))) if reason == "ended"));
        bob.stop().await.unwrap();
    }
    alice.close().await.unwrap();

    let listener = TcpListener::bind((CA, 0)).await.unwrap();
    let stalled_host = listener.local_addr().unwrap().to_string();
    let attempt = tokio::spawn({
        let worker = bob_worker.clone();
        async move {
            let link = format!("https://{stalled_host}/call#{LINK}");
            Connection::connect(
                worker,
                Connect {
                    generation: 3,
                    host: &stalled_host,
                    link: &link,
                    codecs: &[Codec::Av1],
                    policy: PeerPolicy::default(),
                },
            )
            .await
        }
    });
    // A connection attempt opens the worker slot before dialing the SFU.
    let _stalled = timeout(HANDSHAKE, listener.accept())
        .await
        .unwrap()
        .unwrap();
    attempt.abort();
    assert!(matches!(attempt.await, Err(error) if error.is_cancelled()));
    retired(&bob_worker, 3).await;
}

async fn red_keyframe(executable: std::path::PathBuf) -> Vec<u8> {
    use nuschl_ipc::video::{Encoder, Output, Settings, Source};

    let mut encoder = Encoder::start(
        executable,
        Settings {
            source: Source::Screen,
            width: 32,
            height: 32,
            fps: 30,
            bitrate: 256_000,
        },
    )
    .unwrap();
    // Limited-range I420 for a uniform red picture.
    let mut pixels = vec![81; 32 * 32];
    pixels.extend([90; 16 * 16]);
    pixels.extend([240; 16 * 16]);
    let Output::Keyframe(red) = encoder.encode(pixels).await.unwrap() else {
        panic!("expected a synthetic red keyframe");
    };
    encoder.stop().await.unwrap();
    red
}

#[tokio::test]
#[ignore = "runs under vm-call-page with the stream host and Chromium"]
async fn native_publisher_in_call_page_vm() {
    use rustls::pki_types::{CertificateDer, pem::PemObject as _};
    use std::{env, fs, path::PathBuf};

    nuschl_testkit::sandbox();
    let host = env::var("NUSCHL_NATIVE_CALL_HOST").unwrap();
    let link = env::var("NUSCHL_NATIVE_CALL_LINK").unwrap();
    let sfu_port = env::var("NUSCHL_NATIVE_CALL_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let state = PathBuf::from(env::var_os("NUSCHL_NATIVE_CALL_STATE").unwrap());
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_file(env::var_os("NUSCHL_NATIVE_CALL_CA").unwrap()).unwrap())
        .unwrap();
    let directory = nuschl_testkit::temp_dir();
    let red = red_keyframe(fixture("nuschl-video-worker")).await;
    let call = Connection::connect_with_roots(
        CryptoHandle::start(
            fixture("nuschl-crypto-worker"),
            directory.path().join("keys"),
        ),
        Connect {
            generation: 1,
            host: &host,
            link: &link,
            codecs: &[Codec::Av1],
            policy: PeerPolicy::default(),
        },
        roots,
    )
    .await
    .unwrap();
    let peer = call.welcome().peer;
    let mut media = call
        .into_media(MediaOptions {
            executable: fixture("nuschl-rtc-worker"),
            sfu_port,
            publish: Some(Purpose::Camera),
        })
        .await
        .unwrap();
    media.switch_source(Purpose::ScreenVideo).unwrap();
    fs::write(state.join("peer"), peer.to_string()).unwrap();

    let mut tick = tokio::time::interval(Duration::from_millis(34));
    let mut timestamp = 0;
    let mut ready = false;
    let stop = state.join("stop");
    let switch = state.join("switch");
    let mut switched = false;
    timeout(Duration::from_secs(180), async {
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if stop.exists() { break; }
                    if ready && !switched && switch.exists() {
                        media.switch_source(Purpose::Camera).unwrap();
                        ready = false;
                        switched = true;
                        timestamp = 0;
                    }
                    if ready {
                        timestamp += 3_060;
                        media.send_keyframe(timestamp, red.clone()).unwrap();
                    }
                }
                event = media.next() => {
                    ready |= matches!(event.expect("publisher remains live").expect("RTC publisher"), MediaEvent::SourceReady { .. });
                }
            }
        }
    }).await.expect("browser observed the native publisher");
    media.stop().await.unwrap();
}

#[tokio::test]
async fn native_video_crosses_the_sfu_between_confined_rtc_workers() {
    let _media = MEDIA_TEST.lock().await;
    native_video_for_policy(false).await;
    native_video_for_policy(true).await;
}

async fn native_video_for_policy(relay_only: bool) {
    use nuschl_ipc::video::{Decoder, Source};

    // Successive AV1 packets from a synthetic 32×32 color source.
    const KEY: [u8; 30] = [
        0x12, 0x00, 0x0a, 0x0a, 0x00, 0x00, 0x00, 0x02, 0x27, 0xfe, 0x6d, 0x7c, 0x80, 0x20, 0x32,
        0x0e, 0x10, 0x00, 0x8c, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x3e, 0xfc, 0xc1, 0x08,
    ];
    const DELTAS: [[u8; 21]; 3] = [
        [
            0x12, 0, 0x32, 0x11, 0x30, 3, 0xc0, 0x80, 0, 0, 0x46, 0xb8, 0, 0, 2, 0x40, 0, 0x20, 0,
            0x8c, 0x70,
        ],
        [
            0x12, 0, 0x32, 0x11, 0x30, 4, 1, 4, 0, 0, 0x46, 0xb8, 0, 0, 2, 0x40, 0, 0x20, 0, 0x89,
            0x40,
        ],
        [
            0x12, 0, 0x32, 0x11, 0x30, 6, 2, 8, 0x80, 0, 0x46, 0xb8, 0, 0, 2, 0x40, 0, 0x20, 0,
            0x86, 0x80,
        ],
    ];
    const FRAME_INTERVAL: Duration = Duration::from_millis(34);

    async fn send_until_keyframe(
        sender: &mut Media,
        viewer: &mut Media,
        publisher: u64,
        frame: &mut u64,
        expected: &[u8],
    ) -> Vec<u8> {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut latest = None;
        loop {
            tokio::select! {
                biased;
                (timestamp, data) = rendered(viewer, publisher, true) => {
                    // The next delta uses the latest submitted reference.
                    if Some(timestamp) == latest && data == expected {
                        return data;
                    }
                }
                event = sender.next() => {
                    event.expect("publisher remains live").expect("RTC publisher");
                }
                _ = tick.tick() => {
                    *frame += 1;
                    let timestamp = *frame * 3_060;
                    latest = sender.send_keyframe(timestamp, expected.to_vec()).unwrap()
                        .then_some(timestamp);
                    tick.reset();
                }
            }
        }
    }

    async fn rendered(media: &mut Media, publisher: u64, key: bool) -> (u64, Vec<u8>) {
        loop {
            if let MediaEvent::Frame {
                publisher: sender,
                purpose,
                key_frame,
                timestamp,
                data,
                ..
            } = media
                .next()
                .await
                .expect("viewer remains live")
                .expect("RTC viewer")
            {
                assert_eq!(sender, publisher);
                assert_eq!(purpose, Purpose::ScreenVideo);
                if key_frame == key {
                    return (timestamp, data);
                }
            }
        }
    }
    nuschl_testkit::sandbox();
    let crypto = fixture("nuschl-crypto-worker");
    let rtc = fixture("nuschl-rtc-worker");
    let video = fixture("nuschl-video-worker");
    let turn = relay_only.then(nuschl_testkit::turn::start);
    let sfu = match &turn {
        Some(turn) => {
            nuschl_testkit::sfu::start_with_turn(fixture("nuschl-stream-sfu"), "", &turn.url)
        }
        None => nuschl_testkit::sfu::start(fixture("nuschl-stream-sfu"), ""),
    };
    let (host, roots) = tls_proxy(&sfu).await;
    let directory = nuschl_testkit::temp_dir();
    let posted = format!("https://{host}/call#{LINK}");
    let owner = format!("{posted}&p={OWNER}");
    let connect = |link| Connect {
        generation: 1,
        host: &host,
        link,
        codecs: &[Codec::Av1],
        policy: PeerPolicy { relay_only },
    };
    let mut alice = Connection::connect_with_roots(
        CryptoHandle::start(crypto.clone(), directory.path().join("alice")),
        connect(&owner),
        roots.clone(),
    )
    .await
    .unwrap();
    if relay_only {
        timeout(
            HANDSHAKE,
            relay_datagrams(alice.welcome().turn.as_ref().unwrap()),
        )
        .await
        .unwrap();
    }
    let bob = Connection::connect_with_roots(
        CryptoHandle::start(crypto, directory.path().join("bob")),
        connect(&posted),
        roots,
    )
    .await
    .unwrap();
    // The viewer can join the room while its owner is choosing a screen.
    tokio::time::timeout(Duration::from_secs(5), async {
        while alice.welcome().peers.is_empty() {
            alice.poll_idle().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(alice.welcome().peers[0].id, bob.welcome().peer);
    // In the relay-only run the invited participant presents to an idle host.
    let (sender, receiver) = if relay_only {
        (bob, alice)
    } else {
        (alice, bob)
    };
    let publisher = sender.welcome().peer;
    let mut alice = sender
        .into_media(MediaOptions {
            executable: rtc.clone(),
            sfu_port: sfu.media.port(),
            publish: Some(Purpose::ScreenVideo),
        })
        .await
        .unwrap();
    let mut bob = receiver
        .into_media(MediaOptions {
            executable: rtc.clone(),
            sfu_port: sfu.media.port(),
            publish: None,
        })
        .await
        .unwrap();

    let mut decoder = Decoder::start(video.clone(), Source::Screen).unwrap();
    let red = red_keyframe(video.clone()).await;

    let mut phase = "publisher readiness";
    let playback = timeout(HANDSHAKE, async {
        loop {
            if matches!(
                alice
                    .next()
                    .await
                    .expect("publisher remains live")
                    .expect("RTC publisher"),
                MediaEvent::SourceReady { .. }
            ) {
                break;
            }
        }
        phase = "initial keyframe";
        let mut frame = 0u64;
        let initial = loop {
            let key = send_until_keyframe(&mut alice, &mut bob, publisher, &mut frame, &KEY).await;
            if let Some(frame) = decoder.decode(true, key).await.unwrap() {
                break frame;
            }
        };
        // Each dependent frame follows its acknowledged reference at capture cadence.
        tokio::time::sleep(FRAME_INTERVAL).await;
        frame += 1;
        assert!(
            alice
                .send_frame(frame * 3_060, false, DELTAS[0].to_vec())
                .unwrap()
        );
        assert!(
            !alice
                .send_frame((frame + 1) * 3_060, false, DELTAS[1].to_vec())
                .unwrap()
        );
        phase = "first dependent frame";
        let (_, delta) = rendered(&mut bob, publisher, false).await;
        assert_eq!(delta, DELTAS[0]);
        let next = decoder
            .decode(false, delta)
            .await
            .unwrap()
            .expect("decoded interframe");
        assert_eq!((initial.width, initial.height), (32, 32));
        assert_eq!(next.bgra, initial.bgra);

        // Dropping an input frame invalidates later dependent frames until a
        // fresh keyframe. The RTC worker remains live through the recovery.
        tokio::time::sleep(FRAME_INTERVAL).await;
        frame += 2;
        assert!(
            alice
                .send_frame(frame * 3_060, false, DELTAS[2].to_vec())
                .unwrap()
        );
        assert!(
            timeout(
                Duration::from_millis(200),
                rendered(&mut bob, publisher, false)
            )
            .await
            .is_err()
        );
        phase = "keyframe request after a dropped sample";
        loop {
            if matches!(
                alice
                    .next()
                    .await
                    .expect("publisher remains live")
                    .expect("RTC publisher"),
                MediaEvent::SourceKeyframeRequired { .. }
            ) {
                break;
            }
        }
        phase = "recovery keyframe";
        send_until_keyframe(&mut alice, &mut bob, publisher, &mut frame, &KEY).await;
        tokio::time::sleep(FRAME_INTERVAL).await;
        frame += 1;
        assert!(
            alice
                .send_frame(frame * 3_060, false, DELTAS[0].to_vec())
                .unwrap()
        );
        phase = "dependent frame after recovery";
        assert_eq!(rendered(&mut bob, publisher, false).await.1, DELTAS[0]);

        tokio::time::sleep(FRAME_INTERVAL).await;
        phase = "second picture";
        let packet = send_until_keyframe(&mut alice, &mut bob, publisher, &mut frame, &red).await;
        let decoded = decoder
            .decode(true, packet)
            .await
            .unwrap()
            .expect("decoded red keyframe");
        assert_eq!((decoded.width, decoded.height), (32, 32));
        assert_eq!(decoded.bgra.len(), 32 * 32 * 4);
        assert!(
            decoded
                .bgra
                .as_chunks::<4>()
                .0
                .iter()
                .all(|&[b, g, r, a]| b < 45 && g < 45 && r > 200 && a == 255)
        );
    })
    .await;
    if playback.is_ok() && !relay_only {
        async fn encoder_limits(publisher: &mut Media, tier: usize) {
            let expected = &VideoKind::Screen.ladder()[tier];
            timeout(HANDSHAKE, async {
                loop {
                    if let MediaEvent::SourceLimits {
                        purpose: Purpose::ScreenVideo,
                        height,
                        fps,
                        bitrate,
                        ..
                    } = publisher.next().await.unwrap().unwrap()
                        && height == expected.height
                    {
                        assert_eq!(fps, expected.fps);
                        // The fixture SFU's 25 Mbit/s viewer cap leaves 10% for
                        // packet overhead. The measured path can lower that ceiling.
                        assert!((64_000..=22_500_000).contains(&bitrate), "{bitrate}");
                        return;
                    }
                }
            })
            .await
            .expect("the native viewer's quality reaches the publisher");
        }

        // An explicit selection can request 1440p from a much shorter tile.
        bob.set_quality(publisher, Purpose::ScreenVideo, Quality::Tier(2), 180)
            .unwrap();
        encoder_limits(&mut alice, 2).await;

        // A burst during the signaling cooldown keeps the latest selection.
        bob.set_quality(publisher, Purpose::ScreenVideo, Quality::Tier(0), 180)
            .unwrap();
        bob.set_quality(publisher, Purpose::ScreenVideo, Quality::Auto, 1000)
            .unwrap();
        encoder_limits(&mut alice, 1).await;
    }
    decoder.stop().await.unwrap();
    bob.stop().await.unwrap();
    alice.stop().await.unwrap();
    nuschl_testkit::none_running(&video, Duration::from_secs(3)).await;
    nuschl_testkit::none_running(&rtc, Duration::from_secs(3)).await;
    playback.unwrap_or_else(|_| {
        panic!("native video timed out during {phase}; relay-only={relay_only}")
    });
}

async fn relay_datagrams(credentials: &nuschl_protocol::sfu::TurnCredentials) {
    use super::turn::NetworkLeg;
    use tokio::net::UdpSocket;

    let peer = UdpSocket::bind((CA, 0)).await.unwrap();
    let mut path = NetworkLeg::open(peer.local_addr().unwrap(), Some(credentials))
        .await
        .unwrap();
    assert!(path.is_relayed());
    assert_eq!(path.address().ip(), nuschl_testkit::turn::RELAY_IP);
    path.send(b"relay").await.unwrap();
    let mut packet = [0; 16];
    let (size, from) = peer.recv_from(&mut packet).await.unwrap();
    assert_eq!(from, path.address());
    assert_eq!(&packet[..size], b"relay");

    let wrong_port = UdpSocket::bind((CA, 0)).await.unwrap();
    wrong_port.send_to(b"unexpected", from).await.unwrap();
    assert!(
        timeout(Duration::from_millis(100), path.recv(&mut packet))
            .await
            .is_err()
    );
    peer.send_to(b"answer", from).await.unwrap();
    let size = path.recv(&mut packet).await.unwrap();
    assert_eq!(&packet[..size], b"answer");
}
