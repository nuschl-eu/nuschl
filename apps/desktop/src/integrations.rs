// SPDX-License-Identifier: EUPL-1.2
//! Integration surfaces drawn above the voice status: a bot publishes a
//! small, closed set of nodes and this file draws them with native widgets.
//! Nothing is fetched or executed; icons come from the client's own set.
//!
//! The progress bar is interpolated from the snapshot's position and the
//! time since it arrived, so a playing song needs no per-second message. A
//! one-second timer repaints while any surface advances.
//!
//! Two cards at most sit between the channel list and the voice status, so the
//! list gives up the height they take. Hiding one is the user's choice and
//! lasts for the session: the compact row it leaves brings the card back, and a
//! later revision of a hidden surface stays hidden.
use std::time::Duration;

use gpui_kit::{prelude::*, *};
use nuschl_client::client::ReceivedSurface;
use nuschl_core::integration::{Button, Icon, Node, Style};
use nuschl_session::{connection::Command, model::permission as perm};

use crate::{layout, state::Desktop, theme::*};

impl Desktop {
    /// Starts the repaint timer for a playing card, if one is not running.
    pub fn animate_surfaces(&mut self, cx: &mut Context<Self>) {
        if self.surface_timer || !advancing(self) {
            return;
        }
        self.surface_timer = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let more = this.update(cx, |this, cx| {
                    this.client.set_time(crate::state::monotonic_ms());
                    let more = advancing(this);
                    this.surface_timer = more;
                    cx.notify();
                    more
                });
                if !matches!(more, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }
}

fn advancing(this: &Desktop) -> bool {
    this.client.servers.iter().enumerate().any(|(index, _)| {
        this.client.surfaces(index).iter().any(|entry| {
            !this.dismissed_surfaces.contains(&key(this, index, entry))
                && entry.surface.nodes.iter().any(|node| {
                    matches!(
                        node,
                        Node::Progress {
                            advancing: true,
                            duration_ms,
                            ..
                        } if *duration_ms > 0
                    )
                })
        })
    })
}

/// Cards drawn at once. A bot publishes one; a second bot's card stacks above
/// the voice status, and the rest wait until one is hidden.
pub const MAX_CARDS: usize = 2;

/// What the user's hidden set names: the server, the sender's account when the
/// server gave one (the presence otherwise) and the surface id. A presence is
/// its instance, which a reconnecting session keeps and a new one does not.
pub(crate) type SurfaceKey = (String, Option<u32>, u64, u32);

/// The hidden-set key of `entry` on the server called `server`.
pub(crate) fn surface_key(server: &str, entry: &ReceivedSurface) -> SurfaceKey {
    (
        server.to_owned(),
        entry.sender_user_id,
        entry.sender_instance,
        entry.surface.id,
    )
}

/// The cards of server `index` the user wants to see, above the voice status.
/// A card the user hid takes no room at all; the picker in the voice card
/// header brings it back.
pub fn cards(this: &Desktop, index: usize, cx: &mut Context<Desktop>) -> Vec<AnyElement> {
    let surfaces = this.client.surfaces(index);
    surfaces
        .iter()
        .filter(|entry| !this.dismissed_surfaces.contains(&key(this, index, entry)))
        .take(MAX_CARDS)
        .map(|entry| card(this, index, entry, key(this, index, entry), cx))
        .collect()
}

/// Whether the user hid a card of server `index`.
pub fn hidden(this: &Desktop, index: usize) -> bool {
    this.client
        .surfaces(index)
        .iter()
        .any(|entry| this.dismissed_surfaces.contains(&key(this, index, entry)))
}

/// A row of the integrations picker: the sender and what its card says.
pub(crate) fn describe(this: &Desktop, index: usize, entry: &ReceivedSurface) -> String {
    let who = this
        .client
        .servers
        .get(index)
        .and_then(|server| server.users.get(&entry.sender))
        .map_or("Integration", |user| user.name.as_str());
    let title = entry.surface.nodes.iter().find_map(|node| match node {
        Node::Text {
            style: Style::Title,
            text,
        } => Some(text.as_str()),
        _ => None,
    });
    match title {
        Some(title) => format!("{who} · {title}"),
        None => who.to_owned(),
    }
}

/// One card: its header, its nodes after the title and subtitle, and a way to
/// hide it.
fn card(
    this: &Desktop,
    index: usize,
    entry: &nuschl_client::client::ReceivedSurface,
    dismiss_key: SurfaceKey,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let surface = &entry.surface;
    let card_id = format!("{}-{}", entry.sender, surface.id);
    let who = this
        .client
        .servers
        .get(index)
        .and_then(|server| server.users.get(&entry.sender))
        .map_or("Integration", |user| user.name.as_str())
        .to_owned();
    let title = surface.nodes.iter().find_map(|node| match node {
        Node::Text {
            style: Style::Title,
            text,
        } => Some(text.as_str()),
        _ => None,
    });
    let subtitle = surface.nodes.iter().find_map(|node| match node {
        Node::Text {
            style: Style::Subtitle,
            text,
        } => Some(text.as_str()),
        _ => None,
    });
    let initial = title
        .or(Some(who.as_str()))
        .and_then(|text| text.chars().next())
        .unwrap_or('♪')
        .to_uppercase()
        .to_string();
    let received_ms = entry.received_ms;
    let now = this.client.now_ms();
    let mut rest = div().w_full().flex().flex_col().gap_2();
    for (position, node) in surface.nodes.iter().enumerate() {
        if matches!(
            node,
            Node::Text {
                style: Style::Title | Style::Subtitle,
                ..
            }
        ) {
            continue;
        }
        rest = rest.child(node_view(
            this,
            index,
            &card_id,
            surface.id,
            surface.revision,
            entry,
            node,
            position,
            received_ms,
            now,
            cx,
        ));
    }
    let header = div()
        .w_full()
        .flex()
        .items_start()
        .gap_2()
        .child(
            div()
                .flex_shrink_0()
                .size(px(40.))
                .rounded(px(8.))
                .flex()
                .items_center()
                .justify_center()
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color(TEXT))
                .bg(linear_gradient(
                    135.,
                    linear_color_stop(tint(ACCENT, 0xdd), 0.),
                    linear_color_stop(tint(ACCENT, 0x55), 1.),
                ))
                .child(initial),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(
                    div()
                        .truncate()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color(TEXT))
                        .child(layout::label(
                            format!("surface-title-{card_id}"),
                            title.unwrap_or(who.as_str()).to_owned(),
                        )),
                )
                .child(
                    div()
                        .truncate()
                        .text_xs()
                        .text_color(color(TEXT_MUTED))
                        .child(layout::label(
                            format!("surface-subtitle-{card_id}"),
                            subtitle.unwrap_or_default().to_owned(),
                        )),
                ),
        )
        .child({
            let key = dismiss_key.clone();
            crate::dialogs::icon_button(
                format!("surface-dismiss-{card_id}"),
                "window-close",
                "Hide card",
                TEXT_MUTED,
                None,
                move |this: &mut Desktop, _, cx: &mut Context<Desktop>| {
                    this.dismissed_surfaces.insert(key.clone());
                    cx.notify();
                },
                cx,
            )
        });
    div()
        .id(format!("surface-{card_id}"))
        .role(Role::Group)
        .aria_label(format!(
            "{who}: {}, {}",
            title.unwrap_or("integration card"),
            subtitle.unwrap_or("")
        ))
        .w_full()
        .flex_shrink_0()
        .p_2()
        .bg(color(SIDEBAR_FOOTER))
        .flex()
        .flex_col()
        .gap_2()
        .child(header)
        .child(rest)
        .into_any_element()
}

