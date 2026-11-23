// SPDX-License-Identifier: EUPL-1.2

use super::*;

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

    // An independent synthetic AV1 frame plus a 4 KiB AV1 padding OBU.
    let mut picture = vec![
        0x12, 0, 0x0a, 0x0a, 0, 0, 0, 2, 0x27, 0xfe, 0x6d, 0x7c, 0x80, 0x20, 0x32, 0x0e, 0x10, 0,
        0x8c, 0, 0, 2, 0, 0, 0, 0, 0x3e, 0xfc, 0xc1, 8, 0x7a, 0x80, 0x20,
    ];
    picture.resize(picture.len() + 4096, 0);
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
