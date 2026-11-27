// SPDX-License-Identifier: EUPL-1.2
//! DJ mode: two decks, each fed by its own decode child, mixed live into the
//! player's outputs (Mumble and browser listeners). Controls set targets in
//! the state; the engine moves towards them frame by frame (`dsp.rs`), so
//! changes that arrive in bursts over the network glide smoothly.
use super::{
    CHANNELS, Decoding, Entry, FRAME, HistoryOutcome, MAX_QUEUE, Output, Player, Playing, State,
    TICK, UNDERRUN_END, apply_gain, decode,
    dsp::{self, Curve, Limiter, Ramp, Resampler, Tone, ToneFilter},
    next_gain, target_gain, wall_clock_ms,
};
use crate::{
    preload::{Buffer, Fill, Need, Pool, Read, Want},
    subsonic::{Library, Track},
};
use std::sync::Arc;
use std::{collections::VecDeque, fmt, time::Duration};
use tokio::{
    sync::mpsc::error::TryRecvError,
    time::{Instant, sleep, sleep_until},
};

pub const DECKS: usize = 2;
/// The tempo fader's ranges, in percent.
pub const TEMPO_RANGES: [u8; 2] = [8, 16];
/// The one-button transition's length, in seconds.
pub const MIN_TRANSITION_SECONDS: u32 = 1;
pub const MAX_TRANSITION_SECONDS: u32 = 60;
const FRAME_SAMPLES: u64 = FRAME as u64;
const SAMPLES_PER_MS: u64 = 48;
/// A deck sounds in the room at this gain in the mix or more (−20 dB).
const AUDIBLE: f32 = 0.1;
/// The longest free loop and the shortest one, in samples.
const MAX_LOOP_SAMPLES: f64 = 60.0 * 48_000.0;
const MIN_FREE_LOOP_SAMPLES: f64 = 480.0;
/// A loop's end crossfades into its start over this many samples (5 ms).
const LOOP_FADE: usize = 240;
/// Songs in a row that fail to decode before auto load stops.
const MAX_FAILURES: u32 = 3;
/// The pause before a deck's next decoder after a failure; it doubles with
/// every failure in a row, up to `MAX_BACKOFF`.
const BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(8);
/// Encoder failures in a row before DJ mode pauses.
const MAX_ENCODE_FAILURES: u32 = 3;

/// One deck: a loaded song, its transport and its channel strip.
#[derive(Clone, Debug)]
pub struct Deck {
    /// The loaded song and its play.
    pub track: Option<Playing>,
    pub playing: bool,
    /// The position in the song, in samples at 48 kHz.
    pub position: u64,
    /// The cue point, in samples.
    pub cue: u64,
    /// The channel fader, in percent.
    pub volume: u8,
    /// Low, mid and high, in dB.
    pub eq: [f32; 3],
    /// −1 (low-pass closed) to 1 (high-pass closed); 0 is neutral.
    pub filter: f32,
    /// The speed change as a fraction: 0.08 plays 8 % faster and higher.
    pub tempo: f32,
    /// The tempo fader's range, in percent.
    pub range: u8,
    /// Why this deck's last song stopped early.
    pub error: Option<String>,
    /// The queue the song came from; it returns there unheard.
    origin: Origin,
    /// Load the queue's next song once this deck has faded out.
    reload: bool,
    /// After a failure, this deck's next decoder starts at this time.
    restart_at: Option<Instant>,
    /// Samples of the song decoded in memory, or None while the deck streams.
    pub loaded: Option<u64>,
    /// The whole song is in memory.
    pub preloaded: bool,
    /// The deck plays its song from memory, so it can loop.
    pub memory: bool,
    /// The song's BPM estimated from its audio, used without a library BPM.
    pub estimate: Option<f32>,
    /// The BPM estimate or the beat grid is being computed.
    pub analysing: bool,
    /// Where the song's beats fall, when known.
    pub grid: Option<BeatGrid>,
    pub looping: Option<Loop>,
    /// Loop points snap to beats when the grid is known.
    pub quantize: bool,
}

/// A song's beats at a constant tempo: `first + k · 60 / bpm` seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeatGrid {
    pub bpm: f64,
    /// The first beat, in samples.
    pub first: f64,
}

impl BeatGrid {
    /// Samples per beat.
    pub fn period(&self) -> f64 {
        48_000.0 * 60.0 / self.bpm
    }

    /// The beat nearest to `position`.
    pub fn nearest(&self, position: f64) -> f64 {
        self.first + ((position - self.first) / self.period()).round() * self.period()
    }

    /// The beat at or before `position`.
    pub fn floor(&self, position: f64) -> f64 {
        self.first + ((position - self.first) / self.period()).floor() * self.period()
    }
}

/// A loop in song time, in samples.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Loop {
    pub start: f64,
    /// Unset between loop in and loop out.
    pub end: Option<f64>,
    pub active: bool,
}

impl Default for Deck {
    fn default() -> Self {
        Self {
            track: None,
            playing: false,
            position: 0,
            cue: 0,
            volume: 100,
            eq: [0.0; 3],
            filter: 0.0,
            tempo: 0.0,
            range: TEMPO_RANGES[0],
            error: None,
            origin: Origin::Library,
            reload: false,
            restart_at: None,
            loaded: None,
            preloaded: false,
            memory: false,
            estimate: None,
            analysing: false,
            grid: None,
            looping: None,
            quantize: true,
        }
    }
}

impl Deck {
    /// How much faster than recorded the song plays.
    pub fn ratio(&self) -> f64 {
        1.0 + f64::from(self.tempo)
    }

    /// The song's BPM before the tempo fader: the library's, else the estimate.
    pub fn base_bpm(&self) -> Option<f64> {
        let playing = self.track.as_ref()?;
        playing
            .entry
            .track
            .bpm
            .map(f64::from)
            .or(self.estimate.map(f64::from))
    }

    /// Whether the BPM comes from the library (true) or the estimate (false).
    pub fn bpm_from_library(&self) -> Option<bool> {
        let playing = self.track.as_ref()?;
        if playing.entry.track.bpm.is_some() {
            Some(true)
        } else {
            self.estimate.map(|_| false)
        }
    }

    /// The loop playing now: start and end in samples.
    fn active_loop(&self) -> Option<(f64, f64)> {
        let looping = self.looping?;
        looping
            .end
            .filter(|_| looping.active)
            .map(|end| (looping.start, end))
    }

    /// Samples per beat: from the grid, else from the BPM.
    fn beat(&self) -> Option<f64> {
        self.grid
            .map(|grid| grid.period())
            .or_else(|| self.base_bpm().map(|bpm| 48_000.0 * 60.0 / bpm))
    }

    /// `position` on the nearest beat when quantising with a known grid.
    fn snap(&self, position: f64) -> f64 {
        match self.grid {
            Some(grid) if self.quantize => grid.nearest(position),
            _ => position,
        }
    }

    /// The song's length in samples, when known.
    fn length(&self) -> f64 {
        self.track
            .as_ref()
            .and_then(|playing| playing.entry.track.duration)
            .map_or(f64::MAX, |seconds| f64::from(seconds) * 48_000.0)
    }

    /// The shortest and longest loop: a quarter beat to 64 beats with a known
    /// beat, else 10 ms to 60 s.
    fn loop_bounds(&self) -> (f64, f64) {
        match self.beat() {
            Some(beat) => (beat / 4.0, (beat * 64.0).min(MAX_LOOP_SAMPLES * 2.0)),
            None => (MIN_FREE_LOOP_SAMPLES, MAX_LOOP_SAMPLES),
        }
    }

    /// A loop from `start` to `end`, kept within the bounds and the song.
    fn bounded_loop(&self, start: f64, end: f64) -> (f64, f64) {
        let (shortest, longest) = self.loop_bounds();
        let length = (end - start).clamp(shortest, longest);
        let start = start.clamp(0.0, (self.length() - length).max(0.0));
        (start, start + length)
    }

    /// The BPM at the current tempo.
    pub fn bpm(&self) -> Option<f64> {
        self.base_bpm().map(|bpm| bpm * self.ratio())
    }

    pub fn position_ms(&self) -> u64 {
        self.position / SAMPLES_PER_MS
    }

    pub fn cue_ms(&self) -> u64 {
        self.cue / SAMPLES_PER_MS
    }

    fn tone(&self) -> Tone {
        Tone {
            eq: self.eq,
            filter: self.filter,
        }
    }

    /// Moves to `position` (samples, clamped to the song's length); the
    /// decoder restarts there.
    fn seek_to(&mut self, position: u64) -> Result<(), DjError> {
        let track = self.track.as_mut().ok_or(DjError::Empty)?;
        let length = track
            .entry
            .track
            .duration
            .map_or(u64::MAX, |seconds| u64::from(seconds) * 48_000);
        self.position = position.min(length);
        track.frames = self.position / FRAME_SAMPLES;
        track.generation += 1;
        Ok(())
    }

    /// Takes the song off; the deck keeps its channel strip.
    fn eject(&mut self) -> Option<Playing> {
        self.playing = false;
        self.position = 0;
        self.cue = 0;
        self.reload = false;
        self.origin = Origin::Library;
        self.loaded = None;
        self.preloaded = false;
        self.memory = false;
        self.estimate = None;
        self.analysing = false;
        self.grid = None;
        self.looping = None;
        self.track.take()
    }
}

/// The one-button fade from one deck to the other.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transition {
    /// The deck the mix moves to.
    pub to: usize,
    from: f32,
    target: f32,
    pub frames: u32,
    pub elapsed: u32,
}

impl Transition {
    fn new(crossfader: f32, to: usize, seconds: u32) -> Self {
        Self {
            to,
            from: crossfader,
            target: if to == 0 { -1.0 } else { 1.0 },
            frames: (seconds * 50).max(1),
            elapsed: 0,
        }
    }

    /// One frame on: the crossfader's position and whether the fade is done.
    fn advance(&mut self) -> (f32, bool) {
        self.elapsed = (self.elapsed + 1).min(self.frames);
        let share = self.elapsed as f32 / self.frames as f32;
        (
            self.from + (self.target - self.from) * share,
            self.elapsed == self.frames,
        )
    }

    pub fn duration_ms(&self) -> u64 {
        u64::from(self.frames) * 20
    }

    pub fn elapsed_ms(&self) -> u64 {
        u64::from(self.elapsed) * 20
    }
}

/// DJ mode's state: the decks and the mixer.
#[derive(Clone, Debug)]
pub struct Dj {
    pub decks: [Deck; DECKS],
    /// −1 (only deck A) to 1 (only deck B).
    pub crossfader: f32,
    pub curve: Curve,
    pub transition: Option<Transition>,
    /// After a transition or at the end of a song, the silent deck loads the
    /// queue's next song.
    pub auto_load: bool,
    /// A message for the page, such as why auto load stopped.
    pub notice: Option<String>,
    failures: u32,
    encode_failures: u32,
}

impl Default for Dj {
    fn default() -> Self {
        Self {
            decks: [Deck::default(), Deck::default()],
            crossfader: -1.0,
            curve: Curve::Smooth,
            transition: None,
            auto_load: true,
            notice: None,
            failures: 0,
            encode_failures: 0,
        }
    }
}

impl Dj {
    /// Each deck's gain in the mix: its channel fader times the crossfader.
    pub fn gains(&self) -> [f32; DECKS] {
        let crossfade = dsp::crossfade(self.crossfader, self.curve);
        [0, 1].map(|deck| dsp::fader_gain(self.decks[deck].volume) * crossfade[deck])
    }

    /// The deck the room hears most: a playing deck before a paused one,
    /// then the louder one; deck A on a tie.
    pub fn lead(&self) -> Option<usize> {
        let gains = self.gains();
        (0..DECKS)
            .filter(|&deck| self.decks[deck].track.is_some())
            .max_by(|&a, &b| {
                (self.decks[a].playing, gains[a])
                    .partial_cmp(&(self.decks[b].playing, gains[b]))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(b.cmp(&a))
            })
    }

    /// The deck song whose heard play started last, else the lead deck's song.
    pub(super) fn now_playing(&self) -> Option<&Playing> {
        self.decks
            .iter()
            .filter_map(|deck| deck.track.as_ref())
            .filter(|playing| playing.started_at_ms.is_some())
            .max_by_key(|playing| playing.started_at_ms)
            .or_else(|| self.decks[self.lead()?].track.as_ref())
    }
}

/// Why a DJ control was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DjError {
    Off,
    Empty,
    NotQueued,
    NoBpm,
    OutOfRange,
    NothingToFadeTo,
    QueueFull,
    NoLoop,
    Streaming,
}

impl fmt::Display for DjError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Off => "DJ mode is off for this queue.",
            Self::Empty => "That deck has no song.",
            Self::NotQueued => "That song has left the queue.",
            Self::NoBpm => "Sync needs the BPM of both songs.",
            Self::OutOfRange => "The other deck's BPM is beyond this deck's tempo range.",
            Self::NothingToFadeTo => "Load a song into the other deck first.",
            Self::QueueFull => "That queue is full.",
            Self::NoLoop => "Set a loop first.",
            Self::Streaming => "Loops play from memory; this song plays from the library.",
        })
    }
}

/// Where a deck's song came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Library,
    /// The main queue.
    Main,
    /// A deck's queue.
    Deck(usize),
}

