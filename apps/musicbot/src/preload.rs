// SPDX-License-Identifier: EUPL-1.2
//! Songs DJ decks play or play next, decoded completely into memory as fast
//! as their decode child delivers, so cue jumps and seeks read from memory.
//! One pool serves every player within one memory budget: at most
//! [`MAX_DECODES`] preload decode children at once (each keeps its process
//! credit until it has exited), songs up to [`MAX_SONG`] long, and the least
//! needed songs give way first. Samples are interleaved 16-bit stereo at
//! 48 kHz (11.5 MB a minute).
use crate::beats::Beats;
use crate::decode::{self, CHANNELS, Decoding, FRAME};
use crate::spectrum::Spectrum;
use crate::subsonic::Track;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::Semaphore,
    time::{Instant, sleep, timeout},
};

/// Preload decode children at once across the bot.
pub const MAX_DECODES: usize = 4;
/// Longer songs stream from their decoder.
pub const MAX_SONG: Duration = Duration::from_secs(30 * 60);
/// Samples per channel in one chunk of a buffer (one second).
const CHUNK: usize = 48_000;
const BYTES_PER_SAMPLE: usize = 2;
/// A song may decode this much longer than its stated duration.
const SLACK: f64 = 1.05;
/// Preload failures in a row after which the pool stops preloading.
const MAX_FAILURES: u32 = 3;
const BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(8);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How far a buffer has come.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fill {
    /// Decoding, or waiting for a decode child.
    Partial,
    /// The whole song is in memory.
    Complete,
    Failed(String),
    /// The song decoded past its stated length plus slack, or past
    /// [`MAX_SONG`]; the deck streams it from its decoder.
    Overflow,
}

struct Content {
    chunks: Vec<Box<[i16]>>,
    /// Samples per channel decoded.
    frames: u64,
    fill: Fill,
}

/// One song's PCM in memory, and its band energies for the deck's waveform.
pub struct Buffer {
    content: Mutex<Content>,
    spectrum: Mutex<Spectrum>,
    /// Samples per channel the buffer may hold.
    limit: u64,
    used: Arc<AtomicUsize>,
}

/// What a read of a buffer found.
#[derive(Debug, PartialEq)]
pub enum Read {
    Samples(Vec<f32>),
    /// Decoding has yet to reach the start of the read.
    Wait,
    /// The song ends before the read.
    End,
    Failed(String),
    Overflow,
}

impl Buffer {
    fn new(limit: u64, used: Arc<AtomicUsize>) -> Self {
        Self {
            content: Mutex::new(Content {
                chunks: Vec::new(),
                frames: 0,
                fill: Fill::Partial,
            }),
            spectrum: Mutex::new(Spectrum::new()),
            limit,
            used,
        }
    }

    /// The band energies of the decoded part.
    pub fn spectrum(&self) -> MutexGuard<'_, Spectrum> {
        lock(&self.spectrum)
    }

    /// Samples per channel decoded so far.
    pub fn frames(&self) -> u64 {
        lock(&self.content).frames
    }

    pub fn fill(&self) -> Fill {
        lock(&self.content).fill.clone()
    }

    /// Appends one decoded frame (interleaved stereo f32); false when the
    /// buffer is full, which marks it [`Fill::Overflow`].
    fn append(&self, frame: &[f32]) -> bool {
        let room = {
            let content = lock(&self.content);
            self.limit.saturating_sub(content.frames)
        };
        let fits = (frame.len() / CHANNELS).min(room as usize);
        self.store(&frame[..fits * CHANNELS]);
        // The waveform follows the PCM, one frame of work at a time.
        self.spectrum().push(&frame[..fits * CHANNELS]);
        if fits * CHANNELS < frame.len() {
            lock(&self.content).fill = Fill::Overflow;
            return false;
        }
        true
    }

    fn store(&self, frame: &[f32]) {
        let mut content = lock(&self.content);
        for pair in frame.as_chunks::<CHANNELS>().0 {
            let offset = (content.frames % CHUNK as u64) as usize * CHANNELS;
            if offset == 0 {
                content
                    .chunks
                    .push(vec![0i16; CHUNK * CHANNELS].into_boxed_slice());
                self.used
                    .fetch_add(CHUNK * CHANNELS * BYTES_PER_SAMPLE, Ordering::AcqRel);
            }
            let chunk = content.chunks.last_mut().expect("chunk just pushed");
            for (channel, sample) in pair.iter().enumerate() {
                chunk[offset + channel] = (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            }
            content.frames += 1;
        }
    }

    fn finish(&self, fill: Fill) {
        let mut content = lock(&self.content);
        if content.fill == Fill::Partial {
            content.fill = fill;
        }
    }

    /// Reads `count` samples per channel from sample `start` as interleaved f32.
    /// The last read of a complete song may be shorter.
    pub fn read(&self, start: u64, count: usize) -> Read {
        let content = lock(&self.content);
        let available = content.frames.saturating_sub(start);
        let whole = available >= count as u64;
        if !whole {
            match &content.fill {
                Fill::Partial => return Read::Wait,
                Fill::Overflow => return Read::Overflow,
                Fill::Failed(error) if available == 0 => return Read::Failed(error.clone()),
                Fill::Complete if available == 0 => return Read::End,
                Fill::Failed(_) | Fill::Complete => {}
            }
        }
        let take = available.min(count as u64) as usize;
        let mut samples = Vec::with_capacity(take * CHANNELS);
        let mut at = start;
        while samples.len() < take * CHANNELS {
            let chunk = &content.chunks[(at / CHUNK as u64) as usize];
            let offset = (at % CHUNK as u64) as usize;
            let run = (CHUNK - offset).min(take - samples.len() / CHANNELS);
            samples.extend(
                chunk[offset * CHANNELS..(offset + run) * CHANNELS]
                    .iter()
                    .map(|sample| f32::from(*sample) / 32767.0),
            );
            at += run as u64;
        }
        Read::Samples(samples)
    }

    /// Calls `each` with the decoded samples from `start` up to `end` (per
    /// channel) in pieces, for analysis; the lock is held per piece only.
    pub fn visit(&self, start: u64, end: u64, mut each: impl FnMut(&[f32])) {
        let mut at = start;
        while at < end {
            let count = (end - at).min(FRAME as u64 * 50) as usize;
            match self.read(at, count) {
                Read::Samples(samples) if !samples.is_empty() => {
                    at += (samples.len() / CHANNELS) as u64;
                    each(&samples);
                }
                _ => return,
            }
        }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        let chunks = lock(&self.content).chunks.len();
        self.used.fetch_sub(
            chunks * CHUNK * CHANNELS * BYTES_PER_SAMPLE,
            Ordering::AcqRel,
        );
    }
}

