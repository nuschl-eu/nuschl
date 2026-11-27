// SPDX-License-Identifier: EUPL-1.2
//! Integration surfaces: the small, declarative UI a bot publishes to the
//! people who can see it. Nothing here is executable, fetched or styled by
//! the sender: clients draw a closed set of nodes with native widgets.
//!
//! # Wire
//!
//! Both messages are versioned, length-bounded byte strings inside plugin
//! data, so they fit a stock Murmur server's 1,000-byte limit:
//!
//! ```text
//! surface: version u8 = 1, id varint, revision varint, nodes u8, nodes…
//!   text:     tag 1, style u8, len u16, UTF-8
//!   progress: tag 2, position_ms varint, duration_ms varint, flags u8
//!   buttons:  tag 3, count u8, { id u8, icon u8, flags u8, len u8, label }
//!   volume:   tag 4, no body (the client draws its own per-user volume)
//! action:  version u8 = 1, surface varint, revision varint, control u8, key varint
//! ```
//!
//! A surface is a full snapshot: the bot owns its state and clients replace
//! the previous revision. Progress is a baseline, not a tick: a client adds
//! the time elapsed since it received the snapshot while `advancing` is set,
//! so a playing song needs no per-second messages. A newer revision always
//! wins; an action names the revision it was drawn from, and the bot refuses
//! actions for a surface it has already replaced.
use std::collections::BTreeSet;

/// Plugin data ID of a surface (the bot's direction). Reserved by Nuschl.
pub const KIT_DATA_ID: &str = "nuschl/kit/1";
/// Plugin data ID of an action (the user's direction). Reserved by Nuschl.
pub const KIT_ACTION_DATA_ID: &str = "nuschl/kit-action/1";
/// Plugin data ID of a page link (the bot's direction, to one session).
/// Reserved by Nuschl. Only a sender whose card the user pressed may send one.
pub const KIT_LINK_DATA_ID: &str = "nuschl/kit-link/1";
/// The longest page link a bot may hand a client to open.
pub const MAX_LINK: usize = 512;

/// The longest encoded surface.
pub const MAX_SURFACE: usize = 960;
/// Nodes one surface may carry.
pub const MAX_NODES: usize = 8;
/// Buttons one buttons node may carry.
pub const MAX_BUTTONS: usize = 4;
/// Buttons all nodes of a surface may carry together.
pub const MAX_SURFACE_BUTTONS: usize = 8;
/// Bytes of text one surface may carry together.
pub const MAX_SURFACE_TEXT: usize = 512;
/// Bytes of one text node.
pub const MAX_TEXT: usize = 160;
/// Bytes of one button label.
pub const MAX_LABEL: usize = 24;
/// The longest encoded action.
pub const MAX_ACTION: usize = 32;

const VERSION: u8 = 1;
const TEXT: u8 = 1;
const PROGRESS: u8 = 2;
const BUTTONS: u8 = 3;
const VOLUME: u8 = 4;
const ADVANCING: u8 = 1;
const ENABLED: u8 = 1;

/// How prominently a client draws a text node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Title,
    Subtitle,
    Body,
}

impl Style {
    fn encode(self) -> u8 {
        match self {
            Self::Title => 0,
            Self::Subtitle => 1,
            Self::Body => 2,
        }
    }

    fn decode(value: u8) -> Result<Self, &'static str> {
        match value {
            0 => Ok(Self::Title),
            1 => Ok(Self::Subtitle),
            2 => Ok(Self::Body),
            _ => Err("text style"),
        }
    }
}

/// The fixed icon set a client may draw on a button. The sender names an
/// icon; it never supplies an image or markup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    None,
    Play,
    Pause,
    Next,
    Previous,
    /// The client's own listen toggle for the sender's channel, never a press
    /// the bot receives: the wire asks for the control, the client owns it.
    Headphones,
    /// A page the bot answers with, because the link carries a token minted for
    /// this user: the press reaches the bot and its reply is a page link.
    Web,
}