impl State {
    /// The queue `origin` names: a deck's queue, or the main queue.
    fn queue_mut(&mut self, deck: Option<usize>) -> &mut VecDeque<Entry> {
        match deck {
            Some(deck) => &mut self.deck_queues[deck.min(DECKS - 1)],
            None => &mut self.queue,
        }
    }

    /// New queue entries for `tracks`, each with its own ID.
    fn entries(&mut self, tracks: Vec<Track>, added_by: &str) -> Vec<Entry> {
        tracks
            .into_iter()
            .map(|track| {
                let id = self.next_id;
                self.next_id += 1;
                Entry {
                    id,
                    track,
                    added_by: added_by.to_owned(),
                }
            })
            .collect()
    }

    /// Takes the entry `id` out of whichever queue holds it.
    fn take_entry(&mut self, id: u64) -> Option<(Entry, Option<usize>)> {
        for queue in [None, Some(0), Some(1)] {
            let list = self.queue_mut(queue);
            if let Some(position) = list.iter().position(|entry| entry.id == id) {
                return list.remove(position).map(|entry| (entry, queue));
            }
        }
        None
    }

    /// Turns DJ mode on: the current song continues on deck A where it is,
    /// and with auto load the queue's next songs wait on the empty decks.
    fn enter_dj(&mut self, now: fn() -> i64) -> bool {
        if self.dj.is_some() {
            return false;
        }
        let mut dj = Dj::default();
        if let Some(playing) = self.current.take() {
            let deck = &mut dj.decks[0];
            deck.position = playing.frames * FRAME_SAMPLES;
            deck.playing = !self.paused;
            deck.track = Some(playing);
        }
        self.paused = false;
        self.error = None;
        self.dj = Some(dj);
        for deck in 0..DECKS {
            if self
                .dj
                .as_ref()
                .is_some_and(|dj| dj.decks[deck].track.is_none())
            {
                self.load_next(deck, now);
            }
        }
        true
    }

    /// Turns DJ mode off: the lead deck's song continues as the queue's
    /// current song from its position, the other deck's play ends.
    fn leave_dj(&mut self, now: fn() -> i64) -> bool {
        let Some(dj) = self.dj.as_mut() else {
            return false;
        };
        let lead = dj.lead();
        let continued = lead.and_then(|index| {
            let deck = &mut dj.decks[index];
            let playing = deck.playing;
            let position = deck.position;
            let mut song = deck.eject()?;
            song.frames = position / FRAME_SAMPLES;
            song.generation += 1;
            song.buffering = true;
            Some((song, playing))
        });
        for deck in 0..DECKS {
            self.clear_deck(deck, HistoryOutcome::Skipped, None, now);
        }
        self.dj = None;
        self.paused = continued.as_ref().is_some_and(|(_, playing)| !playing);
        self.current = continued.map(|(song, _)| song);
        true
    }

    /// Empties `deck`; its play ends with `outcome`. A queue song the room
    /// has not heard yet goes back to the front of the queue it came from.
    fn clear_deck(
        &mut self,
        deck: usize,
        outcome: HistoryOutcome,
        error: Option<String>,
        now: fn() -> i64,
    ) {
        let Some(dj) = self.dj.as_mut() else {
            return;
        };
        let origin = dj.decks[deck].origin;
        let Some(playing) = dj.decks[deck].eject() else {
            return;
        };
        let back = match origin {
            Origin::Library => None,
            Origin::Main => Some(None),
            Origin::Deck(queue) => Some(Some(queue)),
        };
        if let Some(queue) = back
            && outcome == HistoryOutcome::Skipped
            && playing.started_at_ms.is_none()
            && self.queue_mut(queue).len() < MAX_QUEUE
        {
            self.queue_mut(queue).push_front(playing.entry);
        } else {
            self.finish_play(playing, outcome, error, now);
        }
    }

    /// Puts `entry` on `deck`, paused at its start.
    fn put_on_deck(&mut self, deck: usize, entry: Entry, origin: Origin, now: fn() -> i64) {
        self.clear_deck(deck, HistoryOutcome::Skipped, None, now);
        if let Some(dj) = self.dj.as_mut() {
            let deck = &mut dj.decks[deck];
            deck.track = Some(Playing::new(entry));
            deck.origin = origin;
        }
    }

    /// The song `deck` loads next: the first of its own queue, else the
    /// main queue's first.
    fn next_for(&self, deck: usize) -> Option<&Entry> {
        self.deck_queues[deck]
            .front()
            .or_else(|| self.queue.front())
    }

    /// Loads the next song (see [`State::next_for`]) onto `deck`.
    fn load_next(&mut self, deck: usize, now: fn() -> i64) -> bool {
        let (entry, origin) = match self.deck_queues[deck].pop_front() {
            Some(entry) => (entry, Origin::Deck(deck)),
            None => match self.queue.pop_front() {
                Some(entry) => (entry, Origin::Main),
                None => return false,
            },
        };
        self.put_on_deck(deck, entry, origin, now);
        true
    }

    /// The end of a deck's decoder; false when it belongs to an older song or seek.
    fn deck_decoded(
        &mut self,
        deck: usize,
        entry: u64,
        generation: u64,
        outcome: Result<(), String>,
        now: fn() -> i64,
        at: Instant,
    ) -> bool {
        let Some(dj) = self.dj.as_ref() else {
            return false;
        };
        let Some(playing) = dj.decks[deck]
            .track
            .as_ref()
            .filter(|playing| playing.entry.id == entry && playing.generation == generation)
        else {
            return false;
        };
        match outcome {
            Ok(()) => self.clear_deck(deck, HistoryOutcome::Completed, None, now),
            Err(error) => {
                let error = format!("{}: {error}", playing.entry.track.title);
                self.clear_deck(deck, HistoryOutcome::Failed, Some(error.clone()), now);
                let Some(dj) = self.dj.as_mut() else {
                    return true;
                };
                dj.failures = dj.failures.saturating_add(1);
                let backoff = BACKOFF
                    .saturating_mul(1 << (dj.failures - 1).min(5))
                    .min(MAX_BACKOFF);
                dj.decks[deck].restart_at = Some(at + backoff);
                dj.decks[deck].error = Some(error);
                if dj.failures >= MAX_FAILURES && dj.auto_load {
                    dj.auto_load = false;
                    dj.notice = Some(format!(
                        "Auto load stopped: {MAX_FAILURES} songs in a row failed to play."
                    ));
                }
            }
        }
        if self.dj.as_ref().is_some_and(|dj| dj.auto_load) {
            self.load_next(deck, now);
        }
        true
    }
}

/// Where a deck's song comes from.
pub enum DeckSong {
    /// Songs from a library, added by this person: the first goes onto the
    /// deck, the rest to the front of the deck's queue.
    Library(Vec<Track>, String),
    /// The entry with this ID in the main queue or a deck queue, which
    /// leaves its queue.
    Queued(u64),
}

impl Player {
    /// Turns DJ mode on or off; see [`State::enter_dj`] and [`State::leave_dj`].
    pub fn set_dj(&self, on: bool) {
        self.change(|state| {
            let changed = if on {
                state.enter_dj(wall_clock_ms)
            } else {
                state.leave_dj(wall_clock_ms)
            };
            if changed {
                for cue in &self.cues {
                    cue.reset();
                }
                if !on {
                    self.reset_browser();
                }
            }
        });
    }

    fn dj<R>(&self, change: impl FnOnce(&mut State) -> Result<R, DjError>) -> Result<R, DjError> {
        self.change(|state| {
            if state.dj.is_some() {
                change(state)
            } else {
                Err(DjError::Off)
            }
        })
    }

    fn deck<R>(
        &self,
        deck: usize,
        change: impl FnOnce(&mut Deck) -> Result<R, DjError>,
    ) -> Result<R, DjError> {
        self.dj(|state| change(&mut state.dj.as_mut().unwrap().decks[deck.min(DECKS - 1)]))
    }

    pub fn dj_load(&self, deck: usize, song: DeckSong) -> Result<(), DjError> {
        let deck = deck.min(DECKS - 1);
        self.dj(|state| {
            match song {
                DeckSong::Library(tracks, added_by) => {
                    let mut entries = state.entries(tracks, &added_by);
                    if entries.is_empty() {
                        return Err(DjError::Empty);
                    }
                    let first = entries.remove(0);
                    let queue = &mut state.deck_queues[deck];
                    let room = MAX_QUEUE.saturating_sub(queue.len());
                    for entry in entries.into_iter().take(room).rev() {
                        queue.push_front(entry);
                    }
                    state.put_on_deck(deck, first, Origin::Library, wall_clock_ms);
                }
                DeckSong::Queued(id) => {
                    let (entry, queue) = state.take_entry(id).ok_or(DjError::NotQueued)?;
                    let origin = queue.map_or(Origin::Main, Origin::Deck);
                    state.put_on_deck(deck, entry, origin, wall_clock_ms);
                }
            }
            if let Some(dj) = state.dj.as_mut() {
                dj.decks[deck].error = None;
            }
            self.cues[deck].reset();
            Ok(())
        })
    }

    /// Adds songs to `deck`'s queue at `index` (clamped; `None`: at the end);
    /// returns how many fit.
    pub fn dj_queue_add(
        &self,
        deck: usize,
        tracks: Vec<Track>,
        added_by: &str,
        index: Option<usize>,
    ) -> Result<usize, DjError> {
        let deck = deck.min(DECKS - 1);
        self.dj(|state| {
            let room = MAX_QUEUE.saturating_sub(state.deck_queues[deck].len());
            let entries = state.entries(tracks.into_iter().take(room).collect(), added_by);
            let queue = &mut state.deck_queues[deck];
            let first = index.unwrap_or(usize::MAX).min(queue.len());
            let added = entries.len();
            for (at, entry) in (first..).zip(entries) {
                queue.insert(at, entry);
            }
            Ok(added)
        })
    }

    /// Moves the entry `entry` from any queue into `deck`'s queue (`None`: the
    /// main queue) at `index` of that queue without the entry (clamped).
    pub fn dj_queue_move(
        &self,
        entry: u64,
        deck: Option<usize>,
        index: usize,
    ) -> Result<(), DjError> {
        self.dj(|state| {
            let deck = deck.map(|deck| deck.min(DECKS - 1));
            let from = [None, Some(0), Some(1)]
                .into_iter()
                .find(|queue| state.queue_mut(*queue).iter().any(|item| item.id == entry))
                .ok_or(DjError::NotQueued)?;
            if from != deck && state.queue_mut(deck).len() >= MAX_QUEUE {
                return Err(DjError::QueueFull);
            }
            let (moved, _) = state.take_entry(entry).ok_or(DjError::NotQueued)?;
            let target = state.queue_mut(deck);
            let index = index.min(target.len());
            target.insert(index, moved);
            Ok(())
        })
    }

    pub fn dj_queue_remove(&self, entry: u64) -> Result<(), DjError> {
        self.dj(|state| {
            state
                .take_entry(entry)
                .map(|_| ())
                .ok_or(DjError::NotQueued)
        })
    }

    pub fn dj_queue_clear(&self, deck: usize) -> Result<(), DjError> {
        self.dj(|state| {
            state.deck_queues[deck.min(DECKS - 1)].clear();
            Ok(())
        })
    }

    pub fn dj_eject(&self, deck: usize) -> Result<(), DjError> {
        let deck = deck.min(DECKS - 1);
        self.dj(|state| {
            state.clear_deck(deck, HistoryOutcome::Skipped, None, wall_clock_ms);
            self.cues[deck].reset();
            Ok(())
        })
    }

    pub fn dj_play(&self, deck: usize, playing: bool) -> Result<(), DjError> {
        self.deck(deck, |deck| {
            if deck.track.is_none() {
                return Err(DjError::Empty);
            }
            deck.playing = playing;
            deck.reload = false;
            Ok(())
        })
    }

    pub fn dj_seek(&self, deck: usize, position: Duration) -> Result<(), DjError> {
        let samples = u64::try_from(position.as_millis())
            .unwrap_or(u64::MAX)
            .saturating_mul(SAMPLES_PER_MS);
        self.seek_deck(deck, |_| samples)
    }

    /// Moves the deck to a position; a seek outside the playing loop ends it.
    fn seek_deck(&self, deck: usize, position: impl FnOnce(&Deck) -> u64) -> Result<(), DjError> {
        let index = deck.min(DECKS - 1);
        self.deck(index, |deck| {
            let target = position(deck);
            if let Some((start, end)) = deck.active_loop()
                && !(start..end).contains(&(target as f64))
                && let Some(looping) = deck.looping.as_mut()
            {
                looping.active = false;
            }
            deck.seek_to(target)
        })?;
        self.cues[index].reset();
        Ok(())
    }

    /// A loop change on `deck`; Err for songs that stream, which cannot loop.
    fn loop_change(
        &self,
        deck: usize,
        change: impl FnOnce(&mut Deck) -> Result<Option<u64>, DjError>,
    ) -> Result<(), DjError> {
        let index = deck.min(DECKS - 1);
        let seek = self.deck(index, |deck| {
            if deck.track.is_none() {
                return Err(DjError::Empty);
            }
            if !deck.memory {
                return Err(DjError::Streaming);
            }
            change(deck)
        })?;
        if let Some(position) = seek {
            self.seek_deck(index, |_| position)?;
        }
        Ok(())
    }