fn key(this: &Desktop, index: usize, entry: &ReceivedSurface) -> SurfaceKey {
    surface_key(&this.client.servers[index].id, entry)
}

#[allow(clippy::too_many_arguments)]
fn node_view(
    this: &Desktop,
    index: usize,
    surface: &str,
    id: u32,
    revision: u64,
    entry: &ReceivedSurface,
    node: &Node,
    position: usize,
    received_ms: u64,
    now: u64,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    match node {
        Node::Text { style, text } => {
            let line =
                div().w_full().min_w_0().truncate().child(
                    text!(text.clone()).with_id(format!("surface-text-{surface}-{position}")),
                );
            match style {
                Style::Title => line
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(color(TEXT))
                    .into_any_element(),
                Style::Subtitle | Style::Body => line
                    .text_xs()
                    .text_color(color(TEXT_MUTED))
                    .into_any_element(),
            }
        }
        Node::Volume => {
            // The sender asked for a volume control; the client draws its own
            // setting for that person. Nothing goes to the bot, and the bot
            // never learns the level.
            let user = this
                .client
                .servers
                .get(index)
                .and_then(|server| server.users.get(&entry.sender));
            let Some(user) = user else {
                return div().into_any_element();
            };
            let who = nuschl_client::native::UserKey::of(user);
            let (volume, _) = who.playback(&this.settings, &this.presences);
            let setting = match &who {
                nuschl_client::native::UserKey::Certificate(hash) => {
                    crate::slider::Setting::UserVolume(hash.clone())
                }
                nuschl_client::native::UserKey::Presence(instance) => {
                    crate::slider::Setting::PresenceVolume(*instance)
                }
            };
            let read_only = this.managed.contains(&setting.managed_key());
            let readout = setting.readout(volume);
            div()
                .w_full()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(crate::assets::icon("volume", 15., color(TEXT_MUTED)))
                        .child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .text_xs()
                                .text_color(color(TEXT_MUTED))
                                .child(layout::label(
                                    format!("surface-volume-label-{surface}"),
                                    "Volume",
                                )),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(color(TEXT_MUTED))
                                .child(layout::label(format!("surface-volume-{surface}"), readout)),
                        ),
                )
                .child(crate::slider::slider(
                    crate::slider::Spec {
                        id: format!("surface-volume-slider-{surface}").into(),
                        name: "Volume".into(),
                        target: crate::slider::Target {
                            owner: crate::slider::Owner::Card,
                            setting,
                        },
                        value: volume,
                        read_only,
                    },
                    cx,
                ))
                .into_any_element()
        }
        Node::Progress {
            position_ms,
            duration_ms,
            advancing,
        } => {
            let elapsed = if *advancing {
                now.saturating_sub(received_ms)
            } else {
                0
            };
            let position = position_ms
                .saturating_add(elapsed)
                .min((*duration_ms).max(*position_ms));
            let fraction = if *duration_ms == 0 {
                0.
            } else {
                (position as f32 / *duration_ms as f32).clamp(0., 1.)
            };
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .id(format!("surface-progress-{surface}"))
                        .role(Role::ProgressIndicator)
                        .aria_label(format!("{} of {}", clock(position), clock(*duration_ms)))
                        .h(px(4.))
                        .w_full()
                        .rounded_full()
                        .bg(color(ELEVATED))
                        .overflow_hidden()
                        .child(
                            div()
                                .h_full()
                                .w(relative(fraction))
                                .rounded_full()
                                .bg(color(ACCENT)),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .text_xs()
                        .text_color(color(TEXT_MUTED))
                        .child(layout::label(
                            format!("surface-elapsed-{surface}"),
                            clock(position),
                        ))
                        .child(layout::label(
                            format!("surface-duration-{surface}"),
                            clock(*duration_ms),
                        )),
                )
                .into_any_element()
        }
        Node::Buttons(buttons) => {
            let mut row = div().flex().items_center().gap_1();
            let who = (entry.sender, entry.sender_instance);
            // A page control sits at the right of the same row, so the card
            // keeps its height.
            let mut trailing = Vec::new();
            for button in buttons {
                if button.icon == Icon::Web {
                    let control = button.id;
                    let label = button.label.clone();
                    if !button.enabled {
                        trailing.push(dimmed(
                            format!("surface-button-{surface}-{control}"),
                            Some("web"),
                            label,
                        ));
                        continue;
                    }
                    trailing.push(
                        crate::dialogs::icon_button(
                            format!("surface-button-{surface}-{control}"),
                            "web",
                            label,
                            TEXT,
                            Some(ELEVATED),
                            move |this: &mut Desktop, _, cx: &mut Context<Desktop>| {
                                if this
                                    .client
                                    .press_surface(index, who.0, who.1, id, revision, control)
                                    .is_ok()
                                {
                                    cx.notify();
                                }
                            },
                            cx,
                        )
                        .into_any_element(),
                    );
                    continue;
                }
                if button.icon == Icon::None {
                    // A control with a label and no icon: the wire allows it and
                    // a bot may publish nothing else.
                    let control = button.id;
                    let label = button.label.clone();
                    if !button.enabled {
                        row = row.child(dimmed(
                            format!("surface-button-{surface}-{control}"),
                            None,
                            label,
                        ));
                        continue;
                    }
                    row = row.child(crate::dialogs::styled_button(
                        format!("surface-button-{surface}-{control}"),
                        label,
                        crate::dialogs::Tone::Secondary,
                        move |this: &mut Desktop, _, cx: &mut Context<Desktop>| {
                            if this
                                .client
                                .press_surface(index, who.0, who.1, id, revision, control)
                                .is_ok()
                            {
                                cx.notify();
                            }
                        },
                        cx,
                    ));
                    continue;
                }
                if button.icon == Icon::Headphones {
                    // The one control the client owns: listening to the card's
                    // channel. It never becomes a press the bot receives.
                    row = row.child(listen_toggle(this, index, surface, entry, button, cx));
                    continue;
                }
                let control = button.id;
                let label = button.label.clone();
                // The transport a player is used to: play and pause carry the
                // accent, skipping carries the neutral control tint.
                let background = match button.icon {
                    Icon::Play | Icon::Pause => ACCENT,
                    _ => ELEVATED,
                };
                let Some(icon) = transport_icon(button.icon) else {
                    continue;
                };
                if !button.enabled {
                    // Drawn, dimmed and unpressed: the bot said this control is
                    // unavailable now and will publish a revision when it is not.
                    row = row.child(dimmed(
                        format!("surface-button-{surface}-{control}"),
                        Some(icon),
                        label,
                    ));
                    continue;
                }
                row = row.child(crate::dialogs::icon_button(
                    format!("surface-button-{surface}-{control}"),
                    icon,
                    label,
                    TEXT,
                    Some(background),
                    move |this: &mut Desktop, _, cx: &mut Context<Desktop>| {
                        if this
                            .client
                            .press_surface(index, who.0, who.1, id, revision, control)
                            .is_ok()
                        {
                            cx.notify();
                        }
                    },
                    cx,
                ));
            }
            if !trailing.is_empty() {
                row = row.child(div().flex_1());
                for control in trailing {
                    row = row.child(control);
                }
            }
            row.into_any_element()
        }
    }
}

