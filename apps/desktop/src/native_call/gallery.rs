// SPDX-License-Identifier: EUPL-1.2
//! Source identity, quality choices and the bounded set of visible tiles.

use nuschl_client::native::sfu::{Purpose, VideoKind};
use nuschl_ipc::video::Source;
use nuschl_protocol::sfu::Mode;

pub(super) const REMOTE_SOURCES: usize = 16;
pub(super) const REMOTE_TILES: usize = 2;
pub(super) const TILES: usize = 2 + REMOTE_TILES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Feed {
    pub(super) publisher: Option<u64>,
    pub(super) kind: VideoKind,
}

impl Feed {
    pub(super) fn local(kind: VideoKind) -> Self {
        Self {
            publisher: None,
            kind,
        }
    }

    pub(super) fn remote(publisher: u64, purpose: Purpose) -> Option<Self> {
        let kind = match purpose {
            Purpose::Camera => VideoKind::Camera,
            Purpose::ScreenVideo => VideoKind::Screen,
            _ => return None,
        };
        Some(Self {
            publisher: Some(publisher),
            kind,
        })
    }

    pub(super) fn purpose(self) -> Purpose {
        match self.kind {
            VideoKind::Camera => Purpose::Camera,
            VideoKind::Screen => Purpose::ScreenVideo,
        }
    }

    pub(super) fn source(self) -> Source {
        match self.kind {
            VideoKind::Camera => Source::Camera,
            VideoKind::Screen => Source::Screen,
        }
    }

    pub(super) fn label(self) -> String {
        match (self.publisher, self.kind) {
            (None, VideoKind::Camera) => "Your camera".into(),
            (None, VideoKind::Screen) => "Your screen".into(),
            (Some(peer), VideoKind::Camera) => format!("Camera · Guest {peer}"),
            (Some(peer), VideoKind::Screen) => format!("Screen · Guest {peer}"),
        }
    }
}

/// The capture or subscription that delivered a frame for this feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Stream {
    pub(super) feed: Feed,
    pub(super) serial: u64,
    pub(super) preview: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Sources {
    pub(super) local: [Option<Stream>; 2],
    pub(super) remote: [Option<Stream>; REMOTE_SOURCES],
}

impl Sources {
    pub(super) fn iter(&self) -> impl Iterator<Item = Stream> + '_ {
        self.remote
            .iter()
            .chain(self.local.iter())
            .copied()
            .flatten()
    }

    pub(super) fn featured(&self, requested: Option<Feed>) -> Option<Stream> {
        requested
            .and_then(|feed| self.iter().find(|stream| stream.feed == feed))
            .or_else(|| {
                self.iter()
                    .find(|stream| stream.feed.kind == VideoKind::Screen)
            })
            .or_else(|| self.iter().next())
    }

    pub(super) fn tiles(&self, selection: Selection) -> [Option<Stream>; TILES] {
        let featured = self.featured(selection.view);
        if !selection.tiled {
            return [featured, None, None, None];
        }
        let mut remote = 0;
        let mut visible = self.iter().filter(|stream| {
            if stream.feed.publisher.is_none() {
                return true;
            }
            let slots = REMOTE_TILES
                - usize::from(featured.is_some_and(|first| first.feed.publisher.is_some()));
            if Some(*stream) == featured {
                return true;
            }
            remote += 1;
            remote <= slots
        });
        std::array::from_fn(|_| visible.next())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Preference {
    pub(super) max_height: Option<u16>,
    pub(super) max_fps: Option<u16>,
    /// An own source's mode; `None` for its kind's default.
    pub(super) mode: Option<Mode>,
}

impl Preference {
    pub(super) fn mode(self, kind: VideoKind) -> Mode {
        self.mode
            .unwrap_or_else(|| Mode::default_for(kind == VideoKind::Screen))
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Selection {
    pub(super) view: Option<Feed>,
    pub(super) tiled: bool,
    preferences: [Preference; 2],
    tile_pixels: [Option<(Stream, u32)>; REMOTE_SOURCES],
}

impl Selection {
    pub(super) fn preference(&self, kind: VideoKind) -> Preference {
        self.preferences[preference_index(kind)]
    }

    pub(super) fn change_preference(
        &mut self,
        kind: VideoKind,
        change: impl FnOnce(&mut Preference),
    ) -> bool {
        let preference = &mut self.preferences[preference_index(kind)];
        let previous = *preference;
        change(preference);
        *preference != previous
    }

    pub(super) fn tile_pixels(&self, slot: usize, stream: Stream) -> u32 {
        self.tile_pixels[slot]
            .filter(|(current, _)| *current == stream)
            .map_or(0, |(_, pixels)| pixels)
    }

    pub(super) fn resize_tile(&mut self, stream: Stream, sources: &Sources, pixels: u32) -> bool {
        let Some(slot) = sources
            .remote
            .iter()
            .position(|source| *source == Some(stream))
        else {
            return false;
        };
        let measurement = Some((stream, pixels));
        std::mem::replace(&mut self.tile_pixels[slot], measurement) != measurement
    }
}

fn preference_index(kind: VideoKind) -> usize {
    match kind {
        VideoKind::Camera => 0,
        VideoKind::Screen => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::{Feed, Purpose, Selection, Sources, Stream, VideoKind};

    fn stream(feed: Feed, serial: u64) -> Stream {
        Stream {
            feed,
            serial,
            preview: true,
        }
    }

    #[test]
    fn a_pinned_feed_uses_the_stage_and_the_grid_reserves_its_remote_tile() {
        let mut sources = Sources::default();
        let camera = stream(Feed::local(VideoKind::Camera), 1);
        let screen = stream(Feed::local(VideoKind::Screen), 2);
        sources.local = [Some(camera), Some(screen)];
        for peer in 1..=3 {
            sources.remote[(peer - 1) as usize] =
                Some(stream(Feed::remote(peer, Purpose::Camera).unwrap(), peer));
        }
        let chosen = sources.remote[2].unwrap();
        let mut selection = Selection {
            view: Some(chosen.feed),
            ..Selection::default()
        };
        assert_eq!(sources.tiles(selection), [Some(chosen), None, None, None]);
        selection.tiled = true;
        assert_eq!(
            sources.tiles(selection),
            [sources.remote[0], Some(chosen), Some(camera), Some(screen)]
        );
        sources.remote[2] = None;
        assert_eq!(sources.featured(selection.view), Some(screen));
    }

    #[test]
    fn sender_quality_and_viewer_tile_sizes_are_independent() {
        let mut sources = Sources::default();
        let kind = VideoKind::Camera;
        let guest = stream(Feed::remote(1, Purpose::Camera).unwrap(), 1);
        sources.remote[0] = Some(guest);
        let mut selection = Selection::default();
        assert!(selection.change_preference(kind, |choice| choice.max_height = Some(720)));
        assert!(selection.resize_tile(guest, &sources, 720));
        assert!(!selection.resize_tile(guest, &sources, 720));
        assert_eq!(selection.preference(kind).max_height, Some(720));
        assert_eq!(selection.preference(VideoKind::Screen).max_height, None);
        assert_eq!(selection.tile_pixels(0, guest), 720);
        assert_eq!(selection.tile_pixels(0, stream(guest.feed, 2)), 0);
    }
}
