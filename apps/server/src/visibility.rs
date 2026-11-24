// SPDX-License-Identifier: EUPL-1.2
//! What each client sees (#341): the channels it may traverse and the people
//! in them; [`State::hidden_channels`] decides. Every `ChannelState`,
//! `ChannelRemove`, `UserState` and `UserRemove` that goes to more than one
//! client passes through here. Each user keeps the channels its client was
//! not told about ([`crate::state::User::hidden`]); the client knows a user
//! exactly when it knows that user's channel.
//!
//! When what a client may see changes, it gets the difference: added
//! channels (parents first, then their links, the people in them and the
//! listeners elsewhere), or removed ones (the people in them and the
//! listeners first, then the channels, children first). A user who moves
//! learns the channels its new place shows before everyone hears of the move;
//! the rest follows in [`State::finish`], after the handler, for the
//! connections whose authorisation changed (`authz_pending`). A change sends a
//! client at most the channel tree it sees, as at login, through its bounded
//! queue.
use crate::state::{State, User};
use bytes::Bytes;
use nuschl_protocol::{control as proto, encode_control, kind, nuschl};
use std::{cmp::Reverse, collections::BTreeSet};

impl State {
    /// Whether `viewer`'s client knows `channel`.
    pub fn sees_channel(&self, viewer: u32, channel: u32) -> bool {
        self.channels.get(channel).is_some()
            && self
                .users
                .get(&viewer)
                .is_some_and(|user| !user.hidden.contains(&channel))
    }

    /// Whether `viewer`'s client knows the user `session`.
    pub fn sees_user(&self, viewer: u32, session: u32) -> bool {
        self.users
            .get(&session)
            .is_some_and(|user| self.sees_channel(viewer, user.channel))
    }

    /// `user`'s full `UserState` for `viewer`, without listeners in channels
    /// it does not see.
    pub fn user_state_for(viewer: &User, user: &User) -> proto::UserState {
        let mut message = user.state_message();
        message
            .listening_channel_add
            .retain(|channel| !viewer.hidden.contains(channel));
        message
    }

    /// A `UserState` about `message.session` to everyone who sees that user,
    /// except `except`, without listeners in channels a client does not see
    /// and without an actor it does not see.
    pub fn broadcast_user(&self, message: &proto::UserState, except: Option<u32>) {
        let Some(channel) = message
            .session
            .and_then(|session| self.users.get(&session))
            .map(|user| user.channel)
        else {
            return;
        };
        let Some(frame) = encode_control(kind::USER_STATE, message) else {
            tracing::warn!("dropped a user state larger than clients accept");
            return;
        };
        for user in self.users.values() {
            if Some(user.session) == except || user.hidden.contains(&channel) {
                continue;
            }
            let hides = |id: &u32| user.hidden.contains(id);
            let unseen_actor = message
                .actor
                .is_some_and(|actor| !self.sees_user(user.session, actor));
            let volumes = message
                .listening_volume_adjustment
                .iter()
                .filter_map(|adjustment| adjustment.listening_channel);
            if !unseen_actor
                && !message
                    .listening_channel_add
                    .iter()
                    .chain(&message.listening_channel_remove)
                    .copied()
                    .chain(volumes)
                    .any(|id| hides(&id))
            {
                user.link.send(frame.clone());
                continue;
            }
            let mut seen = message.clone();
            if unseen_actor {
                seen.actor = None;
            }
            seen.listening_channel_add.retain(|id| !hides(id));
            seen.listening_channel_remove.retain(|id| !hides(id));
            seen.listening_volume_adjustment
                .retain(|adjustment| adjustment.listening_channel.is_none_or(|id| !hides(&id)));
            // Nothing left but who it is about and who did it: nothing to say.
            if seen
                != (proto::UserState {
                    session: message.session,
                    actor: seen.actor,
                    ..Default::default()
                })
            {
                user.link.message(kind::USER_STATE, &seen);
            }
        }
    }

    /// A stock `TextMessage` to `to` if it sees the sender (or the sender is
    /// gone), without the channels and people it does not see; `frame` is
    /// `message` encoded.
    pub fn send_text(&self, to: u32, message: &proto::TextMessage, frame: &Bytes) {
        let Some(user) = self.users.get(&to) else {
            return;
        };
        if message
            .actor
            .is_some_and(|actor| self.users.contains_key(&actor) && !self.sees_user(to, actor))
        {
            return;
        }
        let hides = |id: &u32| user.hidden.contains(id);
        let unseen = |session: &u32| !self.sees_user(to, *session);
        if !message.channel_id.iter().chain(&message.tree_id).any(hides)
            && !message.session.iter().any(unseen)
        {
            user.link.send(frame.clone());
            return;
        }
        let mut seen = message.clone();
        seen.channel_id.retain(|id| !hides(id));
        seen.tree_id.retain(|id| !hides(id));
        seen.session.retain(|session| !unseen(session));
        user.link.message(kind::TEXT_MESSAGE, &seen);
    }

