// SPDX-License-Identifier: EUPL-1.2
//! What arrived, and what this machine made of it (issue #17).
//!
//! The connection counts the voice packets the server sent, up to this
//! machine's network path. The jitter buffer, the decoder and the output
//! device each count what they did with the packets that arrived, so every
//! number is read for the stage it covers.
use crate::{layout, state::ServerView, theme::*};
use gpui_kit::{prelude::*, *};
use nuschl_client::server::{AudioHealth, Status};
use nuschl_core::{audio::AudioPlayout, model::TransportStats};

/// One line: what was counted, and what it measured.
pub type Line = (&'static str, String);

/// The connection's own voice counters, as the server reports them. The
/// Connection panel shows the same lines.
pub fn transport_rows(transport: &TransportStats) -> Vec<Line> {
    let path = if transport.udp_active {
        "UDP"
    } else if transport.forced_tcp {
        "TCP (forced)"
    } else {
        "TCP tunnel (UDP unavailable)"
    };
    let ping = |stats: nuschl_core::model::PingStats| {
        format!(
            "{:.1} ms ± {:.1} ({} pings)",
            stats.average_ms,
            stats.variance_ms.sqrt(),
            stats.packets
        )
    };
    let total = (f64::from(transport.good) + f64::from(transport.lost)).max(1.);
    let packets = format!(
        "{} good · {} late · {} lost ({:.1}%) · {} resyncs",
        transport.good,
        transport.late,
        transport.lost,
        f64::from(transport.lost) * 100. / total,
        transport.resync
    );
    vec![
        ("Voice transport", path.to_string()),
        ("UDP ping", ping(transport.udp)),
        ("TCP ping", ping(transport.tcp)),
        ("UDP from server", packets),
    ]
}

/// This device's playout counters, as the media worker measured them.
fn playout_rows(playout: &AudioPlayout, health: &AudioHealth) -> Vec<Line> {
    let plural = |count: u32, one: &str, many: &str| {
        format!("{count} {}", if count == 1 { one } else { many })
    };
    let packets = format!(
        "{} queued · {} late · {} duplicate · {} caught up",
        playout.received, playout.late, playout.duplicate, playout.caught_up
    );
    let gaps = format!(
        "{} FEC frames · {} concealed · {}",
        playout.recovered_frames,
        playout.concealed_frames,
        plural(playout.dry_talkspurts, "dry talkspurt", "dry talkspurts")
    );
    // The jitter buffer and the output ring are separate delays; both are
    // named so neither number is read as the other.
    let buffer = format!(
        "{} of {} ms · {}",
        playout.buffered_ms,
        playout.target_ms,
        plural(playout.speakers, "speaker", "speakers")
    );
    let device = format!(
        "{} ms buffer · {} · {}",
        health.playback_ms,
        plural(health.underruns, "dropout", "dropouts"),
        plural(playout.decoder_errors, "decoder error", "decoder errors")
    );
    vec![
        ("Voice packets", packets),
        ("Gaps", gaps),
        ("Jitter buffer", buffer),
        ("Output device", device),
    ]
}

fn group(label: String, rows: Vec<Line>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color(TEXT_MUTED))
                .child(text!(label)),
        )
        .children(rows.into_iter().map(|(name, value)| {
            layout::row()
                .text_xs()
                .child(
                    layout::fixed()
                        .w(px(150.))
                        .text_color(color(TEXT_MUTED))
                        .child(text!(name)),
                )
                .child(
                    layout::flexible()
                        .text_color(color(TEXT))
                        .child(text!(value)),
                )
        }))
}

