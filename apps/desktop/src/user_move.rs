// SPDX-License-Identifier: EUPL-1.2
//! Moving someone to another channel (Mumble's Move permission): drag their
//! row onto a channel, or pick them up with "Move to channel…" and choose
//! with Up and Down. The tree previews the move and its rows slide as the
//! server rail's do; the drop sends the move, and the preview holds until
//! the server shows it or refuses.
use crate::{
    menus::{ActionOrigin, MenuTarget},
    rail::{Offset, edge_scroll, shift},
    state::{Desktop, ServerView},
    theme::*,
    view::TreeFocus,
};
use gpui_kit::{prelude::*, *};
use nuschl_client::{model::permission as perm, tree::TreeRow};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

/// Row heights and the gap between rows, as the tree draws them.
pub const CHANNEL_ROW: f32 = 32.;
pub const USER_ROW: f32 = 30.;
pub const LISTENER_ROW: f32 = 26.;
pub const ROW_GAP: f32 = 2.;
/// How long a sent move waits for the server before its preview goes.
const ANSWER: Duration = Duration::from_secs(3);
/// Channels whose permissions one pick-up asks for, at most.
const QUERIES: usize = 128;

#[derive(Clone)]
pub struct DragUser {
    pub server: String,
    pub session: u32,
}

/// A row of the tree, kept by identity while rows move.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RowKey {
    Channel(u32),
    User(u32),
    /// A listener's session and the channel they listen to.
    Listener(u32, u32),
}

struct Slot {
    key: RowKey,
    top: f32,
    bottom: f32,
    /// The channel whose row, or whose person or listener, this is.
    channel: u32,
}

/// The rows' places from `top` down.
fn slots(server: &ServerView, rows: &[TreeRow], top: f32) -> Vec<Slot> {
    let (mut y, mut channel) = (top, 0);
    rows.iter()
        .map(|row| {
            let (key, height) = match *row {
                TreeRow::Channel(row) => {
                    channel = server.channels[row.index].id;
                    (RowKey::Channel(channel), CHANNEL_ROW)
                }
                TreeRow::User { session, .. } => (RowKey::User(session), USER_ROW),
                TreeRow::Listener { session, .. } => {
                    (RowKey::Listener(session, channel), LISTENER_ROW)
                }
            };
            let slot = Slot {
                key,
                top: y,
                bottom: y + height + ROW_GAP,
                channel,
            };
            y = slot.bottom;
            slot
        })
        .collect()
}

/// The channel whose row, or a person under it, is at `y`.
fn group_at(slots: &[Slot], y: f32) -> Option<u32> {
    slots
        .iter()
        .find(|slot| y >= slot.top && y < slot.bottom)
        .map(|slot| slot.channel)
}

/// Where a pointer at `y` takes the person: a channel they may be moved to,
/// or `None` for where they are. `top` is the first row's top.
fn target_at(server: &ServerView, picked: &Picked, top: f32, y: f32) -> Option<u32> {
    let takes = |channel| channel != picked.source && server.may_move(picked.session, channel);
    let shown = server.tree_rows_moving(picked.target.map(|target| (picked.session, target)));
    match group_at(&slots(server, &shown, top), y) {
        Some(channel) if channel == picked.source || takes(channel) => {
            (channel != picked.source).then_some(channel)
        }
        // Elsewhere the preview goes back, unless that brings a channel they
        // may go to under the pointer: it holds then and cannot flicker.
        _ => match group_at(&slots(server, &server.tree_rows(), top), y) {
            Some(channel) if takes(channel) => picked.target,
            _ => None,
        },
    }
}

struct Picked {
    origin: ActionOrigin,
    server: String,
    session: u32,
    source: u32,
    target: Option<u32>,
    /// A pointer drag, and where the pointer is; the keyboard has neither.
    drag: bool,
    pointer: Option<Point<Pixels>>,
    /// The visible channels' permissions were asked for.
    asked: bool,
    /// Sent: when, and the server's notice revision then.
    sent: Option<(Instant, u64)>,
}

