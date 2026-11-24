// SPDX-License-Identifier: EUPL-1.2
//! Check access (#341): what someone may do in the edited channel and what
//! decided each permission, evaluated over the parents' rules and the draft,
//! so unsaved changes count.
use super::{Editor, active_editor, group_target, name, note, parents, section, with_help};
use crate::{
    panels::{Panel, channel_name},
    picker::{Choice, Picker},
    state::{Desktop, ServerView},
};
use gpui_kit::{prelude::*, *};
use nuschl_core::{
    access::Who,
    acl::{self, Reason, perm::ROOT_ONLY},
    admin::AclDraft,
    model::permission,
};
use std::collections::HashMap;

pub(super) fn key(who: &Who) -> String {
    match who {
        Who::Everyone => "all".into(),
        Who::Group(group) => format!("group:{group}"),
        Who::Person(id) => format!("person:{id}"),
    }
}

pub(super) fn from_key(key: &str) -> Option<Who> {
    if key == "all" {
        return Some(Who::Everyone);
    }
    if let Some(group) = key.strip_prefix("group:") {
        return Some(Who::Group(group.to_owned()));
    }
    key.strip_prefix("person:")?.parse().ok().map(Who::Person)
}

/// The search field that picks whom to check, for `Editor::new`.
pub(super) fn picker(token: u64, window: &mut Window, cx: &mut Context<Desktop>) -> Picker {
    Picker::new(
        "Check access for",
        "Everyone, a group or a person",
        |this| match this.panel.as_mut() {
            Some(Panel::Acl { editor, .. }) => Some(&mut editor.check_picker),
            _ => None,
        },
        move |this, key, _, cx| {
            if let Some(editor) = active_editor(this, token) {
                editor.check = from_key(key);
                editor.check_reveal.set(true);
            }
            cx.notify();
        },
        window,
        cx,
    )
}

fn why(reason: Reason, allowed: bool, tree: &HashMap<u32, acl::Node>, view: &ServerView) -> String {
    match reason {
        Reason::Default if allowed => "Mumble's default".into(),
        Reason::Default => "Not allowed by default".into(),
        Reason::Rule { channel, index } => {
            let target = tree
                .get(&channel)
                .and_then(|node| node.acls.get(index))
                .map_or_else(String::new, |entry| match entry.user_id {
                    Some(id) => name(view, id),
                    None => group_target(&entry.group),
                });
            format!("{target} rule in {}", channel_name(view, channel))
        }
        Reason::Write => "Comes with Write ACL".into(),
        Reason::Superuser => "SuperUser may do everything except talk".into(),
        Reason::Unreachable { channel } => {
            format!("Cannot pass {}", channel_name(view, channel))
        }
    }
}

/// Everyone, Registered users and the channel's groups first, then every
/// account the client knows; `except` are left out.
pub(super) fn choices(view: &ServerView, draft: &AclDraft, except: &[Who]) -> Vec<Choice> {
    // Everyone and the groups first, then every account the client knows.
    let mut accounts: Vec<(u32, String)> = view
        .registered_users
        .iter()
        .flatten()
        .filter_map(|account| Some((account.user_id, account.name.clone()?)))
        .chain(
            view.users
                .values()
                .filter_map(|user| Some((user.user_id?, user.name.clone()))),
        )
        .collect();
    accounts.sort_by(|a, b| {
        a.1.to_lowercase()
            .cmp(&b.1.to_lowercase())
            .then(a.0.cmp(&b.0))
    });
    accounts.dedup_by_key(|account| account.0);
    let online = |id: u32| view.users.values().any(|user| user.user_id == Some(id));
    [
        (Who::Everyone, "Everyone".to_owned(), "Without an account"),
        (
            Who::Group("auth".into()),
            group_target("auth"),
            "Built-in group",
        ),
    ]
    .into_iter()
    .chain(draft.groups().iter().map(|row| {
        (
            Who::Group(row.value.name.clone()),
            format!("@{}", row.value.name),
            "Group",
        )
    }))
    .chain(accounts.into_iter().map(|(id, account)| {
        (
            Who::Person(id),
            account,
            if online(id) { "Online" } else { "Account" },
        )
    }))
    .filter(|(who, _, _)| !except.contains(who))
    .map(|(who, label, kind)| Choice {
        key: key(&who),
        label,
        kind,
    })
    .collect()
}

pub(super) fn render(view: &ServerView, editor: &Editor, cx: &mut Context<Desktop>) -> AnyElement {
    let token = editor.token;
    let Some(draft) = editor.state.draft() else {
        return div().into_any_element();
    };
    let channel = draft.channel();
    let choices = choices(view, draft, &[]);
    let section = section(
        "acl-check-title",
        "Check access",
        "Pick someone to see what they may do here and why: their account, groups and certificate, in this channel.",
    )
    .child(
        editor
            .check_picker
            .render(&format!("acl-check-{token}"), &choices, cx),
    );
    let Some(who) = &editor.check else {
        return section.into_any_element();
    };
    let parents = &editor.parents;
    if let Some(error) = &parents.error {
        return section
            .child(note("acl-check-error", error.clone()))
            .into_any_element();
    }
    if !parents.done {
        let reading = parents
            .reading()
            .map_or_else(String::new, |parent| channel_name(view, parent));
        return section
            .child(note(
                "acl-check-reading",
                format!("Reading the rules of {reading}…"),
            ))
            .into_any_element();
    }
    let tree = parents::tree(&parents.acls, &draft.snapshot(), &[], who);
    let hash = match who {
        Who::Person(id) => view
            .users
            .values()
            .find(|user| user.user_id == Some(*id))
            .and_then(|user| user.certificate_hash.as_deref()),
        _ => None,
    };
    let (granted, reasons) = acl::explain(&tree, &parents::subject(who, channel, hash), channel);
    let mut results = div().relative().flex().flex_col().gap_2();
    if editor.check_reveal.replace(false) {
        // The results of a new pick come into view once.
        results = results.child(
            canvas(
                |bounds, window, _| window.request_autoscroll(bounds),
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        );
    }
    if editor.dirty {
        results = results.child(note("acl-check-draft", "Includes unsaved changes"));
    }
    for &(bit, label) in permission::NAMED {
        if channel != 0 && bit & ROOT_ONLY != 0 {
            continue;
        }
        let allowed = granted & bit != 0;
        let reason = reasons[bit.trailing_zeros() as usize];
        results = results.child(with_help(
            format!("acl-check-row-help-{token}-{bit}"),
            permission::description(bit),
            note(
                SharedString::from(format!("acl-check-row-{token}-{bit}")),
                format!(
                    "{label}: {} · {}",
                    if allowed { "Allowed" } else { "Not allowed" },
                    why(reason, allowed, &tree, view)
                ),
            ),
        ));
    }
    section.child(results).into_any_element()
}