    /// Tell everyone who knows `channel` that the user it held is gone
    /// (`message` says why, without an actor the client does not see); the
    /// user may already be removed.
    pub fn broadcast_user_remove(&self, channel: u32, message: &proto::UserRemove) {
        let Some(frame) = encode_control(kind::USER_REMOVE, message) else {
            return;
        };
        for user in self.users.values() {
            if user.hidden.contains(&channel) {
                continue;
            }
            if message
                .actor
                .is_some_and(|actor| !self.sees_user(user.session, actor))
            {
                user.link.message(
                    kind::USER_REMOVE,
                    &proto::UserRemove {
                        actor: None,
                        ..message.clone()
                    },
                );
            } else {
                user.link.send(frame.clone());
            }
        }
    }

    /// A `ChannelState` about `message.channel_id` to everyone who sees that
    /// channel and, when it moves, its new parent, without links to channels
    /// a client does not see. With `extended`, Nuschl connections also get
    /// the channel's extension properties; stock clients never see them.
    pub fn broadcast_channel_state(&self, message: &proto::ChannelState, extended: bool) {
        let Some(channel) = message.channel_id.and_then(|id| self.channels.get(id)) else {
            return;
        };
        let frames = |message: &proto::ChannelState| {
            (
                encode_control(kind::CHANNEL_STATE, message),
                extended
                    .then(|| nuschl::channel_state_frame(message, self.channel_info(channel)))
                    .flatten(),
            )
        };
        let all = frames(message);
        for user in self.users.values() {
            let hides = |id: &u32| user.hidden.contains(id);
            if hides(&channel.id) || message.parent.as_ref().is_some_and(hides) {
                continue;
            }
            let (plain, extension) = if message
                .links
                .iter()
                .chain(&message.links_add)
                .chain(&message.links_remove)
                .any(hides)
            {
                let mut seen = message.clone();
                seen.links.retain(|id| !hides(id));
                seen.links_add.retain(|id| !hides(id));
                seen.links_remove.retain(|id| !hides(id));
                if seen
                    == (proto::ChannelState {
                        channel_id: message.channel_id,
                        ..Default::default()
                    })
                {
                    continue;
                }
                frames(&seen)
            } else {
                all.clone()
            };
            match (extension, &user.extension, plain) {
                (Some(frame), Some(_), _) | (_, _, Some(frame)) => user.link.send(frame),
                _ => {}
            }
        }
    }

    /// Tell everyone who knows channel `id` that it is gone.
    pub fn broadcast_channel_remove(&mut self, id: u32) {
        let Some(frame) = encode_control(
            kind::CHANNEL_REMOVE,
            &proto::ChannelRemove { channel_id: id },
        ) else {
            return;
        };
        for user in self.users.values_mut() {
            if !user.hidden.remove(&id) {
                user.link.send(frame.clone());
            }
        }
    }

    /// A new channel, shown at once to the clients that see it.
    pub fn channel_added(&mut self, id: u32) {
        for user in self.users.values_mut() {
            user.hidden.insert(id);
        }
        self.reveal_to_everyone();
    }

    /// `channels` (parents first) to `viewer`, then their links to channels
    /// it knows. Nuschl connections also get each channel's conversation id
    /// and history properties.
    pub fn send_channels(&self, viewer: u32, channels: &[u32]) {
        let Some(user) = self.users.get(&viewer) else {
            return;
        };
        for channel in channels.iter().filter_map(|id| self.channels.get(*id)) {
            let message = self.channel_state(channel, viewer);
            match user
                .extension
                .is_some()
                .then(|| nuschl::channel_state_frame(&message, self.channel_info(channel)))
                .flatten()
            {
                Some(frame) => user.link.send(frame),
                None => user.link.message(kind::CHANNEL_STATE, &message),
            }
        }
        for channel in channels.iter().filter_map(|id| self.channels.get(*id)) {
            let links: Vec<u32> = channel
                .links
                .iter()
                .copied()
                .filter(|id| !user.hidden.contains(id))
                .collect();
            if !links.is_empty() {
                user.link.message(
                    kind::CHANNEL_STATE,
                    &proto::ChannelState {
                        channel_id: Some(channel.id),
                        links,
                        ..Default::default()
                    },
                );
            }
        }
    }