impl Icon {
    fn encode(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Play => 1,
            Self::Pause => 2,
            Self::Next => 3,
            Self::Previous => 4,
            Self::Headphones => 5,
            Self::Web => 6,
        }
    }

    fn decode(value: u8) -> Result<Self, &'static str> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Play),
            2 => Ok(Self::Pause),
            3 => Ok(Self::Next),
            4 => Ok(Self::Previous),
            5 => Ok(Self::Headphones),
            6 => Ok(Self::Web),
            _ => Err("button icon"),
        }
    }
}

/// One pressable control. `id` is what an action names back; `label` is the
/// accessible name, and the icon is drawn from the client's own set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Button {
    pub id: u8,
    pub icon: Icon,
    pub label: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Text {
        style: Style,
        text: String,
    },
    /// `position_ms` is where the song was when the bot sent this revision.
    /// While `advancing`, a client adds the time since it received it.
    Progress {
        position_ms: u64,
        duration_ms: u64,
        advancing: bool,
    },
    Buttons(Vec<Button>),
    /// A control the client draws for its own setting: the sender's playback
    /// volume. The wire asks for it and carries no value, and turning it edits
    /// this client only, so a bot never learns or waits for it.
    Volume,
}

/// A bot's full view of one card. `id` is stable for the bot's lifetime and
/// `revision` strictly increases per id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Surface {
    pub id: u32,
    pub revision: u64,
    pub nodes: Vec<Node>,
}

/// A page link a bot may hand a client, checked here so every client applies the
/// same rule: a web page, one line, and at most [`MAX_LINK`] bytes. The scheme
/// allowlist is [`crate::allowed_link`]'s.
pub fn link(url: &str) -> Result<(), &'static str> {
    if url.is_empty() || url.len() > MAX_LINK {
        return Err("link length");
    }
    if url.chars().any(char::is_control) || url.chars().any(char::is_whitespace) {
        return Err("link character");
    }
    let web = url.split_once("://").is_some_and(|(scheme, rest)| {
        !rest.is_empty()
            && (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
    });
    if !web || !crate::allowed_link(url) {
        return Err("link scheme");
    }
    Ok(())
}

/// A press: the surface and revision it was drawn from, the control, and a
/// key the bot may use to treat retries as one action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Action {
    pub surface: u32,
    pub revision: u64,
    pub control: u8,
    pub key: u32,
}

