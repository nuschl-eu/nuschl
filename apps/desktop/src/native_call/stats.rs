// SPDX-License-Identifier: EUPL-1.2

use nuschl_client::native::sfu::{Reason, Route};
use nuschl_ipc::video::CaptureLimits;
use nuschl_protocol::sfu::VideoLimit;
use std::{
    mem,
    time::{Duration, Instant},
};

pub(super) const INTERVAL: Duration = Duration::from_secs(1);
const MAX_SAMPLE_SPAN: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Path {
    route: Route,
    relay_only: bool,
}

impl Path {
    fn label(self) -> String {
        let reason = match self.route {
            Route::Direct => return "Direct".into(),
            Route::Sfu(Reason::RelayOnly) => "relay-only",
            Route::Sfu(Reason::TwoWay) => "video both ways",
            Route::Sfu(Reason::Viewers) => "more than two viewers",
            Route::Sfu(Reason::Senders) => "several senders",
            Route::Sfu(Reason::Viewer) => "waiting for a native viewer",
            Route::Sfu(Reason::Sender) => "set by the sender",
            Route::Sfu(Reason::Measuring) => "measuring upload",
            Route::Sfu(Reason::Upload) => "upload too low for direct",
            Route::Sfu(Reason::Connecting) => "connecting directly",
            Route::Sfu(Reason::Failed) => "direct connection failed",
        };
        let path = if self.relay_only {
            "TURN relay"
        } else {
            "Call server (SFU)"
        };
        format!("{path} · {reason}")
    }
}

#[derive(Default)]
struct Interval {
    bytes: u64,
    frames: u32,
    decode_time: Duration,
    decoded: u32,
}

/// Per-source video counters. The call task snapshots them at its stats tick.
pub(super) struct Stats {
    since: Instant,
    resolution: Option<(u16, u16)>,
    window: Interval,
    path: Path,
    requested: Option<CaptureLimits>,
    sfu_limit: Option<VideoLimit>,
}

impl Stats {
    pub(super) fn new(relay_only: bool, now: Instant) -> Self {
        Self {
            since: now,
            resolution: None,
            window: Interval::default(),
            requested: None,
            sfu_limit: None,
            path: Path {
                route: Route::Sfu(if relay_only {
                    Reason::RelayOnly
                } else {
                    Reason::Connecting
                }),
                relay_only,
            },
        }
    }

    pub(super) fn encoded(&mut self, bytes: usize) {
        self.window.bytes = self.window.bytes.saturating_add(bytes as u64);
    }

    /// For received video, pass the time spent waiting for the confined decoder.
    pub(super) fn frame(&mut self, width: u16, height: u16, decode_time: Option<Duration>) {
        self.resolution = Some((width, height));
        self.window.frames = self.window.frames.saturating_add(1);
        if let Some(time) = decode_time {
            self.window.decode_time = self.window.decode_time.saturating_add(time);
            self.window.decoded = self.window.decoded.saturating_add(1);
        }
    }

    pub(super) fn route(&mut self, route: Route) {
        self.path.route = route;
    }

    pub(super) fn requested(&mut self, requested: CaptureLimits) {
        self.requested = Some(requested);
    }

    pub(super) fn limit(&mut self, limit: Option<VideoLimit>) {
        self.sfu_limit = limit;
    }

    pub(super) fn clear(&mut self, now: Instant) {
        self.since = now;
        self.resolution = None;
        self.window = Interval::default();
        self.sfu_limit = None;
    }

