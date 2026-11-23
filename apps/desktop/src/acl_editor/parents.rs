// SPDX-License-Identifier: EUPL-1.2
//! The parent channels' own rules, read once per editor with one scoped query
//! at a time, so permissions are evaluated over the whole channel path. Once
//! the draft changes, the subchannels' rules follow, for the effect preview.
use super::{MEMBER, active_editor, group_target};
use crate::{
    menus::{ActionOrigin, MenuTarget},
    panels::Panel,
    state::{Desktop, ServerView},
};
use nuschl_client::platform::request_id;
use nuschl_core::{
    access::{self, Who},
    acl,
    admin::{self, AclDraft},
    model::{Acl, TextTarget},
};
use nuschl_session::connection::Command;
use std::collections::HashMap;

/// Subchannels the effect preview reads, at most.
pub const MAX_BELOW: usize = 64;
/// Groups the effect preview compares, at most.
const MAX_GROUPS: usize = 32;

#[derive(Default)]
pub struct Parents {
    /// Root first.
    pub acls: Vec<(u32, Acl)>,
    /// The subchannels to read, breadth first, with their parents; set once
    /// the draft changes. One more than `MAX_BELOW` means there are more.
    wanted: Option<Vec<(u32, u32)>>,
    /// The subchannels read so far: channel, parent and rules, if readable.
    below: Vec<(u32, u32, Option<Acl>)>,
    /// The channel being read, and the request.
    reading: Option<(u32, u64)>,
    pub error: Option<String>,
    pub done: bool,
}

impl Parents {
    pub fn reading(&self) -> Option<u32> {
        self.reading.map(|(channel, _)| channel)
    }

    /// Whether subchannels are still being read, and whether there are more
    /// than the preview reads.
    pub fn below_status(&self) -> (bool, bool) {
        let wanted = self.wanted.as_ref().map_or(0, Vec::len);
        (
            self.below.len() < wanted.min(MAX_BELOW) && self.error.is_none(),
            wanted > MAX_BELOW,
        )
    }
}

/// The channels below `channel`, breadth first, with their parents; at most
/// one more than `MAX_BELOW`.
fn descendants(view: &ServerView, channel: u32) -> Vec<(u32, u32)> {
    let mut found = Vec::new();
    let mut queue = std::collections::VecDeque::from([channel]);
    while let Some(parent) = queue.pop_front() {
        for child in &view.channels {
            if found.len() > MAX_BELOW {
                return found;
            }
            if matches!(child.target, TextTarget::Channel(_)) && child.parent == Some(parent) {
                found.push((child.id, parent));
                queue.push_back(child.id);
            }
        }
    }
    found
}

/// The channels above `channel`, root first.
fn chain(view: &ServerView, channel: u32) -> Option<Vec<u32>> {
    let mut chain = Vec::new();
    let mut current = channel;
    while current != 0 && chain.len() <= 64 {
        current = view
            .channels
            .iter()
            .find(|view| view.target == TextTarget::Channel(current))?
            .parent?;
        chain.push(current);
    }
    chain.reverse();
    Some(chain)
}

/// Reads the next parent's rules, or marks them all read; then, once the
/// draft changes, the next subchannel's.
pub(super) fn next(this: &mut Desktop, token: u64) {
    let Some(Panel::Acl {
        origin,
        channel,
        editor,
        ..
    }) = this.panel.as_ref()
    else {
        return;
    };
    let Some(index) = origin.live_index(this) else {
        return;
    };
    let parents = &editor.parents;
    let read = parents
        .wanted
        .as_ref()
        .is_some_and(|wanted| parents.below.len() >= wanted.len().min(MAX_BELOW));
    if editor.token != token
        || parents.reading.is_some()
        || (parents.done && (read || !editor.dirty))
        || parents.error.is_some()
    {
        return;
    }
    let view = &this.client.servers[index];
    let found = (parents.done && parents.wanted.is_none()).then(|| descendants(view, *channel));
    let wanted = match found.as_ref().or(parents.wanted.as_ref()) {
        Some(below) => Some(
            below
                .get(parents.below.len())
                .filter(|_| parents.below.len() < MAX_BELOW)
                .map(|(child, _)| *child),
        ),
        None => chain(view, *channel).map(|chain| chain.get(parents.acls.len()).copied()),
    };
    let sent = match wanted {
        None => Err("This channel's place in the tree is unknown.".to_owned()),
        Some(None) => Ok(None),
        Some(Some(parent)) => {
            let request = request_id();
            let server = view.id.clone();
            ActionOrigin::capture(view, MenuTarget::Channel(parent))
                .with_scope(this, |this| {
                    this.driver.command(
                        &server,
                        Command::QueryAclDraft {
                            channel: parent,
                            request,
                        },
                    )
                })
                .map(|()| Some((parent, request)))
        }
    };
    if let Some(editor) = active_editor(this, token) {
        if found.is_some() {
            editor.parents.wanted = found;
        }
        match sent {
            Ok(Some(reading)) => editor.parents.reading = Some(reading),
            Ok(None) => {
                editor.parents.done = true;
                super::refresh_preview(editor);
            }
            Err(error) => editor.parents.error = Some(error),
        }
    }
}