/// How much a song is needed; higher first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Need {
    /// The next song of a deck.
    Next,
    /// A paused deck's song.
    Loaded,
    /// A playing deck's song.
    Playing,
}

/// A song a player wants in memory: a queue entry ID and its track.
#[derive(Clone, Debug)]
pub struct Want {
    pub entry: u64,
    pub track: Track,
    pub need: Need,
}

type Key = (usize, u64);
/// Starts a decoder for a song from a frame; tests pass controlled ones.
pub type Starter = Arc<dyn Fn(Track, decode::Start) -> Decoding + Send + Sync>;

struct Song {
    buffer: Arc<Buffer>,
    track: Track,
    need: Need,
    /// Bytes the song takes when complete.
    size: usize,
    job: Option<Arc<AtomicBool>>,
}

struct Inner {
    songs: BTreeMap<Key, Song>,
    failures: u32,
    retry_at: Option<Instant>,
    notice: Option<String>,
}

/// The preload pool of the bot.
pub struct Pool {
    budget: usize,
    used: Arc<AtomicUsize>,
    decodes: Arc<Semaphore>,
    start: Starter,
    beats: Arc<Beats>,
    inner: Mutex<Inner>,
}

impl Pool {
    /// A pool within `budget` bytes; 0 preloads nothing.
    pub fn new(budget: usize, start: Starter, beats: Arc<Beats>) -> Arc<Self> {
        Arc::new(Self {
            beats,
            budget,
            used: Arc::new(AtomicUsize::new(0)),
            decodes: Arc::new(Semaphore::new(MAX_DECODES)),
            start,
            inner: Mutex::new(Inner {
                songs: BTreeMap::new(),
                failures: 0,
                retry_at: None,
                notice: None,
            }),
        })
    }

    /// The tempo and beat grids of songs, estimated from their PCM.
    pub fn beats(&self) -> &Arc<Beats> {
        &self.beats
    }

    /// Bytes of PCM in memory, including buffers decks still read.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    /// Preload decode children running or retiring.
    pub fn decoding(&self) -> usize {
        MAX_DECODES - self.decodes.available_permits()
    }

    /// Why preloading stopped, if it did.
    pub fn notice(&self) -> Option<String> {
        lock(&self.inner).notice.clone()
    }

    /// The buffer of `player`'s entry, while the pool holds it.
    pub fn buffer(&self, player: usize, entry: u64) -> Option<Arc<Buffer>> {
        lock(&self.inner)
            .songs
            .get(&(player, entry))
            .map(|song| song.buffer.clone())
    }