    /// Loop in: the loop starts at the position (on the nearest beat when
    /// quantising); loop out sets its end.
    pub fn dj_loop_in(&self, deck: usize) -> Result<(), DjError> {
        self.loop_change(deck, |deck| {
            let start = deck.snap(deck.position as f64).max(0.0);
            deck.looping = Some(Loop {
                start,
                end: None,
                active: false,
            });
            Ok(None)
        })
    }

    /// Loop out: the loop ends at the position and plays.
    pub fn dj_loop_out(&self, deck: usize) -> Result<(), DjError> {
        self.loop_change(deck, |deck| {
            let looping = deck.looping.ok_or(DjError::NoLoop)?;
            let mut end = deck.snap(deck.position as f64);
            if end <= looping.start {
                end = looping.start + deck.beat().unwrap_or(MIN_FREE_LOOP_SAMPLES);
            }
            let (start, end) = deck.bounded_loop(looping.start, end);
            deck.looping = Some(Loop {
                start,
                end: Some(end),
                active: true,
            });
            Ok(None)
        })
    }

    /// Turns the loop off, or plays the last loop again from its start
    /// unless the deck already is inside it.
    pub fn dj_loop_toggle(&self, deck: usize) -> Result<(), DjError> {
        self.loop_change(deck, |deck| {
            let position = deck.position as f64;
            let looping = deck.looping.as_mut().ok_or(DjError::NoLoop)?;
            let end = looping.end.ok_or(DjError::NoLoop)?;
            looping.active = !looping.active;
            let inside = (looping.start..end).contains(&position);
            Ok((looping.active && !inside).then_some(looping.start as u64))
        })
    }

    /// A loop of `beats` from the current beat (the beat at or before the
    /// position when quantising with a known grid).
    pub fn dj_beat_loop(&self, deck: usize, beats: f64) -> Result<(), DjError> {
        self.loop_change(deck, |deck| {
            let beat = deck.beat().ok_or(DjError::NoBpm)?;
            let position = deck.position as f64;
            let start = match deck.grid {
                Some(grid) if deck.quantize => grid.floor(position),
                _ => position,
            };
            let (start, end) = deck.bounded_loop(start.max(0.0), start.max(0.0) + beat * beats);
            deck.looping = Some(Loop {
                start,
                end: Some(end),
                active: true,
            });
            Ok(None)
        })
    }

    /// Halves (0.5) or doubles (2) the loop, keeping its start.
    pub fn dj_loop_resize(&self, deck: usize, factor: f64) -> Result<(), DjError> {
        self.loop_change(deck, |deck| {
            let looping = deck.looping.ok_or(DjError::NoLoop)?;
            let end = looping.end.ok_or(DjError::NoLoop)?;
            let (start, end) = deck.bounded_loop(
                looping.start,
                looping.start + (end - looping.start) * factor,
            );
            deck.looping = Some(Loop {
                start,
                end: Some(end),
                ..looping
            });
            Ok(None)
        })
    }

    /// Moves the loop by its length or by one beat; a deck playing inside it
    /// moves along.
    pub fn dj_loop_move(&self, deck: usize, by_beat: bool, direction: f64) -> Result<(), DjError> {
        self.loop_change(deck, |deck| {
            let looping = deck.looping.ok_or(DjError::NoLoop)?;
            let end = looping.end.ok_or(DjError::NoLoop)?;
            let step = if by_beat {
                deck.beat().ok_or(DjError::NoBpm)?
            } else {
                end - looping.start
            };
            let length = end - looping.start;
            let start = (looping.start + step * direction.signum())
                .clamp(0.0, (deck.length() - length).max(0.0));
            let shift = start - looping.start;
            deck.looping = Some(Loop {
                start,
                end: Some(start + length),
                ..looping
            });
            let position = deck.position as f64;
            let inside = looping.active && (looping.start..end).contains(&position);
            Ok(inside.then(|| (position + shift).max(0.0) as u64))
        })
    }

    /// Sets both loop points (dragged handles), on beats when quantising.
    pub fn dj_loop_set(&self, deck: usize, start: Duration, end: Duration) -> Result<(), DjError> {
        self.loop_change(deck, |deck| {
            let at = |time: Duration| deck.snap(time.as_secs_f64() * 48_000.0);
            let (start, mut end) = (at(start), at(end));
            if end <= start {
                end = start + deck.beat().unwrap_or(MIN_FREE_LOOP_SAMPLES);
            }
            let (start, end) = deck.bounded_loop(start, end);
            let active = deck.looping.is_some_and(|looping| looping.active);
            deck.looping = Some(Loop {
                start,
                end: Some(end),
                active,
            });
            Ok(None)
        })
    }

    pub fn dj_quantize(&self, deck: usize, on: bool) -> Result<(), DjError> {
        self.deck(deck, |deck| {
            deck.quantize = on;
            Ok(())
        })
    }

    /// Sets the cue point at the deck's position.
    pub fn dj_set_cue(&self, deck: usize) -> Result<(), DjError> {
        self.deck(deck, |deck| {
            if deck.track.is_none() {
                return Err(DjError::Empty);
            }
            deck.cue = deck.position;
            Ok(())
        })
    }

    /// Moves the deck to its cue point.
    pub fn dj_jump_to_cue(&self, deck: usize) -> Result<(), DjError> {
        self.seek_deck(deck, |deck| deck.cue)
    }

    pub fn dj_volume(&self, deck: usize, volume: u8) -> Result<(), DjError> {
        self.deck(deck, |deck| {
            deck.volume = volume.min(100);
            Ok(())
        })
    }

    pub fn dj_eq(&self, deck: usize, band: usize, db: f32) -> Result<(), DjError> {
        self.deck(deck, |deck| {
            deck.eq[band.min(2)] = db.clamp(dsp::EQ_MIN_DB, dsp::EQ_MAX_DB);
            Ok(())
        })
    }

    pub fn dj_filter(&self, deck: usize, filter: f32) -> Result<(), DjError> {
        self.deck(deck, |deck| {
            deck.filter = filter.clamp(-1.0, 1.0);
            Ok(())
        })
    }

    /// Sets the tempo as a fraction, within the deck's range.
    pub fn dj_tempo(&self, deck: usize, tempo: f32) -> Result<(), DjError> {
        self.deck(deck, |deck| {
            let range = f32::from(deck.range) / 100.0;
            deck.tempo = tempo.clamp(-range, range);
            Ok(())
        })
    }

    pub fn dj_tempo_range(&self, deck: usize, range: u8) -> Result<(), DjError> {
        self.deck(deck, |deck| {
            deck.range = if range > TEMPO_RANGES[0] {
                TEMPO_RANGES[1]
            } else {
                TEMPO_RANGES[0]
            };
            let range = f32::from(deck.range) / 100.0;
            deck.tempo = deck.tempo.clamp(-range, range);
            Ok(())
        })
    }

    /// Sets this deck's tempo so its BPM matches the other deck's; widens
    /// the range to 16 % when 8 % is too little.
    pub fn dj_sync(&self, deck: usize) -> Result<(), DjError> {
        let deck = deck.min(DECKS - 1);
        self.dj(|state| {
            let dj = state.dj.as_mut().unwrap();
            let target = dj.decks[1 - deck].bpm().ok_or(DjError::NoBpm)?;
            let own = &mut dj.decks[deck];
            let base = own.base_bpm().ok_or(DjError::NoBpm)?;
            let tempo = (target / base - 1.0) as f32;
            let widest = f32::from(TEMPO_RANGES[1]) / 100.0;
            if tempo.abs() > widest + 1e-6 {
                return Err(DjError::OutOfRange);
            }
            if tempo.abs() > f32::from(own.range) / 100.0 + 1e-6 {
                own.range = TEMPO_RANGES[1];
            }
            own.tempo = tempo.clamp(-widest, widest);
            Ok(())
        })
    }

    /// Moves the crossfader; a running transition stops where it is.
    pub fn dj_crossfader(&self, position: f32) -> Result<(), DjError> {
        self.dj(|state| {
            let dj = state.dj.as_mut().unwrap();
            dj.transition = None;
            dj.crossfader = position.clamp(-1.0, 1.0);
            Ok(())
        })
    }

    pub fn dj_curve(&self, curve: Curve) -> Result<(), DjError> {
        self.dj(|state| {
            state.dj.as_mut().unwrap().curve = curve;
            Ok(())
        })
    }

    /// Fades to the deck the crossfader is away from over `seconds`, starting
    /// that deck when it is paused.
    pub fn dj_transition(&self, seconds: u32) -> Result<(), DjError> {
        self.dj(|state| {
            let dj = state.dj.as_mut().unwrap();
            let to = usize::from(dj.crossfader <= 0.0);
            let deck = &mut dj.decks[to];
            if deck.track.is_none() {
                return Err(DjError::NothingToFadeTo);
            }
            deck.playing = true;
            deck.reload = false;
            let seconds = seconds.clamp(MIN_TRANSITION_SECONDS, MAX_TRANSITION_SECONDS);
            dj.transition = Some(Transition::new(dj.crossfader, to, seconds));
            Ok(())
        })
    }

    pub fn dj_auto_load(&self, on: bool) -> Result<(), DjError> {
        self.dj(|state| {
            let dj = state.dj.as_mut().unwrap();
            dj.auto_load = on;
            if on {
                dj.failures = 0;
                dj.notice = None;
            }
            Ok(())
        })
    }
}

/// What a deck's decoder offers for the next frame.
enum Pull {
    Frame(Vec<f32>),
    Empty,
    End,
    Failed(String),
}

trait Source {
    fn pull(&mut self) -> Pull;
}

impl Source for Decoding {
    fn pull(&mut self) -> Pull {
        match self.frames.try_recv() {
            Ok(Ok(frame)) => Pull::Frame(frame),
            Ok(Err(error)) => Pull::Failed(error),
            Err(TryRecvError::Empty) => Pull::Empty,
            // Whether the song ended well is the decoder's final result.
            Err(TryRecvError::Disconnected) => Pull::End,
        }
    }
}

/// Reads a song's PCM from memory, from sample `next` on.
struct BufferReader {
    buffer: Arc<Buffer>,
    next: u64,
    /// The song and seek it reads for.
    play: (u64, u64),
}

impl Source for BufferReader {
    fn pull(&mut self) -> Pull {
        match self.buffer.read(self.next, FRAME) {
            Read::Samples(samples) => {
                self.next += (samples.len() / CHANNELS) as u64;
                Pull::Frame(samples)
            }
            // Past the decoded part: wait for decoding, or for the deck to
            // switch to its decoder (Overflow).
            Read::Wait | Read::Overflow => Pull::Empty,
            Read::End => Pull::End,
            Read::Failed(error) => Pull::Failed(error),
        }
    }
}

/// How a deck took part in a frame.
#[derive(Debug, PartialEq)]
enum Rendered {
    /// Paused or empty: nothing.
    Idle,
    /// Playing, but its decoder has no audio yet.
    Starved,
    /// Audio in `cue`, with the deck's mix gain moving from `from` to `to`.
    /// `fading` is the last frame of a song or position the deck just left.
    Audio {
        from: f32,
        to: f32,
        fading: bool,
    },
    Failed(String),
}

/// A deck's signal path in the engine.
struct DeckEngine {
    resampler: Resampler,
    tone: ToneFilter,
    ratio: f64,
    /// 1 while playing, 0 while paused; changes ramp over one frame.
    play: f32,
    fader: Ramp,
    /// The song and seek the buffered audio belongs to.
    bound: Option<(u64, u64)>,
    /// The deck's output before its channel fader, reused every frame.
    cue: Vec<f32>,
    /// What played past a loop's end, fading out as the loop's start fades in.
    tail: Option<Tail>,
}

/// The continuation past a loop's end during the loop crossfade.
struct Tail {
    resampler: Resampler,
    reader: BufferReader,
    /// Crossfade samples done.
    done: usize,
    /// Where in the current frame the loop started again.
    offset: usize,
}

impl Default for DeckEngine {
    fn default() -> Self {
        Self {
            resampler: Resampler::default(),
            tone: ToneFilter::default(),
            ratio: 1.0,
            play: 0.0,
            fader: Ramp::default(),
            bound: None,
            cue: vec![0.0; FRAME * CHANNELS],
            tail: None,
        }
    }
}

impl DeckEngine {
    /// A new decoder for `play` delivers audio from sample `start`; the
    /// deck plays from `position`.
    fn attach(&mut self, play: (u64, u64), start: u64, position: u64, ratio: f64) {
        self.resampler.reset(start, position);
        self.tone.reset();
        self.play = 0.0;
        self.ratio = ratio;
        self.bound = Some(play);
        self.tail = None;
    }

    fn silent(&self) -> bool {
        self.play == 0.0
    }

    /// Buffered audio of its decoder remains to be played.
    fn pending(&self) -> bool {
        self.bound.is_some() && self.resampler.unplayed()
    }