#[derive(Default)]
pub struct UserMove {
    picked: Option<Picked>,
    offsets: BTreeMap<RowKey, Offset>,
    /// The selected server's rows and their tops, as last laid out.
    laid: Option<(String, Vec<(RowKey, f32)>)>,
    /// Rows slide while a move is picked and in the frame after it ends.
    sliding: bool,
    /// Enter sent a move; the row's click on its key-up opens nothing.
    pub keyboard_sent: bool,
    frame: Option<Instant>,
}

impl UserMove {
    fn picked_in(&self, server: &str) -> Option<&Picked> {
        self.picked
            .as_ref()
            .filter(|picked| picked.server == server)
    }
    /// The person shown elsewhere in `server`'s tree, and where.
    pub fn preview(&self, server: &str) -> Option<(u32, u32)> {
        let picked = self.picked_in(server)?;
        Some((picked.session, picked.target?))
    }
    /// Who is being moved in `server`, and whether the move was sent.
    pub fn moving(&self, server: &str) -> Option<(u32, bool)> {
        self.picked_in(server)
            .map(|picked| (picked.session, picked.sent.is_some()))
    }
    /// Who is being moved in `server` with the keyboard, before it is sent.
    pub fn keyboard(&self, server: &str) -> Option<u32> {
        self.picked_in(server)
            .filter(|picked| !picked.drag && picked.sent.is_none())
            .map(|picked| picked.session)
    }
    /// Who is being moved in `server` while channels still take a drop.
    pub fn choosing(&self, server: &str) -> Option<u32> {
        self.picked_in(server)
            .filter(|picked| picked.sent.is_none())
            .map(|picked| picked.session)
    }
    pub fn target(&self, server: &str) -> Option<u32> {
        self.picked_in(server)?.target
    }
    pub fn picked(&self) -> bool {
        self.picked.is_some()
    }
    /// Where `key` is among the selected server's rows, as last laid out.
    pub fn row(&self, key: RowKey) -> Option<usize> {
        self.laid
            .as_ref()?
            .1
            .iter()
            .position(|(laid, _)| *laid == key)
    }
    pub fn offset(&self, key: RowKey) -> f32 {
        self.offsets.get(&key).map_or(0., Offset::value)
    }
}

/// The person under the pointer while dragging: their row as it looks.
pub struct Ghost {
    name: String,
    image: Option<Arc<RenderImage>>,
    width: f32,
    indent: f32,
}
impl Render for Ghost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // As wide as the row it came from, so GPUI's cursor offset holds.
        div().w(px(self.width)).h(px(USER_ROW)).child(
            div()
                .absolute()
                .left(px(self.indent - 6.))
                .h_full()
                .max_w(px(self.width - self.indent + 6.))
                .pl(px(6.))
                .pr_3()
                .flex()
                .items_center()
                .gap_2()
                .rounded(px(6.))
                .bg(color(SELECTED))
                .shadow_lg()
                .text_sm()
                .text_color(color(TEXT))
                .child(crate::view::avatar(
                    &self.name,
                    22.,
                    false,
                    ("drag-avatar", 0),
                    self.image.clone(),
                ))
                .child(div().truncate().child(self.name.clone())),
        )
    }
}

impl Desktop {
    /// Pick up `session` on server `index`, where we hold Move.
    fn pick_user(&mut self, index: usize, session: u32, drag: bool) -> bool {
        let view = &self.client.servers[index];
        let Some(user) = view.users.get(&session) else {
            return false;
        };
        if view.own_session == Some(session) || !view.may(user.channel, perm::MOVE) {
            return false;
        }
        self.user_move.picked = Some(Picked {
            origin: ActionOrigin::capture(view, MenuTarget::User(session)),
            server: view.id.clone(),
            session,
            source: user.channel,
            target: None,
            drag,
            pointer: None,
            asked: false,
            sent: None,
        });
        true
    }

