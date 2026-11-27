// SPDX-License-Identifier: EUPL-1.2
use super::*;
use crate::decode::tests::controlled;
use tokio::sync::{mpsc, oneshot};

fn track(id: &str, seconds: Option<u32>) -> Track {
    Track {
        library: 0,
        id: id.into(),
        title: id.into(),
        artist: None,
        album: None,
        album_id: None,
        artist_id: None,
        duration: seconds,
        track: None,
        suffix: None,
        content_type: None,
        cover_art: None,
        bpm: None,
    }
}

/// A started controlled decoder: its entry, first frame, frame sender and exit.
pub(crate) struct Started {
    pub entry: u64,
    pub frames: u64,
    pub sender: mpsc::Sender<decode::Frame>,
    pub terminal: oneshot::Sender<Result<(), String>>,
    pub finish: oneshot::Sender<Result<(), String>>,
}

#[derive(Clone, Default)]
pub(crate) struct Starts(pub Arc<Mutex<Vec<Started>>>);

impl Starts {
    pub(crate) fn starter(&self) -> Starter {
        let starts = self.0.clone();
        Arc::new(move |_, request| {
            let controlled = controlled();
            lock(&starts).push(Started {
                entry: request.entry,
                frames: request.frames,
                sender: controlled.sender,
                terminal: controlled.terminal,
                finish: controlled.finish,
            });
            controlled.decoding
        })
    }

    pub(crate) fn take(&self, entry: u64) -> Started {
        let mut starts = lock(&self.0);
        let index = starts
            .iter()
            .position(|started| started.entry == entry)
            .unwrap();
        starts.remove(index)
    }

    pub(crate) fn entries(&self) -> Vec<u64> {
        lock(&self.0).iter().map(|started| started.entry).collect()
    }
}

/// A frame whose samples count up from `first` (left) and down (right).
fn frame(first: u64) -> Vec<f32> {
    (first..first + FRAME as u64)
        .flat_map(|index| {
            let value = (index % 1000) as f32 / 1000.0;
            [value, -value]
        })
        .collect()
}