    fn render(&mut self, deck: &Deck, gain: f32, mut feed: Feed<'_>) -> Rendered {
        let current = deck
            .track
            .as_ref()
            .map(|playing| (playing.entry.id, playing.generation));
        if self.bound != current {
            // The deck moved on: fade out what it was playing, from what is
            // buffered, then wait for the new decoder.
            // The audio past what is buffered reads as silence, under a gain
            // that is close to zero by then.
            let fading = self.bound.is_some() && self.play > 0.0;
            if !fading {
                self.play = 0.0;
                return if deck.playing {
                    Rendered::Starved
                } else {
                    Rendered::Idle
                };
            }
            self.resampler.render(self.ratio, self.ratio, &mut self.cue);
            self.tone.process(&mut self.cue, deck.tone());
            dsp::scale(&mut self.cue, self.play, 0.0);
            self.play = 0.0;
            self.bound = None;
            self.tail = None;
            let (from, to) = self.fader.step(gain);
            return Rendered::Audio {
                from,
                to,
                fading: true,
            };
        }
        if current.is_none() || (!deck.playing && self.play == 0.0) {
            return Rendered::Idle;
        }
        let from = self.ratio;
        let to = {
            let next = from + (deck.ratio() - from) * 0.35;
            if (deck.ratio() - next).abs() < 1e-6 {
                deck.ratio()
            } else {
                next
            }
        };
        if let Err(rendered) = fill(&mut self.resampler, &mut feed, from, to) {
            return rendered;
        }
        self.ratio = to;
        // Loops play from memory only.
        let looping = deck
            .active_loop()
            .filter(|_| matches!(feed, Feed::Memory(_)));
        let mut written = 0;
        for _ in 0..16 {
            written += self.resampler.render_part(
                from,
                to,
                FRAME,
                written,
                &mut self.cue[written * CHANNELS..],
                looping.map(|(_, end)| end),
            );
            let (Some((start, end)), Feed::Memory(reader)) = (looping, &mut feed) else {
                break;
            };
            if self.resampler.position() < end {
                break;
            }
            // Wrap to the loop's start; the audio past its end fades out.
            self.tail = Some(Tail {
                resampler: self.resampler.clone(),
                reader: BufferReader {
                    buffer: reader.buffer.clone(),
                    next: reader.next,
                    play: reader.play,
                },
                done: 0,
                offset: written,
            });
            let overshoot = self.resampler.position() - end;
            let restart = if (0.0..end - start).contains(&overshoot) {
                start + overshoot
            } else {
                start
            };
            let first = (restart.floor() as u64).saturating_sub(1);
            self.resampler.reset_at(first, restart);
            reader.next = first;
            if fill(&mut self.resampler, &mut feed, from, to).is_err() || written >= FRAME {
                break;
            }
        }
        self.crossfade_tail(from, to);
        self.tone.process(&mut self.cue, deck.tone());
        let play = if deck.playing { 1.0 } else { 0.0 };
        dsp::scale(&mut self.cue, self.play, play);
        self.play = play;
        let (from, to) = self.fader.step(gain);
        Rendered::Audio {
            from,
            to,
            fading: false,
        }
    }
}

/// What feeds a deck: its song in memory, its decoder, or nothing yet.
enum Feed<'a> {
    Memory(&'a mut BufferReader),
    Stream(&'a mut Decoding),
    Nothing,
}

impl Feed<'_> {
    fn pull(&mut self) -> Pull {
        match self {
            Self::Memory(reader) => reader.pull(),
            Self::Stream(decoding) => decoding.pull(),
            Self::Nothing => Pull::Empty,
        }
    }
}

/// Pulls input until a frame at ratios `from` to `to` (and one frame more,
/// so a deck that moves away can still fade out) is there; Err when the
/// deck starves or its song fails.
fn fill(
    resampler: &mut Resampler,
    feed: &mut Feed<'_>,
    from: f64,
    to: f64,
) -> Result<(), Rendered> {
    while !resampler.ready_ahead(from, to) {
        match feed.pull() {
            Pull::Frame(frame) => resampler.push(&frame),
            Pull::Empty if resampler.ready(from, to) => break,
            Pull::Empty => return Err(Rendered::Starved),
            Pull::End => resampler.end(),
            Pull::Failed(error) => return Err(Rendered::Failed(error)),
        }
    }
    Ok(())
}

impl DeckEngine {
    /// Mixes what played past a loop's end into the loop's start with an
    /// equal-power crossfade over [`LOOP_FADE`] samples.
    fn crossfade_tail(&mut self, from: f64, to: f64) {
        let Some(tail) = self.tail.as_mut() else {
            return;
        };
        let count = (LOOP_FADE - tail.done).min(FRAME - tail.offset);
        let mut feed = Feed::Memory(&mut tail.reader);
        // Missing input past the decoded part reads as silence.
        let _ = fill(&mut tail.resampler, &mut feed, from, to);
        let mut old = vec![0.0; count * CHANNELS];
        tail.resampler
            .render_part(from, to, FRAME, tail.offset, &mut old, None);
        let new = &mut self.cue[tail.offset * CHANNELS..(tail.offset + count) * CHANNELS];
        for (index, (new, old)) in new
            .as_chunks_mut::<CHANNELS>()
            .0
            .iter_mut()
            .zip(old.as_chunks::<CHANNELS>().0)
            .enumerate()
        {
            let share = (tail.done + index + 1) as f32 / LOOP_FADE as f32;
            let angle = share * std::f32::consts::FRAC_PI_2;
            for (new, old) in new.iter_mut().zip(old) {
                *new = *new * angle.sin() + old * angle.cos();
            }
        }
        tail.done += count;
        tail.offset = 0;
        if tail.done >= LOOP_FADE {
            self.tail = None;
        }
    }
}

/// One mixed frame and what it took.
struct Mixed {
    frame: Vec<f32>,
    /// A deck rendered audio into its cue buffer.
    rendered: [bool; DECKS],
    /// The play of each deck heard in this frame (entry, generation).
    heard: [Option<(u64, u64)>; DECKS],
    audio: bool,
    /// Viewers should see the new state.
    changed: bool,
}

/// DJ mode's playback: each deck's decoder or in-memory song, its signal
/// path, and the limiter.
#[derive(Default)]
struct Engine {
    decks: [DeckEngine; DECKS],
    /// Decoders of decks that stream their song.
    slots: [Option<Decoding>; DECKS],
    /// Readers of decks that play their song from memory.
    readers: [Option<BufferReader>; DECKS],
    limiter: Limiter,
    preload: Option<Preloads>,
}

/// This player's share of the preload pool.
struct Preloads {
    pool: Arc<Pool>,
    player: usize,
    /// The songs last asked for, and when.
    asked: Vec<(u64, Need)>,
    at: Option<Instant>,
}

impl Engine {
    /// Whether DJ mode has anything to play or fade.
    fn active(&self, dj: &Dj) -> bool {
        dj.transition.is_some()
            || (0..DECKS).any(|deck| {
                (dj.decks[deck].track.is_some() && dj.decks[deck].playing)
                    || !self.decks[deck].silent()
            })
    }

    /// Mixes one frame, with the state lock held.
    fn mix(&mut self, state: &mut State, now: fn() -> i64, at: Instant) -> Mixed {
        let mut mixed = Mixed {
            frame: vec![0.0; FRAME * CHANNELS],
            rendered: [false; DECKS],
            heard: [None; DECKS],
            audio: false,
            changed: false,
        };
        let mut failed: [Option<(u64, u64, String)>; DECKS] = [None, None];
        let mut ended: [Option<(u64, u64)>; DECKS] = [None, None];
        let Some(dj) = state.dj.as_mut() else {
            return mixed;
        };
        if let Some(transition) = dj.transition.as_mut() {
            let (position, done) = transition.advance();
            dj.crossfader = position;
            if done {
                let from = 1 - transition.to;
                dj.transition = None;
                dj.decks[from].playing = false;
                dj.decks[from].reload = dj.auto_load;
                mixed.changed = true;
            }
        }
        let gains = dj.gains();
        let mut started = false;
        for (index, (deck, engine)) in dj.decks.iter_mut().zip(&mut self.decks).enumerate() {
            let play = deck
                .track
                .as_ref()
                .map(|playing| (playing.entry.id, playing.generation));
            let reader = self.readers[index]
                .as_mut()
                .filter(|reader| play == Some(reader.play));
            let from_memory = reader.is_some();
            let feed = match reader {
                Some(reader) => Feed::Memory(reader),
                None => match self.slots[index].as_mut().filter(|running| {
                    play == Some((running.entry, running.generation)) && !running.retiring()
                }) {
                    Some(running) => Feed::Stream(running),
                    None => Feed::Nothing,
                },
            };
            match engine.render(deck, gains[index], feed) {
                Rendered::Idle => {}
                Rendered::Starved => {
                    if let Some(playing) = deck.track.as_mut()
                        && !std::mem::replace(&mut playing.buffering, true)
                    {
                        mixed.changed = true;
                    }
                }
                Rendered::Audio { from, to, fading } => {
                    dsp::mix_into(&mut mixed.frame, &engine.cue, from, to);
                    mixed.audio = true;
                    mixed.rendered[index] = true;
                    if fading {
                        continue;
                    }
                    if from.max(to) >= AUDIBLE {
                        mixed.heard[index] = play;
                    }
                    deck.position = engine.resampler.position() as u64;
                    // A song played from memory ends when all of it played;
                    // a streamed one when its decoder reports the end.
                    if from_memory && engine.resampler.drained() {
                        ended[index] = play;
                    }
                    if let Some(playing) = deck.track.as_mut() {
                        playing.frames = deck.position / FRAME_SAMPLES;
                        if std::mem::take(&mut playing.buffering) {
                            mixed.changed = true;
                            started = true;
                        }
                    }
                }
                Rendered::Failed(error) => {
                    if let Some((entry, generation)) = play {
                        failed[index] = Some((entry, generation, error));
                    }
                }
            }
        }
        if started {
            dj.failures = 0;
        }
        if mixed.audio {
            self.limiter.process(&mut mixed.frame);
        }
        for (index, failure) in failed.into_iter().enumerate() {
            if let Some((entry, generation, error)) = failure {
                mixed.changed |= state.deck_decoded(index, entry, generation, Err(error), now, at);
            }
        }
        for (index, end) in ended.into_iter().enumerate() {
            if let Some((entry, generation)) = end {
                mixed.changed |= state.deck_decoded(index, entry, generation, Ok(()), now, at);
            }
        }
        mixed
    }
}