    pub fn begin_user_drag(&mut self, item: &DragUser, cx: &mut Context<Self>) -> Entity<Ghost> {
        let index = self.client.server_index(&item.server);
        let picked = index.is_some_and(|index| self.pick_user(index, item.session, true));
        let user = index
            .filter(|_| picked)
            .and_then(|index| self.client.servers[index].users.get(&item.session));
        let name = user.map_or_else(String::new, |user| {
            crate::names::display_name(user, &self.settings).to_string()
        });
        let image = index
            .and_then(|index| crate::view::avatar_image(self, &self.client.servers[index], user));
        // The row's width and indent, as last laid out.
        let width = self
            .user_move
            .row(RowKey::User(item.session))
            .and_then(|row| self.channel_scroll.bounds_for_item(row + 1))
            .map_or(240., |bounds| f32::from(bounds.size.width));
        let depth = index
            .and_then(|index| {
                let server = &self.client.servers[index];
                server.tree_rows().into_iter().find_map(|row| match row {
                    TreeRow::User { session, depth } if session == item.session => Some(depth),
                    _ => None,
                })
            })
            .unwrap_or(1);
        cx.notify();
        cx.new(|_| Ghost {
            name,
            image,
            width,
            indent: crate::view::user_indent(depth),
        })
    }

    pub fn move_user_drag(&mut self, event: &DragMoveEvent<DragUser>, cx: &mut Context<Self>) {
        let item = event.drag(cx);
        let Some(picked) = self.user_move.picked.as_mut().filter(|picked| {
            picked.drag
                && picked.sent.is_none()
                && picked.server == item.server
                && picked.session == item.session
        }) else {
            return;
        };
        picked.pointer = Some(event.event.position);
        self.point_user_move();
        // A pointer held at an edge keeps scrolling (animate_user_move).
        cx.notify();
    }

    /// Choose the target under the pointer.
    fn point_user_move(&mut self) {
        let Some(picked) = &self.user_move.picked else {
            return;
        };
        let handle = &self.channel_scroll;
        let (Some(point), Some(first)) = (picked.pointer, handle.bounds_for_item(1)) else {
            return;
        };
        let Some(view) = self
            .selected_indices()
            .map(|(index, _)| &self.client.servers[index])
            .filter(|view| view.id == picked.server)
        else {
            return;
        };
        if !handle.bounds().contains(&point) {
            return;
        }
        // Rows are laid out unscrolled; the first (the root) never moves.
        let y = f32::from(point.y - handle.offset().y);
        let target = target_at(view, picked, f32::from(first.top()), y);
        if let Some(picked) = &mut self.user_move.picked {
            picked.target = target;
        }
    }

    pub fn drop_user_drag(&mut self, item: &DragUser, window: &mut Window, cx: &mut Context<Self>) {
        if self.user_move.choosing(&item.server) == Some(item.session) {
            self.point_user_move();
            self.send_user_move(window, cx);
        }
    }

    /// "Move to channel…": pick up `session`; Up and Down choose.
    pub fn begin_keyboard_move(
        &mut self,
        index: usize,
        session: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pick_user(index, session, false) {
            self.tree_cursor = Some((index, TreeFocus::User(session)));
            window.focus(&self.tree_focus, cx);
        }
        cx.notify();
    }

    /// Up (-1) or Down (1): the previous or next channel they may go to.
    pub fn step_user_move(&mut self, step: isize) {
        let Some(picked) = &self.user_move.picked else {
            return;
        };
        let Some(view) = self
            .client
            .server_index(&picked.server)
            .map(|index| &self.client.servers[index])
        else {
            return;
        };
        let choices: Vec<u32> = view
            .tree_rows()
            .into_iter()
            .filter_map(|row| match row {
                TreeRow::Channel(row) => Some(view.channels[row.index].id),
                _ => None,
            })
            .filter(|&id| id == picked.source || view.may_move(picked.session, id))
            .collect();
        let Some(at) = choices
            .iter()
            .position(|&id| id == picked.target.unwrap_or(picked.source))
        else {
            return;
        };
        let next = choices[at.saturating_add_signed(step).min(choices.len() - 1)];
        let target = Some(next).filter(|&id| id != picked.source);
        if let Some(picked) = &mut self.user_move.picked {
            picked.target = target;
        }
    }