async fn settle() {
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn want(entry: u64, seconds: u32, need: Need) -> Want {
    Want {
        entry,
        track: track(&entry.to_string(), Some(seconds)),
        need,
    }
}

#[test]
fn buffers_keep_16_bit_samples_across_chunks_and_report_their_fill() {
    let used = Arc::new(AtomicUsize::new(0));
    let buffer = Buffer::new(200_000, used.clone());
    assert_eq!(buffer.read(0, FRAME), Read::Wait);
    for index in 0..60 {
        assert!(buffer.append(&frame(index * FRAME as u64)));
    }
    assert_eq!(buffer.frames(), 60 * FRAME as u64);
    assert_eq!(used.load(Ordering::Acquire), 2 * CHUNK * CHANNELS * 2);
    // A read across the first chunk boundary at 48,000 samples.
    let Read::Samples(samples) = buffer.read(47_990, 20) else {
        panic!("decoded part reads");
    };
    assert_eq!(samples.len(), 40);
    for (offset, pair) in samples.as_chunks::<CHANNELS>().0.iter().enumerate() {
        let expected = ((47_990 + offset as u64) % 1000) as f32 / 1000.0;
        assert!((pair[0] - expected).abs() <= 1.0 / 32767.0);
        assert!((pair[1] + expected).abs() <= 1.0 / 32767.0);
    }
    assert_eq!(buffer.read(57_000, FRAME), Read::Wait);
    buffer.finish(Fill::Complete);
    let Read::Samples(tail) = buffer.read(57_000, FRAME) else {
        panic!("the last read of a complete song is shorter");
    };
    assert_eq!(tail.len(), 600 * CHANNELS);
    assert_eq!(buffer.read(57_600, FRAME), Read::End);
    let mut visited = 0;
    buffer.visit(1_000, 50_000, |samples| visited += samples.len() / CHANNELS);
    assert_eq!(visited, 49_000);
    drop(buffer);
    assert_eq!(
        used.load(Ordering::Acquire),
        0,
        "memory returns with the buffer"
    );
}

#[test]
fn a_buffer_stops_at_its_limit_and_failures_follow_the_decoded_part() {
    let used = Arc::new(AtomicUsize::new(0));
    let buffer = Buffer::new(1_500, used.clone());
    assert!(buffer.append(&frame(0)));
    assert!(!buffer.append(&frame(960)));
    assert_eq!(buffer.fill(), Fill::Overflow);
    assert_eq!(buffer.frames(), 1_500);
    assert_eq!(buffer.read(1_000, FRAME), Read::Overflow);
    let failed = Buffer::new(10_000, used);
    assert!(failed.append(&frame(0)));
    failed.finish(Fill::Failed("decoder stopped".into()));
    assert!(matches!(failed.read(0, FRAME), Read::Samples(_)));
    assert_eq!(
        failed.read(960, FRAME),
        Read::Failed("decoder stopped".into())
    );
}

#[test]
fn songs_beyond_the_cap_or_of_unknown_length_stream() {
    assert_eq!(song_size(&track("unknown", None)), None);
    assert_eq!(song_size(&track("zero", Some(0))), None);
    assert_eq!(song_size(&track("long", Some(30 * 60 + 1))), None);
    let half_hour = song_size(&track("half hour", Some(30 * 60))).unwrap();
    // 30 minutes, 5 % slack, whole one-second chunks: about 363 MB.
    assert_eq!(half_hour, 1890 * CHUNK * CHANNELS * 2);
}

#[tokio::test(start_paused = true)]
async fn the_budget_holds_and_less_needed_songs_give_way() {
    let starts = Starts::default();
    // Room for two four-minute songs (each 252 one-second chunks).
    let pool = Pool::new(
        2 * 252 * CHUNK * CHANNELS * 2 + 1,
        starts.starter(),
        crate::beats::Beats::memory(),
    );
    pool.want(0, &[want(1, 240, Need::Next), want(2, 240, Need::Loaded)]);
    assert!(pool.buffer(0, 1).is_some() && pool.buffer(0, 2).is_some());
    // A playing deck's song takes the next song's place.
    pool.want(
        0,
        &[
            want(1, 240, Need::Next),
            want(2, 240, Need::Loaded),
            want(3, 240, Need::Playing),
        ],
    );
    assert!(pool.buffer(0, 1).is_none(), "the next song gives way");
    assert!(pool.buffer(0, 3).is_some());
    // A song no less needed than every held one finds no room.
    pool.want(1, &[want(7, 240, Need::Loaded)]);
    assert!(pool.buffer(1, 7).is_none());
    settle().await;
    assert!(starts.entries().len() <= MAX_DECODES);
}

#[tokio::test(start_paused = true)]
async fn preloads_decode_ahead_resume_after_a_stop_and_free_their_memory() {
    let starts = Starts::default();
    let pool = Pool::new(1 << 30, starts.starter(), crate::beats::Beats::memory());
    pool.want(0, &[want(1, 60, Need::Next)]);
    settle().await;
    let first = starts.take(1);
    assert_eq!(first.frames, 0);
    for index in 0..10 {
        first
            .sender
            .try_send(Ok(frame(index * FRAME as u64)))
            .unwrap();
    }
    settle().await;
    let buffer = pool.buffer(0, 1).unwrap();
    assert_eq!(
        buffer.frames(),
        10 * FRAME as u64,
        "decoded as fast as delivered"
    );
    assert_eq!(pool.decoding(), 1);
    // Four more wanted songs, two of them more needed: the next song's decode
    // stops for one of them and resumes later where it stopped.
    pool.want(
        0,
        &[
            want(1, 60, Need::Next),
            want(2, 60, Need::Playing),
            want(3, 60, Need::Playing),
            want(4, 60, Need::Loaded),
            want(5, 60, Need::Loaded),
        ],
    );
    settle().await;
    assert_eq!(
        starts.entries().len(),
        3,
        "within {MAX_DECODES} decode children"
    );
    assert_eq!(
        pool.decoding(),
        MAX_DECODES,
        "the stopped one keeps its credit until it exits"
    );
    first.finish.send(Ok(())).unwrap();
    settle().await;
    assert_eq!(starts.entries().len(), 4);
    assert_eq!(pool.decoding(), MAX_DECODES);
    for entry in [2, 3, 4, 5] {
        let started = starts.take(entry);
        drop(started.sender);
        started.terminal.send(Ok(())).unwrap();
        started.finish.send(Ok(())).unwrap();
    }
    settle().await;
    let resumed = starts.take(1);
    assert_eq!(resumed.frames, 10, "the decode resumes at the decoded part");
    resumed
        .sender
        .try_send(Ok(frame(10 * FRAME as u64)))
        .unwrap();
    drop(resumed.sender);
    resumed.terminal.send(Ok(())).unwrap();
    resumed.finish.send(Ok(())).unwrap();
    settle().await;
    assert_eq!(buffer.frames(), 11 * FRAME as u64);
    assert_eq!(buffer.fill(), Fill::Complete);
    assert_eq!(pool.decoding(), 0, "every decode child was reaped");
    assert!(pool.used() > 0);
    pool.want(0, &[]);
    assert!(pool.buffer(0, 1).is_none());
    drop(buffer);
    assert_eq!(pool.used(), 0, "songs that left free their memory");
}

#[tokio::test(start_paused = true)]
async fn failed_preloads_back_off_and_stop_after_three() {
    let starts = Starts::default();
    let pool = Pool::new(1 << 30, starts.starter(), crate::beats::Beats::memory());
    for entry in 1..=MAX_FAILURES as u64 {
        pool.want(0, &[want(entry, 60, Need::Loaded)]);
        settle().await;
        let started = starts.take(entry);
        drop(started.sender);
        started
            .terminal
            .send(Err("decoder crashed".into()))
            .unwrap();
        started.finish.send(Err("decoder crashed".into())).unwrap();
        settle().await;
        assert_eq!(
            pool.buffer(0, entry).unwrap().fill(),
            Fill::Failed("decoder crashed".into())
        );
        tokio::time::sleep(MAX_BACKOFF).await;
    }
    assert!(pool.notice().unwrap().contains("Preloading stopped"));
    pool.want(0, &[want(9, 60, Need::Playing)]);
    settle().await;
    assert!(
        starts.entries().is_empty(),
        "no decoder starts after the stop"
    );
    assert_eq!(pool.decoding(), 0);
}