    /// Replaces the songs `player` wants in memory. Songs it no longer wants
    /// leave the pool; their memory returns once no deck reads them.
    pub fn want(self: &Arc<Self>, player: usize, wants: &[Want]) {
        {
            let mut inner = lock(&self.inner);
            let keys: Vec<Key> = inner
                .songs
                .range((player, 0)..=(player, u64::MAX))
                .map(|(key, _)| *key)
                .filter(|(_, entry)| wants.iter().all(|want| want.entry != *entry))
                .collect();
            for key in keys {
                if let Some(song) = inner.songs.remove(&key)
                    && let Some(job) = song.job
                {
                    job.store(true, Ordering::Release);
                }
            }
            for want in wants {
                if let Some(song) = inner.songs.get_mut(&(player, want.entry)) {
                    song.need = want.need;
                    continue;
                }
                let Some(size) = song_size(&want.track) else {
                    continue;
                };
                if !self.make_room(&mut inner, size, want.need) {
                    continue;
                }
                let limit = (size / (CHANNELS * BYTES_PER_SAMPLE)) as u64;
                inner.songs.insert(
                    (player, want.entry),
                    Song {
                        buffer: Arc::new(Buffer::new(limit, self.used.clone())),
                        track: want.track.clone(),
                        need: want.need,
                        size,
                        job: None,
                    },
                );
            }
        }
        self.schedule();
    }

    /// Frees space for a song of `size` bytes and `need` by dropping less
    /// needed songs; false when the budget cannot hold it.
    fn make_room(&self, inner: &mut Inner, size: usize, need: Need) -> bool {
        loop {
            let reserved: usize = inner.songs.values().map(|song| song.size).sum();
            if reserved + size <= self.budget {
                return true;
            }
            let victim = inner
                .songs
                .iter()
                .filter(|(_, song)| song.need < need)
                .min_by_key(|(_, song)| song.need)
                .map(|(key, _)| *key);
            let Some(victim) = victim else {
                return false;
            };
            if let Some(song) = inner.songs.remove(&victim)
                && let Some(job) = song.job
            {
                job.store(true, Ordering::Release);
            }
        }
    }

    /// Starts decoders for waiting songs, most needed first, within the
    /// decode children the pool may run; a more needed song stops a less
    /// needed decode, which resumes later where it stopped.
    fn schedule(self: &Arc<Self>) {
        let mut inner = lock(&self.inner);
        if inner.notice.is_some() || inner.retry_at.is_some_and(|at| at > Instant::now()) {
            return;
        }
        let mut waiting: Vec<Key> = inner
            .songs
            .iter()
            .filter(|(_, song)| song.job.is_none() && song.buffer.fill() == Fill::Partial)
            .map(|(key, _)| *key)
            .collect();
        waiting.sort_by_key(|key| std::cmp::Reverse(inner.songs[key].need));
        for key in waiting {
            let need = inner.songs[&key].need;
            let Ok(permit) = self.decodes.clone().try_acquire_owned() else {
                let running = inner
                    .songs
                    .values()
                    .filter(|song| song.job.is_some() && song.need < need)
                    .min_by_key(|song| song.need);
                if let Some(song) = running
                    && let Some(job) = &song.job
                {
                    job.store(true, Ordering::Release);
                }
                return;
            };
            let song = inner.songs.get_mut(&key).expect("waiting song");
            let cancel = Arc::new(AtomicBool::new(false));
            song.job = Some(cancel.clone());
            let skip = song.buffer.frames() / FRAME as u64;
            let decoding = (self.start)(
                song.track.clone(),
                decode::Start {
                    entry: key.1,
                    generation: 0,
                    frames: skip,
                },
            );
            let buffer = song.buffer.clone();
            let pool = self.clone();
            tokio::spawn(async move {
                let result = pump(decoding, &buffer, skip, &cancel).await;
                drop(permit);
                pool.finished(key, &cancel, result);
            });
        }
    }

    /// A decode job ended: `result` is None when it was stopped.
    fn finished(
        self: &Arc<Self>,
        key: Key,
        cancel: &Arc<AtomicBool>,
        result: Option<Result<(), String>>,
    ) {
        {
            let mut inner = lock(&self.inner);
            if let Some(song) = inner.songs.get_mut(&key)
                && song
                    .job
                    .as_ref()
                    .is_some_and(|job| Arc::ptr_eq(job, cancel))
            {
                song.job = None;
            }
            match result {
                Some(Ok(())) => {
                    inner.failures = 0;
                    inner.retry_at = None;
                }
                Some(Err(_)) => {
                    inner.failures = inner.failures.saturating_add(1);
                    let backoff = BACKOFF
                        .saturating_mul(1 << (inner.failures - 1).min(5))
                        .min(MAX_BACKOFF);
                    inner.retry_at = Some(Instant::now() + backoff);
                    if inner.failures >= MAX_FAILURES {
                        inner.notice = Some(format!(
                            "Preloading stopped after {MAX_FAILURES} failed decodes in a row; decks play from the library."
                        ));
                    }
                    let pool = self.clone();
                    tokio::spawn(async move {
                        sleep(backoff).await;
                        pool.schedule();
                    });
                }
                None => {}
            }
        }
        self.schedule();
    }
}

