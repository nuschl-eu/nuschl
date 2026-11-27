// SPDX-License-Identifier: EUPL-1.2
//! Tempo and beat grids estimated from songs DJ decks hold in memory
//! (`preload.rs`, `bpm.rs`), kept in `beats.json` in the state directory so
//! each song is analysed once. The library's BPM wins over an estimate; the
//! grid uses the library's BPM when it has one.
use crate::{
    bpm::{Analyzer, Onsets, beat_grid},
    persist,
    preload::{Buffer, Fill},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::sync::Semaphore;

pub const FILE: &str = "beats.json";
/// Songs the cache keeps; the oldest analyses leave first.
pub const MAX_SONGS: usize = 20_000;
const MAX_FILE: u64 = 8 << 20;
const VERSION: u32 = 1;
const RATE: u64 = 48_000;
/// The tempo estimate reads this window: 60 s from 30 s into the song, or
/// from its start for songs shorter than 90 s.
const WINDOW_START: u64 = 30 * RATE;
const WINDOW: u64 = 60 * RATE;

/// What the analysis of one song found.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Analysis {
    /// The estimated BPM; None when the library gives one or the estimate
    /// was too unsure.
    pub bpm: Option<f32>,
    /// The beat grid's tempo and first beat, when the grid is known.
    pub grid_bpm: Option<f64>,
    pub first_beat_ms: Option<f64>,
    /// The grid used the whole song.
    pub whole: bool,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    songs: Vec<StoredSong>,
}

#[derive(Serialize, Deserialize)]
struct StoredSong {
    library: String,
    id: String,
    #[serde(flatten)]
    analysis: Analysis,
}

/// A song by its library's key and its ID in that library.
pub type Key = (String, String);

struct Inner {
    /// Analyses with the order they were made in.
    songs: BTreeMap<Key, (u64, Analysis)>,
    next: u64,
    running: BTreeSet<Key>,
}

pub struct Beats {
    path: Option<PathBuf>,
    inner: Mutex<Inner>,
    /// One analysis at a time across the bot.
    analyses: Arc<Semaphore>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The samples per channel of the tempo window of a song `frames` long.
pub fn window(frames: u64) -> (u64, u64) {
    if frames >= WINDOW_START + WINDOW {
        (WINDOW_START, WINDOW_START + WINDOW)
    } else {
        (0, frames.min(WINDOW))
    }
}

impl Beats {
    /// The cache in `directory`; a damaged file starts an empty one.
    pub fn open(directory: &Path) -> Arc<Self> {
        let path = directory.join(FILE);
        let songs = match read(&path) {
            Ok(songs) => songs,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                tracing::warn!("{}: {error}; BPM analyses start anew", path.display());
                Vec::new()
            }
        };
        Self::with(Some(path), songs)
    }

    /// A cache that stays in memory.
    #[cfg(test)]
    pub fn memory() -> Arc<Self> {
        Self::with(None, Vec::new())
    }

    fn with(path: Option<PathBuf>, songs: Vec<StoredSong>) -> Arc<Self> {
        let mut map = BTreeMap::new();
        let mut next = 0;
        for song in songs.into_iter().rev().take(MAX_SONGS).rev() {
            map.insert((song.library, song.id), (next, song.analysis));
            next += 1;
        }
        Arc::new(Self {
            path,
            inner: Mutex::new(Inner {
                songs: map,
                next,
                running: BTreeSet::new(),
            }),
            analyses: Arc::new(Semaphore::new(1)),
        })
    }

    pub fn get(&self, key: &Key) -> Option<Analysis> {
        lock(&self.inner)
            .songs
            .get(key)
            .map(|(_, analysis)| *analysis)
    }

    pub fn running(&self, key: &Key) -> bool {
        lock(&self.inner).running.contains(key)
    }

    pub fn len(&self) -> usize {
        lock(&self.inner).songs.len()
    }

    fn insert(&self, key: Key, analysis: Analysis) {
        let mut inner = lock(&self.inner);
        let order = inner.next;
        inner.next += 1;
        inner.songs.insert(key, (order, analysis));
        while inner.songs.len() > MAX_SONGS {
            let oldest = inner
                .songs
                .iter()
                .min_by_key(|(_, (order, _))| *order)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(oldest) => inner.songs.remove(&oldest),
                None => break,
            };
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let stored = {
            let inner = lock(&self.inner);
            let mut songs: Vec<_> = inner.songs.iter().collect();
            songs.sort_by_key(|(_, (order, _))| *order);
            Stored {
                version: VERSION,
                songs: songs
                    .into_iter()
                    .map(|((library, id), (_, analysis))| StoredSong {
                        library: library.clone(),
                        id: id.clone(),
                        analysis: *analysis,
                    })
                    .collect(),
            }
        };
        let result = serde_json::to_vec(&stored)
            .map_err(io::Error::other)
            .and_then(|bytes| persist::write(path, &bytes));
        if let Err(error) = result {
            tracing::warn!("cannot save {}: {error}", path.display());
        }
    }