impl Surface {
    pub fn encode(&self) -> Result<Vec<u8>, &'static str> {
        if self.nodes.len() > MAX_NODES {
            return Err("too many nodes");
        }
        let mut out = Vec::new();
        out.push(VERSION);
        varint(&mut out, u64::from(self.id));
        varint(&mut out, self.revision);
        out.push(self.nodes.len() as u8);
        let mut text = 0;
        let mut buttons = 0;
        // An action names the control id alone, so ids are unique per surface.
        let mut ids = BTreeSet::new();
        for node in &self.nodes {
            match node {
                Node::Text { style, text: body } => {
                    check_text(body, MAX_TEXT)?;
                    text += body.len();
                    out.push(TEXT);
                    out.push(style.encode());
                    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
                    out.extend_from_slice(body.as_bytes());
                }
                Node::Progress {
                    position_ms,
                    duration_ms,
                    advancing,
                } => {
                    out.push(PROGRESS);
                    varint(&mut out, *position_ms);
                    varint(&mut out, *duration_ms);
                    out.push(u8::from(*advancing));
                }
                Node::Volume => out.push(VOLUME),
                Node::Buttons(items) => {
                    if items.len() > MAX_BUTTONS {
                        return Err("too many buttons");
                    }
                    buttons += items.len();
                    out.push(BUTTONS);
                    out.push(items.len() as u8);
                    for button in items {
                        if !ids.insert(button.id) {
                            return Err("duplicate button");
                        }
                        check_text(&button.label, MAX_LABEL)?;
                        out.push(button.id);
                        out.push(button.icon.encode());
                        out.push(if button.enabled { ENABLED } else { 0 });
                        out.push(button.label.len() as u8);
                        out.extend_from_slice(button.label.as_bytes());
                    }
                }
            }
        }
        if text > MAX_SURFACE_TEXT {
            return Err("too much text");
        }
        if buttons > MAX_SURFACE_BUTTONS {
            return Err("too many buttons");
        }
        if out.len() > MAX_SURFACE {
            return Err("surface too long");
        }
        Ok(out)
    }

    pub fn decode(data: &[u8]) -> Result<Self, &'static str> {
        if data.len() > MAX_SURFACE {
            return Err("surface too long");
        }
        let mut input = Input::new(data);
        if input.byte()? != VERSION {
            return Err("surface version");
        }
        let id = u32::try_from(input.varint()?).map_err(|_| "surface id")?;
        let revision = input.varint()?;
        let count = input.byte()?;
        if count as usize > MAX_NODES {
            return Err("too many nodes");
        }
        let mut nodes = Vec::with_capacity(count as usize);
        let mut text = 0;
        let mut buttons = 0;
        // An action names the control id alone, so ids are unique per surface.
        let mut ids = BTreeSet::new();
        for _ in 0..count {
            nodes.push(match input.byte()? {
                TEXT => {
                    let style = Style::decode(input.byte()?)?;
                    let len = input.be_u16()? as usize;
                    let body = input.text(len, MAX_TEXT)?;
                    text += body.len();
                    Node::Text { style, text: body }
                }
                PROGRESS => Node::Progress {
                    position_ms: input.varint()?,
                    duration_ms: input.varint()?,
                    advancing: input.flags(ADVANCING)?,
                },
                VOLUME => Node::Volume,
                BUTTONS => {
                    let count = input.byte()?;
                    if count as usize > MAX_BUTTONS {
                        return Err("too many buttons");
                    }
                    buttons += count as usize;
                    let mut items = Vec::with_capacity(count as usize);
                    for _ in 0..count {
                        let id = input.byte()?;
                        if !ids.insert(id) {
                            return Err("duplicate button");
                        }
                        let icon = Icon::decode(input.byte()?)?;
                        let enabled = input.flags(ENABLED)?;
                        let len = input.byte()? as usize;
                        let label = input.text(len, MAX_LABEL)?;
                        items.push(Button {
                            id,
                            icon,
                            label,
                            enabled,
                        });
                    }
                    Node::Buttons(items)
                }
                _ => return Err("node kind"),
            });
        }
        if !input.done() {
            return Err("trailing bytes");
        }
        if text > MAX_SURFACE_TEXT {
            return Err("too much text");
        }
        if buttons > MAX_SURFACE_BUTTONS {
            return Err("too many buttons");
        }
        Ok(Self {
            id,
            revision,
            nodes,
        })
    }

    /// The action for pressing `control` on this revision.
    pub fn action(&self, control: u8, key: u32) -> Action {
        Action {
            surface: self.id,
            revision: self.revision,
            control,
            key,
        }
    }
}

impl Action {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(VERSION);
        varint(&mut out, u64::from(self.surface));
        varint(&mut out, self.revision);
        out.push(self.control);
        varint(&mut out, u64::from(self.key));
        out
    }

    pub fn decode(data: &[u8]) -> Result<Self, &'static str> {
        if data.len() > MAX_ACTION {
            return Err("action too long");
        }
        let mut input = Input::new(data);
        if input.byte()? != VERSION {
            return Err("action version");
        }
        let surface = u32::try_from(input.varint()?).map_err(|_| "action surface")?;
        let revision = input.varint()?;
        let control = input.byte()?;
        let key = u32::try_from(input.varint()?).map_err(|_| "action key")?;
        if !input.done() {
            return Err("trailing bytes");
        }
        Ok(Self {
            surface,
            revision,
            control,
            key,
        })
    }
}

