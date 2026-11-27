// SPDX-License-Identifier: EUPL-1.2
//! The music bot's integration card: a `nuschl/kit/1` surface with the song,
//! a progress baseline, Play/Pause and Next. The bot owns the state; the
//! client interpolates the position between revisions, so a playing song
//! sends nothing per second. A press names the revision it was drawn from,
//! and a press for a replaced revision is ignored.
use crate::{
    mumble::View,
    player::{Player, State},
};
use nuschl_core::integration::{
    Button, Icon, KIT_DATA_ID, MAX_LABEL, MAX_TEXT, Node, Style, Surface,
};
use nuschl_protocol::plugin::Payload;
use nuschl_session::connection::{Command, MAX_ACTION_TARGETS};
use std::collections::HashMap;
use tokio::sync::mpsc;

/// The surface id this bot publishes.
pub const SURFACE: u32 = 1;
/// Play or pause the room.
pub const PLAY_PAUSE: u8 = 1;
/// Skip to the next queue entry.
pub const NEXT: u8 = 2;
/// Ask for the user's own page link; the bot answers with one.
pub const WEB: u8 = 3;

/// What a press asked of the bot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pressed {
    /// The player changed, and the next revision follows.
    Applied,
    /// The user asked for their page; the caller sends them the link.
    Web,
}

/// What the bot has sent to each listener, so a joiner gets the current card
/// without a per-second message.
#[derive(Default)]
pub struct Cards {
    revision: u64,
    /// The logical state of the last card: queue entry, paused, buffering and
    /// listener count. The position is deliberately absent; it is a baseline
    /// a client interpolates.
    signature: Option<(u64, bool, bool, usize)>,
    sent: HashMap<u32, (u64, u64)>,
    request: u64,
    /// The client's key of the last press applied, so a repeat of it is one
    /// action rather than two.
    applied_key: Option<u32>,
}

impl Cards {
    /// Sends a new revision to the listeners when the song, pause state,
    /// buffering state or listener count changes, and to a listener who just
    /// arrived. A playing position alone never sends anything.
    pub fn sync(&mut self, player: &Player, view: &View, commands: &mpsc::Sender<Command>) {
        let listeners = view.listener_sessions();
        let state = player.state();
        let signature = state.now_playing().map(|playing| {
            (
                playing.entry.id,
                state.paused,
                playing.buffering,
                listeners.len(),
            )
        });
        if signature == self.signature {
            let behind = listeners.iter().any(|(session, instance)| {
                self.sent.get(session) != Some(&(*instance, self.revision))
            });
            if !behind {
                return;
            }
        }
        drop(state);
        self.revision = self.revision.saturating_add(1);
        let surface = match signature {
            Some(_) => build(&player.state(), self.revision, listeners.len()),
            // Nothing plays: an empty surface withdraws the card.
            None => Surface {
                id: SURFACE,
                revision: self.revision,
                nodes: Vec::new(),
            },
        };
        if send(commands, &listeners, &mut self.request, &surface).is_none() {
            // Refused locally (rate or queue): the next event retries.
            return;
        }
        self.signature = signature;
        self.sent.clear();
        for (session, instance) in listeners {
            self.sent.insert(session, (instance, self.revision));
        }
    }

    /// The request id of a send that is not a card revision (a page link).
    pub fn link_request(&mut self) -> u64 {
        self.request = self.request.wrapping_add(1);
        self.request
    }

    /// Records a press's key; false when it is the same press again, which the
    /// client repeats only on a retry.
    fn fresh(&mut self, key: u32) -> bool {
        if self.applied_key == Some(key) {
            return false;
        }
        self.applied_key = Some(key);
        true
    }

    /// The session refused a send (rate or queue): the next sync retries.
    pub fn retry(&mut self) {
        self.sent.clear();
    }

    /// A press for this bot's card. `None` when it is not ours, stale, or the
    /// same press arrived twice.
    pub fn press(
        &mut self,
        player: &Player,
        action: nuschl_core::integration::Action,
    ) -> Option<Pressed> {
        if action.surface != SURFACE || action.revision != self.revision {
            return None;
        }
        if !self.fresh(action.key) {
            return None;
        }
        match action.control {
            PLAY_PAUSE => {
                let paused = player.state().paused;
                player.set_paused(!paused);
                Some(Pressed::Applied)
            }
            NEXT => {
                player.skip();
                Some(Pressed::Applied)
            }
            WEB => Some(Pressed::Web),
            _ => None,
        }
    }
}

