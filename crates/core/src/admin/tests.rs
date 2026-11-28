// SPDX-License-Identifier: EUPL-1.2
use super::*;

fn acl() -> Acl {
    Acl {
        channel: 7,
        inherit_acls: true,
        entries: vec![
            AclEntry {
                apply_here: true,
                apply_subs: true,
                inherited: true,
                user_id: None,
                group: Some("admin".into()),
                grant: 1,
                deny: 0,
            },
            AclEntry {
                apply_here: false,
                apply_subs: true,
                inherited: false,
                user_id: Some(9876),
                group: None,
                grant: 0x8000_0000,
                deny: 0x4000_0000,
            },
        ],
        groups: vec![AclGroup {
            name: "admin".into(),
            inherited: true,
            inherit: true,
            inheritable: true,
            add: vec![],
            remove: vec![],
            inherited_members: vec![42],
        }],
    }
}

#[test]
fn a_node_holds_the_channels_own_rules_and_groups() {
    let mut acl = acl();
    acl.groups.push(AclGroup {
        name: "helpers".into(),
        inherited: false,
        inherit: false,
        inheritable: true,
        add: vec![5],
        remove: vec![6],
        inherited_members: vec![],
    });
    let node = node(&acl, Some(3));
    assert_eq!(node.parent, Some(3));
    assert!(node.inherit_acl);
    assert_eq!(node.acls.len(), 1);
    assert_eq!(node.acls[0].user_id, Some(9876));
    assert_eq!(node.groups.keys().collect::<Vec<_>>(), ["helpers"]);
    let helpers = &node.groups["helpers"];
    assert!(!helpers.inherit && helpers.add.contains(&5) && helpers.remove.contains(&6));
}

#[test]
fn a_table_replaces_the_own_rules_and_keeps_the_password() {
    use crate::access::Who;
    let mut draft = AclDraft::new(acl());
    assert_eq!(draft.table(), None, "a subchannel-only rule is advanced");
    draft.set_password("door").unwrap();
    let mut table = crate::access::Table::default();
    table
        .row(Who::Group("admin".into()))
        .set(4, crate::access::Cell::Allow);
    draft.set_table(&table).unwrap();
    assert_eq!(draft.table(), Some(table));
    assert_eq!(draft.password(), Some("door"));
    let entries: Vec<_> = draft.entries().iter().map(|row| &row.value).collect();
    assert!(entries[0].inherited);
    assert_eq!(entries[1].group.as_deref(), Some("admin"));
    assert_eq!(entries[3].group.as_deref(), Some("#door"));
}

#[test]
fn a_flat_node_applies_inherited_rules_here() {
    use crate::acl::{Subject, effective_permissions};
    let acl = acl();
    let mut tree = std::collections::HashMap::new();
    tree.insert(7, flat_node(&acl));
    let admin = |user_id| Subject {
        user_id: Some(user_id),
        channel: 7,
        hash: None,
        tokens: &[],
        strong: false,
    };
    // The inherited @admin rule grants Write to the inherited member 42.
    assert_ne!(
        effective_permissions(&tree, &admin(42), 7) & permission::WRITE,
        0
    );
    assert_eq!(
        effective_permissions(&tree, &admin(43), 7) & permission::WRITE,
        0
    );
    let mut detached = acl;
    detached.inherit_acls = false;
    tree.insert(7, flat_node(&detached));
    assert_eq!(
        effective_permissions(&tree, &admin(42), 7) & permission::WRITE,
        0
    );
}