impl Player {
    /// Plays DJ mode until it is turned off and both deck decoders have
    /// retired. `handover` is the queue's decoder, which continues as deck A's
    /// when deck A has its song; `held` is a frame it already delivered.
    pub(super) async fn run_dj(
        &self,
        out: &mut Output,
        configured_bitrate: u32,
        libraries: &[Library],
        start: &mut impl FnMut(Track, decode::Start) -> Decoding,
        (handover, held): (Option<Decoding>, Option<Vec<f32>>),
        preload: Option<(Arc<Pool>, usize)>,
    ) {
        let mut engine = Engine {
            preload: preload.map(|(pool, player)| Preloads {
                pool,
                player,
                asked: Vec::new(),
                at: None,
            }),
            ..Engine::default()
        };
        if let Some(decoding) = handover {
            {
                let state = self.state();
                let deck = state.dj.as_ref().map(|dj| &dj.decks[0]);
                if let Some(deck) = deck
                    && let Some(playing) = deck.track.as_ref()
                    && (playing.entry.id, playing.generation)
                        == (decoding.entry, decoding.generation)
                    && !decoding.retiring()
                {
                    engine.decks[0].attach(
                        (decoding.entry, decoding.generation),
                        playing.frames * FRAME_SAMPLES,
                        deck.position,
                        deck.ratio(),
                    );
                    if let Some(frame) = held {
                        engine.decks[0].resampler.push(&frame);
                    }
                }
            }
            engine.slots[0] = Some(decoding);
        }
        let mut deadline: Option<Instant> = None;
        let mut underrun = 0;
        loop {
            for deck in 0..DECKS {
                self.poll_deck(&mut engine, deck).await;
            }
            if self.state().dj.is_none() {
                if out.talking {
                    self.end(out).await;
                }
                engine.readers = [None, None];
                if let Some(preloads) = engine.preload.as_mut()
                    && !preloads.asked.is_empty()
                {
                    preloads.pool.want(preloads.player, &[]);
                    preloads.asked.clear();
                }
                for running in engine.slots.iter_mut().flatten() {
                    if !running.retiring() {
                        running.request_cancel();
                    }
                }
                if engine.slots.iter().all(Option::is_none) {
                    return;
                }
                self.wait_for_retirement().await;
                continue;
            }
            if out.encoder.retirement_pending() && !self.retire_encoder(out).await {
                continue;
            }
            if out.reset_encoder && !self.clear_encoder(out).await {
                continue;
            }
            let due = self.follow_decks(&mut engine, libraries, start);
            let (paused, volume, ducking, active) = {
                let state = self.state();
                let active = state.dj.as_ref().is_some_and(|dj| engine.active(dj));
                (state.paused, state.volume, state.ducking, active)
            };
            let cueing = self.cue_listeners().iter().any(|listeners| *listeners > 0);
            if paused || !active || !(cueing || self.has_output(configured_bitrate)) {
                if out.talking {
                    self.end(out).await;
                }
                deadline = None;
                let wait = due.map_or(Duration::from_millis(500), |due| {
                    due.saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(500))
                });
                tokio::select! {
                    _ = self.wake.notified() => {}
                    _ = sleep(wait) => {}
                }
                continue;
            }
            let next = deadline.get_or_insert_with(Instant::now);
            sleep_until(*next).await;
            *next += TICK;
            let now = Instant::now();
            if *next + TICK * 10 < now {
                *next = now;
            }
            let ducked = self.ducked(&ducking);
            if ducked != out.was_ducked {
                out.was_ducked = ducked;
                self.changes.notify();
            }
            let mixed = {
                let mut state = self.state();
                if state.paused || state.dj.is_none() {
                    continue;
                }
                let mixed = engine.mix(&mut state, wall_clock_ms, now);
                // Under the lock, so a cue reset cannot follow its stale audio.
                for (deck, rendered) in mixed.rendered.iter().enumerate() {
                    if *rendered {
                        self.cues[deck].send(&engine.decks[deck].cue);
                    }
                }
                mixed
            };
            if mixed.changed {
                self.changes.notify();
            }
            if !mixed.audio {
                underrun += 1;
                if underrun >= UNDERRUN_END && out.talking {
                    self.end(out).await;
                }
                continue;
            }
            underrun = 0;
            let bitrate = self.native_bitrate(configured_bitrate);
            if out.talking && bitrate == 0 {
                self.end(out).await;
            }
            let encoded = if bitrate > 0 {
                let target = target_gain(volume, &ducking, ducked);
                let next = next_gain(out.gain, target);
                let mut samples = mixed.frame.clone();
                apply_gain(&mut samples, out.gain, next);
                Some((out.encoder.encode(samples, bitrate).await, next))
            } else {
                None
            };
            let backoff = self.send_mix(out, configured_bitrate, bitrate, encoded, &mixed);
            if let Some(backoff) = backoff {
                self.changes.notify();
                sleep(backoff).await;
            }
        }
    }

    /// Sends a mixed frame to the room and credits the decks it heard;
    /// returns the pause after an encoder failure.
    fn send_mix(
        &self,
        out: &mut Output,
        configured_bitrate: u32,
        bitrate: u32,
        encoded: Option<(Result<Vec<u8>, String>, f32)>,
        mixed: &Mixed,
    ) -> Option<Duration> {
        let mut state = self.state();
        if state.paused
            || state.dj.is_none()
            || !self.has_output(configured_bitrate)
            || (encoded.is_some() && self.native_bitrate(configured_bitrate) < bitrate)
        {
            out.reset_encoder |= encoded.is_some();
            return None;
        }
        let opus = match encoded {
            Some((Ok(opus), next)) => {
                out.last_bitrate = bitrate;
                out.gain = next;
                if let Some(dj) = state.dj.as_mut() {
                    dj.encode_failures = 0;
                }
                Some(opus)
            }
            Some((Err(error), _)) => return Some(self.encode_failed(&mut state, error)),
            None => None,
        };
        let (browser, native) = self.send_room(&mixed.frame, opus);
        out.talking |= native;
        if (browser || native)
            && let Some(dj) = state.dj.as_mut()
        {
            for (deck, heard) in dj.decks.iter_mut().zip(mixed.heard) {
                if let Some(playing) = deck.track.as_mut()
                    && heard == Some((playing.entry.id, playing.generation))
                {
                    playing.record_audio(wall_clock_ms);
                }
            }
        }
        None
    }

    /// An encoder failure costs its frame; after several in a row DJ mode
    /// pauses and says why. Returns the pause before the next attempt.
    fn encode_failed(&self, state: &mut State, error: String) -> Duration {
        tracing::warn!("{}: DJ mix encoding failed: {error}", self.name);
        let Some(dj) = state.dj.as_mut() else {
            return BACKOFF;
        };
        dj.encode_failures = dj.encode_failures.saturating_add(1);
        let failures = dj.encode_failures;
        if failures >= MAX_ENCODE_FAILURES {
            dj.encode_failures = 0;
            dj.notice = Some(format!(
                "DJ mode paused: encoding failed {MAX_ENCODE_FAILURES} times in a row ({error})."
            ));
            state.paused = true;
            self.reset_browser();
        }
        BACKOFF
            .saturating_mul(1 << (failures - 1).min(4))
            .min(MAX_BACKOFF)
    }

    /// Reports a deck decoder's end and frees its slot once it has retired.
    async fn poll_deck(&self, engine: &mut Engine, deck: usize) {
        let pending = engine.decks[deck].pending();
        let Some(running) = engine.slots[deck].as_mut() else {
            return;
        };
        if let Some(outcome) = running.poll_outcome(pending).await {
            let (entry, generation) = (running.entry, running.generation);
            running.request_cancel();
            let mut state = self.state();
            if state.deck_decoded(
                deck,
                entry,
                generation,
                outcome,
                wall_clock_ms,
                Instant::now(),
            ) {
                self.cues[deck].reset();
                drop(state);
                self.changes.notify();
            }
        }
        if running.retiring() && running.retired() {
            engine.slots[deck] = None;
        }
    }

    /// Asks the preload pool for the songs this player needs in memory:
    /// each deck's song and the song each deck loads next.
    fn want_preloads(&self, engine: &mut Engine) {
        let Some(preloads) = engine.preload.as_mut() else {
            return;
        };
        let state = self.state();
        let mut asked: Vec<(u64, Need, &Track)> = Vec::new();
        if let Some(dj) = state.dj.as_ref() {
            for deck in &dj.decks {
                if let Some(playing) = deck.track.as_ref() {
                    let need = if deck.playing {
                        Need::Playing
                    } else {
                        Need::Loaded
                    };
                    asked.push((playing.entry.id, need, &playing.entry.track));
                }
            }
            for deck in 0..DECKS {
                if let Some(entry) = state.next_for(deck)
                    && asked.iter().all(|(id, _, _)| *id != entry.id)
                {
                    asked.push((entry.id, Need::Next, &entry.track));
                }
            }
        }
        let summary: Vec<(u64, Need)> = asked.iter().map(|(id, need, _)| (*id, *need)).collect();
        let stale = preloads
            .at
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(1));
        if summary == preloads.asked && !stale {
            return;
        }
        let wants: Vec<Want> = asked
            .iter()
            .map(|(entry, need, track)| Want {
                entry: *entry,
                track: (*track).clone(),
                need: *need,
            })
            .collect();
        drop(state);
        preloads.pool.want(preloads.player, &wants);
        preloads.asked = summary;
        preloads.at = Some(Instant::now());
    }

    /// Analyses the songs the decks load next once enough of them is in memory.
    fn analyse_next(&self, engine: &Engine, libraries: &[Library]) {
        let Some(preloads) = engine.preload.as_ref() else {
            return;
        };
        let next: Vec<(u64, (String, String), Option<u32>)> = {
            let state = self.state();
            if state.dj.is_none() {
                return;
            }
            (0..DECKS)
                .filter_map(|deck| {
                    let entry = state.next_for(deck)?;
                    let key = libraries.get(entry.track.library)?.key().to_owned();
                    Some((entry.id, (key, entry.track.id.clone()), entry.track.bpm))
                })
                .collect()
        };
        for (entry, key, bpm) in next {
            if let Some(buffer) = preloads.pool.buffer(preloads.player, entry) {
                preloads.pool.beats().consider(&key, &buffer, bpm);
            }
        }
    }

    /// Keeps, retires or starts what feeds each deck so that it plays the
    /// deck's song from its position: the song in memory when the preload
    /// pool holds it, otherwise one decoder per deck at most. A deck keeps
    /// its source until the song or position changes; after a change it
    /// first fades out what it played. Returns when a start that waits after
    /// a failure is due.
    fn follow_decks(
        &self,
        engine: &mut Engine,
        libraries: &[Library],
        start: &mut impl FnMut(Track, decode::Start) -> Decoding,
    ) -> Option<Instant> {
        let now = Instant::now();
        let mut due: Option<Instant> = None;
        self.want_preloads(engine);
        self.analyse_next(engine, libraries);
        for index in 0..DECKS {
            'deck: {
                {
                    let mut state = self.state();
                    if state.dj.as_ref().is_some_and(|dj| dj.decks[index].reload)
                        && engine.decks[index].silent()
                    {
                        if let Some(dj) = state.dj.as_mut() {
                            dj.decks[index].reload = false;
                        }
                        if state.load_next(index, wall_clock_ms) {
                            self.cues[index].reset();
                        }
                        drop(state);
                        self.changes.notify();
                    }
                }
                let wanted = {
                    let state = self.state();
                    state.dj.as_ref().and_then(|dj| {
                        let deck = &dj.decks[index];
                        let playing = deck.track.as_ref()?;
                        Some((playing.entry.id, playing.generation, deck.restart_at))
                    })
                };
                let current = wanted.map(|(entry, generation, _)| (entry, generation));
                let buffer = match (&engine.preload, current) {
                    (Some(preloads), Some((entry, _))) => {
                        preloads.pool.buffer(preloads.player, entry)
                    }
                    _ => None,
                };
                self.show_song(index, buffer.as_ref(), engine.preload.as_ref(), libraries);
                // A song that outgrew its buffer streams from where the memory ends.
                if let Some(reader) = engine.readers[index].as_ref()
                    && Some(reader.play) == current
                    && reader.buffer.fill() == Fill::Overflow
                    && reader.next >= reader.buffer.frames()
                {
                    engine.readers[index] = None;
                    engine.decks[index].bound = None;
                    engine.decks[index].play = 0.0;
                }
                let buffer = buffer.filter(|buffer| buffer.fill() != Fill::Overflow);
                let streaming = engine.slots[index].as_ref().is_some_and(|running| {
                    current == Some((running.entry, running.generation)) && !running.retiring()
                });
                let reading = engine.readers[index]
                    .as_ref()
                    .is_some_and(|reader| Some(reader.play) == current);
                if let Some(running) = engine.slots[index].as_mut()
                    && !streaming
                {
                    if !running.retiring() {
                        running.request_cancel();
                    }
                    if running.retired() {
                        engine.slots[index] = None;
                    }
                }
                if !reading {
                    engine.readers[index] = None;
                }
                if streaming || reading {
                    break 'deck;
                }
                let Some((_, _, restart_at)) = wanted else {
                    break 'deck;
                };
                let deck_engine = &engine.decks[index];
                if deck_engine.bound.is_some()
                    && deck_engine.bound != current
                    && deck_engine.play > 0.0
                {
                    // Fade out first; the next frame attaches.
                    break 'deck;
                }
                if let Some(at) = restart_at
                    && at > now
                {
                    due = Some(due.map_or(at, |due| due.min(at)));
                    break 'deck;
                }
                if buffer.is_none() && engine.slots[index].is_some() {
                    // The old decoder keeps the deck's slot until it has retired.
                    break 'deck;
                }
                let request = {
                    let mut state = self.state();
                    state.dj.as_mut().and_then(|dj| {
                        let deck = &mut dj.decks[index];
                        deck.restart_at = None;
                        let (position, ratio) = (deck.position, deck.ratio());
                        let playing = deck.track.as_mut()?;
                        if playing.source.is_none() {
                            playing.source = libraries
                                .get(playing.entry.track.library)
                                .map(Library::identity);
                        }
                        let request = decode::Start {
                            entry: playing.entry.id,
                            generation: playing.generation,
                            frames: position / FRAME_SAMPLES,
                        };
                        Some((playing.entry.track.clone(), request, position, ratio))
                    })
                };
                let Some((track, request, position, ratio)) = request else {
                    break 'deck;
                };
                let play = (request.entry, request.generation);
                if let Some(buffer) = buffer {
                    // From memory: the position may lie anywhere, also mid-frame.
                    let first = position.saturating_sub(1);
                    engine.readers[index] = Some(BufferReader {
                        buffer,
                        next: first,
                        play,
                    });
                    engine.decks[index].attach(play, first, position, ratio);
                } else {
                    let first = request.frames * FRAME_SAMPLES;
                    engine.slots[index] = Some(start(track, request));
                    engine.decks[index].attach(play, first, position, ratio);
                }
            }
            let reading = {
                let state = self.state();
                state.dj.as_ref().and_then(|dj| {
                    let playing = dj.decks[index].track.as_ref()?;
                    Some((playing.entry.id, playing.generation))
                })
            };
            let memory = engine.readers[index]
                .as_ref()
                .is_some_and(|reader| Some(reader.play) == reading);
            if let Some(dj) = self.state().dj.as_mut() {
                dj.decks[index].memory = memory;
            }
        }
        due
    }

    /// Shows how much of a deck's song is in memory, and its estimated
    /// tempo and beat grid, analysing the song once enough of it is there.
    fn show_song(
        &self,
        deck: usize,
        buffer: Option<&Arc<Buffer>>,
        preloads: Option<&Preloads>,
        libraries: &[Library],
    ) {
        let song = {
            let state = self.state();
            state.dj.as_ref().and_then(|dj| {
                let track = &dj.decks[deck].track.as_ref()?.entry.track;
                let key = libraries.get(track.library)?.key().to_owned();
                Some(((key, track.id.clone()), track.bpm))
            })
        };
        let (analysing, analysis) = match (song, preloads) {
            (Some((key, library_bpm)), Some(preloads)) => {
                let beats = preloads.pool.beats();
                let running =
                    buffer.is_some_and(|buffer| beats.consider(&key, buffer, library_bpm));
                (running, beats.get(&key))
            }
            _ => (false, None),
        };
        let estimate = analysis.and_then(|analysis| analysis.bpm);
        let grid = analysis.and_then(|analysis| {
            Some(BeatGrid {
                bpm: analysis.grid_bpm?,
                first: analysis.first_beat_ms? * 48.0,
            })
        });
        let buffer = buffer.map(Arc::as_ref);
        let (loaded, preloaded) = match buffer {
            Some(buffer) => match buffer.fill() {
                Fill::Partial | Fill::Complete => {
                    (Some(buffer.frames()), buffer.fill() == Fill::Complete)
                }
                Fill::Failed(_) | Fill::Overflow => (None, false),
            },
            None => (None, false),
        };
        let mut state = self.state();
        if let Some(dj) = state.dj.as_mut() {
            let deck = &mut dj.decks[deck];
            deck.loaded = loaded;
            let changed = deck.preloaded != preloaded
                || deck.analysing != analysing
                || deck.estimate != estimate
                || deck.grid != grid;
            deck.preloaded = preloaded;
            deck.analysing = analysing;
            deck.estimate = estimate;
            deck.grid = grid;
            if changed {
                drop(state);
                self.changes.notify();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode::tests::controlled, player::Changes};
    use std::sync::{Arc, Mutex};
    use tokio::sync::{mpsc, oneshot};

    fn track(id: &str, bpm: Option<u32>) -> Track {
        Track {
            library: 0,
            id: id.into(),
            title: id.into(),
            artist: None,
            album: None,
            album_id: None,
            artist_id: None,
            duration: Some(300),
            track: None,
            suffix: None,
            content_type: None,
            cover_art: None,
            bpm,
        }
    }

    fn player() -> Arc<Player> {
        Arc::new(Player::new(
            "test".into(),
            100,
            super::super::Ducking::default(),
            Changes::default(),
        ))
    }

    fn now() -> i64 {
        1_000
    }

    fn deck_ids(player: &Player) -> [Option<String>; DECKS] {
        let state = player.state();
        let dj = state.dj.as_ref().unwrap();
        [0, 1].map(|deck| {
            dj.decks[deck]
                .track
                .as_ref()
                .map(|playing| playing.entry.track.id.clone())
        })
    }

    fn queued(player: &Player) -> Vec<String> {
        player
            .state()
            .queue
            .iter()
            .map(|entry| entry.track.id.clone())
            .collect()
    }

    /// A controlled decoder for a deck's song, and the sender of its frames.
    struct Fed {
        sender: mpsc::Sender<decode::Frame>,
        _terminal: oneshot::Sender<Result<(), String>>,
        finish: oneshot::Sender<Result<(), String>>,
    }

    fn feed(engine: &mut Engine, state: &State, deck: usize) -> Fed {
        let mut controlled = controlled();
        let dj = state.dj.as_ref().unwrap();
        let playing = dj.decks[deck].track.as_ref().unwrap();
        controlled.decoding.entry = playing.entry.id;
        controlled.decoding.generation = playing.generation;
        let first = dj.decks[deck].position / FRAME_SAMPLES * FRAME_SAMPLES;
        engine.decks[deck].attach(
            (playing.entry.id, playing.generation),
            first,
            dj.decks[deck].position,
            dj.decks[deck].ratio(),
        );
        engine.slots[deck] = Some(controlled.decoding);
        Fed {
            sender: controlled.sender,
            _terminal: controlled.terminal,
            finish: controlled.finish,
        }
    }

    /// Sends `frames` frames of a continuous 1 kHz tone at `level`.
    fn send_tone(sender: &mpsc::Sender<decode::Frame>, level: f32, frames: usize) {
        for frame in 0..frames {
            let samples = (0..FRAME)
                .flat_map(|index| {
                    let at = (frame * FRAME + index) as f32 / 48.0;
                    [(std::f32::consts::TAU * at).sin() * level; CHANNELS]
                })
                .collect();
            sender.try_send(Ok(samples)).unwrap();
        }
    }

    fn peak(samples: &[f32]) -> f32 {
        samples
            .iter()
            .fold(0.0, |peak, sample| peak.max(sample.abs()))
    }

    #[test]
    fn a_transition_moves_the_crossfader_linearly_over_its_time() {
        let mut transition = Transition::new(-1.0, 1, 2);
        assert_eq!(transition.frames, 100);
        assert_eq!(transition.duration_ms(), 2_000);
        let mut position = -1.0;
        for frame in 1..=100 {
            let (next, done) = transition.advance();
            assert!(next > position);
            position = next;
            if frame == 50 {
                assert!(next.abs() < 1e-6);
                assert_eq!(transition.elapsed_ms(), 1_000);
            }
            assert_eq!(done, frame == 100);
        }
        assert_eq!(position, 1.0);
        // From halfway back to A.
        let mut back = Transition::new(0.5, 0, 1);
        for _ in 0..25 {
            back.advance();
        }
        assert!((back.advance().0 - (0.5 - 1.5 * 26.0 / 50.0)).abs() < 1e-6);
    }

    #[test]
    fn entering_continues_the_current_song_on_deck_a_and_loads_the_queue() {
        let player = player();
        player.enqueue(
            vec![track("a", None), track("b", None), track("c", None)],
            "ada",
            None,
        );
        let (entry, generation) = {
            let mut state = player.state();
            assert!(state.start_next(|_| None));
            let playing = state.current.as_mut().unwrap();
            playing.frames = 250;
            (playing.entry.id, playing.generation)
        };
        player.set_dj(true);
        assert_eq!(deck_ids(&player), [Some("a".into()), Some("b".into())]);
        assert_eq!(queued(&player), ["c"]);
        let state = player.state();
        let dj = state.dj.as_ref().unwrap();
        let a = &dj.decks[0];
        assert!(a.playing && !dj.decks[1].playing);
        assert_eq!(a.position, 250 * FRAME_SAMPLES);
        assert_eq!(a.position_ms(), 5_000);
        // The same play and decoder generation: the decoder carries on.
        let playing = a.track.as_ref().unwrap();
        assert_eq!((playing.entry.id, playing.generation), (entry, generation));
        assert!(state.current.is_none());
        assert_eq!(dj.crossfader, -1.0);
    }

    #[test]
    fn leaving_continues_the_lead_deck_and_returns_unheard_queue_songs() {
        let player = player();
        player.enqueue(
            vec![track("a", None), track("b", None), track("c", None)],
            "ada",
            None,
        );
        player.set_dj(true);
        player.dj_play(1, true).unwrap();
        player.dj_crossfader(1.0).unwrap();
        {
            let mut state = player.state();
            let deck = &mut state.dj.as_mut().unwrap().decks[1];
            deck.position = 480_000;
            deck.track.as_mut().unwrap().started_at_ms = Some(5);
        }
        player.set_dj(false);
        let state = player.state();
        assert!(state.dj.is_none());
        let current = state.current.as_ref().unwrap();
        assert_eq!(current.entry.track.id, "b");
        assert_eq!(current.frames, 500);
        assert_eq!(
            current.generation, 1,
            "the decoder restarts at the position"
        );
        assert_eq!(current.started_at_ms, Some(5), "one play continues");
        assert!(!state.paused);
        // Deck A's song was never heard and goes back to the queue.
        let queue: Vec<_> = state.queue.iter().map(|entry| &*entry.track.id).collect();
        assert_eq!(queue, ["a", "c"]);
        assert!(state.history.is_empty());
    }

    fn deck_queue(player: &Player, deck: usize) -> Vec<String> {
        player.state().deck_queues[deck]
            .iter()
            .map(|entry| entry.track.id.clone())
            .collect()
    }

    #[test]
    fn deck_queues_feed_their_decks_before_the_main_queue() {
        let player = player();
        player.enqueue(vec![track("m1", None), track("m2", None)], "ada", None);
        player.set_dj(true);
        assert_eq!(deck_ids(&player), [Some("m1".into()), Some("m2".into())]);
        let added = player
            .dj_queue_add(0, vec![track("a1", None), track("a2", None)], "bo", None)
            .unwrap();
        assert_eq!(added, 2);
        player
            .dj_queue_add(0, vec![track("a0", None)], "bo", Some(0))
            .unwrap();
        assert_eq!(deck_queue(&player, 0), ["a0", "a1", "a2"]);
        // An album: its first song goes onto the deck, the rest to the front
        // of the deck's queue; deck B's unheard main-queue song goes back.
        player
            .dj_load(
                1,
                DeckSong::Library(
                    vec![track("b1", None), track("b2", None), track("b3", None)],
                    "cy".into(),
                ),
            )
            .unwrap();
        assert_eq!(deck_ids(&player)[1].as_deref(), Some("b1"));
        assert_eq!(deck_queue(&player, 1), ["b2", "b3"]);
        assert_eq!(queued(&player), ["m2"]);
        // Auto load takes deck A's own queue first; the unheard song returns to
        // the main queue it came from.
        let mut state = player.state();
        assert!(state.load_next(0, now));
        drop(state);
        assert_eq!(deck_ids(&player)[0].as_deref(), Some("a0"));
        assert_eq!(queued(&player), ["m1", "m2"]);
        // A deck-queue song nobody heard returns to the front of that queue.
        player.dj_eject(0).unwrap();
        assert_eq!(deck_queue(&player, 0), ["a0", "a1", "a2"]);
        // Moving entries between the queues.
        let a1 = player.state().deck_queues[0][1].id;
        player.dj_queue_move(a1, None, 0).unwrap();
        assert_eq!(queued(&player), ["a1", "m1", "m2"]);
        let m2 = player.state().queue[2].id;
        player.dj_queue_move(m2, Some(1), 1).unwrap();
        assert_eq!(deck_queue(&player, 1), ["b2", "m2", "b3"]);
        player.dj_load(0, DeckSong::Queued(m2)).unwrap();
        assert_eq!(deck_ids(&player)[0].as_deref(), Some("m2"));
        player.dj_eject(0).unwrap();
        assert_eq!(deck_queue(&player, 1), ["m2", "b2", "b3"]);
        let b3 = player.state().deck_queues[1][2].id;
        player.dj_queue_remove(b3).unwrap();
        assert_eq!(player.dj_queue_remove(b3), Err(DjError::NotQueued));
        // Deck queues stay while DJ mode is off.
        player.set_dj(false);
        assert_eq!(deck_queue(&player, 1), ["m2", "b2"]);
        assert_eq!(player.dj_queue_clear(1), Err(DjError::Off));
        player.set_dj(true);
        player.dj_queue_clear(1).unwrap();
        assert!(deck_queue(&player, 1).is_empty());
    }

    #[test]
    fn deck_queues_share_the_main_queue_cap() {
        let player = player();
        player.set_dj(true);
        let many: Vec<_> = (0..MAX_QUEUE + 3)
            .map(|n| track(&n.to_string(), None))
            .collect();
        assert_eq!(player.dj_queue_add(0, many, "ada", None), Ok(MAX_QUEUE));
        player.enqueue(vec![track("main", None)], "ada", None);
        let id = player.state().queue[0].id;
        assert_eq!(
            player.dj_queue_move(id, Some(0), 0),
            Err(DjError::QueueFull)
        );
        assert_eq!(queued(&player), ["main"]);
    }

    #[test]
    fn controls_validate_state_and_clamp() {
        let player = player();
        assert_eq!(player.dj_volume(0, 50), Err(DjError::Off));
        player.set_dj(true);
        assert_eq!(player.dj_play(0, true), Err(DjError::Empty));
        assert_eq!(player.dj_transition(5), Err(DjError::NothingToFadeTo));
        assert_eq!(
            player.dj_load(0, DeckSong::Queued(99)),
            Err(DjError::NotQueued)
        );
        player
            .dj_load(
                0,
                DeckSong::Library(vec![track("x", Some(120))], "ada".into()),
            )
            .unwrap();
        player.dj_tempo(0, 0.5).unwrap();
        player.dj_eq(0, 1, -100.0).unwrap();
        player.dj_filter(0, 3.0).unwrap();
        player.dj_volume(0, 250).unwrap();
        {
            let state = player.state();
            let deck = &state.dj.as_ref().unwrap().decks[0];
            assert_eq!(deck.tempo, 0.08);
            assert_eq!(deck.eq[1], dsp::EQ_MIN_DB);
            assert_eq!(deck.filter, 1.0);
            assert_eq!(deck.volume, 100);
            assert_eq!(deck.track.as_ref().unwrap().entry.added_by, "ada");
        }
        player.dj_tempo_range(0, 16).unwrap();
        player.dj_tempo(0, 0.12).unwrap();
        player.dj_tempo_range(0, 8).unwrap();
        assert_eq!(player.state().dj.as_ref().unwrap().decks[0].tempo, 0.08);
    }

    #[test]
    fn sync_matches_the_other_decks_tempo_adjusted_bpm() {
        let player = player();
        player.set_dj(true);
        player
            .dj_load(
                0,
                DeckSong::Library(vec![track("a", Some(120))], "ada".into()),
            )
            .unwrap();
        assert_eq!(player.dj_sync(0), Err(DjError::NoBpm));
        player
            .dj_load(
                1,
                DeckSong::Library(vec![track("b", Some(125))], "ada".into()),
            )
            .unwrap();
        player.dj_tempo(1, 0.04).unwrap();
        player.dj_sync(0).unwrap();
        {
            let state = player.state();
            let dj = state.dj.as_ref().unwrap();
            assert!((dj.decks[0].bpm().unwrap() - 130.0).abs() < 1e-3);
            // 130 / 120 needs more than 8 %: the range widens.
            assert_eq!(dj.decks[0].range, 16);
        }
        player
            .dj_load(
                1,
                DeckSong::Library(vec![track("c", Some(174))], "ada".into()),
            )
            .unwrap();
        assert_eq!(player.dj_sync(0), Err(DjError::OutOfRange));
    }

    #[test]
    fn seeking_counts_song_time_whatever_the_tempo() {
        let player = player();
        player.set_dj(true);
        player
            .dj_load(0, DeckSong::Library(vec![track("a", None)], "ada".into()))
            .unwrap();
        player.dj_tempo(0, -0.08).unwrap();
        player.dj_seek(0, Duration::from_millis(10_010)).unwrap();
        player.dj_set_cue(0).unwrap();
        let state = player.state();
        let deck = &state.dj.as_ref().unwrap().decks[0];
        assert_eq!(deck.position, 480_480);
        assert_eq!(deck.cue_ms(), 10_010);
        let playing = deck.track.as_ref().unwrap();
        assert_eq!(playing.frames, 500, "the decoder skips whole frames");
        assert_eq!(playing.generation, 1);
    }

    #[tokio::test]
    async fn position_follows_the_tempo_and_starts_mid_frame() {
        let player = player();
        player.set_dj(true);
        player
            .dj_load(0, DeckSong::Library(vec![track("a", None)], "ada".into()))
            .unwrap();
        player.dj_seek(0, Duration::from_millis(1_005)).unwrap();
        player.dj_tempo(0, 0.08).unwrap();
        player.dj_play(0, true).unwrap();
        let mut engine = Engine::default();
        let fed = feed(&mut engine, &player.state(), 0);
        send_tone(&fed.sender, 0.25, 80);
        for _ in 0..50 {
            let mixed = engine.mix(&mut player.state(), now, Instant::now());
            assert!(mixed.audio);
            assert_eq!(mixed.frame.len(), FRAME * CHANNELS);
        }
        let state = player.state();
        let deck = &state.dj.as_ref().unwrap().decks[0];
        let expected = 48_240.0 + 50.0 * FRAME as f64 * 1.08;
        assert!(
            (deck.position as f64 - expected).abs() <= 1.0,
            "{} vs {expected}",
            deck.position
        );
        assert_eq!(
            deck.track.as_ref().unwrap().frames,
            deck.position / FRAME_SAMPLES
        );
        drop(fed.finish);
    }

    #[tokio::test]
    async fn the_crossfader_decides_which_deck_the_room_hears() {
        let player = player();
        player.enqueue(vec![track("a", None), track("b", None)], "ada", None);
        player.set_dj(true);
        player.dj_play(0, true).unwrap();
        player.dj_play(1, true).unwrap();
        let mut engine = Engine::default();
        let fed = [0, 1].map(|deck| feed(&mut engine, &player.state(), deck));
        for sender in &fed {
            send_tone(&sender.sender, 0.5, 80);
        }
        let mut mixed = engine.mix(&mut player.state(), now, Instant::now());
        for _ in 0..30 {
            mixed = engine.mix(&mut player.state(), now, Instant::now());
        }
        let a = player.state().dj.as_ref().unwrap().decks[0]
            .track
            .as_ref()
            .map(|playing| (playing.entry.id, playing.generation));
        assert_eq!(mixed.heard, [a, None]);
        assert!(mixed.rendered[1], "deck B still feeds its cue");
        assert!(
            (peak(&mixed.frame) - 0.5).abs() < 5e-3,
            "{}",
            peak(&mixed.frame)
        );
        assert!(
            (peak(&engine.decks[1].cue) - 0.5).abs() < 5e-3,
            "the cue is taken before the faders"
        );
        player.dj_crossfader(0.0).unwrap();
        for _ in 0..30 {
            mixed = engine.mix(&mut player.state(), now, Instant::now());
        }
        assert!(mixed.heard.iter().all(Option::is_some));
        // Both at −3 dB: 0.5 · √2 = 0.707.
        let both = peak(&mixed.frame);
        assert!((both - 0.707).abs() < 5e-3, "{both}");
        for fed in fed {
            drop(fed.finish);
        }
    }

    #[tokio::test]
    async fn a_finished_transition_pauses_the_old_deck_and_loads_the_next_song() {
        let player = player();
        player.enqueue(
            vec![track("a", None), track("b", None), track("c", None)],
            "ada",
            None,
        );
        player.set_dj(true);
        player.dj_play(0, true).unwrap();
        player.state().dj.as_mut().unwrap().decks[0]
            .track
            .as_mut()
            .unwrap()
            .started_at_ms = Some(7);
        let mut engine = Engine::default();
        let fed = [0, 1].map(|deck| feed(&mut engine, &player.state(), deck));
        for sender in &fed {
            send_tone(&sender.sender, 0.3, 120);
        }
        player.dj_transition(1).unwrap();
        assert!(player.state().dj.as_ref().unwrap().decks[1].playing);
        for _ in 0..50 {
            engine.mix(&mut player.state(), now, Instant::now());
        }
        {
            let state = player.state();
            let dj = state.dj.as_ref().unwrap();
            assert!(dj.transition.is_none());
            assert_eq!(dj.crossfader, 1.0);
            assert!(!dj.decks[0].playing);
            assert!(dj.decks[0].reload);
        }
        // The old deck fades out within a frame, then the queue's next song loads.
        engine.mix(&mut player.state(), now, Instant::now());
        assert!(engine.decks[0].silent());
        let start = &mut |_: Track, _: decode::Start| controlled().decoding;
        player.follow_decks(&mut engine, &[], start);
        assert_eq!(deck_ids(&player), [Some("c".into()), Some("b".into())]);
        let state = player.state();
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].entry.track.id, "a");
        assert_eq!(state.history[0].outcome, HistoryOutcome::Skipped);
        assert!(state.queue.is_empty());
    }

    #[test]
    fn a_finished_song_is_history_and_auto_load_takes_the_next() {
        let player = player();
        player.enqueue(
            vec![track("a", None), track("b", None), track("c", None)],
            "ada",
            None,
        );
        player.set_dj(true);
        let (entry, generation) = {
            let mut state = player.state();
            let playing = state.dj.as_mut().unwrap().decks[0].track.as_mut().unwrap();
            playing.started_at_ms = Some(3);
            playing.played_ms = 200_000;
            (playing.entry.id, playing.generation)
        };
        let mut state = player.state();
        assert!(!state.deck_decoded(0, entry, generation + 1, Ok(()), now, Instant::now()));
        assert!(state.deck_decoded(0, entry, generation, Ok(()), now, Instant::now()));
        assert_eq!(state.history[0].outcome, HistoryOutcome::Completed);
        assert_eq!(state.history[0].played_ms, 200_000);
        drop(state);
        assert_eq!(deck_ids(&player), [Some("c".into()), Some("b".into())]);
    }

    #[test]
    fn repeated_failures_back_off_and_stop_auto_load() {
        let player = player();
        let songs = (0..6).map(|n| track(&n.to_string(), None)).collect();
        player.enqueue(songs, "ada", None);
        player.set_dj(true);
        let at = Instant::now();
        let mut waits = Vec::new();
        for _ in 0..MAX_FAILURES {
            let mut state = player.state();
            let playing = state.dj.as_ref().unwrap().decks[0].track.clone().unwrap();
            assert!(state.deck_decoded(
                0,
                playing.entry.id,
                playing.generation,
                Err("decoder crashed".into()),
                now,
                at,
            ));
            let deck = &state.dj.as_ref().unwrap().decks[0];
            waits.push(deck.restart_at.unwrap() - at);
            assert!(deck.error.as_ref().unwrap().ends_with("decoder crashed"));
        }
        assert_eq!(
            waits,
            [
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2)
            ]
        );
        let state = player.state();
        let dj = state.dj.as_ref().unwrap();
        assert!(!dj.auto_load);
        assert!(dj.notice.as_ref().unwrap().contains("Auto load stopped"));
        assert!(
            dj.decks[0].track.is_none(),
            "nothing loads by itself any more"
        );
        assert_eq!(state.history.len(), 3);
        assert!(
            state
                .history
                .iter()
                .all(|item| item.outcome == HistoryOutcome::Failed)
        );
    }

    #[tokio::test]
    async fn a_seek_on_a_playing_deck_fades_out_before_it_waits() {
        let player = player();
        player.enqueue(vec![track("a", None)], "ada", None);
        player.set_dj(true);
        player.dj_play(0, true).unwrap();
        let mut engine = Engine::default();
        let fed = feed(&mut engine, &player.state(), 0);
        send_tone(&fed.sender, 0.5, 40);
        for _ in 0..25 {
            engine.mix(&mut player.state(), now, Instant::now());
        }
        player.dj_seek(0, Duration::from_secs(30)).unwrap();
        let mixed = engine.mix(&mut player.state(), now, Instant::now());
        assert!(mixed.audio);
        let (first, last) = (
            peak(&mixed.frame[..96]),
            peak(&mixed.frame[mixed.frame.len() - 96..]),
        );
        assert!(first > 0.4 && last < 0.03, "{first} … {last}");
        assert_eq!(
            player.state().dj.as_ref().unwrap().decks[0].position,
            1_440_000,
            "the fading frame leaves the new position alone"
        );
        let mixed = engine.mix(&mut player.state(), now, Instant::now());
        assert!(!mixed.audio);
        drop(fed.finish);
    }

    /// Records the decoders the player starts; each is a controlled one.
    #[derive(Clone, Default)]
    struct Starts(Arc<Mutex<Vec<(u64, u64, Fed)>>>);

    impl Starts {
        fn starter(&self) -> impl FnMut(Track, decode::Start) -> Decoding + use<> {
            let starts = self.0.clone();
            move |_, request| {
                let mut controlled = controlled();
                controlled.decoding.entry = request.entry;
                controlled.decoding.generation = request.generation;
                starts.lock().unwrap().push((
                    request.entry,
                    request.generation,
                    Fed {
                        sender: controlled.sender,
                        _terminal: controlled.terminal,
                        finish: controlled.finish,
                    },
                ));
                controlled.decoding
            }
        }

        fn count(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn two_decks_cost_two_decoders_that_retire_before_the_queue_resumes() {
        let player = player();
        player.enqueue(
            vec![track("a", None), track("b", None), track("c", None)],
            "ada",
            None,
        );
        let _listener = player.subscribe_browser(super::super::Listen::Room);
        let starts = Starts::default();
        let output = Output::new(
            super::super::Client::new(
                super::super::Role::Encode,
                "/nonexistent/nuschl-musicbot-codec-worker".into(),
            ),
            96_000,
        );
        let task = tokio::spawn(player.clone().play(
            Arc::new(Vec::new()),
            96_000,
            output,
            starts.starter(),
            None,
        ));
        settle().await;
        assert_eq!(starts.count(), 1, "the queue plays a");
        player.set_dj(true);
        settle().await;
        // Deck A continues with the queue's decoder; deck B gets its own.
        assert_eq!(starts.count(), 2);
        assert_eq!(deck_ids(&player), [Some("a".into()), Some("b".into())]);
        player.dj_seek(1, Duration::from_secs(10)).unwrap();
        settle().await;
        assert_eq!(
            starts.count(),
            2,
            "deck B's old decoder keeps its slot until it has retired"
        );
        let first_b = starts.0.lock().unwrap().remove(1);
        first_b.2.finish.send(Ok(())).unwrap();
        settle().await;
        assert_eq!(starts.count(), 2, "one replacement for deck B");
        player.set_dj(false);
        settle().await;
        assert_eq!(
            starts.count(),
            2,
            "the queue waits until both deck decoders retire"
        );
        let decks: Vec<_> = starts.0.lock().unwrap().drain(..).collect();
        for (_, _, fed) in decks {
            fed.finish.send(Ok(())).unwrap();
        }
        settle().await;
        assert_eq!(starts.count(), 1, "then exactly one queue decoder starts");
        assert!(player.state().current.is_some());
        task.abort();
    }

    fn timed(id: &str, seconds: u32) -> Track {
        let mut song = track(id, None);
        song.duration = Some(seconds);
        song
    }

    fn deck_state(player: &Player, deck: usize) -> Deck {
        player.state().dj.as_ref().unwrap().decks[deck].clone()
    }

    fn engine_with_pool(starts: &crate::preload::tests::Starts) -> Engine {
        Engine {
            preload: Some(Preloads {
                pool: Pool::new(1 << 30, starts.starter(), crate::beats::Beats::memory()),
                player: 0,
                asked: Vec::new(),
                at: None,
            }),
            ..Engine::default()
        }
    }

    fn no_start(_: Track, _: decode::Start) -> Decoding {
        panic!("decks with songs in memory start no decoder of their own")
    }

    async fn feed_frames(sender: &mpsc::Sender<decode::Frame>, from: usize, to: usize) {
        for frame in from..to {
            let samples = (0..FRAME)
                .flat_map(|index| {
                    let at = (frame * FRAME + index) as f32 / 48.0;
                    [(std::f32::consts::TAU * at).sin() * 0.5; CHANNELS]
                })
                .collect();
            sender.send(Ok(samples)).await.unwrap();
        }
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn step(player: &Arc<Player>, engine: &mut Engine) -> Mixed {
        player.follow_decks(engine, &[], &mut no_start);
        engine.mix(&mut player.state(), now, Instant::now())
    }

    #[tokio::test(start_paused = true)]
    async fn cue_jumps_in_memory_play_in_the_next_frame_and_later_ones_wait_for_decoding() {
        let player = player();
        player.enqueue(vec![timed("a", 60), timed("b", 60)], "ada", None);
        player.set_dj(true);
        player.dj_play(0, true).unwrap();
        let starts = crate::preload::tests::Starts::default();
        let mut engine = engine_with_pool(&starts);
        step(&player, &mut engine);
        let a = deck_state(&player, 0).track.unwrap().entry.id;
        let b = deck_state(&player, 1).track.unwrap().entry.id;
        // Both decks' songs preload; neither starts a decoder of its own.
        assert_eq!(starts.entries(), [a, b]);
        let decoder = starts.take(a);
        feed_frames(&decoder.sender, 0, 1_000).await;
        for _ in 0..30 {
            assert!(step(&player, &mut engine).audio);
        }
        assert_eq!(deck_state(&player, 0).loaded, Some(1_000 * FRAME_SAMPLES));
        // A jump inside the decoded part: one frame fades out, the next plays
        // from the new position; the deck never shows buffering.
        player.dj_seek(0, Duration::from_secs(10)).unwrap();
        let fade = step(&player, &mut engine);
        assert!(fade.audio && !deck_state(&player, 0).track.unwrap().buffering);
        let jumped = step(&player, &mut engine);
        assert!(jumped.audio);
        let deck = deck_state(&player, 0);
        assert!(!deck.track.as_ref().unwrap().buffering);
        assert_eq!(deck.position, 480_000 + FRAME_SAMPLES);
        // A jump past it waits for decoding, then plays from there.
        player.dj_seek(0, Duration::from_secs(40)).unwrap();
        step(&player, &mut engine);
        let waiting = step(&player, &mut engine);
        assert!(!waiting.audio);
        assert!(deck_state(&player, 0).track.unwrap().buffering);
        feed_frames(&decoder.sender, 1_000, 2_200).await;
        assert!(step(&player, &mut engine).audio);
        let deck = deck_state(&player, 0);
        assert!(!deck.track.as_ref().unwrap().buffering);
        assert_eq!(deck.position, 40 * 48_000 + FRAME_SAMPLES);
        // The decoder is reaped once done; leaving frees the memory.
        drop(decoder.sender);
        decoder.terminal.send(Ok(())).unwrap();
        decoder.finish.send(Ok(())).unwrap();
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pool = engine.preload.as_ref().unwrap().pool.clone();
        assert_eq!(pool.decoding(), 1, "deck B's song still decodes");
        assert!(
            deck_state(&player, 0).preloaded || pool.buffer(0, a).unwrap().fill() == Fill::Complete
        );
        // Ejected unheard, the song returns to the queue and stays as a next song.
        player.dj_eject(0).unwrap();
        step(&player, &mut engine);
        assert!(pool.buffer(0, a).is_some());
        player.dj_queue_remove(a).unwrap();
        step(&player, &mut engine);
        assert!(
            pool.buffer(0, a).is_none(),
            "a song that left frees its buffer"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_next_song_of_a_deck_queue_preloads() {
        let player = player();
        player.set_dj(true);
        player
            .dj_queue_add(0, vec![timed("next", 120)], "ada", None)
            .unwrap();
        let starts = crate::preload::tests::Starts::default();
        let mut engine = engine_with_pool(&starts);
        step(&player, &mut engine);
        let next = player.state().deck_queues[0][0].id;
        assert_eq!(starts.entries(), [next]);
        // Loading it takes the buffer that is already decoding.
        player.dj_load(0, DeckSong::Queued(next)).unwrap();
        step(&player, &mut engine);
        assert!(
            engine.readers[0]
                .as_ref()
                .is_some_and(|reader| reader.play.0 == next)
        );
        assert_eq!(starts.entries(), [next], "no second decoder for it");
    }

    /// Deck A with `buffer` in memory, playing from `position`.
    fn looping_deck(buffer: Arc<Buffer>, position: u64, tempo: f32) -> (Arc<Player>, Engine) {
        let player = player();
        player.enqueue(vec![timed("a", 60)], "ada", None);
        player.set_dj(true);
        player.dj_play(0, true).unwrap();
        player.dj_tempo_range(0, 16).unwrap();
        player.dj_tempo(0, tempo).unwrap();
        let play = {
            let mut state = player.state();
            let deck = &mut state.dj.as_mut().unwrap().decks[0];
            deck.position = position;
            deck.loaded = Some(buffer.frames());
            deck.memory = true;
            let playing = deck.track.as_ref().unwrap();
            (playing.entry.id, playing.generation)
        };
        let mut engine = Engine::default();
        let first = position.saturating_sub(1);
        engine.readers[0] = Some(BufferReader {
            buffer,
            next: first,
            play,
        });
        engine.decks[0].attach(play, first, position, 1.0 + f64::from(tempo));
        (player, engine)
    }

    #[test]
    fn a_loop_repeats_its_exact_length_at_any_tempo() {
        for tempo in [0.0, 0.08, -0.12] {
            let buffer = crate::preload::test_buffers::from_fn(60 * 48_000, |_| 0.25);
            let (player, mut engine) = looping_deck(buffer, 100_000, tempo);
            player.dj_quantize(0, false).unwrap();
            player.dj_loop_in(0).unwrap();
            player.state().dj.as_mut().unwrap().decks[0].position = 104_800;
            player.dj_loop_out(0).unwrap();
            let looping = deck_state(&player, 0).looping.unwrap();
            assert_eq!((looping.start, looping.end), (100_000.0, Some(104_800.0)));
            assert!(looping.active);
            player.state().dj.as_mut().unwrap().decks[0].position = 100_000;
            let ratio = 1.0 + f64::from(tempo);
            let mut song_time = 0.0;
            for _ in 0..200 {
                engine.mix(&mut player.state(), now, Instant::now());
                song_time += FRAME as f64 * ratio;
                let expected = 100_000.0 + song_time.rem_euclid(4_800.0);
                let position = engine.decks[0].resampler.position();
                assert!(
                    (position - expected).abs() < 1e-6 || (position - expected).abs() > 4_799.0,
                    "tempo {tempo}: {position} vs {expected}"
                );
                assert!((100_000.0..104_800.0).contains(&position));
            }
        }
    }

    #[test]
    fn a_loop_wraps_with_a_short_crossfade() {
        // A ramp: the loop's end and start differ by half of full scale.
        let buffer = crate::preload::test_buffers::from_fn(60 * 48_000, |index| {
            (index % 96_000) as f32 / 96_000.0 - 0.5
        });
        let (player, mut engine) = looping_deck(buffer, 0, 0.0);
        player.dj_quantize(0, false).unwrap();
        player.dj_loop_in(0).unwrap();
        player.state().dj.as_mut().unwrap().decks[0].position = 48_000;
        player.dj_loop_out(0).unwrap();
        player.state().dj.as_mut().unwrap().decks[0].position = 40_000;
        let mut largest: f32 = 0.0;
        let mut previous: Option<f32> = None;
        for _ in 0..120 {
            let mixed = engine.mix(&mut player.state(), now, Instant::now());
            // The cue is the deck before its fader: the signal path's output.
            for pair in engine.decks[0].cue.as_chunks::<CHANNELS>().0 {
                if let Some(previous) = previous {
                    largest = largest.max((pair[0] - previous).abs());
                }
                previous = Some(pair[0]);
            }
            assert!(mixed.audio);
        }
        // A cut would jump by 0.5; the 5 ms equal-power crossfade moves at
        // most a few thousandths per sample.
        assert!(largest < 0.02, "largest step {largest}");
    }

    #[test]
    fn beat_loops_quantise_to_the_grid_and_halve_double_and_move() {
        for bpm in [100.0, 128.0, 174.0] {
            let buffer = crate::preload::test_buffers::from_fn(60 * 48_000, |_| 0.1);
            let (player, _engine) = looping_deck(buffer, 300_000, 0.0);
            let grid = BeatGrid {
                bpm,
                first: 4_321.0,
            };
            player.state().dj.as_mut().unwrap().decks[0].grid = Some(grid);
            let period = grid.period();
            for beats in [0.25, 0.5, 1.0, 4.0, 16.0, 32.0] {
                player.dj_beat_loop(0, beats).unwrap();
                let looping = deck_state(&player, 0).looping.unwrap();
                assert!((looping.start - grid.floor(300_000.0)).abs() < 1e-6);
                assert!(
                    (looping.end.unwrap() - looping.start - beats * period).abs() < 1e-6,
                    "{bpm} {beats}"
                );
            }
            player.dj_beat_loop(0, 4.0).unwrap();
            player.dj_loop_resize(0, 0.5).unwrap();
            let length = |player: &Player| {
                let looping = deck_state(player, 0).looping.unwrap();
                looping.end.unwrap() - looping.start
            };
            assert!((length(&player) - 2.0 * period).abs() < 1e-6);
            player.dj_loop_resize(0, 2.0).unwrap();
            player.dj_loop_resize(0, 2.0).unwrap();
            assert!((length(&player) - 8.0 * period).abs() < 1e-6);
            let start = deck_state(&player, 0).looping.unwrap().start;
            player.dj_loop_move(0, false, 1.0).unwrap();
            assert!(
                (deck_state(&player, 0).looping.unwrap().start - start - 8.0 * period).abs() < 1e-6
            );
            player.dj_loop_move(0, true, -1.0).unwrap();
            assert!(
                (deck_state(&player, 0).looping.unwrap().start - start - 7.0 * period).abs() < 1e-6
            );
            // Bounds: at most 64 beats, at least a quarter beat.
            for _ in 0..10 {
                player.dj_loop_resize(0, 2.0).unwrap();
            }
            assert!((length(&player) - 64.0 * period).abs() < 1e-6);
            for _ in 0..20 {
                player.dj_loop_resize(0, 0.5).unwrap();
            }
            assert!((length(&player) - period / 4.0).abs() < 1e-6);
        }
    }

    #[test]
    fn loop_points_snap_to_beats_when_quantising_and_stay_free_otherwise() {
        let buffer = crate::preload::test_buffers::from_fn(60 * 48_000, |_| 0.1);
        let (player, _engine) = looping_deck(buffer, 77_000, 0.0);
        let set_position = |position: u64| {
            player.state().dj.as_mut().unwrap().decks[0].position = position;
        };
        player.state().dj.as_mut().unwrap().decks[0].grid = Some(BeatGrid {
            bpm: 120.0,
            first: 0.0,
        });
        player.dj_loop_in(0).unwrap();
        set_position(133_000);
        player.dj_loop_out(0).unwrap();
        let looping = deck_state(&player, 0).looping.unwrap();
        assert_eq!((looping.start, looping.end), (72_000.0, Some(144_000.0)));
        player.dj_quantize(0, false).unwrap();
        set_position(77_000);
        player.dj_loop_in(0).unwrap();
        set_position(133_000);
        player.dj_loop_out(0).unwrap();
        let looping = deck_state(&player, 0).looping.unwrap();
        assert_eq!((looping.start, looping.end), (77_000.0, Some(133_000.0)));
        // Without a grid or BPM, beat loops have no length to use.
        player.state().dj.as_mut().unwrap().decks[0].grid = None;
        assert_eq!(player.dj_beat_loop(0, 4.0), Err(DjError::NoBpm));
        // Free loops last 10 ms to 60 s.
        for _ in 0..10 {
            player.dj_loop_resize(0, 2.0).unwrap();
        }
        let looping = deck_state(&player, 0).looping.unwrap();
        assert_eq!(looping.end.unwrap() - looping.start, MAX_LOOP_SAMPLES);
    }

    #[test]
    fn seeks_outside_a_loop_end_it_and_loops_need_the_song_in_memory() {
        let buffer = crate::preload::test_buffers::from_fn(60 * 48_000, |_| 0.1);
        let (player, _engine) = looping_deck(buffer, 96_000, 0.0);
        player.state().dj.as_mut().unwrap().decks[0].grid = Some(BeatGrid {
            bpm: 120.0,
            first: 0.0,
        });
        player.dj_beat_loop(0, 4.0).unwrap();
        player.dj_seek(0, Duration::from_millis(2_500)).unwrap();
        assert!(
            deck_state(&player, 0).looping.unwrap().active,
            "inside the loop it plays on"
        );
        player.dj_seek(0, Duration::from_secs(20)).unwrap();
        assert!(!deck_state(&player, 0).looping.unwrap().active);
        // Reloop: back to the loop's start.
        player.dj_loop_toggle(0).unwrap();
        let deck = deck_state(&player, 0);
        assert!(deck.looping.unwrap().active);
        assert_eq!(deck.position, 96_000);
        player.state().dj.as_mut().unwrap().decks[0].memory = false;
        assert_eq!(player.dj_loop_toggle(0), Err(DjError::Streaming));
    }
}