    /// Send the move to the chosen channel; without one, put them back.
    pub fn send_user_move(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picked) = &self.user_move.picked else {
            return;
        };
        let (session, target, origin) = (picked.session, picked.target, picked.origin.clone());
        let index = origin.live_index(self);
        let (Some(target), Some(index)) = (target, index) else {
            return self.cancel_user_move(window, cx);
        };
        let origin = origin.with_channel(&self.client.servers[index], target);
        let revision = self.client.servers[index].notice_revision();
        match origin.with_scope(self, |this| this.client.move_user(index, session, target)) {
            Ok(()) => {
                if let Some(picked) = &mut self.user_move.picked {
                    picked.sent = Some((Instant::now(), revision));
                }
            }
            Err(error) => {
                self.client.servers[index].set_notice(error);
                self.user_move.picked = None;
            }
        }
        cx.notify();
    }

    pub fn cancel_user_move(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .user_move
            .picked
            .take()
            .is_some_and(|picked| picked.drag)
        {
            cx.stop_active_drag(window);
        }
        cx.notify();
    }

    pub fn animate_user_move(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let reduce = cx.reduce_motion();
        // A move ends with its person, connection or drag, or with the answer.
        if let Some(picked) = &self.user_move.picked {
            let view = self
                .client
                .server_index(&picked.server)
                .map(|index| &self.client.servers[index]);
            let stays = view
                .and_then(|view| view.users.get(&picked.session))
                .is_some_and(|user| user.channel == picked.source);
            let over = picked.origin.live_index(self).is_none()
                || !stays
                || match picked.sent {
                    Some((at, revision)) => {
                        at.elapsed() > ANSWER
                            || view.is_none_or(|view| view.notice_revision() != revision)
                    }
                    None => picked.drag && !cx.has_active_drag(),
                };
            if over {
                self.user_move.picked = None;
            }
        }
        // Ask where they may go: the visible channels, outside any menu's scope.
        if let Some(picked) = self
            .user_move
            .picked
            .as_mut()
            .filter(|picked| !picked.asked)
        {
            picked.asked = true;
            if let Some(index) = self.client.server_index(&picked.server) {
                let view = &self.client.servers[index];
                let unknown: Vec<u32> = view
                    .tree_rows()
                    .into_iter()
                    .filter_map(|row| match row {
                        TreeRow::Channel(row) => Some(view.channels[row.index].id),
                        _ => None,
                    })
                    .filter(|id| !view.permissions.contains_key(id))
                    .take(QUERIES)
                    .collect();
                for channel in unknown {
                    self.client.query_permissions(index, channel);
                }
            }
        }
        // Whether people may be dragged: our permissions where they are.
        if let Some((index, _)) = self.selected_indices() {
            let view = &self.client.servers[index];
            let unknown: std::collections::BTreeSet<u32> = view
                .users
                .values()
                .map(|user| user.channel)
                .filter(|channel| !view.permissions.contains_key(channel))
                .take(QUERIES)
                .collect();
            for channel in unknown {
                self.client.query_permissions(index, channel);
            }
        }
        let picked = self.user_move.picked();
        let sliding = std::mem::replace(&mut self.user_move.sliding, picked);
        let Some((index, _)) = self.selected_indices() else {
            self.user_move.laid = None;
            return;
        };
        let view = &self.client.servers[index];
        let rows = view.tree_rows_moving(self.user_move.preview(&view.id));
        let laid: Vec<(RowKey, f32)> = slots(view, &rows, 0.)
            .into_iter()
            .map(|slot| (slot.key, slot.top))
            .collect();
        if (sliding || picked)
            && let Some((server, prior)) = &self.user_move.laid
            && *server == view.id
            && *prior != laid
        {
            let prior: BTreeMap<RowKey, f32> = prior.iter().copied().collect();
            for (key, top) in &laid {
                if let Some(old) = prior.get(key) {
                    shift(&mut self.user_move.offsets, *key, old - top, reduce);
                }
            }
        }
        self.user_move.laid = Some((view.id.clone(), laid));
        self.user_move
            .offsets
            .retain(|_, offset| !reduce && offset.moving());
        let now = Instant::now();
        let dt = self.user_move.frame.replace(now).map_or(0., |prior| {
            now.duration_since(prior).as_secs_f32().min(0.05)
        });
        let pointer = self
            .user_move
            .picked
            .as_ref()
            .filter(|picked| picked.sent.is_none())
            .and_then(|picked| picked.pointer);
        let scrolling = pointer.is_some_and(|point| edge_scroll(&self.channel_scroll, point, dt));
        if scrolling {
            self.point_user_move();
        }
        if scrolling || !self.user_move.offsets.is_empty() {
            window.request_animation_frame();
        }
    }
}