#[test]
fn a_password_is_stored_as_mumble_stores_it() {
    let mut draft = AclDraft::new(acl());
    assert_eq!(draft.password(), None);
    draft.set_password("hunter2").unwrap();
    let rules: Vec<_> = draft.entries()[2..]
        .iter()
        .map(|row| row.value.clone())
        .collect();
    let rule = |group: &str, grant, deny| AclEntry {
        apply_here: true,
        apply_subs: false,
        inherited: false,
        user_id: None,
        group: Some(group.into()),
        grant,
        deny,
    };
    assert_eq!(
        rules,
        [rule("all", 0, PASSWORD), rule("#hunter2", PASSWORD, 0)]
    );
    assert_eq!(draft.password(), Some("hunter2"));
    draft.set_password("changed").unwrap();
    assert_eq!(
        (draft.password(), draft.entries().len()),
        (Some("changed"), 4)
    );
    draft.set_password("").unwrap();
    assert_eq!((draft.password(), draft.entries().len()), (None, 2));
    // Mumble before 1.4 left out Traverse; an inherited rule is no password here.
    let mut old = acl();
    old.entries
        .push(rule("all", 0, PASSWORD & !permission::TRAVERSE));
    old.entries
        .push(rule("#old", PASSWORD & !permission::TRAVERSE, 0));
    old.entries.push(AclEntry {
        inherited: true,
        ..rule("#parent", PASSWORD, 0)
    });
    let mut draft = AclDraft::new(old);
    assert_eq!(draft.password(), Some("old"));
    draft.set_password("").unwrap();
    assert_eq!(draft.entries().len(), 3);
}

/// A channel as Murmur creates it under the root: its own `admin` group
/// holds the creator, account 7.
fn new_channel() -> Acl {
    Acl {
        channel: 5,
        inherit_acls: true,
        entries: Vec::new(),
        groups: vec![AclGroup {
            name: "admin".into(),
            inherited: false,
            inherit: true,
            inheritable: true,
            add: vec![7],
            remove: vec![],
            inherited_members: vec![],
        }],
    }
}

fn rule(group: &str, grant: u32, deny: u32, subs: bool) -> AclEntry {
    AclEntry {
        apply_here: true,
        apply_subs: subs,
        inherited: false,
        user_id: None,
        group: Some(group.into()),
        grant,
        deny,
    }
}

#[test]
fn channel_types_compile_to_mumble_rules_and_read_back() {
    use crate::access::Kind;
    use permission::*;
    let access = TRAVERSE | ENTER | LISTEN | SPEAK | WHISPER | TEXT_MESSAGE;
    let expected = |kind| match kind {
        Kind::Open => vec![],
        Kind::Members => vec![
            rule("all", 0, access & !TRAVERSE, true),
            rule("auth", access, 0, true),
        ],
        Kind::Password => vec![
            rule("all", 0, PASSWORD, false),
            rule("#door", PASSWORD, 0, false),
        ],
        Kind::Announcements => vec![
            rule("all", 0, SPEAK | WHISPER, true),
            rule("admin", SPEAK | WHISPER, 0, true),
        ],
        Kind::Private => vec![rule("all", 0, access, true), rule("admin", access, 0, true)],
    };
    let mut switched = AclDraft::new(new_channel());
    assert_eq!(switched.kind(), Some(Kind::Open));
    for kind in Kind::ALL {
        let mut draft = AclDraft::new(new_channel());
        draft.set_kind(kind, "door").unwrap();
        assert_eq!(draft.snapshot().entries, expected(kind), "{kind:?}");
        assert_eq!(draft.kind(), Some(kind));
        // From any other type, too.
        switched.set_kind(kind, "door").unwrap();
        assert_eq!(switched.snapshot().entries, expected(kind), "{kind:?}");
    }
    // Password keeps the current password when none is typed.
    switched.set_kind(Kind::Password, "door").unwrap();
    switched.set_kind(Kind::Password, "").unwrap();
    assert_eq!(switched.password(), Some("door"));
    switched.set_kind(Kind::Open, "").unwrap();
    assert_eq!(
        AclDraft::new(new_channel()).set_kind(Kind::Password, ""),
        Err("Type a password first.")
    );
    // A create that carries the rules starts with the same ones.
    for kind in Kind::ALL {
        assert_eq!(
            super::new_channel_rules(kind, "door"),
            Ok(expected(kind)),
            "{kind:?}"
        );
    }
    assert_eq!(
        super::new_channel_rules(Kind::Password, ""),
        Err("Type a password first.")
    );
}