/// The diagnostics section of the audio settings, for the selected server.
pub fn audio(server: Option<&ServerView>) -> Option<AnyElement> {
    let server = server.filter(|server| server.status == Status::Connected)?;
    let transport = server.transport.as_ref()?;
    Some(
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(group(
                "Connection · since it started".into(),
                transport_rows(transport),
            ))
            // One worker mixes every server's voice into one output, so these
            // counters describe this machine.
            .child(group(
                "This device · all servers, last 2 s".into(),
                playout_rows(&server.audio.playout, &server.audio.health),
            ))
            .child(
                div().text_xs().text_color(color(TEXT_FAINT)).child(
                    text!(
                        "The connection counts the packets the server sent. Each buffer, \
                             decoder and output device below counts what it did with them."
                    )
                    .with_id("audio-diagnostics-note"),
                ),
            )
            .into_any_element(),
    )
}

/// `gpui_kit::*` exports its own `#[test]`, so this module imports what it
/// asserts on instead of glob-importing `super`.
#[cfg(test)]
mod tests {
    use super::{playout_rows, transport_rows};
    use nuschl_client::server::AudioHealth;
    use nuschl_core::{
        audio::AudioPlayout,
        model::{PingStats, TransportStats},
    };

    fn link(udp_active: bool) -> TransportStats {
        TransportStats {
            udp_active,
            forced_tcp: !udp_active,
            udp: PingStats {
                packets: 12,
                average_ms: 23.4,
                variance_ms: 16.,
            },
            tcp: PingStats {
                packets: 12,
                average_ms: 41.,
                variance_ms: 4.,
            },
            ..TransportStats::default()
        }
    }

    #[test]
    fn the_connection_reports_its_own_packet_counters() {
        let mut stats = link(true);
        stats.good = 1200;
        stats.late = 3;
        stats.lost = 4;
        let rows = transport_rows(&stats);
        assert_eq!(rows[0], ("Voice transport", "UDP".into()));
        assert_eq!(rows[1].1, "23.4 ms ± 4.0 (12 pings)");
        assert_eq!(rows[2].1, "41.0 ms ± 2.0 (12 pings)");
        assert_eq!(rows[3].1, "1200 good · 3 late · 4 lost (0.3%) · 0 resyncs");
    }

    #[test]
    fn a_tcp_path_names_the_tunnel() {
        let rows = transport_rows(&link(false));
        assert_eq!(rows[0].1, "TCP (forced)");
        assert!(
            transport_rows(&TransportStats::default())[0]
                .1
                .contains("tunnel")
        );
    }

    #[test]
    fn the_device_reports_playout_apart_from_arrival() {
        let playout = AudioPlayout {
            received: 1206,
            late: 3,
            duplicate: 1,
            caught_up: 1,
            concealed_frames: 5,
            recovered_frames: 3,
            dry_talkspurts: 1,
            buffered_ms: 60,
            target_ms: 60,
            speakers: 2,
            ..AudioPlayout::default()
        };
        let health = AudioHealth {
            underruns: 4,
            playback_ms: 40,
            ..AudioHealth::default()
        };
        let rows = playout_rows(&playout, &health);
        assert_eq!(
            rows[0].1,
            "1206 queued · 3 late · 1 duplicate · 1 caught up"
        );
        assert_eq!(rows[1].1, "3 FEC frames · 5 concealed · 1 dry talkspurt");
        assert_eq!(rows[2].1, "60 of 60 ms · 2 speakers");
        assert_eq!(rows[3].1, "40 ms buffer · 4 dropouts · 0 decoder errors");
    }

    #[test]
    fn one_speaker_and_one_dropout_are_not_plural() {
        let playout = AudioPlayout {
            speakers: 1,
            buffered_ms: 40,
            ..AudioPlayout::default()
        };
        let health = AudioHealth {
            underruns: 1,
            ..AudioHealth::default()
        };
        let rows = playout_rows(&playout, &health);
        assert_eq!(rows[1].1, "0 FEC frames · 0 concealed · 0 dry talkspurts");
        assert_eq!(rows[2].1, "40 of 0 ms · 1 speaker");
        assert_eq!(rows[3].1, "0 ms buffer · 1 dropout · 0 decoder errors");
    }
}