    /// Analyses `buffer`, the song `key`, when it needs it and the buffer
    /// holds enough: the tempo window for a first analysis, the whole song
    /// for the final grid. `library_bpm` replaces the estimate. Returns
    /// whether an analysis of the song runs.
    pub fn consider(
        self: &Arc<Self>,
        key: &Key,
        buffer: &Arc<Buffer>,
        library_bpm: Option<u32>,
    ) -> bool {
        let fill = buffer.fill();
        if matches!(fill, Fill::Failed(_) | Fill::Overflow) {
            return false;
        }
        let complete = fill == Fill::Complete;
        let frames = buffer.frames();
        let cached = {
            let mut inner = lock(&self.inner);
            if inner.running.contains(key) {
                return true;
            }
            let cached = inner.songs.get(key).map(|(_, analysis)| *analysis);
            let ready = match cached {
                Some(analysis) => !analysis.whole && complete,
                None => complete || frames >= WINDOW_START + WINDOW,
            };
            if !ready {
                return false;
            }
            inner.running.insert(key.clone());
            cached
        };
        let beats = self.clone();
        let key = key.clone();
        let buffer = buffer.clone();
        tokio::spawn(async move {
            let Ok(_permit) = beats.analyses.clone().acquire_owned().await else {
                return;
            };
            let work = {
                let key = key.clone();
                let beats = beats.clone();
                move || {
                    let analysis = analyse(&buffer, frames, complete, library_bpm, cached);
                    beats.insert(key, analysis);
                    beats.save();
                }
            };
            let _ = tokio::task::spawn_blocking(work).await;
            lock(&beats.inner).running.remove(&key);
        });
        true
    }
}

/// The tempo (from the window, without a library BPM) and the grid (over
/// the first `frames` samples) of a song in memory.
fn analyse(
    buffer: &Buffer,
    frames: u64,
    complete: bool,
    library_bpm: Option<u32>,
    cached: Option<Analysis>,
) -> Analysis {
    let bpm = match (library_bpm, cached) {
        (Some(_), _) => None,
        (None, Some(cached)) => cached.bpm,
        (None, None) => {
            let (start, end) = window(frames);
            let mut analyzer = Analyzer::new();
            buffer.visit(start, end, |samples| analyzer.push(samples));
            analyzer.finish().map(|estimate| estimate.bpm)
        }
    };
    let tempo = library_bpm.map(|bpm| bpm as f32).or(bpm);
    let grid = tempo.and_then(|tempo| {
        let mut onsets = Onsets::new();
        buffer.visit(0, frames, |samples| onsets.push(samples));
        beat_grid(&onsets, tempo)
    });
    Analysis {
        bpm,
        grid_bpm: grid.map(|grid| f64::from(grid.bpm)),
        first_beat_ms: grid.map(|grid| grid.first_beat_ms),
        whole: complete,
    }
}

fn read(path: &Path) -> io::Result<Vec<StoredSong>> {
    let file = persist::open_private(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(io::Error::other("the file is larger than 8 MiB"));
    }
    let stored: Stored = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if stored.version != VERSION {
        return Err(io::Error::other(format!(
            "unsupported version {}",
            stored.version
        )));
    }
    Ok(stored.songs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preload::tests_support::filled;

    fn key(id: &str) -> Key {
        ("0123456789abcdef0123456789abcdef".into(), id.into())
    }

    #[test]
    fn the_cache_keeps_the_newest_songs_and_survives_a_restart() {
        let directory = tempfile::tempdir().unwrap();
        let beats = Beats::open(directory.path());
        let analysis = Analysis {
            bpm: Some(124.0),
            grid_bpm: Some(124.0),
            first_beat_ms: Some(12.5),
            whole: true,
        };
        for index in 0..MAX_SONGS + 5 {
            beats.insert(key(&index.to_string()), analysis);
        }
        assert_eq!(beats.len(), MAX_SONGS);
        assert_eq!(beats.get(&key("4")), None, "the oldest left");
        assert_eq!(beats.get(&key("5")), Some(analysis));
        beats.save();
        let reopened = Beats::open(directory.path());
        assert_eq!(reopened.len(), MAX_SONGS);
        assert_eq!(reopened.get(&key("20004")), Some(analysis));
        assert_eq!(reopened.get(&key("4")), None);
        std::fs::write(directory.path().join(FILE), b"{broken").unwrap();
        assert_eq!(Beats::open(directory.path()).len(), 0);
    }

    #[test]
    fn the_window_lies_inside_the_song() {
        assert_eq!(window(200 * RATE), (30 * RATE, 90 * RATE));
        assert_eq!(window(80 * RATE), (0, 60 * RATE));
        assert_eq!(window(20 * RATE), (0, 20 * RATE));
    }

    #[tokio::test]
    async fn an_analysis_waits_for_its_window_and_finds_the_tempo_and_grid() {
        let beats = Beats::memory();
        // 100 s of clicks at 125 BPM, the first at 0.25 s.
        let (buffer, complete) = filled(100, 125.0, 0.25);
        assert!(
            !beats.consider(&key("partial"), &buffer, None),
            "the window is decoding"
        );
        complete();
        assert!(beats.consider(&key("partial"), &buffer, None));
        assert!(beats.running(&key("partial")));
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while beats.running(&key("partial")) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let analysis = beats.get(&key("partial")).unwrap();
        assert!((analysis.bpm.unwrap() - 125.0).abs() <= 1.0, "{analysis:?}");
        let period = 60_000.0 / analysis.grid_bpm.unwrap();
        let phase = (analysis.first_beat_ms.unwrap() - 250.0).rem_euclid(period);
        assert!(phase.min(period - phase) < 10.0, "{analysis:?}");
        assert!(analysis.whole);
        // Analysed once.
        assert!(!beats.consider(&key("partial"), &buffer, None));
        // With a library BPM the grid uses it and no estimate is kept.
        assert!(beats.consider(&key("tagged"), &buffer, Some(125)));
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while beats.running(&key("tagged")) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let tagged = beats.get(&key("tagged")).unwrap();
        assert_eq!(tagged.bpm, None);
        assert!(tagged.grid_bpm.is_some());
    }
}
