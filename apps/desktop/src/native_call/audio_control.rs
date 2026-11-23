// SPDX-License-Identifier: EUPL-1.2
//! The shared-audio control on a received screen share, as in a video player:
//! a speaker that mutes and unmutes, and a volume slider while the pointer or
//! the keyboard is on the control. It is greyed out while the share carries no
//! audio. The choices are this viewer's, kept per publisher for the call.

use super::{Surface, audio::Volumes};
use crate::{
    assets::icon,
    dialogs::activation,
    layout,
    slider::Scale,
    theme::{ACCENT, MAIN, SELECTED, TEXT, TEXT_FAINT, color, tint},
};
use gpui_kit::{prelude::*, *};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};
use tokio::sync::watch;

/// Slider travel per arrow key.
const STEP: f32 = 0.05;
/// The slider's track width.
const TRACK: f32 = 72.;

#[derive(Clone, Copy)]
struct Level {
    volume: f32,
    muted: bool,
}

impl Default for Level {
    fn default() -> Self {
        Self {
            volume: 1.,
            muted: false,
        }
    }
}

impl Level {
    fn heard(self) -> f32 {
        if self.muted { 0. } else { self.volume }
    }
}

pub(super) struct Controls {
    /// Publishers whose screen share carries audio.
    pub(super) audio: BTreeSet<u64>,
    levels: BTreeMap<u64, Level>,
    volumes: watch::Sender<Volumes>,
    hovered: Option<u64>,
    /// Each control's focus: the speaker and the slider.
    focus: BTreeMap<u64, [FocusHandle; 2]>,
}

impl Controls {
    pub(super) fn new(volumes: watch::Sender<Volumes>) -> Self {
        Self {
            audio: BTreeSet::new(),
            levels: BTreeMap::new(),
            volumes,
            hovered: None,
            focus: BTreeMap::new(),
        }
    }

    /// Focus for the controls of `publishers`' screen shares.
    pub(super) fn prepare(&mut self, publishers: impl Iterator<Item = u64>, cx: &mut App) {
        for publisher in publishers {
            self.focus
                .entry(publisher)
                .or_insert_with(|| [cx.focus_handle(), cx.focus_handle()]);
        }
    }

    fn level(&self, publisher: u64) -> Level {
        self.levels.get(&publisher).copied().unwrap_or_default()
    }

    fn set(&mut self, publisher: u64, level: Level) {
        self.levels.insert(publisher, level);
        self.volumes.send_modify(|volumes| {
            volumes.insert(publisher, level.heard());
        });
    }

    fn toggle(&mut self, publisher: u64) {
        let level = self.level(publisher);
        self.set(
            publisher,
            if level.heard() == 0. {
                Level {
                    volume: if level.volume == 0. { 1. } else { level.volume },
                    muted: false,
                }
            } else {
                Level {
                    muted: true,
                    ..level
                }
            },
        );
    }

    fn slide(&mut self, publisher: u64, position: f32) {
        let volume = Scale::Amplitude.value(position.clamp(0., 1.));
        self.set(
            publisher,
            Level {
                volume,
                muted: false,
            },
        );
    }

    fn position(&self, publisher: u64) -> f32 {
        Scale::Amplitude.position(self.level(publisher).heard())
    }
}