/// Listening to the channel the card's sender speaks in: on when the user is in
/// it or already listening, otherwise one press away when the server allows it.
fn listen_toggle(
    this: &Desktop,
    index: usize,
    surface: &str,
    entry: &ReceivedSurface,
    button: &Button,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let Some(server) = this.client.servers.get(index) else {
        return div().into_any_element();
    };
    let Some(channel) = server.users.get(&entry.sender).map(|user| user.channel) else {
        return div().into_any_element();
    };
    let own = server
        .own_session
        .and_then(|session| server.users.get(&session));
    let here = own.is_some_and(|own| own.channel == channel);
    let listening = here || own.is_some_and(|own| own.listening.contains(&channel));
    // The server drops a listen into a channel the client does not see, so the
    // control follows the same visibility it does.
    let seen = here || server.seen_channels.contains(&channel);
    let allowed = seen && (here || server.may_try(channel, perm::LISTEN));
    if !button.enabled || (!allowed && !listening) {
        // Drawn, dimmed and unpressed: the server would refuse the listen.
        return dimmed(
            format!("surface-listen-{surface}"),
            Some("headphones-off"),
            button.label.clone(),
        );
    }
    crate::dialogs::icon_button(
        format!("surface-listen-{surface}"),
        if listening {
            "headphones"
        } else {
            "headphones-off"
        },
        button.label.clone(),
        if listening { TEXT } else { TEXT_MUTED },
        listening.then_some(ACCENT),
        move |this, _, cx| {
            if here {
                return;
            }
            this.run(
                index,
                Command::Listen {
                    channel,
                    listen: !listening,
                },
            );
            cx.notify();
        },
        cx,
    )
}

