// SPDX-License-Identifier: EUPL-1.2

use super::*;

/// An independent synthetic AV1 frame plus a 4 KiB AV1 padding OBU.
pub(super) fn picture() -> Vec<u8> {
    let mut picture = vec![
        0x12, 0, 0x0a, 0x0a, 0, 0, 0, 2, 0x27, 0xfe, 0x6d, 0x7c, 0x80, 0x20, 0x32, 0x0e, 0x10, 0,
        0x8c, 0, 0, 2, 0, 0, 0, 0, 0x3e, 0xfc, 0xc1, 8, 0x7a, 0x80, 0x20,
    ];
    picture.resize(picture.len() + 4096, 0);
    picture
}

#[tokio::test]
async fn native_call_switches_paths_and_sources() {
    let _media = MEDIA_TEST.lock().await;
    nuschl_testkit::sandbox();
    let crypto = fixture("nuschl-crypto-worker");
    let rtc = fixture("nuschl-rtc-worker");
    let sfu = nuschl_testkit::sfu::start(fixture("nuschl-stream-sfu"), "");
    let (host, roots) = tls_proxy(&sfu).await;
    let directory = nuschl_testkit::temp_dir();
    let posted = format!("https://{host}/call#{LINK}");
    let owner = format!("{posted}&p={OWNER}");
    let connect = |link| Connect {
        generation: 1,
        host: &host,
        link,
        codecs: &[Codec::Av1],
        policy: PeerPolicy::default(),
    };
    let make_media = |publish| MediaOptions {
        executable: rtc.clone(),
        sfu_port: sfu.media.port(),
        publish,
    };
    let mut publisher = Connection::connect_with_roots(
        CryptoHandle::start(crypto.clone(), directory.path().join("publisher")),
        connect(&owner),
        roots.clone(),
    )
    .await
    .unwrap();
    let from = publisher.welcome().peer;
    // The host path uses only loopback sockets. The STUN test owns its server.
    publisher.welcome.turn = None;
    let mut publisher = publisher.into_media(make_media(None)).await.unwrap();
    publisher.set_source(Purpose::Camera, true).unwrap();
    // The requests coalesce before the media task can negotiate either of them.
    publisher.switch_source(Purpose::ScreenVideo).unwrap();
    publisher.switch_source(Purpose::Camera).unwrap();
    let mut viewer = Connection::connect_with_roots(
        CryptoHandle::start(crypto.clone(), directory.path().join("viewer")),
        connect(&posted),
        roots.clone(),
    )
    .await
    .unwrap();
    // Version 1, the first camera and screen tier. A modest upload can
    // qualify without a high bitrate or long-running test stream.
    write(
        &mut viewer.socket,
        &ClientMessage::Capacity {
            decode: "010000".into(),
        },
    )
    .await
    .unwrap();
    let viewer_id = viewer.welcome().peer;
    viewer.welcome.turn = None;
    let mut viewer = viewer.into_media(make_media(None)).await.unwrap();

    let picture = picture();
    let mut ready = false;
    let mut over_sfu = false;
    let mut sender_direct = false;
    let mut receiver_direct = false;
    let mut requested_keyframe = false;
    let mut outage = None;
    let mut sender_relayed = false;
    let mut receiver_relayed = false;
    let mut resumed = false;
    let mut timestamp = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(33));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    timeout(Duration::from_secs(20), async {
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if ready {
                        timestamp += 2970;
                        publisher.send_keyframe(timestamp, picture.clone()).unwrap();
                    }
                }
                event = publisher.next() => match event.unwrap().unwrap() {
                    MediaEvent::SourceReady { .. } => ready = true,
                    MediaEvent::Route(Route::Direct) => sender_direct = true,
                    MediaEvent::Route(Route::Sfu(_)) if sender_direct => sender_relayed = true,
                    MediaEvent::SourceKeyframeRequired { .. } if requested_keyframe && outage.is_none() => {
                        // Drop packets after the direct leg delivered the viewer's request.
                        publisher.lose_direct().await;
                        outage = Some(tokio::time::Instant::now());
                    }
                    _ => {}
                },
                event = viewer.next() => match event.unwrap().unwrap() {
                    MediaEvent::Route(Route::Direct) => receiver_direct = true,
                    MediaEvent::Route(Route::Sfu(_)) if receiver_direct => receiver_relayed = true,
                    MediaEvent::Frame { publisher: sender, purpose, contiguous, key_frame, data, .. } => {
                        assert_eq!(sender, from);
                        assert_eq!(purpose, Purpose::Camera);
                        assert!(key_frame);
                        assert_eq!(data, picture);
                        if receiver_relayed && !resumed {
                            assert!(!contiguous);
                            resumed = true;
                        }
                        if sender_direct && receiver_direct && !requested_keyframe {
                            viewer.request_keyframe(Purpose::Camera).unwrap();
                            requested_keyframe = true;
                        }
                        if !sender_direct {
                            over_sfu = true;
                        }
                    }
                    _ => {}
                },
            }
            if resumed && sender_relayed {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("direct recovery did not complete; SFU={over_sfu}, sender={sender_direct}, receiver={receiver_direct}, keyframe request={requested_keyframe}, outage={outage:?}, sender fell back={sender_relayed}, receiver fell back={receiver_relayed}, resumed={resumed}"));
    assert!(over_sfu);
    eprintln!(
        "SFU resumed with both endpoints reporting fallback after {:?}",
        outage.unwrap().elapsed()
    );
    for (old, new) in [
        (Purpose::Camera, Purpose::ScreenVideo),
        (Purpose::ScreenVideo, Purpose::Camera),
    ] {
        publisher.switch_source(new).unwrap();
        let mut ready = false;
        let mut ended = false;
        let mut timestamp = 0;
        timeout(HANDSHAKE, async {
            loop {
                tokio::select! {
                    _ = tick.tick(), if ready => {
                        timestamp += 2970;
                        publisher.send_keyframe(timestamp, picture.clone()).unwrap();
                    }
                    event = publisher.next() => {
                        ready |= matches!(event.unwrap().unwrap(), MediaEvent::SourceReady { .. });
                    }
                    event = viewer.next() => match event.unwrap().unwrap() {
                        MediaEvent::SourceEnded { publisher, purpose } => {
                            assert_eq!((publisher, purpose), (from, old));
                            ended = true;
                        }
                        MediaEvent::Frame { publisher: sender, purpose, key_frame, data, .. } if purpose == new => {
                            assert!(ended);
                            assert_eq!(sender, from);
                            assert!(key_frame);
                            assert_eq!(data, picture);
                            break;
                        }
                        _ => {}
                    },
                }
            }
        })
        .await
        .expect("fresh frames arrive for the replaced source on the same call");
    }
    publisher.set_source(Purpose::ScreenVideo, true).unwrap();
    let mut seen = [false; 2];
    let mut screen_ready = false;
    let mut screen_available = false;
    let mut timestamp = 1_000_000;
    timeout(HANDSHAKE, async {
        while seen != [true, true] {
            tokio::select! {
                _ = tick.tick() => {
                    timestamp += 2970;
                    publisher.send_source_frame(Purpose::Camera, timestamp, true, picture.clone()).unwrap();
                    if screen_ready {
                        publisher.send_source_frame(Purpose::ScreenVideo, timestamp, true, picture.clone()).unwrap();
                    }
                }
                event = publisher.next() => {
                    screen_ready |= matches!(event.unwrap().unwrap(), MediaEvent::SourceReady { purpose: Purpose::ScreenVideo });
                }
                event = viewer.next() => match event.unwrap().unwrap() {
                    MediaEvent::SourceAvailable { publisher, purpose: Purpose::ScreenVideo } => {
                        assert_eq!(publisher, from);
                        screen_available = true;
                    }
                    MediaEvent::SourceEnded { .. } => panic!("starting a screen retired an active source"),
                    MediaEvent::Frame { publisher, purpose, key_frame, data, .. } if screen_ready => {
                        assert_eq!(publisher, from);
                        assert!(key_frame);
                        assert_eq!(data, picture);
                        if purpose == Purpose::ScreenVideo {
                            assert!(screen_available);
                        }
                        seen[usize::from(purpose == Purpose::ScreenVideo)] = true;
                    }
                    _ => {}
                }
            }
        }
    }).await.expect("both camera and screen remain live");

    let extra = Connection::connect_with_roots(
        CryptoHandle::start(crypto, directory.path().join("extra")),
        connect(&posted),
        roots,
    )
    .await
    .unwrap();
    assert_eq!(extra.welcome().peers.len(), 2);

    publisher.set_source(Purpose::ScreenVideo, false).unwrap();
    let mut screen_ended = false;
    timeout(HANDSHAKE, async {
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    timestamp += 2970;
                    publisher.send_source_frame(Purpose::Camera, timestamp, true, picture.clone()).unwrap();
                }
                event = publisher.next() => { event.unwrap().unwrap(); }
                event = viewer.next() => match event.unwrap().unwrap() {
                    MediaEvent::SourceEnded { publisher, purpose } => {
                        assert_eq!((publisher, purpose), (from, Purpose::ScreenVideo));
                        screen_ended = true;
                    }
                    MediaEvent::Frame { purpose: Purpose::Camera, .. } if screen_ended => break,
                    _ => {}
                }
            }
        }
    }).await.expect("stopping the screen keeps the camera live");
    publisher.set_source(Purpose::Camera, false).unwrap();
    timeout(HANDSHAKE, async {
        loop {
            if matches!(
                viewer.next().await.unwrap().unwrap(),
                MediaEvent::SourceEnded {
                    publisher,
                    purpose: Purpose::Camera
                } if publisher == from
            ) {
                break;
            }
        }
    })
    .await
    .expect("the room stays open after stopping its final source");

    publisher.set_source(Purpose::Camera, true).unwrap();
    let mut ready = false;
    let mut timestamp = 0;
    timeout(HANDSHAKE, async {
        loop {
            tokio::select! {
                _ = tick.tick(), if ready => {
                    timestamp += 2970;
                    publisher.send_source_frame(Purpose::Camera, timestamp, true, picture.clone()).unwrap();
                }
                event = publisher.next() => {
                    ready |= matches!(event.unwrap().unwrap(), MediaEvent::SourceReady { purpose: Purpose::Camera });
                }
                event = viewer.next() => {
                    if matches!(event.unwrap().unwrap(), MediaEvent::Frame { purpose: Purpose::Camera, .. }) {
                        break;
                    }
                }
            }
        }
    }).await.expect("the presenter can resume video without opening a new room");
    // Both media workers now publish the same kind with independent keys.
    assert!(viewer.can_publish());
    viewer.set_source(Purpose::Camera, true).unwrap();
    let peers = [from, viewer_id];
    let pictures = std::array::from_fn::<_, 2, _>(|index| {
        let mut frame = picture.clone();
        frame[picture.len() - 4098] += index as u8;
        frame.resize(picture.len() + index, 0);
        frame
    });
    let mut ready = [true, false];
    let mut seen = [false; 2];
    let mut clocks = [timestamp, 0];
    timeout(HANDSHAKE, async {
        while seen != [true, true] {
            let (receiver, event) = tokio::select! {
                _ = tick.tick() => {
                    for (index, media) in [&publisher, &viewer].into_iter().enumerate() {
                        if ready[index] {
                            clocks[index] += 2970;
                            media.send_keyframe(clocks[index], pictures[index].clone()).unwrap();
                        }
                    }
                    continue;
                }
                event = publisher.next() => (0, event),
                event = viewer.next() => (1, event),
            };
            match event.unwrap().unwrap() {
                MediaEvent::SourceReady {
                    purpose: Purpose::Camera,
                } => ready[receiver] = true,
                MediaEvent::Frame {
                    publisher,
                    purpose,
                    key_frame,
                    data,
                    ..
                } => {
                    let sender = peers.iter().position(|peer| *peer == publisher).unwrap();
                    assert_ne!(sender, receiver);
                    assert_eq!(purpose, Purpose::Camera);
                    assert!(key_frame);
                    assert_eq!(data, pictures[sender]);
                    seen[receiver] = true;
                }
                MediaEvent::Route(Route::Direct) => {
                    panic!("a group call selected a direct path")
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("duplex frames did not reach every viewer: {seen:?}"));
    viewer.set_source(Purpose::Camera, false).unwrap();
    timeout(HANDSHAKE, async {
        loop {
            if matches!(publisher.next().await.unwrap().unwrap(), MediaEvent::SourceEnded {
                publisher, purpose: Purpose::Camera,
            } if publisher == viewer_id)
            {
                break;
            }
        }
    })
    .await
    .expect("the guest camera ended independently");
    publisher.stop().await.unwrap();
    viewer.stop().await.unwrap();
    extra.close().await.unwrap();
}

/// One publisher's camera frames and both endpoints' events.
struct Call {
    publisher: Media,
    viewer: Media,
    from: u64,
    tick: tokio::time::Interval,
    ready: bool,
    timestamp: u64,
    routes: [Option<Route>; 2],
    published: Vec<Route>,
    frames: usize,
    bytes: usize,
}

impl Call {
    /// The bytes the SFU sent and the frame bytes the viewer received over
    /// three seconds, after a second for the SFU's count to catch up.
    async fn measure(&mut self, sent: impl Fn() -> u64) -> (u64, usize) {
        self.run(Duration::from_millis(1500), |_| false).await;
        let (before, received) = (sent(), self.bytes);
        self.run(Duration::from_secs(3), |_| false).await;
        (sent() - before, self.bytes - received)
    }

    /// Runs the call until `done` holds or `limit` passes; tells whether it held.
    async fn run(&mut self, limit: Duration, done: impl Fn(&Self) -> bool) -> bool {
        let picture = picture();
        timeout(limit, async {
            while !done(self) {
                tokio::select! {
                    _ = self.tick.tick() => if self.ready {
                        self.timestamp += 2970;
                        self.publisher.send_keyframe(self.timestamp, picture.clone()).unwrap();
                    },
                    event = self.publisher.next() => match event.unwrap().unwrap() {
                        MediaEvent::SourceReady { .. } => self.ready = true,
                        MediaEvent::Route(route) => {
                            self.routes[0] = Some(route);
                            self.published.push(route);
                        }
                        _ => {}
                    },
                    event = self.viewer.next() => match event.unwrap().unwrap() {
                        MediaEvent::Route(route) => self.routes[1] = Some(route),
                        MediaEvent::Frame { publisher, data, .. } => {
                            assert_eq!(publisher, self.from);
                            assert_eq!(data, picture);
                            self.frames += 1;
                            self.bytes += data.len();
                        }
                        _ => {}
                    },
                }
            }
        })
        .await
        .is_ok()
    }
}

#[tokio::test]
async fn native_video_follows_the_upload_and_relay_only_stays_relayed() {
    let _media = MEDIA_TEST.lock().await;
    nuschl_testkit::sandbox();
    let crypto = fixture("nuschl-crypto-worker");
    let rtc = fixture("nuschl-rtc-worker");
    let turn = nuschl_testkit::turn::start();
    // The SFU publishes the charged byte count once a second.
    let sfu = nuschl_testkit::sfu::start_with_turn(
        fixture("nuschl-stream-sfu"),
        "traffic_budget = 1000000000000000\nstate_directory = \"{directory}\"",
        &turn.url,
    );
    let traffic = sfu.directory.path().join("traffic.usage");
    let sent = || -> u64 {
        let count = std::fs::read_to_string(&traffic).unwrap();
        count.split_whitespace().nth(1).unwrap().parse().unwrap()
    };
    let (host, roots) = tls_proxy(&sfu).await;
    let directory = nuschl_testkit::temp_dir();
    let posted = format!("https://{host}/call#{LINK}");
    let owner = format!("{posted}&p={OWNER}");
    let join = |name: &str, link, relay_only| {
        Connection::connect_with_roots(
            CryptoHandle::start(crypto.clone(), directory.path().join(name)),
            Connect {
                generation: 1,
                host: &host,
                link,
                codecs: &[Codec::Av1],
                policy: PeerPolicy { relay_only },
            },
            roots.clone(),
        )
    };
    let options = |publish| MediaOptions {
        executable: rtc.clone(),
        sfu_port: sfu.media.port(),
        publish,
    };
    // Version 1, the first camera and screen tier, as in the test above.
    let capacity = ClientMessage::Capacity {
        decode: "010000".into(),
    };
    let publisher = join("publisher", &owner, false).await.unwrap();
    let from = publisher.welcome().peer;
    let publisher = publisher
        .into_media(options(Some(Purpose::Camera)))
        .await
        .unwrap();
    let mut viewer = join("viewer", &posted, false).await.unwrap();
    write(&mut viewer.socket, &capacity).await.unwrap();
    let mut tick = tokio::time::interval(Duration::from_millis(33));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut call = Call {
        publisher,
        viewer: viewer.into_media(options(None)).await.unwrap(),
        from,
        tick,
        ready: false,
        timestamp: 0,
        routes: [None; 2],
        published: Vec::new(),
        frames: 0,
        bytes: 0,
    };

    let direct = [Some(Route::Direct); 2];
    assert!(
        call.run(Duration::from_secs(20), |call| call.routes == direct)
            .await,
        "both endpoints go direct: {:?}",
        call.routes
    );
    // Media runs on one path: the SFU forwards none of it while the
    // direct leg carries every frame. Probes and RTCP remain.
    let (forwarded, received) = call.measure(&sent).await;
    assert_eq!(call.routes, direct);
    eprintln!("direct: the SFU sent {forwarded} bytes, the viewer received {received}");
    assert!(received > 100_000 && forwarded * 3 < received as u64);

    // A throttled leg backs its estimate off below the stream rate. Both
    // endpoints return to the SFU, and frames keep decrypting under the
    // keys they used on the direct leg.
    call.publisher.throttle_direct(12_000).await;
    let throttled = std::time::Instant::now();
    let relayed = |route: &Option<Route>| matches!(route, Some(Route::Sfu(_)));
    assert!(
        call.run(Duration::from_secs(20), |call| call
            .routes
            .iter()
            .all(relayed))
            .await,
        "both endpoints return to the SFU: {:?}",
        call.routes
    );
    eprintln!("fallback after {:?}", throttled.elapsed());
    let (forwarded, received) = call.measure(&sent).await;
    assert!(call.routes.iter().all(relayed));
    eprintln!("SFU: the SFU sent {forwarded} bytes, the viewer received {received}");
    assert!(received > 40_000 && forwarded * 3 > received as u64 * 2);
    eprintln!("publisher routes: {:?}", call.published);

    // A relay-only viewer never sends a candidate, and its crypto worker
    // seals no direct signaling.
    let Call { viewer, .. } = call;
    viewer.stop().await.unwrap();
    let mut viewer = join("relay-only", &posted, true).await.unwrap();
    let refused = viewer
        .signal(Action::Seal {
            peer: from,
            publisher: false,
            data: b"candidates".to_vec(),
        })
        .await;
    assert!(
        matches!(&refused, Err(error) if error.to_string().contains("direct signaling disabled")),
        "{:?}",
        refused.err()
    );
    write(&mut viewer.socket, &capacity).await.unwrap();
    let mut call = Call {
        viewer: viewer.into_media(options(None)).await.unwrap(),
        routes: [None; 2],
        published: Vec::new(),
        frames: 0,
        ..call
    };
    call.run(Duration::from_secs(8), |_| false).await;
    assert!(call.frames > 30, "{} frames through TURN", call.frames);
    assert!(!call.published.contains(&Route::Direct));
    assert_eq!(
        call.routes,
        [
            Some(Route::Sfu(Reason::Viewer)),
            Some(Route::Sfu(Reason::RelayOnly))
        ]
    );
    call.publisher.stop().await.unwrap();
    call.viewer.stop().await.unwrap();
}

/// Shared audio takes the direct leg with the screen: while both endpoints are
/// direct, the viewer receives 25 consecutive audio packets. A switch of path
/// can lose the packets in flight on the old one, and a busy host can send the
/// call back to the SFU; either starts the count again. A process runs two RTC
/// workers, so the second viewer's leg is the RTC worker's own test.
#[tokio::test]
async fn shared_audio_follows_the_screen_onto_the_direct_leg() {
    let _media = MEDIA_TEST.lock().await;
    nuschl_testkit::sandbox();
    let crypto = fixture("nuschl-crypto-worker");
    let rtc = fixture("nuschl-rtc-worker");
    let sfu = nuschl_testkit::sfu::start(fixture("nuschl-stream-sfu"), "");
    let (host, roots) = tls_proxy(&sfu).await;
    let directory = nuschl_testkit::temp_dir();
    let posted = format!("https://{host}/call#{LINK}");
    let owner = format!("{posted}&p={OWNER}");
    let join = async |name: &str, link: &str| {
        let mut connection = Connection::connect_with_roots(
            CryptoHandle::start(crypto.clone(), directory.path().join(name)),
            Connect {
                generation: 1,
                host: &host,
                link,
                codecs: &[Codec::Av1],
                policy: PeerPolicy::default(),
            },
            roots.clone(),
        )
        .await
        .unwrap();
        connection.welcome.turn = None;
        connection
    };
    let options = |publish| MediaOptions {
        executable: rtc.clone(),
        sfu_port: sfu.media.port(),
        publish,
    };
    let publisher = join("publisher", &owner).await;
    let from = publisher.welcome().peer;
    let mut publisher = publisher
        .into_media(options(Some(Purpose::ScreenVideo)))
        .await
        .unwrap();
    publisher.set_source(Purpose::SystemAudio, true).unwrap();
    let mut viewer = join("viewer", &posted).await;
    // Version 1, the first camera and screen tier, as in the tests above.
    write(
        &mut viewer.socket,
        &ClientMessage::Capacity {
            decode: "010000".into(),
        },
    )
    .await
    .unwrap();
    let mut viewer = viewer.into_media(options(None)).await.unwrap();
    let picture = picture();
    let packet = |n: u64| [nuschl_core::audio::SILENT_OPUS.as_slice(), &n.to_be_bytes()].concat();
    let mut video = tokio::time::interval(Duration::from_millis(33));
    let mut audio = tokio::time::interval(Duration::from_millis(20));
    let (mut ready, mut timestamp, mut sent) = (false, 0, 0_u64);
    let mut routes = [None; 2];
    // The consecutive packets the viewer received while both endpoints were direct.
    let mut direct = Vec::new();
    timeout(Duration::from_secs(60), async {
        while direct.len() < 25 {
            let both_direct = routes == [Some(Route::Direct); 2];
            let (index, event) = tokio::select! {
                _ = video.tick(), if ready => {
                    timestamp += 2970;
                    publisher.send_source_frame(Purpose::ScreenVideo, timestamp, true, picture.clone()).unwrap();
                    continue;
                }
                _ = audio.tick() => {
                    sent += 1;
                    let _ = publisher.send_source_frame(Purpose::SystemAudio, sent * 960, false, packet(sent));
                    continue;
                }
                event = publisher.next() => (0, event),
                event = viewer.next() => (1, event),
            };
            match event.unwrap().unwrap() {
                MediaEvent::SourceReady { purpose: Purpose::ScreenVideo } => ready = true,
                MediaEvent::Route(route) => {
                    routes[index] = Some(route);
                    direct.clear();
                }
                MediaEvent::Frame { publisher: sender, purpose: Purpose::SystemAudio, data, .. } if both_direct => {
                    assert_eq!(sender, from);
                    let number = u64::from_be_bytes(data[data.len() - 8..].try_into().unwrap());
                    if direct.last().is_some_and(|&last| number != last + 1) {
                        direct.clear();
                    }
                    direct.push(number);
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("routes {routes:?}, {} consecutive audio packets over the direct leg", direct.len()));
    publisher.stop().await.unwrap();
    viewer.stop().await.unwrap();
}