    pub(super) fn sample(&mut self, now: Instant) -> Snapshot {
        let elapsed = now.saturating_duration_since(mem::replace(&mut self.since, now));
        let window = mem::take(&mut self.window);
        let valid_span = !elapsed.is_zero() && elapsed <= MAX_SAMPLE_SPAN;
        Snapshot {
            resolution: self.resolution,
            fps: (valid_span && self.resolution.is_some())
                .then(|| f64::from(window.frames) / elapsed.as_secs_f64()),
            bitrate: (valid_span && (self.resolution.is_some() || window.bytes > 0))
                .then(|| 8. * window.bytes as f64 / elapsed.as_secs_f64()),
            decode_ms: (valid_span && window.decoded > 0)
                .then(|| 1000. * window.decode_time.as_secs_f64() / f64::from(window.decoded)),
            path: Some(self.path),
            requested: self.requested,
            sfu_limit: self.sfu_limit,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Snapshot {
    resolution: Option<(u16, u16)>,
    fps: Option<f64>,
    bitrate: Option<f64>,
    decode_ms: Option<f64>,
    path: Option<Path>,
    requested: Option<CaptureLimits>,
    sfu_limit: Option<VideoLimit>,
}

impl Snapshot {
    pub(super) fn height(self) -> Option<u16> {
        self.resolution.map(|(width, height)| width.min(height))
    }

    fn receiver_limit(self) -> &'static str {
        match self.path.map(|path| path.route) {
            Some(Route::Direct) => "Direct connection · unavailable",
            _ if self.sfu_limit == Some(VideoLimit::WithinTarget) => "SFU within selected quality",
            _ => self.sfu_limit.map_or("Unavailable", limit_label),
        }
    }

    fn rows(self, owned: bool) -> impl Iterator<Item = (&'static str, String)> {
        let resolution = self
            .resolution
            .map_or_else(unavailable, |(width, height)| format!("{width} × {height}"));
        let frame_label = if owned { "Encoded FPS" } else { "Decoded FPS" };
        [
            ("Resolution", resolution),
            (frame_label, number(self.fps, "fps")),
            ("Video payload bitrate", bitrate(self.bitrate)),
            (
                "Connection",
                self.path.map_or_else(unavailable, Path::label),
            ),
        ]
        .into_iter()
        .chain((!owned).then(|| ("Local decode delay", number(self.decode_ms, "ms"))))
    }
}

#[derive(Clone, Copy)]
pub(super) struct Encoder {
    pub requested: CaptureLimits,
    pub limit: &'static str,
    pub budget_bps: Option<u32>,
    pub target: CaptureLimits,
    pub ceiling: Option<CaptureLimits>,
    pub hardware: Option<bool>,
}

pub(super) fn rows(
    sample: Option<Snapshot>,
    encoder: Option<Encoder>,
    owned: bool,
) -> impl Iterator<Item = (&'static str, String)> {
    let encoder_rows = owned.then(|| {
        [
            (
                "Estimated video budget",
                bitrate(encoder.and_then(|enc| enc.budget_bps).map(f64::from)),
            ),
            (
                "Encoder target",
                encoder.map_or_else(unavailable, |enc| limits(enc.target)),
            ),
            (
                "Server ceiling",
                encoder
                    .and_then(|enc| enc.ceiling)
                    .map_or_else(unavailable, limits),
            ),
            (
                "Video encoder",
                encoder
                    .and_then(|enc| enc.hardware)
                    .map_or_else(unavailable, |hardware| {
                        if hardware {
                            "Hardware AV1".into()
                        } else {
                            "Software AV1".into()
                        }
                    }),
            ),
        ]
    });
    let sample = sample.unwrap_or_default();
    let requested = encoder
        .map(|encoder| encoder.requested)
        .or(sample.requested);
    let limit = encoder.map_or_else(|| sample.receiver_limit(), |encoder| encoder.limit);
    sample
        .rows(owned)
        .chain([
            (
                "Requested quality",
                requested.map_or_else(unavailable, geometry),
            ),
            ("Active limit", limit.into()),
        ])
        .chain(encoder_rows.into_iter().flatten())
}

pub(super) fn limit_label(limit: VideoLimit) -> &'static str {
    match limit {
        VideoLimit::WithinTarget => "Within selected quality",
        VideoLimit::ReceiverQuality => "Viewer quality selection",
        VideoLimit::ReceiverDecode => "Viewer decoding capacity",
        VideoLimit::ReceiverBandwidth => "Viewer connection",
        VideoLimit::SfuHost => "SFU host upload cap",
        VideoLimit::SfuRoom => "SFU room cap",
        VideoLimit::SfuViewer => "SFU per-viewer cap",
        VideoLimit::TrafficBudget => "SFU traffic budget pacing",
        VideoLimit::ForwardingBacklog => "SFU send queue",
        VideoLimit::QualityRecovery => "Restoring quality",
    }
}

fn geometry(limits: CaptureLimits) -> String {
    format!("{}p{}", limits.height, limits.fps)
}

fn limits(limits: CaptureLimits) -> String {
    format!(
        "{} · {}",
        geometry(limits),
        bitrate(Some(f64::from(limits.bitrate)))
    )
}

fn bitrate(rate: Option<f64>) -> String {
    rate.map_or_else(unavailable, |rate| {
        if rate >= 1_000_000. {
            format!("{:.2} Mb/s", rate / 1_000_000.)
        } else {
            format!("{:.1} kb/s", rate / 1000.)
        }
    })
}

fn unavailable() -> String {
    "—".into()
}

fn number(value: Option<f64>, unit: &str) -> String {
    value.map_or_else(unavailable, |value| format!("{value:.1} {unit}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publisher_counts_video_over_the_elapsed_interval() {
        let start = Instant::now();
        let mut stats = Stats::new(false, start);
        for _ in 0..60 {
            stats.frame(1920, 1080, None);
        }
        stats.encoded(375_000);
        let at = start + Duration::from_secs(2);
        let rows = stats.sample(at).rows(true).collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                ("Resolution", "1920 × 1080".into()),
                ("Encoded FPS", "30.0 fps".into()),
                ("Video payload bitrate", "1.50 Mb/s".into()),
                (
                    "Connection",
                    "Call server (SFU) · connecting directly".into()
                ),
            ]
        );
        let quiet = stats.sample(at + INTERVAL);
        assert_eq!(quiet.fps, Some(0.));
        assert_eq!(quiet.bitrate, Some(0.));
        stats.limit(Some(VideoLimit::WithinTarget));
        assert_eq!(
            stats.sample(at + INTERVAL * 2).receiver_limit(),
            "SFU within selected quality"
        );
    }