/// The compiled asset of a transport control. `None` and `Headphones` have
/// their own drawing and no name here.
fn transport_icon(icon: Icon) -> Option<&'static str> {
    match icon {
        Icon::Play => Some("play"),
        Icon::Pause => Some("pause"),
        Icon::Next => Some("next"),
        Icon::Previous => Some("previous"),
        Icon::None | Icon::Headphones | Icon::Web => None,
    }
}

/// A control the client draws and takes no press for: the bot disabled it, or
/// the server would refuse the action. With an icon it is a 32px square; with a
/// label alone it widens to the text.
fn dimmed(id: String, icon: Option<&'static str>, label: String) -> AnyElement {
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.clone())
        .flex_shrink_0()
        .h(px(32.))
        .when(icon.is_some(), |control| control.w(px(32.)))
        .when(icon.is_none(), |control| control.px(px(10.)))
        .rounded(px(6.))
        .flex()
        .items_center()
        .justify_center()
        .bg(tint(ELEVATED, 0x2e))
        .opacity(0.45)
        .child(match icon {
            Some(icon) => crate::assets::icon(icon, 18., color(TEXT_MUTED)).into_any_element(),
            None => div()
                .text_xs()
                .text_color(color(TEXT_MUTED))
                .child(label)
                .into_any_element(),
        })
        .into_any_element()
}

fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}