#[test]
fn channel_types_keep_other_permissions_and_read_other_setups_as_custom() {
    use crate::access::{Cell, Kind, Who};
    let mut draft = AclDraft::new(new_channel());
    let mut table = crate::access::Table::default();
    table.row(Who::Group("admin".into())).set(4, Cell::Allow);
    table.row(Who::Person(3)).set(1, Cell::Allow);
    draft.set_table(&table).unwrap();
    assert_eq!(draft.kind(), None, "a person row in Speak is custom");
    draft.set_kind(Kind::Private, "").unwrap();
    let table = draft.table().unwrap();
    let admin = table
        .rows
        .iter()
        .find(|(who, _)| *who == Who::Group("admin".into()));
    let admin = admin.unwrap().1;
    assert_eq!((admin.cell(0), admin.cell(4)), (Cell::Allow, Cell::Allow));
    assert_eq!(draft.kind(), Some(Kind::Private));
    // Rules the table cannot show stay as they are.
    let mut advanced = new_channel();
    advanced
        .entries
        .push(rule("~sub", permission::SPEAK, 0, true));
    let mut draft = AclDraft::new(advanced);
    assert_eq!(draft.kind(), None);
    assert!(draft.set_kind(Kind::Open, "").is_err());
    assert_eq!(draft.entries().len(), 1);
}

#[test]
fn channel_types_give_everyone_members_and_moderators_their_access() {
    use crate::access::Kind;
    use crate::acl::{Node, Subject, effective_permissions, token_digest};
    use permission::*;
    // Murmur's default root: @admin (account 7) may write, registered users
    // may make temporary channels.
    let mut root = new_channel();
    root.channel = 0;
    root.entries = vec![
        rule("admin", WRITE, 0, true),
        rule("auth", MAKE_TEMP_CHANNEL, 0, true),
        rule("all", SELF_REGISTER, 0, false),
    ];
    let root = node(&root, None);
    fn who(user_id: Option<u32>, tokens: &[[u8; 32]]) -> Subject<'_> {
        Subject {
            user_id,
            channel: 0,
            hash: None,
            tokens,
            strong: false,
        }
    }
    let tokens = [token_digest("door")];
    let everyone = who(None, &[]);
    let member = who(Some(3), &[]);
    let moderator = who(Some(7), &[]);
    let holder = who(None, &tokens);
    // What each may do in the channel (5) and its subchannel (6).
    let check = |kind: Kind, subject: &Subject<'_>| {
        let mut draft = AclDraft::new(new_channel());
        draft.set_kind(kind, "door").unwrap();
        let sub = Node {
            parent: Some(5),
            inherit_acl: true,
            ..Default::default()
        };
        let tree = std::collections::HashMap::from([
            (0, root.clone()),
            (5, node(&draft.snapshot(), Some(0))),
            (6, sub),
        ]);
        let wanted = ENTER | SPEAK | TEXT_MESSAGE;
        (
            effective_permissions(&tree, subject, 5) & wanted,
            effective_permissions(&tree, subject, 6) & ENTER,
        )
    };
    let all = ENTER | SPEAK | TEXT_MESSAGE;
    let listen = ENTER | TEXT_MESSAGE;
    for (kind, subject, here, below) in [
        (Kind::Open, &everyone, all, ENTER),
        (Kind::Open, &member, all, ENTER),
        (Kind::Members, &everyone, 0, 0),
        (Kind::Members, &member, all, ENTER),
        (Kind::Members, &moderator, all, ENTER),
        // Mumble's password rules apply here only; Traverse still guards
        // the subchannels. Write lets a moderator in, without Speak.
        (Kind::Password, &everyone, 0, 0),
        (Kind::Password, &member, 0, 0),
        (Kind::Password, &moderator, listen, ENTER),
        (Kind::Password, &holder, all, ENTER),
        (Kind::Announcements, &everyone, listen, ENTER),
        (Kind::Announcements, &member, listen, ENTER),
        (Kind::Announcements, &moderator, all, ENTER),
        (Kind::Private, &everyone, 0, 0),
        (Kind::Private, &member, 0, 0),
        (Kind::Private, &moderator, all, ENTER),
    ] {
        assert_eq!(
            check(kind, subject),
            (here, below),
            "{kind:?} for {:?}",
            subject.user_id
        );
    }
}

