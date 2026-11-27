// SPDX-License-Identifier: EUPL-1.2
//! Integration surfaces ([`nuschl_core::integration`]): the cards a bot
//! publishes over plugin data. The client keeps the newest revision per
//! sender and surface, drops it when the sender's presence goes, and sends a
//! press back to the exact presence that drew it.
//!
//! This is not an executable UI: the bytes decode into a closed set of nodes
//! and the frontend draws them with native widgets. The reserved data IDs
//! never reach [`Client::plugin_data`](crate::Client::plugin_data).
use super::{Client, plugin::ReceivedPluginData};
use crate::server::{ServerState, Status};
use nuschl_core::integration::{Action, Icon, KIT_ACTION_DATA_ID, Node, Surface};
use nuschl_protocol::plugin::{Payload, Refusal};

/// Surfaces kept per server; the oldest is dropped beyond this.
pub const MAX_SURFACES: usize = 64;

/// How long after pressing a card's web control its answer may arrive.
const WEB_REQUEST_MS: u64 = 30_000;

/// One surface as its sender's presence announced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceivedSurface {
    pub sender: u32,
    pub sender_instance: u64,
    /// The sender's registered account, when the server gave one; a stable
    /// key for a UI's dismissal across sessions.
    pub sender_user_id: Option<u32>,
    pub surface: Surface,
    /// The client's monotonic clock when this revision arrived, for progress
    /// interpolation ([`Client::now_ms`]).
    pub received_ms: u64,
}

impl ReceivedSurface {
    fn live(&self, server: &ServerState) -> bool {
        server
            .users
            .get(&self.sender)
            .is_some_and(|user| user.instance == self.sender_instance)
    }
}

impl Client {
    /// The surfaces a current sender announced on server `index`, newest
    /// revision per `(sender, surface id)`.
    pub fn surfaces(&self, index: usize) -> Vec<&ReceivedSurface> {
        let Some(server) = self.servers.get(index) else {
            return Vec::new();
        };
        if server.status != Status::Connected {
            return Vec::new();
        }
        let mut surfaces: Vec<&ReceivedSurface> = server
            .surfaces
            .iter()
            .filter(|surface| surface.live(server))
            .collect();
        surfaces.sort_by_key(|surface| (surface.sender, surface.surface.id));
        surfaces
    }

    /// Presses `control` on the surface `id` that `sender` announced at
    /// `revision`. The sender is part of the identity: two bots may publish the
    /// same surface id at the same revision. Refused when the surface was
    /// replaced, its sender left, or the send is not admitted; the bot never
    /// learns of a press for another revision.
    pub fn press_surface(
        &mut self,
        index: usize,
        sender: u32,
        sender_instance: u64,
        id: u32,
        revision: u64,
        control: u8,
    ) -> Result<(), Refusal> {
        let now = self.now_ms();
        let server = self.servers.get_mut(index).ok_or(Refusal::NotConnected)?;
        if server.status != Status::Connected {
            return Err(Refusal::NotConnected);
        }
        let web = server
            .surfaces
            .iter()
            .find(|entry| {
                entry.sender == sender
                    && entry.sender_instance == sender_instance
                    && entry.surface.id == id
                    && entry.surface.revision == revision
            })
            .filter(|entry| entry.live(server))
            .map(|entry| {
                entry.surface.nodes.iter().any(|node| {
                    matches!(
                        node,
                        Node::Buttons(buttons)
                            if buttons
                                .iter()
                                .any(|button| button.id == control && button.icon == Icon::Web)
                    )
                })
            })
            .ok_or(Refusal::NotConnected)?;
        if web {
            // The bot answers a web press with a private link, and only a
            // press it just received may open one.
            server.web_request = Some((sender, sender_instance, now + WEB_REQUEST_MS));
        }
        let key = server.kit_key;
        server.kit_key = server.kit_key.wrapping_add(1);
        let action = Action {
            surface: id,
            revision,
            control,
            key,
        };
        let payload = Payload::new(KIT_ACTION_DATA_ID.to_owned(), action.encode())
            .map_err(Refusal::Payload)?;
        let request = self.send_payload(index, &[(sender, sender_instance)], payload)?;
        self.plugin_data.kit_request(request);
        Ok(())
    }

    /// A page link a bot sent in answer to one of its card's web controls. The
    /// client hands it to the platform to open; it opens nothing without a
    /// press that just asked this sender for it.
    pub(super) fn kit_link(&mut self, index: usize, data: &ReceivedPluginData) {
        let Ok(url) = std::str::from_utf8(data.payload.data()) else {
            return;
        };
        if nuschl_core::integration::link(url).is_err() {
            return;
        }
        let now = self.now_ms();
        let Some(server) = self.servers.get_mut(index) else {
            return;
        };
        let asked = server
            .web_request
            .is_some_and(|(sender, instance, expires)| {
                sender == data.sender && instance == data.sender_instance && now <= expires
            });
        if !asked {
            return;
        }
        server.web_request = None;
        self.updates.push(crate::Update::Link(url.to_owned()));
    }

    /// A surface from the server's current connection.
    pub(super) fn kit_surface(&mut self, index: usize, data: &ReceivedPluginData) {
        let Ok(surface) = Surface::decode(data.payload.data()) else {
            return;
        };
        let server = &mut self.servers[index];
        let own = server.own_session;
        if server
            .users
            .get(&data.sender)
            .is_none_or(|user| user.instance != data.sender_instance || Some(user.session) == own)
        {
            return;
        }
        // A new presence owns no revision of its predecessor's surface.
        server.surfaces.retain(|entry| {
            entry.sender != data.sender || entry.sender_instance == data.sender_instance
        });
        if surface.nodes.is_empty() {
            // An empty surface withdraws the card, and only one newer than the
            // revision the sender already published: a late or duplicated
            // withdrawal leaves the current card in place.
            server.surfaces.retain(|entry| {
                entry.sender != data.sender
                    || entry.sender_instance != data.sender_instance
                    || entry.surface.id != surface.id
                    || entry.surface.revision >= surface.revision
            });
            return;
        }
        let received_ms = self.now_ms;
        if let Some(existing) = server.surfaces.iter_mut().find(|entry| {
            entry.sender == data.sender
                && entry.sender_instance == data.sender_instance
                && entry.surface.id == surface.id
        }) {
            if surface.revision <= existing.surface.revision {
                return;
            }
            existing.surface = surface;
            existing.received_ms = received_ms;
            existing.sender_user_id = data.sender_user_id;
            return;
        }
        if server.surfaces.len() >= MAX_SURFACES {
            // Drop the oldest announcement, not the newest card.
            if let Some((oldest, _)) = server
                .surfaces
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.received_ms)
            {
                server.surfaces.remove(oldest);
            }
        }
        server.surfaces.push(ReceivedSurface {
            sender: data.sender,
            sender_instance: data.sender_instance,
            sender_user_id: data.sender_user_id,
            surface,
            received_ms,
        });
    }

    /// The client's monotonic clock, as last set by the platform.
    pub fn now_ms(&self) -> u64 {
        self.now_ms
    }
}