/// The card's nodes, or an empty surface when nothing plays.
fn build(state: &State, revision: u64, listeners: usize) -> Surface {
    let mut nodes = Vec::new();
    let Some(playing) = state.now_playing() else {
        return Surface {
            id: SURFACE,
            revision,
            nodes,
        };
    };
    let track = &playing.entry.track;
    nodes.push(Node::Text {
        style: Style::Title,
        text: text(&track.title, "Untitled", MAX_TEXT),
    });
    let subtitle = track
        .artist
        .as_deref()
        .or(track.album.as_deref())
        .unwrap_or("");
    nodes.push(Node::Text {
        style: Style::Subtitle,
        text: text(subtitle, "Unknown artist", MAX_TEXT),
    });
    let status = if state.paused {
        "Paused"
    } else if playing.buffering {
        "Buffering…"
    } else {
        "Playing"
    };
    let status = match listeners {
        0 => status.to_owned(),
        1 => format!("{status} · 1 listening"),
        count => format!("{status} · {count} listening"),
    };
    nodes.push(Node::Text {
        style: Style::Body,
        text: text(&status, "Playing", MAX_TEXT),
    });
    nodes.push(Node::Progress {
        position_ms: playing.elapsed().as_millis() as u64,
        duration_ms: u64::from(track.duration.unwrap_or(0)) * 1000,
        // The room hears nothing without a listener, so the position holds.
        advancing: !state.paused && !playing.buffering && listeners > 0,
    });
    nodes.push(Node::Buttons(vec![
        // First, so every surface starts with how to hear the bot at all. The
        // client owns this control; the id never reaches the bot.
        Button {
            id: 0,
            icon: Icon::Headphones,
            label: text("Listen", "Listen", MAX_LABEL),
            enabled: true,
        },
        Button {
            id: PLAY_PAUSE,
            icon: if state.paused {
                Icon::Play
            } else {
                Icon::Pause
            },
            label: if state.paused { "Play" } else { "Pause" }.into(),
            enabled: true,
        },
        Button {
            id: NEXT,
            icon: Icon::Next,
            label: text("Next", "Next", MAX_LABEL),
            enabled: true,
        },
        // Drawn at the right of the same row: the page carries a token minted
        // for this user, so the bot answers the press with a link.
        Button {
            id: WEB,
            icon: Icon::Web,
            label: text("Web", "Web", MAX_LABEL),
            enabled: true,
        },
    ]));
    // The client draws its own volume for this bot; no value crosses the wire.
    nodes.push(Node::Volume);
    Surface {
        id: SURFACE,
        revision,
        nodes,
    }
}

/// Sends the surface to every listener, one message per
/// [`MAX_ACTION_TARGETS`] receivers: the session refuses a send that names more.
/// `None` when the first message the queue refused, so the caller retries.
fn send(
    commands: &mpsc::Sender<Command>,
    receivers: &[(u32, u64)],
    request: &mut u64,
    surface: &Surface,
) -> Option<u64> {
    let payload = Payload::new(KIT_DATA_ID.to_owned(), surface.encode().ok()?).ok()?;
    let mut last = None;
    for chunk in receivers.chunks(MAX_ACTION_TARGETS) {
        *request = request.wrapping_add(1);
        commands
            .try_send(Command::PluginData {
                request: *request,
                receivers: chunk.to_vec(),
                payload: payload.clone(),
            })
            .ok()?;
        last = Some(*request);
    }
    last
}

/// A line the kit accepts: one line without control characters, `fallback`
/// when the source held nothing else, and never cut through a UTF-8 sequence.
/// Metadata arrives as the library sends it, and an empty title would make the
/// whole surface invalid, which would leave the previous card up for the song.
fn text(source: &str, fallback: &str, max: usize) -> String {
    let clean: String = source.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    if clean.is_empty() {
        return fallback.to_owned();
    }
    let mut end = clean.len().min(max);
    while end > 0 && !clean.is_char_boundary(end) {
        end -= 1;
    }
    let cut = clean[..end].trim();
    if cut.is_empty() {
        fallback.to_owned()
    } else {
        cut.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_clips_on_character_boundaries() {
        assert_eq!(text("short", "Untitled", 10), "short");
        assert_eq!(text("äöü", "Untitled", 3), "ä");
        assert_eq!(text("   ", "Untitled", 10), "Untitled");
        assert_eq!(text("äöü", "Untitled", 2), "ä");
        assert_eq!(text("äöü", "Untitled", 1), "Untitled");
    }

    #[test]
    fn text_drops_control_characters_and_keeps_a_line() {
        assert_eq!(text("Tab\there", "Untitled", 20), "Tabhere");
        assert_eq!(text("Line\nBreak", "Untitled", 20), "LineBreak");
        assert_eq!(text("", "Unknown artist", 20), "Unknown artist");
    }

    #[test]
    fn a_web_press_asks_for_the_page() {
        let player = Player::new(
            "test".into(),
            50,
            crate::player::Ducking::default(),
            crate::player::Changes::default(),
        );
        let mut cards = Cards {
            revision: 4,
            ..Cards::default()
        };
        let press = |control, key| nuschl_core::integration::Action {
            surface: SURFACE,
            revision: 4,
            control,
            key,
        };
        assert_eq!(cards.press(&player, press(WEB, 9)), Some(Pressed::Web));
        // The same press again is one action; a stale revision is none at all.
        assert_eq!(cards.press(&player, press(WEB, 9)), None);
        assert_eq!(cards.press(&player, press(WEB, 10)), Some(Pressed::Web));
        let mut stale = press(NEXT, 11);
        stale.revision = 3;
        assert_eq!(cards.press(&player, stale), None);
    }

    #[test]
    fn a_repeated_press_is_one_action() {
        let mut cards = Cards::default();
        assert!(cards.fresh(7));
        assert!(!cards.fresh(7), "the same press again");
        assert!(cards.fresh(8), "the next press");
    }

    #[test]
    fn the_card_reaches_past_one_message_of_receivers() {
        let (commands, mut queue) = mpsc::channel(8);
        let surface = Surface {
            id: SURFACE,
            revision: 1,
            nodes: Vec::new(),
        };
        let receivers: Vec<(u32, u64)> = (0..(MAX_ACTION_TARGETS as u32 + 5))
            .map(|session| (session, 1))
            .collect();
        let mut request = 0;
        assert_eq!(send(&commands, &receivers, &mut request, &surface), Some(2));
        let first = queue.try_recv().expect("the first message");
        let second = queue.try_recv().expect("the second message");
        for command in [first, second] {
            let Command::PluginData { receivers, .. } = command else {
                panic!("a plugin-data send");
            };
            assert!(receivers.len() <= MAX_ACTION_TARGETS);
        }
    }
}