/// The control at the bottom of `publisher`'s screen-share tile.
pub(super) fn control(
    controls: &Controls,
    publisher: u64,
    window: &Window,
    cx: &Context<Surface>,
) -> AnyElement {
    let shell = div()
        .id(SharedString::from(format!("shared-audio:{publisher}")))
        .absolute()
        .bottom_2()
        .right_2()
        .flex()
        .items_center()
        .gap_1()
        .p_0p5()
        .rounded(px(8.))
        .bg(tint(MAIN, 0xd0));
    let Some([speaker, slider]) = controls
        .focus
        .get(&publisher)
        .filter(|_| controls.audio.contains(&publisher))
        .cloned()
    else {
        return shell
            .child(
                div()
                    .id(SharedString::from(format!("no-shared-audio:{publisher}")))
                    .role(Role::Button)
                    .aria_label("No shared audio")
                    .size(px(32.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon("volume-off", 18., color(TEXT_FAINT))),
            )
            .into_any_element();
    };
    let silent = controls.level(publisher).heard() == 0.;
    let open = controls.hovered == Some(publisher)
        || speaker.is_focused(window)
        || slider.is_focused(window);
    let entity = cx.entity().downgrade();
    let button = div()
        .id(SharedString::from(format!("shared-audio-mute:{publisher}")))
        .role(Role::Button)
        .aria_label(if silent {
            "Unmute shared audio"
        } else {
            "Mute shared audio"
        })
        .cursor_pointer()
        .size(px(32.))
        .rounded(px(6.))
        .flex()
        .items_center()
        .justify_center()
        .hover(|style| style.bg(color(SELECTED)))
        .on_click(cx.listener(move |this: &mut Surface, _, _, cx| {
            toggle(this, publisher, cx);
        }))
        .on_key_down(
            cx.listener(move |this: &mut Surface, event: &KeyDownEvent, _, cx| {
                if activation(event) {
                    toggle(this, publisher, cx);
                    cx.stop_propagation();
                }
            }),
        )
        .child(icon(
            if silent { "volume-off" } else { "volume" },
            18.,
            color(TEXT),
        ));
    let speaker = layout::focusable_button_with_handle(
        SharedString::from(format!("shared-audio-mute:{publisher}")),
        speaker,
        button,
        move |_, _, cx| {
            let _ = entity.update(cx, |this, cx| toggle(this, publisher, cx));
        },
    );
    shell
        .on_hover(
            cx.listener(move |this: &mut Surface, hovered: &bool, _, cx| {
                if *hovered {
                    this.audio.hovered = Some(publisher);
                } else if this.audio.hovered == Some(publisher) {
                    this.audio.hovered = None;
                }
                cx.notify();
            }),
        )
        .when(open, |shell| {
            shell.child(volume(controls, publisher, slider, cx))
        })
        .child(speaker)
        .into_any_element()
}

fn toggle(surface: &mut Surface, publisher: u64, cx: &mut Context<Surface>) {
    surface.audio.toggle(publisher);
    cx.notify();
}

/// The volume slider: arrow keys, Home and End, or the pointer.
fn volume(
    controls: &Controls,
    publisher: u64,
    focus: FocusHandle,
    cx: &Context<Surface>,
) -> AnyElement {
    let position = controls.position(publisher);
    let percent = f64::from(controls.level(publisher).heard()) * 100.;
    let track = Rc::new(Cell::new(Bounds::<Pixels>::default()));
    let painted = track.clone();
    let pressed = track.clone();
    let moved = track.clone();
    let at = |track: &Cell<Bounds<Pixels>>, x: Pixels| {
        let bounds = track.get();
        (x - bounds.left()) / bounds.size.width.max(px(1.))
    };
    let slider = div()
        .id(SharedString::from(format!(
            "shared-audio-volume:{publisher}"
        )))
        .role(Role::Slider)
        .aria_label("Shared audio volume")
        .aria_numeric_value(percent)
        .aria_min_numeric_value(0.)
        .aria_max_numeric_value(100.)
        .aria_value(format!("{percent:.0} %"))
        .cursor_pointer()
        .relative()
        .w(px(TRACK))
        .h(px(24.))
        .mx_1()
        .child(
            div()
                .absolute()
                .top(px(10.))
                .left_0()
                .w(px(TRACK))
                .h(px(4.))
                .rounded(px(2.))
                .bg(color(TEXT_FAINT)),
        )
        .child(
            div()
                .absolute()
                .top(px(10.))
                .left_0()
                .w(px(TRACK * position))
                .h(px(4.))
                .rounded(px(2.))
                .bg(color(ACCENT)),
        )
        .child(
            div()
                .absolute()
                .top(px(6.))
                .left(px(TRACK * position - 6.))
                .size(px(12.))
                .rounded_full()
                .bg(color(TEXT)),
        )
        .child(
            canvas(move |bounds, _, _| painted.set(bounds), |_, _, _, _| {})
                .absolute()
                .size_full(),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this: &mut Surface, event: &MouseDownEvent, _, cx| {
                this.audio.slide(publisher, at(&pressed, event.position.x));
                cx.notify();
            }),
        )
        .on_mouse_move(
            cx.listener(move |this: &mut Surface, event: &MouseMoveEvent, _, cx| {
                if event.pressed_button == Some(MouseButton::Left) {
                    this.audio.slide(publisher, at(&moved, event.position.x));
                    cx.notify();
                }
            }),
        )
        .on_key_down(
            cx.listener(move |this: &mut Surface, event: &KeyDownEvent, _, cx| {
                let position = this.audio.position(publisher);
                let next = match event.keystroke.key.as_str() {
                    "right" | "up" => position + STEP,
                    "left" | "down" => position - STEP,
                    "home" => 0.,
                    "end" => 1.,
                    _ => return,
                };
                this.audio.slide(publisher, next);
                cx.notify();
                cx.stop_propagation();
            }),
        );
    layout::focusable_with_handle(
        SharedString::from(format!("shared-audio-volume:{publisher}")),
        focus,
        slider,
    )
}