/// Takes the reply to the parent or subchannel being read.
pub(super) fn sync(this: &mut Desktop, token: u64, index: usize) {
    let Some((parent, request)) =
        active_editor(this, token).and_then(|editor| editor.parents.reading)
    else {
        return;
    };
    let Some(reply) = this.client.servers[index]
        .acl_reply
        .as_ref()
        .filter(|reply| reply.request == request)
        .cloned()
    else {
        return;
    };
    let Some(editor) = active_editor(this, token) else {
        return;
    };
    editor.parents.reading = None;
    if editor.parents.done {
        let above = editor
            .parents
            .wanted
            .iter()
            .flatten()
            .find(|(child, _)| *child == parent)
            .map(|(_, above)| *above);
        if let Some(above) = above {
            editor
                .parents
                .below
                .push((parent, above, reply.result.ok()));
        }
        super::refresh_preview(editor);
        next(this, token);
        return;
    }
    match reply.result {
        Ok(acl) => {
            editor.parents.acls.push((parent, acl));
            next(this, token);
        }
        Err(error) => editor.parents.error = Some(error),
    }
}

/// The parents' rules, `channel`'s (the draft) and the subchannels' `below`
/// as the permission model reads them; a member of `who`'s group is added
/// wherever it is defined.
pub fn tree(
    parents: &[(u32, Acl)],
    channel: &Acl,
    below: &[(u32, u32, Option<Acl>)],
    who: &Who,
) -> HashMap<u32, acl::Node> {
    let mut tree = HashMap::new();
    let mut parent = None;
    for (id, acl) in parents {
        tree.insert(*id, admin::node(acl, parent));
        parent = Some(*id);
    }
    tree.insert(channel.channel, admin::node(channel, parent));
    for (id, parent, acl) in below {
        if let Some(acl) = acl {
            tree.insert(*id, admin::node(acl, Some(*parent)));
        }
    }
    if let Who::Group(group) = who {
        for node in tree.values_mut() {
            if let Some(defined) = node.groups.get_mut(group) {
                defined.add.insert(MEMBER);
            }
        }
        if let Some(node) = tree.get_mut(&channel.channel) {
            node.groups
                .entry(group.clone())
                .or_insert_with(acl::Group::new)
                .add
                .insert(MEMBER);
        }
    }
    tree
}

/// Who the permission model sees for a row: no account, a group member, or
/// the account, in `channel`.
pub fn subject<'a>(who: &Who, channel: u32, hash: Option<&'a str>) -> acl::Subject<'a> {
    acl::Subject {
        user_id: match who {
            Who::Everyone => None,
            Who::Group(_) => Some(MEMBER),
            Who::Person(id) => Some(*id),
        },
        channel,
        hash,
        tokens: &[],
        strong: false,
    }
}

/// How the draft changes what Everyone, registered users and the groups
/// visible here may do, here and in the subchannels read so far: a few lines.
pub fn preview(parents: &Parents, draft: &AclDraft) -> Vec<String> {
    if !parents.done || !draft.dirty() {
        return Vec::new();
    }
    let (before, after) = (draft.baseline(), draft.snapshot());
    let mut rows = vec![Who::Everyone, Who::Group("auth".into())];
    for row in draft.groups().iter().take(MAX_GROUPS) {
        let group = Who::Group(row.value.name.clone());
        if !rows.contains(&group) {
            rows.push(group);
        }
    }
    let trees: Vec<_> = rows
        .iter()
        .map(|who| {
            (
                tree(&parents.acls, before, &parents.below, who),
                tree(&parents.acls, &after, &parents.below, who),
            )
        })
        .collect();
    // Subchannels whose rules and parents' rules were all read.
    let mut below: Vec<u32> = Vec::new();
    for (id, parent, acl) in &parents.below {
        if acl.is_some() && (*parent == after.channel || below.contains(parent)) {
            below.push(*id);
        }
    }
    let effects = access::effects(&rows, after.channel, &below, |who, channel| {
        let index = rows.iter().position(|row| row == who).unwrap_or_default();
        let (before, after) = &trees[index];
        let subject = subject(who, channel, None);
        (
            acl::effective_permissions(before, &subject, channel),
            acl::effective_permissions(after, &subject, channel),
        )
    });
    let label = |who: &Who| match who {
        Who::Everyone => "Everyone".to_owned(),
        Who::Group(group) => group_target(group),
        Who::Person(id) => format!("#{id}"),
    };
    access::summary(&effects, label, 3)
}