/// The bytes a song takes in memory, or None for songs the pool streams:
/// unknown or zero length, or longer than [`MAX_SONG`].
fn song_size(track: &Track) -> Option<usize> {
    let seconds = track.duration.filter(|seconds| *seconds > 0)?;
    if u64::from(seconds) > MAX_SONG.as_secs() {
        return None;
    }
    let frames = (f64::from(seconds) * 48_000.0 * SLACK).ceil() as usize;
    let chunks = frames.div_ceil(CHUNK);
    Some(chunks * CHUNK * CHANNELS * BYTES_PER_SAMPLE)
}

/// Moves a decoder's frames into `buffer` until the song ends, fails, the
/// buffer is full or `cancel` is set; returns once the decode child has
/// exited. None: stopped before the end.
async fn pump(
    mut decoding: Decoding,
    buffer: &Buffer,
    skip: u64,
    cancel: &AtomicBool,
) -> Option<Result<(), String>> {
    // A resumed decode starts at a frame boundary at or before the buffer's end.
    let mut drop_samples = buffer.frames().saturating_sub(skip * FRAME as u64) as usize;
    let mut stopped = false;
    loop {
        if cancel.load(Ordering::Acquire) {
            stopped = true;
            break;
        }
        match timeout(Duration::from_millis(100), decoding.frames.recv()).await {
            Ok(Some(Ok(frame))) => {
                let skipped = drop_samples.min(frame.len() / CHANNELS);
                drop_samples -= skipped;
                if !buffer.append(&frame[skipped * CHANNELS..]) {
                    stopped = true;
                    break;
                }
            }
            Ok(Some(Err(error))) => {
                buffer.finish(Fill::Failed(error));
                break;
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }
    if stopped {
        decoding.request_cancel();
    }
    let mut outcome = None;
    loop {
        let result = decoding.poll_outcome(false).await;
        if !decoding.retiring()
            && let Some(result) = result
        {
            outcome = Some(result);
            decoding.request_cancel();
        }
        if decoding.retiring() && decoding.retired() {
            break;
        }
        sleep(Duration::from_millis(20)).await;
    }
    if stopped {
        return None;
    }
    match outcome {
        Some(Ok(())) => {
            buffer.finish(Fill::Complete);
            Some(Ok(()))
        }
        Some(Err(error)) => {
            buffer.finish(Fill::Failed(error.clone()));
            Some(Err(error))
        }
        None => match buffer.fill() {
            Fill::Failed(error) => Some(Err(error)),
            _ => None,
        },
    }
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    /// A buffer of a click track at `bpm`, first click at `first` seconds,
    /// holding its first 50 seconds; the returned function adds the rest of
    /// `seconds` and completes it.
    pub(crate) fn filled(seconds: u64, bpm: f64, first: f64) -> (Arc<Buffer>, impl FnOnce()) {
        let buffer = Arc::new(Buffer::new(
            seconds * 48_000 + 48_000,
            Arc::new(AtomicUsize::new(0)),
        ));
        let period = 60.0 / bpm;
        let sample = move |index: u64| {
            let time = index as f64 / 48_000.0;
            let since = (time - first).rem_euclid(period);
            if time < first || since > 0.03 {
                0.0
            } else {
                ((std::f64::consts::TAU * 1_000.0 * since).sin() * (-since * 150.0).exp() * 0.8)
                    as f32
            }
        };
        let write = move |buffer: &Buffer, from: u64, to: u64| {
            for start in (from..to).step_by(FRAME) {
                let frame: Vec<f32> = (start..start + FRAME as u64)
                    .flat_map(|index| [sample(index); CHANNELS])
                    .collect();
                buffer.append(&frame);
            }
        };
        let head = 50.min(seconds) * 48_000;
        write(&buffer, 0, head);
        let rest = buffer.clone();
        (buffer, move || {
            write(&rest, head, seconds * 48_000);
            rest.finish(Fill::Complete);
        })
    }
}

#[cfg(test)]
pub(crate) mod test_buffers {
    use super::*;

    /// A complete buffer of `frames` samples per channel, both channels `sample(index)`.
    pub(crate) fn from_fn(frames: u64, sample: impl Fn(u64) -> f32) -> Arc<Buffer> {
        let buffer = Arc::new(Buffer::new(frames, Arc::new(AtomicUsize::new(0))));
        let mut pair = Vec::with_capacity(FRAME * CHANNELS);
        for index in 0..frames {
            pair.extend([sample(index); CHANNELS]);
            if pair.len() == FRAME * CHANNELS || index + 1 == frames {
                buffer.append(&pair);
                pair.clear();
            }
        }
        buffer.finish(Fill::Complete);
        buffer
    }
}
