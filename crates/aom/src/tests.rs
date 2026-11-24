// SPDX-License-Identifier: EUPL-1.2
//! The encoder against rav1d, on synthetic pictures.

use super::{Config, Encoder, Frame};
use rav1d::{Decoder, Rav1dError, Settings};

const WIDTH: u16 = 320;
const HEIGHT: u16 = 180;

/// A gradient that moves with `frame`, as limited-range I420.
fn picture(frame: usize) -> Vec<u8> {
    let (width, height) = (usize::from(WIDTH), usize::from(HEIGHT));
    let mut i420 = Vec::with_capacity(width * height * 3 / 2);
    for row in 0..height {
        i420.extend(
            (0..width).map(|column| u8::try_from(16 + (row + column + 4 * frame) % 220).unwrap()),
        );
    }
    i420.resize(width * height * 3 / 2, 128);
    i420
}

fn config(layers: u8) -> Config {
    Config {
        width: WIDTH,
        height: HEIGHT,
        fps: 30,
        layers,
        screen: false,
        threads: 2,
    }
}

fn decoder(operating_point: u8) -> Decoder {
    let mut settings = Settings::new();
    settings.set_n_threads(1);
    settings.set_max_frame_delay(1);
    settings.set_operating_point(operating_point);
    Decoder::with_settings(&settings).unwrap()
}

/// Sends one temporal unit and returns the size of the picture it shows.
fn shown(decoder: &mut Decoder, unit: &[u8]) -> Option<(u32, u32)> {
    let mut sent = decoder.send_data(unit.to_vec().into_boxed_slice(), None, None, None);
    let mut shown = None;
    loop {
        match decoder.get_picture() {
            Ok(picture) => shown = Some((picture.width(), picture.height())),
            Err(Rav1dError::TryAgain) => {}
            Err(error) => panic!("rav1d: {error:?}"),
        }
        match sent {
            Err(Rav1dError::TryAgain) => sent = decoder.send_pending_data(),
            Ok(()) => return shown,
            Err(error) => panic!("rav1d: {error:?}"),
        }
    }
}

#[test]
fn each_operating_point_decodes_its_temporal_layers() {
    let mut encoder = Encoder::new(&config(3)).unwrap();
    let mut decoders: Vec<_> = (0..3).map(decoder).collect();
    // L1T3 as the encoder core plans it: slot 0 holds the newest T0, slot 1
    // the newest T1.
    let pattern = [(0, 0, 0b01), (2, 0, 0), (1, 0, 0b10), (2, 1, 0)];
    for frame in 0..16 {
        let (temporal_id, reference, refresh) = pattern[frame % 4];
        let unit = encoder
            .encode(
                &picture(frame),
                &Frame {
                    key: frame == 0,
                    temporal_id,
                    reference,
                    refresh,
                    qindex: 120,
                },
            )
            .unwrap();
        // Operating point 0 holds every layer, 2 only T0.
        for (point, decoder) in decoders.iter_mut().enumerate() {
            let highest = 2 - point;
            let expected = usize::from(temporal_id) <= highest;
            let size = shown(decoder, &unit);
            assert_eq!(
                size.is_some(),
                expected,
                "frame {frame}, operating point {point}"
            );
            if let Some(size) = size {
                assert_eq!(size, (u32::from(WIDTH), u32::from(HEIGHT)));
            }
        }
    }
}

#[test]
fn each_frame_predicts_from_its_planned_slot() {
    let mut encoder = Encoder::new(&config(3)).unwrap();
    // A T2 frame predicting from the newest T1 one frame back costs about
    // what a T2 frame predicting from the newest T0 one frame back does.
    let pattern = [(0, 0, 0b01), (2, 0, 0), (1, 0, 0b10), (2, 1, 0)];
    let mut sizes = [0; 4];
    for frame in 0..24 {
        let (temporal_id, reference, refresh) = pattern[frame % 4];
        let unit = encoder
            .encode(
                &picture(frame),
                &Frame {
                    key: frame == 0,
                    temporal_id,
                    reference,
                    refresh,
                    qindex: 120,
                },
            )
            .unwrap();
        if frame >= 8 {
            sizes[frame % 4] += unit.len();
        }
    }
    assert!(
        sizes[3] < 2 * sizes[1],
        "T2 after T1 {}, after T0 {}",
        sizes[3],
        sizes[1]
    );
}

#[test]
fn each_frame_takes_its_quantizer_and_keyframes_come_on_request() {
    let mut encoder = Encoder::new(&config(1)).unwrap();
    let mut decoder = decoder(0);
    let mut code = |frame: usize, key: bool, qindex: u8| {
        let unit = encoder
            .encode(
                &picture(frame),
                &Frame {
                    key,
                    temporal_id: 0,
                    reference: 0,
                    refresh: 1,
                    qindex,
                },
            )
            .unwrap();
        assert!(shown(&mut decoder, &unit).is_some());
        unit.len()
    };
    let fine = code(0, true, 40);
    let coarse = code(1, true, 200);
    assert!(
        coarse * 2 < fine,
        "{coarse} bytes at qindex 200, {fine} at 40"
    );
    // A still picture: inter frames reuse it, a requested keyframe codes it
    // again.
    let inter = (0..4).map(|_| code(1, false, 200)).max().unwrap();
    let key = code(1, true, 200);
    assert!(
        inter * 4 < key,
        "{inter} bytes per inter frame, {key} for the keyframe"
    );
}

#[test]
fn invalid_sizes_and_pictures_are_refused() {
    assert!(
        Encoder::new(&Config {
            width: 321,
            ..config(1)
        })
        .is_err()
    );
    assert!(Encoder::new(&config(4)).is_err());
    let mut encoder = Encoder::new(&config(1)).unwrap();
    let frame = Frame {
        key: true,
        temporal_id: 0,
        reference: 0,
        refresh: 1,
        qindex: 100,
    };
    assert!(encoder.encode(&picture(0)[1..], &frame).is_err());
    assert!(
        encoder
            .encode(
                &picture(0),
                &Frame {
                    reference: 8,
                    ..frame
                }
            )
            .is_err()
    );
}