#[test]
fn a_new_channel_names_its_creator() {
    let acl = new_channel();
    assert!(created_by(&acl, Some(7), None));
    assert!(!created_by(&acl, Some(8), None));
    assert!(!created_by(&acl, None, Some("abc")));
    // A creator without Write gets a rule of their own.
    let mut acl = new_channel();
    acl.groups.clear();
    acl.entries.push(AclEntry {
        apply_subs: false,
        ..rule("$abc", permission::WRITE | permission::TRAVERSE, 0, false)
    });
    assert!(created_by(&acl, None, Some("abc")));
    assert!(!created_by(&acl, None, Some("abd")));
    acl.entries[0].group = None;
    acl.entries[0].user_id = Some(8);
    assert!(created_by(&acl, Some(8), None));
    acl.entries[0].inherited = true;
    assert!(!created_by(&acl, Some(8), None));
}

#[test]
fn changes_are_private_and_preserve_unedited_bits_and_fields() {
    let remote = acl();
    let mut draft = AclDraft::new(remote.clone());
    let id = draft.entries()[1].id;
    draft.entry_mut(id).unwrap().grant |= 8;
    assert!(draft.dirty());
    assert!(draft.baseline_matches(&remote));
    assert_eq!(remote, acl());
    let updated = draft.snapshot();
    assert_eq!(updated.entries[0], remote.entries[0]);
    assert_eq!(updated.groups, remote.groups);
    assert_eq!(updated.entries[1].grant, 0x8000_0008);
    assert_eq!(updated.entries[1].deny, 0x4000_0000);
    assert!(!updated.entries[1].apply_here);
    assert_eq!(updated.entries[1].user_id, Some(9876));
}

#[test]
fn row_actions_cannot_retarget_after_remove_reorder_or_reload() {
    let mut draft = AclDraft::new(acl());
    let inherited = draft.entries()[0].id;
    assert!(draft.entry_mut(inherited).is_none());
    assert!(!draft.remove_entry(inherited));
    let old = draft.entries()[1].id;
    let new = draft.add_entry().unwrap();
    assert!(draft.move_entry(new, false));
    assert_eq!(draft.entries()[1].id, new);
    assert_eq!(draft.entry_mut(old).unwrap().user_id, Some(9876));
    assert!(draft.remove_entry(old));
    assert!(draft.entry_mut(old).is_none());
    draft.reload(acl());
    assert!(draft.entry_mut(new).is_none());
    assert!(draft.entry_mut(old).is_none());
    assert!(!draft.dirty());
}

#[test]
fn inherited_group_overrides_are_editable_and_reset_locally() {
    let mut draft = AclDraft::new(acl());
    let id = draft.groups()[0].id;
    assert!(!group_has_local_definition(&draft.groups()[0].value));
    let group = draft.group_mut(id).unwrap();
    group.add.push(9000); // Registered account ID, independent of presence.
    group.remove.push(42);
    group.inheritable = false;
    assert!(group_has_local_definition(&draft.groups()[0].value));
    assert!(draft.remove_group(id));
    let group = &draft.groups()[0].value;
    assert_eq!(group.inherited_members, vec![42]);
    assert!(!group_has_local_definition(group));
    assert!(!draft.dirty());
}

#[test]
fn named_group_creation_and_baseline_conflicts_are_explicit() {
    let mut draft = AclDraft::new(acl());
    assert!(draft.add_group("ADMIN").is_err());
    assert!(draft.add_group("").is_err());
    let id = draft.add_group("  Music  ").unwrap();
    assert_eq!(draft.group_mut(id).unwrap().name, "music");
    assert!(draft.remove_group(id));
    assert!(!draft.dirty());
    let mut changed = acl();
    changed.groups[0].inherited_members.push(55);
    assert!(!draft.baseline_matches(&changed));
    assert!(draft.baseline_matches(&acl()));
}
fn loaded() -> AclEditorState {
    let mut state = AclEditorState::new(7);
    assert!(state.begin(1, DraftRequest::Load));
    state.receive(&AclReply {
        request: 1,
        result: Ok(acl()),
    });
    state
}

