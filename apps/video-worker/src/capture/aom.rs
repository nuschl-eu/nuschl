// SPDX-License-Identifier: EUPL-1.2
//! The software encoder: libaom in real-time mode (`nuschl-aom`) behind the
//! encoder core, for machines without AV1 hardware encoding.

use super::Failure;
use nuschl_ipc::video::{Settings, Source};
use nuschl_video_encode::{Backend, Encoder, Frame};

/// libaom coding the frames the encoder core plans, from I420 pictures.
pub struct Aom(nuschl_aom::Encoder);

impl Backend for Aom {
    type Input = Vec<u8>;
    type Error = nuschl_aom::Error;
    const DISCARDS: bool = false;

    fn encode(&mut self, i420: &Vec<u8>, frame: &Frame<'_>) -> Result<Vec<u8>, Self::Error> {
        let header = frame.header;
        self.0.encode(
            i420,
            &nuschl_aom::Frame {
                key: header.key,
                temporal_id: header.temporal_id,
                reference: header.reference,
                refresh: header.refresh,
                qindex: header.qindex,
            },
        )
    }
}

/// The encoder core over libaom for `settings`' picture.
pub fn open(source: Source, settings: Settings) -> Result<Encoder<Aom>, Failure> {
    // libaom's quantizer steps start at qindex 4; 0 codes losslessly.
    let config = super::config(source, settings, 4..=255);
    let threads = std::thread::available_parallelism().map_or(1, |cores| cores.get().min(4));
    let backend = Aom(nuschl_aom::Encoder::new(&nuschl_aom::Config {
        width: settings.width,
        height: settings.height,
        fps: settings.fps,
        layers: config.layering.layers(),
        screen: config.screen,
        threads: u8::try_from(threads)?,
    })?);
    Encoder::new(backend, config).ok_or_else(|| "the picture needs more than one AV1 tile".into())
}

#[cfg(test)]
mod tests;
