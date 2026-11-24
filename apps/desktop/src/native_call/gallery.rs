// SPDX-License-Identifier: EUPL-1.2
//! Source identity, quality choices and the bounded set of visible tiles.

use nuschl_client::native::sfu::{Purpose, Quality, VideoKind};
use nuschl_ipc::video::Source;
use nuschl_protocol::sfu::Mode;

pub(super) const REMOTE_SOURCES: usize = 16;
const SOURCES: usize = 2 + REMOTE_SOURCES;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Preference {
    pub(super) quality: Quality,
    pub(super) pixels: u32,
    /// An own source's mode; `None` for its kind's default.
    pub(super) mode: Option<Mode>,
}

impl Default for Preference {
    fn default() -> Self {
        Self {
            quality: Quality::Auto,
            pixels: 0,
            mode: None,
        }
    }
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
    preferences: [Option<(Feed, Preference)>; SOURCES],
}

impl Selection {
    pub(super) fn preference(&self, feed: Feed) -> Preference {
        self.preferences
            .iter()
            .flatten()
            .find_map(|(key, settings)| (*key == feed).then_some(*settings))
            .unwrap_or_default()
    }

    pub(super) fn change(
        &mut self,
        feed: Feed,
        sources: &Sources,
        change: impl FnOnce(&mut Preference),
    ) -> bool {
        let previous = self.preference(feed);
        let mut preference = previous;
        change(&mut preference);
        if preference == previous {
            return false;
        }
        if let Some(index) = self
            .preferences
            .iter()
            .position(|entry| entry.is_some_and(|(key, _)| key == feed))
            .or_else(|| {
                self.preferences.iter().position(|entry| {
                    entry.is_none_or(|(key, _)| !sources.iter().any(|stream| stream.feed == key))
                })
            })
        {
            self.preferences[index] = Some((feed, preference));
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Feed, Purpose, Quality, Selection, Sources, Stream, VideoKind};

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
    fn two_cameras_keep_separate_quality_and_pixel_requests() {
        let mut sources = Sources::default();
        let camera = Feed::local(VideoKind::Camera);
        let guest = Feed::remote(1, Purpose::Camera).unwrap();
        sources.local[0] = Some(stream(camera, 1));
        sources.remote[0] = Some(stream(guest, 1));
        let mut selection = Selection::default();
        assert!(selection.change(camera, &sources, |choice| choice.quality = Quality::Tier(2)));
        assert!(selection.change(guest, &sources, |choice| choice.pixels = 720));
        assert_eq!(selection.preference(camera).quality, Quality::Tier(2));
        assert_eq!(selection.preference(guest).quality, Quality::Auto);
        assert_eq!(selection.preference(guest).pixels, 720);
    }
}