#[test]
fn dirty_drafts_ignore_unsolicited_late_and_replaced_query_replies() {
    let mut state = loaded();
    let id = state.draft().unwrap().entries()[1].id;
    state.draft_mut().unwrap().entry_mut(id).unwrap().grant |= 8;
    state.receive(&AclReply {
        request: 1,
        result: Ok(acl()),
    });
    assert!(state.draft().unwrap().dirty());
    assert!(state.begin(2, DraftRequest::Reload));
    assert!(state.draft_mut().is_none());
    state.timeout(2);
    assert!(state.draft().unwrap().dirty());
    assert!(state.begin(3, DraftRequest::Reload));
    state.receive(&AclReply {
        request: 2,
        result: Ok(acl()),
    });
    assert_eq!(state.pending(), Some(3));
    assert!(state.draft().unwrap().dirty());
    state.receive(&AclReply {
        request: 3,
        result: Err("earlier query still unresolved".into()),
    });
    assert!(state.error);
    assert!(state.draft().unwrap().dirty());
    assert!(state.begin(4, DraftRequest::Reload));
    state.receive(&AclReply {
        request: 4,
        result: Ok(acl()),
    });
    assert!(!state.draft().unwrap().dirty());
    assert!(state.draft_mut().unwrap().entry_mut(id).is_none());
}

#[test]
fn save_requires_fresh_unchanged_baseline_and_matching_readback() {
    let mut state = loaded();
    let id = state.draft().unwrap().entries()[1].id;
    state.draft_mut().unwrap().entry_mut(id).unwrap().grant |= 8;
    let mut concurrent = acl();
    concurrent.entries[1].deny |= 4;
    assert!(state.begin(2, DraftRequest::Check));
    assert!(
        state
            .receive(&AclReply {
                request: 2,
                result: Ok(concurrent)
            })
            .is_none()
    );
    assert!(state.error);
    assert!(state.draft().unwrap().dirty());
    assert!(state.begin(3, DraftRequest::Check));
    let write = state
        .receive(&AclReply {
            request: 3,
            result: Ok(acl()),
        })
        .unwrap();
    assert_eq!(write.entries[1].grant, 0x8000_0008);
    assert!(
        state.draft_mut().is_none(),
        "write effect stays frozen until verification"
    );
    assert!(!state.begin(99, DraftRequest::Reload));
    assert!(state.begin(4, DraftRequest::Verify));
    state.receive(&AclReply {
        request: 4,
        result: Ok(acl()),
    }); // denied or not applied
    assert!(state.error);
    assert!(state.draft().unwrap().dirty());
    assert!(state.begin(5, DraftRequest::Check));
    let write = state
        .receive(&AclReply {
            request: 5,
            result: Ok(acl()),
        })
        .unwrap();
    assert!(state.begin(6, DraftRequest::Verify));
    state.receive(&AclReply {
        request: 6,
        result: Ok(write),
    });
    assert!(!state.error);
    assert!(!state.draft().unwrap().dirty());
    assert!(state.draft_mut().unwrap().entry_mut(id).is_none());
}
#[test]
fn initial_channel_wrong_response_and_lossy_snapshots_cannot_initialize_or_replace_draft() {
    let mut state = AclEditorState::new(7);
    state.begin(1, DraftRequest::Load);
    let mut wrong = acl();
    wrong.channel = 8;
    state.receive(&AclReply {
        request: 1,
        result: Ok(wrong),
    });
    assert!(state.error);
    assert!(state.draft().is_none());
    state.begin(2, DraftRequest::Load);
    let mut duplicate = acl();
    duplicate.groups.push(duplicate.groups[0].clone());
    state.receive(&AclReply {
        request: 2,
        result: Ok(duplicate),
    });
    assert!(state.error);
    assert!(state.draft().is_none());
    state.begin(3, DraftRequest::Load);
    state.receive(&AclReply {
        request: 3,
        result: Ok(acl()),
    });
    assert!(
        !state.begin(4, DraftRequest::Load),
        "load cannot silently replace a loaded draft"
    );
    let id = state.draft().unwrap().entries()[1].id;
    state.draft_mut().unwrap().entry_mut(id).unwrap().grant |= 8;
    state.begin(5, DraftRequest::Reload);
    let mut huge = acl();
    huge.entries[1].group = Some("x".repeat(257));
    state.receive(&AclReply {
        request: 5,
        result: Ok(huge),
    });
    assert!(state.error);
    assert!(state.draft().unwrap().dirty());
}
#[test]
fn baseline_ignores_group_set_order_but_preserves_members_flags_and_rule_order() {
    let mut baseline = acl();
    baseline.groups[0].add = vec![100, 200];
    baseline.groups[0].remove = vec![500, 600];
    baseline.groups[0].inherited_members = vec![42, 43];
    let mut other = baseline.groups[0].clone();
    other.name = "other".into();
    baseline.groups.push(other);
    let draft = AclDraft::new(baseline.clone());
    let mut shuffled = baseline.clone();
    shuffled.groups.reverse();
    for group in &mut shuffled.groups {
        group.add.reverse();
        group.remove.reverse();
        group.inherited_members.reverse();
    }
    assert!(draft.baseline_matches(&shuffled));
    let mut changed = shuffled.clone();
    changed.groups[0].inherit = false;
    assert!(!draft.baseline_matches(&changed));
    let mut changed = shuffled.clone();
    changed.groups[0].add.push(999);
    assert!(!draft.baseline_matches(&changed));
    let mut changed = shuffled.clone();
    changed.entries.reverse();
    assert!(!draft.baseline_matches(&changed));
    let mut changed = shuffled;
    changed.entries[0].grant |= 0x8000_0000;
    assert!(!draft.baseline_matches(&changed));
}