    #[test]
    fn receiver_records_decode_delay_and_clears_a_departed_source() {
        let start = Instant::now();
        let mut stats = Stats::new(false, start);
        stats.encoded(32_000);
        stats.frame(1920, 1080, Some(Duration::from_millis(10)));
        stats.frame(1280, 720, Some(Duration::from_millis(20)));
        let at = start + INTERVAL;
        let rows = stats.sample(at).rows(false).collect::<Vec<_>>();
        assert_eq!(
            rows,
            [
                ("Resolution", "1280 × 720".into()),
                ("Decoded FPS", "2.0 fps".into()),
                ("Video payload bitrate", "256.0 kb/s".into()),
                (
                    "Connection",
                    "Call server (SFU) · connecting directly".into()
                ),
                ("Local decode delay", "15.0 ms".into()),
            ]
        );
        stats.clear(at);
        let cleared = stats.sample(at + INTERVAL);
        assert_eq!(cleared.resolution, None);
        assert_eq!(cleared.fps, None);
        assert_eq!(cleared.bitrate, None);
        assert_eq!(cleared.decode_ms, None);
    }

    #[test]
    fn long_gaps_clear_rates_and_route_changes_follow_the_call_policy() {
        let start = Instant::now();
        let mut stats = Stats::new(false, start);
        stats.frame(640, 480, None);
        stats.encoded(16_000);
        stats.route(Route::Direct);
        let delayed = stats.sample(start + Duration::from_secs(6));
        assert_eq!(delayed.path.map(Path::label).as_deref(), Some("Direct"));
        assert_eq!(delayed.fps, None);
        assert_eq!(delayed.bitrate, None);
        stats.route(Route::Sfu(Reason::Upload));
        assert_eq!(
            stats
                .sample(start + Duration::from_secs(7))
                .path
                .map(Path::label)
                .as_deref(),
            Some("Call server (SFU) · upload too low for direct")
        );

        let forced = Stats::new(true, start).sample(start + INTERVAL);
        assert_eq!(
            forced.path.map(Path::label).as_deref(),
            Some("TURN relay · relay-only")
        );
    }

    #[test]
    fn missing_measurements_preserve_the_metric_positions() {
        let limits = CaptureLimits {
            height: 1440,
            fps: 60,
            bitrate: 8_000_000,
        };
        let measured = Encoder {
            requested: limits,
            limit: "Sender upload",
            budget_bps: Some(122_000),
            target: limits,
            ceiling: Some(limits),
            hardware: Some(true),
        };
        let empty = rows(None, None, true).collect::<Vec<_>>();
        let populated = rows(
            Some(Stats::new(false, Instant::now()).sample(Instant::now())),
            Some(measured),
            true,
        )
        .collect::<Vec<_>>();
        assert_eq!(empty[5], ("Active limit", "Unavailable".into()));
        assert!(
            empty
                .iter()
                .all(|(name, value)| *name == "Active limit" || value == "—")
        );
        assert!(
            empty
                .iter()
                .map(|row| row.0)
                .eq(populated.iter().map(|row| row.0))
        );
        assert_eq!(populated[5], ("Active limit", "Sender upload".into()));
        assert_eq!(
            populated[6],
            ("Estimated video budget", "122.0 kb/s".into())
        );
        assert_eq!(
            populated[7],
            ("Encoder target", "1440p60 · 8.00 Mb/s".into())
        );
    }
}