/// Text a person will read: bounded, UTF-8, one line, no control characters.
fn check_text(text: &str, max: usize) -> Result<(), &'static str> {
    if text.is_empty() || text.len() > max {
        return Err("text length");
    }
    if text.chars().any(|c| c.is_control()) {
        return Err("text control character");
    }
    Ok(())
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

struct Input<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Input<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn done(&self) -> bool {
        self.at == self.data.len()
    }

    fn byte(&mut self) -> Result<u8, &'static str> {
        let byte = *self.data.get(self.at).ok_or("truncated")?;
        self.at += 1;
        Ok(byte)
    }

    fn be_u16(&mut self) -> Result<u16, &'static str> {
        let bytes = self.data.get(self.at..self.at + 2).ok_or("truncated")?;
        self.at += 2;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// The shortest encoding of a value and nothing else: a longer one would
    /// decode to the same number, so two senders of the same snapshot could
    /// still disagree byte for byte.
    fn varint(&mut self) -> Result<u64, &'static str> {
        let mut value = 0u64;
        let mut groups = 0;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            groups += 1;
            if shift == 63 && byte & 0x7e != 0 {
                return Err("varint too long");
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                if groups > 1 && byte == 0 {
                    return Err("varint not canonical");
                }
                return Ok(value);
            }
        }
        Err("varint too long")
    }

    fn flags(&mut self, flag: u8) -> Result<bool, &'static str> {
        let flags = self.byte()?;
        if flags & !flag != 0 {
            return Err("unknown flags");
        }
        Ok(flags & flag != 0)
    }

    fn text(&mut self, len: usize, max: usize) -> Result<String, &'static str> {
        if len == 0 || len > max {
            return Err("text length");
        }
        let bytes = self.data.get(self.at..self.at + len).ok_or("truncated")?;
        self.at += len;
        let text = std::str::from_utf8(bytes).map_err(|_| "text utf-8")?;
        check_text(text, max)?;
        Ok(text.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface() -> Surface {
        Surface {
            id: 1,
            revision: 7,
            nodes: vec![
                Node::Text {
                    style: Style::Title,
                    text: "Song title".into(),
                },
                Node::Text {
                    style: Style::Subtitle,
                    text: "The artist".into(),
                },
                Node::Progress {
                    position_ms: 61_000,
                    duration_ms: 245_000,
                    advancing: true,
                },
                Node::Buttons(vec![
                    Button {
                        id: 0,
                        icon: Icon::Headphones,
                        label: "Listen".into(),
                        enabled: true,
                    },
                    Button {
                        id: 1,
                        icon: Icon::Pause,
                        label: "Pause".into(),
                        enabled: true,
                    },
                    Button {
                        id: 2,
                        icon: Icon::Next,
                        label: "Next".into(),
                        enabled: false,
                    },
                    Button {
                        id: 3,
                        icon: Icon::Web,
                        label: "Web".into(),
                        enabled: true,
                    },
                ]),
                Node::Volume,
            ],
        }
    }

    #[test]
    fn surfaces_round_trip_and_fit_plugin_data() {
        let surface = surface();
        let encoded = surface.encode().unwrap();
        assert_eq!(Surface::decode(&encoded), Ok(surface));
        assert!(encoded.len() <= MAX_SURFACE && MAX_SURFACE < 1000);
    }

    #[test]
    fn actions_round_trip_and_fit() {
        let action = surface().action(2, 0xfeed);
        let encoded = action.encode();
        assert_eq!(Action::decode(&encoded), Ok(action));
        assert!(encoded.len() <= MAX_ACTION);
    }

    #[test]
    fn malformed_surfaces_are_refused() {
        let good = surface().encode().unwrap();
        for data in [
            &[][..],
            &[2],
            &[1, 1],
            &[1, 1, 7, 1, 1, 0],
            &[1, 1, 7, 1, 1, 0, 0],
            &[1, 1, 7, 1, 9],
            &[1, 1, 7, 1, 2, 0, 0, 2],
            &[1, 1, 7, 1, 3, 1, 1, 0, 3, b'a', b'b', b'c'],
            &[1, 1, 7, 1, 1, 0, 0, 4],
            // No node kind 5 yet.
            &[1, 1, 7, 1, 5],
        ] {
            assert!(Surface::decode(data).is_err(), "{data:?}");
        }
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(Surface::decode(&trailing).is_err());
        let mut wrong_version = good;
        wrong_version[0] = 2;
        assert!(Surface::decode(&wrong_version).is_err());
    }

    #[test]
    fn text_is_bounded_to_one_line_and_valid_utf8() {
        for text in ["a\nb", "a\rb", "a\tb", "a\0b"] {
            let surface = Surface {
                id: 1,
                revision: 1,
                nodes: vec![Node::Text {
                    style: Style::Body,
                    text: text.into(),
                }],
            };
            assert!(surface.encode().is_err(), "{text:?}");
        }
        let long = "a".repeat(MAX_TEXT + 1);
        let surface = Surface {
            id: 1,
            revision: 1,
            nodes: vec![Node::Text {
                style: Style::Body,
                text: long,
            }],
        };
        assert!(surface.encode().is_err());
    }

    #[test]
    fn button_ids_are_unique_and_counts_bounded() {
        let button = Button {
            id: 1,
            icon: Icon::None,
            label: "One".into(),
            enabled: true,
        };
        let duplicate = Surface {
            id: 1,
            revision: 1,
            nodes: vec![
                Node::Buttons(vec![button.clone()]),
                Node::Buttons(vec![button.clone()]),
            ],
        };
        // An action names the control id alone, so a surface's ids are unique.
        assert!(duplicate.encode().is_err());
        assert!(Surface::decode(&duplicate.encode().unwrap_or_default()).is_err());
        let same_node = Surface {
            id: 1,
            revision: 1,
            nodes: vec![Node::Buttons(vec![button.clone(), button.clone()])],
        };
        assert!(same_node.encode().is_err());
        let too_many = Surface {
            id: 1,
            revision: 1,
            nodes: vec![Node::Buttons(vec![button; MAX_BUTTONS + 1])],
        };
        assert!(too_many.encode().is_err());
    }

    #[test]
    fn links_are_single_line_web_pages() {
        assert_eq!(link("https://music.example.org/u/3f2a/"), Ok(()));
        assert_eq!(link("http://127.0.0.1:8080/u/3f2a/"), Ok(()));
        assert_eq!(link(""), Err("link length"));
        assert_eq!(link("javascript:alert(1)"), Err("link scheme"));
        assert_eq!(link("mumble://example.org/Lobby"), Err("link scheme"));
        assert_eq!(link("https://example.org/"), Ok(()));
        assert_eq!(link("https://example.org/two words"), Err("link character"));
        assert_eq!(link("https://example.org/\nNext"), Err("link character"));
        let long = format!("https://example.org/{}", "a".repeat(MAX_LINK));
        assert_eq!(link(&long), Err("link length"));
    }

    #[test]
    fn varints_are_the_shortest_encoding_only() {
        let mut extra = surface();
        extra.nodes.insert(
            1,
            Node::Progress {
                position_ms: 0x80,
                duration_ms: 1,
                advancing: false,
            },
        );
        let encoded = extra.encode().unwrap();
        // The position's varint is at a known offset only through the tags, so
        // find the two-byte form the encoder wrote and lengthen it by hand.
        let canonical = [0x80, 0x01];
        let at = encoded
            .windows(2)
            .position(|pair| pair == canonical)
            .expect("the position's varint");
        let mut long = encoded[..at].to_vec();
        long.extend_from_slice(&[0x80, 0x80, 0x00]);
        long.extend_from_slice(&encoded[at + 2..]);
        assert_eq!(Surface::decode(&long), Err("varint not canonical"));
        assert_eq!(Surface::decode(&encoded), Ok(extra));
    }
}