    /// After `session` left channel `from`: its client learns the channels
    /// its new place shows, and everyone else sees it arrive or leave as
    /// their view of the two channels says.
    pub fn moved(&mut self, session: u32, from: u32) {
        let guarded = self.guarded_channels();
        self.update_view(session, false, &guarded);
        let Some(user) = self.users.get(&session) else {
            return;
        };
        for viewer in self.users.values() {
            if viewer.session == session {
                continue;
            }
            match (
                viewer.hidden.contains(&from),
                viewer.hidden.contains(&user.channel),
            ) {
                (false, true) => viewer.link.message(
                    kind::USER_REMOVE,
                    &proto::UserRemove {
                        session,
                        ..Default::default()
                    },
                ),
                (true, false) => {
                    viewer
                        .link
                        .message(kind::USER_STATE, &Self::user_state_for(viewer, user));
                }
                _ => {}
            }
        }
    }

    /// After the tree changed: every client learns the channels it sees now
    /// (what it no longer sees follows in [`State::finish`]).
    pub fn reveal_to_everyone(&mut self) {
        let guarded = self.guarded_channels();
        let sessions: Vec<u32> = self.users.keys().copied().collect();
        for session in sessions {
            self.update_view(session, false, &guarded);
        }
    }

    /// After a handler: the connections whose authorisation changed see what
    /// they may see now.
    pub fn sync_visibility(&mut self) {
        let pending = &self.authz_pending;
        let sessions: Vec<u32> = if pending.all {
            self.users.keys().copied().collect()
        } else {
            pending.sessions.iter().copied().collect()
        };
        if sessions.is_empty() {
            return;
        }
        let guarded = self.guarded_channels();
        for session in sessions {
            self.update_view(session, true, &guarded);
        }
    }

    /// Send `viewer` what it may see now and did not know (and, with
    /// `conceal`, remove what it no longer may see).
    fn update_view(&mut self, viewer: u32, conceal: bool, guarded: &[u32]) {
        let hidden = self.hidden_channels(viewer, guarded);
        let Some(user) = self.users.get(&viewer) else {
            return;
        };
        let revealed: BTreeSet<u32> = user.hidden.difference(&hidden).copied().collect();
        let concealed: BTreeSet<u32> = if conceal {
            hidden.difference(&user.hidden).copied().collect()
        } else {
            BTreeSet::new()
        };
        if revealed.is_empty() && concealed.is_empty() {
            return;
        }
        if let Some(user) = self.users.get_mut(&viewer) {
            user.hidden.retain(|id| !revealed.contains(id));
        }
        self.reveal(viewer, &revealed);
        self.conceal(viewer, &concealed);
        if let Some(user) = self.users.get_mut(&viewer) {
            user.hidden.extend(concealed);
        }
    }

    /// Channels `viewer` knows now (its `hidden` no longer has them): the
    /// channels, their links, the people in them and the listeners it sees
    /// elsewhere.
    fn reveal(&self, viewer: u32, revealed: &BTreeSet<u32>) {
        let mut order: Vec<u32> = revealed.iter().copied().collect();
        order.sort_by_key(|id| self.channels.level(*id));
        self.send_channels(viewer, &order);
        let Some(user) = self.users.get(&viewer) else {
            return;
        };
        for channel in order.iter().filter_map(|id| self.channels.get(*id)) {
            for other in channel.users.iter().filter_map(|id| self.users.get(id)) {
                if other.session != viewer {
                    user.link
                        .message(kind::USER_STATE, &Self::user_state_for(user, other));
                }
            }
            for other in channel.listeners.iter().filter_map(|id| self.users.get(id)) {
                if !revealed.contains(&other.channel) && !user.hidden.contains(&other.channel) {
                    user.link.message(
                        kind::USER_STATE,
                        &proto::UserState {
                            session: Some(other.session),
                            listening_channel_add: vec![channel.id],
                            ..Default::default()
                        },
                    );
                }
            }
        }
    }

    /// Channels `viewer` no longer sees (its `hidden` does not have them yet):
    /// the people in them and the listeners it sees elsewhere, then the
    /// channels, children first.
    fn conceal(&self, viewer: u32, concealed: &BTreeSet<u32>) {
        let Some(user) = self.users.get(&viewer) else {
            return;
        };
        let mut order: Vec<u32> = concealed.iter().copied().collect();
        order.sort_by_key(|id| Reverse(self.channels.level(*id)));
        for channel in order.iter().filter_map(|id| self.channels.get(*id)) {
            for &session in &channel.users {
                if session != viewer {
                    user.link.message(
                        kind::USER_REMOVE,
                        &proto::UserRemove {
                            session,
                            ..Default::default()
                        },
                    );
                }
            }
            for other in channel.listeners.iter().filter_map(|id| self.users.get(id)) {
                if !concealed.contains(&other.channel) && !user.hidden.contains(&other.channel) {
                    user.link.message(
                        kind::USER_STATE,
                        &proto::UserState {
                            session: Some(other.session),
                            listening_channel_remove: vec![channel.id],
                            ..Default::default()
                        },
                    );
                }
            }
        }
        for channel_id in order {
            user.link
                .message(kind::CHANNEL_REMOVE, &proto::ChannelRemove { channel_id });
        }
    }
}