fn ban(last: u8, name: &str) -> Ban {
    Ban {
        address: vec![192, 0, 2, last],
        mask: 120,
        name: Some(name.into()),
        hash: None,
        reason: Some("reason".into()),
        start: None,
        duration: None,
    }
}

/// A row that a reply still lists keeps its ID, so a control removes the entry
/// it showed even when the list changed since.
#[test]
fn ban_rows_follow_their_entry_across_replies() {
    let mut draft = BanDraft::new(vec![ban(1, "Ada"), ban(2, "Bob")]);
    let ada = draft.rows()[0].id;
    let bob = draft.rows()[1].id;
    // Another administrator unbanned Ada, banned Cy, and edited Bob's reason.
    let mut edited = ban(2, "Bob");
    edited.reason = Some("spam".into());
    draft.replace(vec![edited, ban(3, "Cy")]);
    assert_eq!(draft.rows()[0].id, bob);
    assert_ne!(draft.rows()[1].id, bob);
    let left = draft.remove(bob).unwrap();
    assert_eq!(
        left.iter().map(|ban| ban.name.clone()).collect::<Vec<_>>(),
        [Some("Cy".into())]
    );
    assert_eq!(draft.rows().len(), 1);
    // A control whose entry a reply dropped sends nothing.
    assert!(draft.remove(ada).is_none());
}

#[test]
fn identical_ban_entries_keep_their_own_rows() {
    let mut draft = BanDraft::new(vec![ban(1, "Ada"), ban(1, "Ada")]);
    let (first, second) = (draft.rows()[0].id, draft.rows()[1].id);
    assert_ne!(first, second);
    draft.replace(vec![ban(1, "Ada")]);
    assert_eq!(draft.rows()[0].id, first);
    let left = draft.remove(first).unwrap();
    assert!(left.is_empty());
    assert!(draft.remove(second).is_none());
}