pub fn drag_view(owner: &WeakEntity<Desktop>, item: &DragUser, cx: &mut App) -> Entity<Ghost> {
    owner
        .update(cx, |this, cx| this.begin_user_drag(item, cx))
        .unwrap_or_else(|_| {
            cx.new(|_| Ghost {
                name: String::new(),
                image: None,
                width: 240.,
                indent: 50.,
            })
        })
}

#[cfg(test)]
mod tests {
    use super::{
        ActionOrigin, MenuTarget, Picked, RowKey, ServerView, group_at, perm, slots, target_at,
    };
    use nuschl_client::{conversation::ConversationState, model::User};
    use nuschl_core::bookmark::ServerBookmark;

    /// Root (0) holds Lobby (1) and Music (2); bob (2) is in Lobby. We may
    /// move people in Lobby and Music, not in the root.
    fn server() -> ServerView {
        let mut server =
            ServerView::new(ServerBookmark::new("Test", "localhost", 64738, "andi").unwrap());
        server.channels = [
            (0, None, "Root"),
            (1, Some(0), "Lobby"),
            (2, Some(0), "Music"),
        ]
        .into_iter()
        .map(|(id, parent, name)| ConversationState::channel(id, parent, name.into(), 0))
        .collect();
        for (session, name, channel) in [(1, "andi", 0), (2, "bob", 1)] {
            server.users.insert(
                session,
                User {
                    session,
                    name: name.into(),
                    channel,
                    ..User::default()
                },
            );
        }
        server.own_session = Some(1);
        server.permissions = [(0, perm::ENTER), (1, perm::MOVE), (2, perm::MOVE)].into();
        server
    }

    fn picked(target: Option<u32>) -> Picked {
        Picked {
            origin: ActionOrigin::capture(&server(), MenuTarget::User(2)),
            server: String::new(),
            session: 2,
            source: 1,
            target,
            drag: true,
            pointer: None,
            asked: true,
            sent: None,
        }
    }

    // Rows: Root 0-34, andi 34-66, Lobby 66-100, bob 100-132, Music 132-166.
    #[test]
    fn rows_take_their_drawn_heights_and_belong_to_their_channel() {
        let server = server();
        let laid = slots(&server, &server.tree_rows(), 0.);
        let tops: Vec<_> = laid.iter().map(|slot| (slot.key, slot.top)).collect();
        assert_eq!(
            tops,
            [
                (RowKey::Channel(0), 0.),
                (RowKey::User(1), 34.),
                (RowKey::Channel(1), 66.),
                (RowKey::User(2), 100.),
                (RowKey::Channel(2), 132.),
            ]
        );
        assert_eq!(group_at(&laid, 40.), Some(0));
        assert_eq!(group_at(&laid, 120.), Some(1));
        assert_eq!(group_at(&laid, 170.), None);
    }

    #[test]
    fn the_pointer_chooses_channels_they_may_go_to_and_never_flickers() {
        let server = server();
        // Over Music: there; over their own row in Music's preview: still there.
        assert_eq!(target_at(&server, &picked(None), 0., 140.), Some(2));
        assert_eq!(target_at(&server, &picked(Some(2)), 0., 150.), Some(2));
        // Over Lobby, where they are: no move.
        assert_eq!(target_at(&server, &picked(Some(2)), 0., 70.), None);
        // Over the root, where we may not move them: no move.
        assert_eq!(target_at(&server, &picked(None), 0., 10.), None);
        // In Music's preview Lobby has moved up and the pointer at 120 is
        // over Music; without the preview that is bob's row in Lobby.
        assert_eq!(target_at(&server, &picked(Some(2)), 0., 120.), Some(2));
    }
}
