// SPDX-License-Identifier: EUPL-1.2
//! The SFU picks temporal layers for a native screen publisher from the
//! Dependency Descriptors the encoder core writes.

use super::*;
use nuschl_stream_quality::Quality;
use nuschl_video_encode::{Backend, Config, Encoder, Frame, Layering};
use std::{convert::Infallible, time::Instant};

/// Codes no picture: each frame is two packets that name its temporal layer.
struct Layers;

impl Backend for Layers {
    type Input = ();
    type Error = Infallible;

    fn encode(&mut self, _: &(), frame: &Frame<'_>) -> Result<Vec<u8>, Infallible> {
        Ok(vec![frame.header.temporal_id; 2000])
    }
}

#[tokio::test]
async fn a_viewer_at_the_lowest_screen_tier_receives_the_lower_temporal_layers() {
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
        policy: PeerPolicy { relay_only: false },
    };
    let mut alice = Connection::connect_with_roots(
        CryptoHandle::start(crypto.clone(), directory.path().join("alice")),
        connect(&owner),
        roots.clone(),
    )
    .await
    .unwrap();
    let bob = Connection::connect_with_roots(
        CryptoHandle::start(crypto, directory.path().join("bob")),
        connect(&posted),
        roots,
    )
    .await
    .unwrap();
    timeout(Duration::from_secs(5), async {
        while alice.welcome().peers.is_empty() {
            alice.poll_idle().await.unwrap();
        }
    })
    .await
    .unwrap();
    let publisher = alice.welcome().peer;
    let options = |publish| MediaOptions {
        executable: rtc.clone(),
        sfu_port: sfu.media.port(),
        publish,
    };
    let mut alice = alice
        .into_media(options(Some(Purpose::ScreenVideo)))
        .await
        .unwrap();
    let mut bob = bob.into_media(options(None)).await.unwrap();
    // 720p15: the 30 fps stream's lower two layers fit, the third does not.
    bob.set_quality(publisher, Purpose::ScreenVideo, Quality::Tier(0), 0)
        .unwrap();
    timeout(HANDSHAKE, async {
        while !matches!(
            alice.next().await.unwrap().unwrap(),
            MediaEvent::SourceReady { .. }
        ) {}
    })
    .await
    .unwrap();

    let config = Config {
        width: 1280,
        height: 720,
        fps: 30,
        bitrate: 2_000_000,
        layering: Layering::L1T3,
        screen: true,
        frame_bytes: 1 << 20,
        window: Duration::from_millis(300),
        qindex: 1..=255,
    };
    let mut encoder = Encoder::new(Layers, config).unwrap();
    let started = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(33));
    let mut frame = 0u64;
    // Per second since the start: frames of each temporal layer the viewer got.
    let mut seconds = vec![[0u32; 3]];
    // The SFU first measures each decode target's frame rate; then the viewer
    // gets T0 and T1 at 15 fps and no T2.
    let lower = |counts: &[u32; 3]| counts[0] > 0 && counts[1] > 0 && counts[2] == 0;
    while !seconds.windows(3).any(|w| lower(&w[0]) && lower(&w[1])) {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "layers per second: {seconds:?}"
        );
        tokio::select! {
            event = alice.next() => {
                if let MediaEvent::SourceKeyframeRequired { .. } = event.unwrap().unwrap() {
                    encoder.request_keyframe();
                }
            }
            event = bob.next() => {
                if let MediaEvent::Frame { data, .. } = event.unwrap().unwrap() {
                    let second = started.elapsed().as_secs() as usize;
                    seconds.resize(seconds.len().max(second + 1), [0; 3]);
                    seconds[second][usize::from(data[0])] += 1;
                }
            }
            _ = tick.tick() => {
                frame += 1;
                let encoded = encoder
                    .encode(&(), Duration::from_millis(frame * 33))
                    .unwrap()
                    .expect("the rate allows every frame");
                alice
                    .send_described_frame(
                        Purpose::ScreenVideo,
                        frame * 3_000,
                        encoded.keyframe,
                        encoded.data,
                        encoded.descriptor,
                    )
                    .unwrap();
            }
        }
    }
}
