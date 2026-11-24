// SPDX-License-Identifier: EUPL-1.2
//! Native ACL draft adapter. Lists instantiate visible rows; remote vectors
//! remain snapshots. Every delayed callback also carries the per-open token.
use crate::{
    dialogs::{Tone, Tooltip, named_button, styled_button},
    layout,
    menus::{ActionOrigin, MenuTarget},
    panels::Panel,
    picker::{Choice, Picker},
    state::{Desktop, ServerView, accessible},
    theme::*,
};
use gpui_kit::{
    base::input::{Input, InputEvent, InputState},
    prelude::*,
    *,
};
use nuschl_client::platform::request_id;
use nuschl_core::{
    access::Kind,
    admin::{self, AclEditorState, DraftRequest, RegisteredAccount, RowId},
    model::{AclEntry, TextTarget, permission},
};
use nuschl_session::connection::Command;
use std::{cell::Cell, ops::Range, rc::Rc};

mod access_table;
mod check_access;
mod parents;

/// A registration number servers leave unused, standing for "a member of
/// this group" when permissions are evaluated.
const MEMBER: u32 = u32::MAX;

// Shared buttons occupy 35 logical pixels; keep their full hit/focus box inside
// every fixed-height virtual row, including the final row at fractional zoom.
const ROW_HEIGHT: f32 = 36.;

#[derive(Clone, Copy, PartialEq, Eq)]
struct LookupSelection {
    member: bool,
    row: RowId,
}
struct Lookup {
    request: u64,
    selection: LookupSelection,
    input: String,
}
struct Resolved {
    selection: LookupSelection,
    input: String,
    account: RegisteredAccount,
}

pub struct Editor {
    pub token: u64,
    stale: bool,
    state: AclEditorState,
    selected_rule: Option<(usize, RowId)>,
    selected_group: Option<(usize, RowId)>,
    target_user: bool,
    target: Entity<InputState>,
    group_name: Entity<InputState>,
    password: Entity<InputState>,
    /// The channel's name when the editor opened, kept for the read-only view.
    pub(crate) channel_name: String,
    /// Whom Check access shows.
    check: Option<nuschl_core::access::Who>,
    /// Bring Check access's results into view once, after a pick.
    check_reveal: Cell<bool>,
    check_picker: crate::picker::Picker,
    /// Adds a row to the permission table.
    table_picker: crate::picker::Picker,
    /// Picks the selected rule's group.
    group_picker: crate::picker::Picker,
    parents: parents::Parents,
    /// Where to go once the Save/Discard question is answered.
    then: Option<Jump>,
    /// A table cell highlighted after arriving from another channel's table.
    flash: Option<Flash>,
    _flash_frames: Option<Task<()>>,
    /// Rows added to the access table that set nothing yet.
    table_rows: Vec<nuschl_core::access::Who>,
    table_scroll: UniformListScrollHandle,
    /// Closing waits for Save, Discard or Keep editing.
    leaving: bool,
    /// The rule list is open although the table shows the channel.
    show_rules: bool,
    /// Close once the save is confirmed.
    close_after_save: bool,
    /// The type picked when creating this channel, applied once it loads.
    create: Option<Applied>,
    member: Entity<InputState>,
    input_readonly: Cell<[bool; 4]>,
    rule_scroll: UniformListScrollHandle,
    group_scroll: UniformListScrollHandle,
    member_scroll: [UniformListScrollHandle; 3],
    timeout: Option<Task<()>>,
    lookup: Option<Lookup>,
    resolved: Option<Resolved>,
    lookup_timeout: Option<Task<()>>,
    _input_changes: [Subscription; 2],
    dirty: bool,
    member_selected: [Option<(usize, u32)>; 3],
    rule_focus: FocusHandle,
    group_focus: FocusHandle,
    member_focus: [FocusHandle; 3],
}

fn input(
    label: &'static str,
    window: &mut Window,
    cx: &mut Context<Desktop>,
) -> Entity<InputState> {
    cx.new(|cx| accessible(InputState::new(window, cx), label, false, false))
}

impl Editor {
    fn new(channel: u32, window: &mut Window, cx: &mut Context<Desktop>) -> Self {
        let token = request_id();
        let target = input("ACL rule target", window, cx);
        let member = input("Registered group member", window, cx);
        let changes = [(&target, false), (&member, true)].map(|(input, member)| {
            cx.subscribe_in(input, window, move |this, _, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    if let Some(editor) = active_editor(this, token) {
                        invalidate_lookup(editor, member);
                    }
                    cx.notify();
                }
            })
        });
        Self {
            token,
            stale: false,
            state: AclEditorState::new(channel),
            selected_rule: None,
            selected_group: None,
            target_user: false,
            target,
            member,
            input_readonly: Cell::new([false; 4]),
            group_name: input("New group name", window, cx),
            password: input("New channel password", window, cx),
            channel_name: String::new(),
            check: None,
            check_reveal: Cell::new(false),
            parents: Default::default(),
            then: None,
            flash: None,
            _flash_frames: None,
            check_picker: check_access::picker(token, window, cx),
            table_picker: access_table::picker(token, window, cx),
            group_picker: group_picker(token, window, cx),
            table_rows: Vec::new(),
            leaving: false,
            show_rules: false,
            close_after_save: false,
            create: None,
            rule_scroll: UniformListScrollHandle::new(),
            table_scroll: UniformListScrollHandle::new(),
            group_scroll: UniformListScrollHandle::new(),
            member_scroll: std::array::from_fn(|_| UniformListScrollHandle::new()),
            timeout: None,
            lookup: None,
            resolved: None,
            lookup_timeout: None,
            _input_changes: changes,
            dirty: false,
            member_selected: [None; 3],
            rule_focus: cx.focus_handle(),
            group_focus: cx.focus_handle(),
            member_focus: std::array::from_fn(|_| cx.focus_handle()),
        }
    }

    fn rule(&self) -> Option<(RowId, &AclEntry)> {
        let (index, id) = self.selected_rule?;
        let row = self.state.draft()?.entries().get(index)?;
        (row.id == id).then_some((id, &row.value))
    }
}

fn invalidate_lookup(editor: &mut Editor, member: bool) {
    let pending = editor
        .lookup
        .as_ref()
        .is_some_and(|lookup| lookup.selection.member == member);
    let resolved = editor
        .resolved
        .as_ref()
        .is_some_and(|resolved| resolved.selection.member == member);
    if pending {
        editor.lookup = None;
        editor.lookup_timeout = None;
    }
    if resolved {
        editor.resolved = None;
    }
    if pending || resolved {
        editor.state.error = false;
        editor.state.message =
            "The account input or selection changed. Resolve it again before applying.".into();
    }
}
fn lookup_selection(editor: &Editor, member: bool) -> Option<LookupSelection> {
    let row = if member {
        let (index, id) = editor.selected_group?;
        (editor.state.draft()?.groups().get(index)?.id == id).then_some(id)?
    } else {
        if !editor.target_user {
            return None;
        }
        editor.rule()?.0
    };
    Some(LookupSelection { member, row })
}
fn selected_account(editor: &Editor, value: &str, member: bool) -> Option<u32> {
    let selection = lookup_selection(editor, member)?;
    if let Some(id) = value
        .trim()
        .strip_prefix('#')
        .and_then(|id| id.parse::<u32>().ok())
    {
        return Some(id);
    }
    editor
        .resolved
        .as_ref()
        .filter(|resolved| resolved.selection == selection && resolved.input == value.trim())
        .map(|resolved| resolved.account.id)
}
fn sync_lookup(this: &mut Desktop, token: u64, index: usize, cx: &mut Context<Desktop>) {
    let request = match this.panel.as_ref() {
        Some(Panel::Acl { editor, .. }) => editor.lookup.as_ref().map(|lookup| lookup.request),
        _ => None,
    };
    let Some(request) = request else {
        return;
    };
    let Some(reply) = this.client.servers[index]
        .account_reply
        .as_ref()
        .filter(|reply| reply.request == request)
        .cloned()
    else {
        return;
    };
    let Some(editor) = active_editor(this, token) else {
        return;
    };
    let lookup = editor.lookup.take().unwrap();
    editor.lookup_timeout = None;
    let current = if lookup.selection.member {
        editor.member.read(cx).value()
    } else {
        editor.target.read(cx).value()
    };
    if lookup_selection(editor, lookup.selection.member) != Some(lookup.selection)
        || current.trim() != lookup.input
    {
        editor.state.error = false;
        editor.state.message =
            "The account selection changed. Resolve it again before applying.".into();
        return;
    }
    match reply.result {
        Ok(account) => {
            editor.state.error = false;
            editor.state.message = format!(
                "Resolved {} (#{}). Apply explicitly to the selected rule or group.",
                account.name, account.id
            );
            editor.resolved = Some(Resolved {
                selection: lookup.selection,
                input: lookup.input,
                account,
            });
        }
        Err(error) => {
            editor.state.error = true;
            editor.state.message = error;
        }
    }
}

/// A local inspector can outlive its network origin. It can select/read rows,
/// never mutate the draft or send commands through this helper.
fn local_editor(this: &mut Desktop, token: u64) -> Option<&mut Editor> {
    match this.panel.as_mut()? {
        Panel::Acl { editor, .. } if editor.token == token => Some(editor),
        _ => None,
    }
}

fn inputs_readonly(editor: &Editor, busy: bool, cx: &mut Context<Desktop>) {
    let target = busy || editor.rule().is_none_or(|(_, entry)| entry.inherited);
    let member = busy
        || editor
            .selected_group
            .and_then(|(index, id)| {
                editor
                    .state
                    .draft()?
                    .groups()
                    .get(index)
                    .filter(|row| row.id == id)
            })
            .is_none();
    let wanted = [target, busy, member, busy];
    let previous = editor.input_readonly.replace(wanted);
    for (index, input) in [
        &editor.target,
        &editor.group_name,
        &editor.member,
        &editor.password,
    ]
    .into_iter()
    .enumerate()
    {
        if previous[index] != wanted[index] {
            input.update(cx, |input, cx| input.set_readonly(wanted[index], cx));
        }
    }
}

pub fn retain_stale(this: &mut Desktop, cx: &mut Context<Desktop>) {
    let Some(Panel::Acl { editor, .. }) = this.panel.as_mut() else {
        return;
    };
    if editor.stale {
        return;
    }
    editor.stale = true;
    inputs_readonly(editor, true, cx);
    if let Some(request) = editor.state.pending() {
        editor.state.timeout(request);
    }
    editor.timeout = None;
    editor.lookup = None;
    editor.lookup_timeout = None;
    editor.resolved = None;
    editor.state.error = true;
    editor.state.message="The original connection or channel changed. Your draft is retained below for inspection, and cannot be saved to a replacement connection. Close it explicitly, then reopen permissions on the intended channel.".into();
}

#[derive(Clone, Copy)]
enum Inspect {
    Rules,
    Groups,
    Members(usize),
}
fn inspect_selection(editor: &Editor, kind: Inspect) -> Option<(usize, u64)> {
    match kind {
        Inspect::Rules => editor.selected_rule,
        Inspect::Groups => editor.selected_group,
        Inspect::Members(kind) => {
            editor.member_selected[kind].map(|(index, id)| (index, u64::from(id)))
        }
    }
}
fn inspect_row(editor: &Editor, kind: Inspect, index: usize) -> Option<(u64, String)> {
    let draft = editor.state.draft()?;
    match kind {
        Inspect::Rules => {
            let row = draft.entries().get(index)?;
            Some((
                row.id,
                format!(
                    "{}{} · {}",
                    if row.value.inherited {
                        "Inherited · "
                    } else {
                        ""
                    },
                    row.value
                        .user_id
                        .map(|id| format!("#{id}"))
                        .unwrap_or_else(|| group_target(row.value.group.as_deref().unwrap_or("?"))),
                    effect(&row.value, true)
                ),
            ))
        }
        Inspect::Groups => {
            let row = draft.groups().get(index)?;
            Some((row.id, format!("@{}", row.value.name)))
        }
        Inspect::Members(kind) => {
            let (group, id) = editor.selected_group?;
            let group = &draft.groups().get(group).filter(|row| row.id == id)?.value;
            let members = match kind {
                0 => &group.add,
                1 => &group.remove,
                _ => &group.inherited_members,
            };
            let id = *members.get(index)?;
            Some((u64::from(id), format!("#{id}")))
        }
    }
}
fn inspect_select(editor: &mut Editor, kind: Inspect, index: usize, id: u64) {
    match kind {
        Inspect::Rules => editor.selected_rule = Some((index, id)),
        Inspect::Groups => {
            editor.selected_group = Some((index, id));
            editor.member_selected = [None; 3];
        }
        Inspect::Members(kind) => editor.member_selected[kind] = Some((index, id as u32)),
    }
}
fn inspect_list(
    editor: &Editor,
    kind: Inspect,
    count: usize,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let token = editor.token;
    let (key, scroll, focus) = match kind {
        Inspect::Rules => (
            "rules".to_string(),
            editor.rule_scroll.clone(),
            editor.rule_focus.clone(),
        ),
        Inspect::Groups => (
            "groups".to_string(),
            editor.group_scroll.clone(),
            editor.group_focus.clone(),
        ),
        Inspect::Members(kind) => (
            format!("members-{kind}"),
            editor.member_scroll[kind].clone(),
            editor.member_focus[kind].clone(),
        ),
    };
    let rows_key = key.clone();
    let list = uniform_list(
        SharedString::from(format!("acl-inspect-{token}-{key}")),
        count,
        cx.processor(move |this, range: Range<usize>, _, cx| {
            let mut rows = Vec::new();
            for index in range {
                let Some((id, label, selected)) = (|| {
                    let editor = local_editor(this, token)?;
                    if !editor.stale {
                        return None;
                    }
                    let (id, label) = inspect_row(editor, kind, index)?;
                    Some((
                        id,
                        label,
                        inspect_selection(editor, kind) == Some((index, id)),
                    ))
                })() else {
                    continue;
                };
                rows.push(row_button(
                    format!("acl-inspect-row-{token}-{rows_key}-{id}-{index}"),
                    label,
                    selected,
                    move |this, _, cx| {
                        if let Some(editor) = local_editor(this, token)
                            && editor.stale
                            && inspect_row(editor, kind, index)
                                .is_some_and(|(current, _)| current == id)
                        {
                            inspect_select(editor, kind, index, id);
                        }
                        cx.notify();
                    },
                    cx,
                ));
            }
            rows
        }),
    )
    .track_scroll(&scroll)
    .h(px(ROW_HEIGHT * count.clamp(1, 5) as f32))
    .w_full();
    let id = match kind {
        Inspect::Rules => "acl-inspect-rules",
        Inspect::Groups => "acl-inspect-groups",
        Inspect::Members(0) => "acl-inspect-members",
        Inspect::Members(1) => "acl-inspect-exclusions",
        Inspect::Members(_) => "acl-inspect-inherited",
    };
    let list = crate::dialogs::nested_virtual_list(
        id,
        token,
        &scroll,
        list,
        move |this| {
            matches!(this.panel.as_ref(), Some(Panel::Acl { editor, .. })
            if editor.token == token && editor.stale)
        },
        cx,
    );
    let focus_on_key = focus.clone();
    layout::focusable_with_handle(
        SharedString::from(format!("acl-inspect-focus-{token}-{key}")),
        focus.clone(),
        div()
            .id(SharedString::from(format!(
                "acl-inspect-owner-{token}-{key}"
            )))
            .role(Role::ListBox)
            .aria_label(format!("Retained draft {key}"))
            .min_w_0()
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                let Some(editor) = local_editor(this, token) else {
                    return;
                };
                if !editor.stale {
                    return;
                }
                let current = inspect_selection(editor, kind).map_or(0, |(index, _)| index);
                if let Some(next) = list_key(event, current, count)
                    && let Some((id, _)) = inspect_row(editor, kind, next)
                {
                    window.focus(&focus_on_key, cx);
                    inspect_select(editor, kind, next, id);
                    scroll.scroll_to_item(next, ScrollStrategy::Nearest);
                    this.cancel_modal_scroll();
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(list),
    )
}

pub fn render_stale(editor: &Editor, cx: &mut Context<Desktop>) -> AnyElement {
    let mut body = div()
        .px_6()
        .pb_6()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_3()
        .child(note("acl-stale", editor.state.message.clone()));
    let Some(draft) = editor.state.draft() else {
        return body.into_any_element();
    };
    if editor.selected_rule.is_some() {
        let field = editor.target.read(cx).value();
        body = body.child(note(
            "acl-stale-target-field",
            format!(
                "Retained target field: {}",
                field.chars().take(512).collect::<String>()
            ),
        ));
    }
    body = body
        .child(note(
            "acl-stale-dirty",
            if editor.dirty {
                "Unsaved draft retained · read-only"
            } else {
                "Original snapshot retained · read-only"
            },
        ))
        .child(note(
            "acl-stale-inherit",
            if draft.inherit_acls() {
                "Inherits the parent channel's permissions"
            } else {
                "Ignores the parent channel's permissions"
            },
        ))
        .child(note(
            "acl-stale-rules-title",
            format!("Advanced rules · {}", draft.entries().len()),
        ))
        .child(inspect_list(
            editor,
            Inspect::Rules,
            draft.entries().len(),
            cx,
        ));
    if let Some((_, entry)) = editor.rule() {
        body = body.child(note(
            "acl-stale-rule-detail",
            format!(
                "{}{} · {}",
                if entry.inherited { "Inherited · " } else { "" },
                scope(entry),
                effect(entry, false)
            ),
        ));
    }
    body = body
        .child(note(
            "acl-stale-groups-title",
            format!("Groups · {}", draft.groups().len()),
        ))
        .child(inspect_list(
            editor,
            Inspect::Groups,
            draft.groups().len(),
            cx,
        ));
    if let Some((index, id)) = editor.selected_group
        && let Some(group) = draft
            .groups()
            .get(index)
            .filter(|row| row.id == id)
            .map(|row| &row.value)
    {
        body = body.child(note(
            "acl-stale-group-flags",
            [
                format!("@{}", group.name),
                (if group.inherited {
                    "from a parent channel"
                } else {
                    "defined here"
                })
                .into(),
                (if group.inherit {
                    "includes the parent's members"
                } else {
                    "ignores the parent's members"
                })
                .into(),
                (if group.inheritable {
                    "used by subchannels"
                } else {
                    "this channel only"
                })
                .into(),
            ]
            .join(" · "),
        ));
        for (kind, title, count) in [
            (0, "Local members", group.add.len()),
            (1, "Local exclusions", group.remove.len()),
            (2, "Inherited members", group.inherited_members.len()),
        ] {
            body = body
                .child(note(
                    SharedString::from(format!("acl-stale-member-title-{kind}")),
                    format!("{title} · {count}"),
                ))
                .child(inspect_list(editor, Inspect::Members(kind), count, cx));
        }
    }
    body.into_any_element()
}

fn target_pending(editor: &Editor, cx: &Context<Desktop>) -> bool {
    let Some((_, entry)) = editor.rule() else {
        return false;
    };
    if entry.inherited {
        return false;
    }
    let field = editor.target.read(cx).value();
    let value: &str = field.as_ref();
    if editor.target_user {
        let proposed = selected_account(editor, value, false);
        proposed.is_none() || proposed != entry.user_id || entry.group.is_some()
    } else {
        entry.user_id.is_some() || entry.group.as_deref().unwrap_or("") != value
    }
}
fn require_applied_target(this: &mut Desktop, token: u64, cx: &Context<Desktop>) -> bool {
    let Some(editor) = active_editor(this, token) else {
        return false;
    };
    if target_pending(editor, cx) {
        editor.state.error = true;
        editor.state.message="Apply or discard the edited rule target before saving or selecting another rule. No permissions were sent.".into();
        false
    } else {
        true
    }
}
fn discard_target(
    this: &mut Desktop,
    token: u64,
    row: RowId,
    window: &mut Window,
    cx: &mut Context<Desktop>,
) {
    let Some(editor) = active_editor(this, token) else {
        return;
    };
    if editor.rule().is_none_or(|(id, _)| id != row) {
        return;
    }
    let Some((_, entry)) = editor.rule() else {
        return;
    };
    let user = entry.user_id.is_some();
    let value = entry
        .user_id
        .map(|id| format!("#{id}"))
        .unwrap_or_else(|| entry.group.clone().unwrap_or_default());
    invalidate_lookup(editor, false);
    editor.target_user = user;
    editor
        .target
        .update(cx, |input, cx| input.set_value(value, window, cx));
    editor.state.error = false;
    editor.state.message = "Target edit discarded.".into();
}

fn active_editor(this: &mut Desktop, token: u64) -> Option<&mut Editor> {
    let valid = match this.panel.as_ref()? {
        Panel::Acl { origin, editor, .. } => {
            editor.token == token && !editor.stale && origin.live_index(this).is_some()
        }
        _ => false,
    };
    if !valid {
        return None;
    }
    match this.panel.as_mut()? {
        Panel::Acl { editor, .. } => Some(editor),
        _ => None,
    }
}
fn change(this: &mut Desktop, token: u64, action: impl FnOnce(&mut Editor)) {
    if let Some(editor) = active_editor(this, token) {
        if editor.state.pending().is_some() {
            return;
        }
        action(editor);
        editor.dirty = editor.state.draft().is_some_and(|draft| draft.dirty());
    }
}
fn change_rule(this: &mut Desktop, token: u64, id: RowId, action: impl FnOnce(&mut Editor)) {
    if !active_editor(this, token)
        .is_some_and(|editor| editor.rule().is_some_and(|(selected, _)| selected == id))
    {
        return;
    }
    change(this, token, action);
}
fn change_group(this: &mut Desktop, token: u64, id: RowId, action: impl FnOnce(&mut Editor)) {
    if !active_editor(this, token).is_some_and(|editor| {
        editor
            .selected_group
            .is_some_and(|(_, selected)| selected == id)
    }) {
        return;
    }
    change(this, token, action);
}
fn select_group(this: &mut Desktop, token: u64, index: usize) {
    if let Some(editor) = active_editor(this, token) {
        let Some(id) = editor
            .state
            .draft()
            .and_then(|draft| draft.groups().get(index))
            .map(|row| row.id)
        else {
            return;
        };
        invalidate_lookup(editor, true);
        editor.selected_group = Some((index, id));
        editor.member_selected = [None; 3];
        editor
            .group_scroll
            .scroll_to_item(index, ScrollStrategy::Nearest);
        for scroll in &editor.member_scroll {
            scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
    }
}
fn fail(this: &mut Desktop, token: u64, message: impl Into<String>) {
    if let Some(editor) = active_editor(this, token) {
        editor.state.error = true;
        editor.state.message = message.into();
    }
}
fn origin(this: &Desktop, token: u64) -> Option<ActionOrigin> {
    match this.panel.as_ref()? {
        Panel::Acl { origin, editor, .. } if editor.token == token => Some(origin.clone()),
        _ => None,
    }
}
fn send(this: &mut Desktop, token: u64, command: Command) -> Result<(), String> {
    let origin = origin(this, token).ok_or("This editor is no longer open.")?;
    let index = origin
        .live_index(this)
        .ok_or("The original connection or channel changed. Reopen its editor.")?;
    origin.with_scope(this, |this| {
        this.driver.command(&this.client.servers[index].id, command)
    })
}
/// A table cell in another channel's permissions: where an inherited value
/// comes from.
#[derive(Clone)]
pub struct Jump {
    pub server: usize,
    pub channel: u32,
    pub who: nuschl_core::access::Who,
    pub column: usize,
}

/// The highlight on the cell a jump arrived at; it fades out once shown.
pub struct Flash {
    who: nuschl_core::access::Who,
    column: usize,
    /// Set when the cell first shows; the fade starts then.
    shown: Cell<Option<std::time::Instant>>,
}

const FLASH: std::time::Duration = std::time::Duration::from_millis(1600);

/// A channel created with a type (#341), until the server shows it on the
/// connection that asked: then its permissions open, take the type and save.
pub struct Create {
    /// The parent channel on the creating connection.
    origin: ActionOrigin,
    parent: u32,
    name: String,
    kind: Kind,
    password: String,
    /// The server's notice when the request went out; a new one is a refusal.
    notice: Option<String>,
    deadline: std::time::Instant,
    /// The new channel, once seen.
    channel: Option<u32>,
}

/// The type to give a new channel, and who asked for it.
struct Applied {
    kind: Kind,
    password: String,
    user_id: Option<u32>,
    hash: Option<String>,
}

/// Waits for the channel `name` that the connection behind `origin` asked to
/// create under `parent`. Murmur refuses a name a sibling already has.
pub fn created(
    this: &mut Desktop,
    origin: ActionOrigin,
    parent: u32,
    name: &str,
    kind: Kind,
    password: String,
) {
    this.acl_create = None;
    let Some(view) = origin
        .live_index(this)
        .map(|index| &this.client.servers[index])
    else {
        return;
    };
    let taken = new_channel(view, parent, name).is_some();
    if kind == Kind::Open || taken {
        return;
    }
    this.acl_create = Some(Create {
        notice: view.notice.clone(),
        origin,
        parent,
        name: name.to_owned(),
        kind,
        password,
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(15),
        channel: None,
    });
}

fn new_channel(view: &ServerView, parent: u32, name: &str) -> Option<u32> {
    view.channels
        .iter()
        .find(|channel| {
            matches!(channel.target, TextTarget::Channel(_))
                && channel.parent == Some(parent)
                && channel.name == name
        })
        .map(|channel| channel.id)
}

/// Notes the channel being created once it shows, and drops the request on
/// a refusal, a reconnect or after its deadline.
fn watch_create(this: &mut Desktop) {
    let Some(create) = this
        .acl_create
        .as_ref()
        .filter(|create| create.channel.is_none())
    else {
        return;
    };
    let view = create
        .origin
        .live_index(this)
        .map(|index| &this.client.servers[index]);
    let channel = match view {
        Some(view)
            if std::time::Instant::now() < create.deadline
                && (view.notice.is_none() || view.notice == create.notice) =>
        {
            new_channel(view, create.parent, &create.name)
        }
        _ => {
            this.acl_create = None;
            return;
        }
    };
    if let (Some(create), Some(channel)) = (this.acl_create.as_mut(), channel) {
        create.channel = Some(channel);
    }
}

/// Opens a channel created with a type, unless another panel is open.
fn open_created(this: &mut Desktop, window: &mut Window, cx: &mut Context<Desktop>) {
    let Some(create) = this.acl_create.take_if(|create| create.channel.is_some()) else {
        return;
    };
    let (Some(index), Some(channel)) = (create.origin.live_index(this), create.channel) else {
        return;
    };
    if this.panel.is_some() {
        return;
    }
    let view = &this.client.servers[index];
    let own = view
        .own_session
        .and_then(|session| view.users.get(&session));
    let applied = Applied {
        kind: create.kind,
        password: create.password,
        user_id: own.and_then(|user| user.user_id),
        hash: own.and_then(|user| user.certificate_hash.clone()),
    };
    open(this, index, channel, window, cx);
    if let Some(Panel::Acl { editor, .. }) = this.panel.as_mut() {
        editor.create = Some(applied);
    }
}

/// Gives a channel just created by this connection the type picked for it,
/// then saves.
fn apply_created(this: &mut Desktop, token: u64, cx: &mut Context<Desktop>) {
    let Some(editor) = active_editor(this, token) else {
        return;
    };
    let Some(applied) = editor.create.take() else {
        return;
    };
    let Some(draft) = editor.state.draft_mut() else {
        return;
    };
    let result = if admin::created_by(&draft.snapshot(), applied.user_id, applied.hash.as_deref()) {
        draft.set_kind(applied.kind, &applied.password)
    } else {
        Err("Someone else made this channel. Its type stays as it is.")
    };
    match result {
        Ok(()) => {
            editor.dirty = true;
            query(this, token, DraftRequest::Check, cx);
        }
        Err(error) => {
            editor.state.error = true;
            editor.state.message = error.into();
        }
    }
}

/// Opens `jump`'s channel permissions after the Save/Discard question, if the
/// current editor holds unsaved changes.
fn go(this: &mut Desktop, token: u64, jump: Jump) {
    if let Some(editor) = active_editor(this, token) {
        if editor.dirty {
            editor.leaving = true;
            editor.then = Some(jump);
        } else {
            this.panel = None;
            this.acl_jump = Some(jump);
        }
    }
}

/// The cell menu's "Show in …": goes to `jump` from the open editor.
pub fn show_source(this: &mut Desktop, jump: Jump) {
    if let Some(Panel::Acl { editor, .. }) = this.panel.as_ref() {
        let token = editor.token;
        go(this, token, jump);
    }
}

/// Opens a pending jump: that channel's permissions, its cell highlighted.
/// Runs in the window's render, which editors need to open.
pub fn follow_jump(this: &mut Desktop, window: &mut Window, cx: &mut Context<Desktop>) {
    open_created(this, window, cx);
    let Some(jump) = this.acl_jump.take() else {
        return;
    };
    open(this, jump.server, jump.channel, window, cx);
    let Some(Panel::Acl { editor, .. }) = this.panel.as_mut() else {
        return;
    };
    editor.flash = Some(Flash {
        who: jump.who,
        column: jump.column,
        shown: Cell::new(None),
    });
    let token = editor.token;
    // Frames for the fade; the editor's draft may still be loading.
    editor._flash_frames = Some(cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(16))
                .await;
            let Ok(going) = this.update(cx, |this, cx| {
                let Some(editor) = active_editor(this, token) else {
                    return false;
                };
                let done = editor
                    .flash
                    .as_ref()
                    .and_then(|flash| flash.shown.get())
                    .is_some_and(|shown| shown.elapsed() >= FLASH);
                if done {
                    editor.flash = None;
                }
                cx.notify();
                !done
            }) else {
                return;
            };
            if !going {
                return;
            }
        }
    }));
}

/// Closes the editor, or first asks to save or discard unsaved changes.
pub fn close(this: &mut Desktop) {
    match this.panel.as_mut() {
        Some(Panel::Acl { editor, .. }) if editor.dirty && !editor.stale => editor.leaving = true,
        _ => this.panel = None,
    }
}

fn leave_prompt(token: u64, cx: &mut Context<Desktop>) -> AnyElement {
    let decide = move |this: &mut Desktop, save: Option<bool>, cx: &mut Context<Desktop>| {
        if let Some(editor) = active_editor(this, token) {
            editor.leaving = false;
            match save {
                Some(true) => {
                    editor.close_after_save = true;
                    query(this, token, DraftRequest::Check, cx);
                }
                Some(false) => {
                    let then = editor.then.take();
                    this.panel = None;
                    this.acl_jump = then;
                }
                None => editor.then = None,
            }
        }
        cx.notify();
    };
    div()
        .px_6()
        .pb_6()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .text_base()
                .font_weight(FontWeight::SEMIBOLD)
                .child(text!("Save your changes?").with_id("acl-leave-title")),
        )
        .child(note(
            "acl-leave-detail",
            "These permissions have unsaved changes.",
        ))
        .child(
            div()
                .flex()
                .gap_2()
                .child(styled_button(
                    SharedString::from(format!("acl-leave-save-{token}")),
                    "Save",
                    Tone::Primary,
                    move |this, _, cx| decide(this, Some(true), cx),
                    cx,
                ))
                .child(styled_button(
                    SharedString::from(format!("acl-leave-discard-{token}")),
                    "Discard",
                    Tone::Danger,
                    move |this, _, cx| decide(this, Some(false), cx),
                    cx,
                ))
                .child(styled_button(
                    SharedString::from(format!("acl-leave-keep-{token}")),
                    "Keep editing",
                    Tone::Ghost,
                    move |this, _, cx| decide(this, None, cx),
                    cx,
                )),
        )
        .into_any_element()
}

fn query(this: &mut Desktop, token: u64, kind: DraftRequest, cx: &mut Context<Desktop>) {
    if kind == DraftRequest::Check && !require_applied_target(this, token, cx) {
        return;
    }
    if let Some(editor) = active_editor(this, token)
        && editor.lookup.is_some()
    {
        editor.state.error = true;
        editor.state.message =
            "Finish the account lookup before loading or saving permissions.".into();
        return;
    }
    let request = request_id();
    let channel = match this.panel.as_ref() {
        Some(Panel::Acl {
            editor, channel, ..
        }) if editor.token == token => *channel,
        _ => return,
    };
    if !active_editor(this, token).is_some_and(|editor| editor.state.begin(request, kind)) {
        return;
    }
    if let Some(editor) = active_editor(this, token) {
        inputs_readonly(editor, true, cx);
    }
    enqueue_query(this, token, channel, request, cx);
}
fn enqueue_query(
    this: &mut Desktop,
    token: u64,
    channel: u32,
    request: u64,
    cx: &mut Context<Desktop>,
) {
    tracing::debug!(token, channel, request, "enqueue ACL draft query");
    if let Err(error) = send(this, token, Command::QueryAclDraft { channel, request }) {
        if let Some(editor) = active_editor(this, token) {
            editor.state.timeout(request);
            inputs_readonly(editor, false, cx);
            editor.state.message = error;
            editor.timeout = None;
        }
        return;
    }
    let task = cx.spawn(async move |this, cx| {
        cx.background_executor()
            .timer(std::time::Duration::from_secs(15))
            .await;
        let _ = this.update(cx, |this, cx| {
            if let Some(editor) = active_editor(this, token)
                && editor.state.pending() == Some(request)
            {
                editor.state.timeout(request);
                inputs_readonly(editor, false, cx);
            }
            cx.notify();
        });
    });
    if let Some(editor) = active_editor(this, token) {
        editor.timeout = Some(task);
    }
}

pub fn open(
    this: &mut Desktop,
    index: usize,
    channel: u32,
    window: &mut Window,
    cx: &mut Context<Desktop>,
) {
    let Some(view) = this.client.servers.get(index) else {
        return;
    };
    let origin = ActionOrigin::capture(view, MenuTarget::Channel(channel));
    if origin.live_index(this).is_none() {
        return;
    }
    let name = crate::panels::channel_name(view, channel);
    let mut editor = Box::new(Editor::new(channel, window, cx));
    editor.channel_name = name;
    let token = editor.token;
    this.panel = Some(Panel::Acl {
        origin,
        server: index,
        channel,
        editor,
    });
    query(this, token, DraftRequest::Load, cx);
    // Check access lists every account; the list needs Register at the root.
    if let Some(view) = this.client.servers.get(index)
        && view.registered_users.is_none()
        && view.may(0, permission::REGISTER)
    {
        this.run(index, Command::QueryRegisteredUsers);
    }
    cx.notify();
}

/// Called after a client input is applied. Only this open panel's active ticket
/// can load/verify its draft; remote/cache events never edit it directly.
pub fn sync(this: &mut Desktop, cx: &mut Context<Desktop>) {
    watch_create(this);
    let (token, index, channel, pending) = match this.panel.as_ref() {
        Some(Panel::Acl {
            origin,
            channel,
            editor,
            ..
        }) => {
            let Some(index) = origin.live_index(this) else {
                return;
            };
            (editor.token, index, *channel, editor.state.pending())
        }
        _ => return,
    };
    sync_lookup(this, token, index, cx);
    parents::sync(this, token, index);
    // Outside the opening action's scope, which covers this channel only.
    parents::next(this, token);
    let Some(pending) = pending else {
        return;
    };
    let Some(reply) = this.client.servers[index]
        .acl_reply
        .as_ref()
        .filter(|reply| reply.request == pending)
        .cloned()
    else {
        return;
    };
    let write = {
        let Some(editor) = active_editor(this, token) else {
            return;
        };
        editor.timeout = None;
        tracing::debug!(
            token,
            request = reply.request,
            error = reply.result.as_ref().err(),
            "apply correlated ACL draft response"
        );
        let write = editor.state.receive(&reply);
        if editor.state.error {
            editor.close_after_save = false;
        }
        if write.is_none() && !editor.state.error {
            editor.selected_rule = None;
            editor.selected_group = None;
            editor.table_rows.clear();
            editor.member_selected = [None; 3];
            editor.lookup = None;
            editor.resolved = None;
            editor.lookup_timeout = None;
        }
        inputs_readonly(editor, write.is_some(), cx);
        editor.dirty = editor.state.draft().is_some_and(|draft| draft.dirty());
        if write.is_none() && editor.close_after_save && !editor.dirty && !editor.state.error {
            let then = editor.then.take();
            this.panel = None;
            this.acl_jump = then;
            cx.notify();
            return;
        }
        if editor.state.error {
            editor.create = None;
        }
        write
    };
    apply_created(this, token, cx);
    if let Some(acl) = write {
        let request = request_id();
        if !active_editor(this, token)
            .is_some_and(|editor| editor.state.begin(request, DraftRequest::Verify))
        {
            return;
        }
        if let Err(error) = send(this, token, Command::UpdateAcl(acl)) {
            if let Some(editor) = active_editor(this, token) {
                editor.state.timeout(request);
                inputs_readonly(editor, false, cx);
                editor.state.message = error;
            }
            return;
        }
        enqueue_query(this, token, channel, request, cx);
    }
}

/// Sets the typed password on the draft, or removes the password with `remove`,
/// as Mumble's channel dialog does.
fn set_password(this: &mut Desktop, token: u64, remove: bool, cx: &mut Context<Desktop>) {
    let value = match active_editor(this, token) {
        Some(_) if remove => String::new(),
        Some(editor) => editor.password.read(cx).value().to_string(),
        None => return,
    };
    change(this, token, |editor| {
        if let Some(Err(error)) = editor
            .state
            .draft_mut()
            .map(|draft| draft.set_password(&value))
        {
            editor.state.error = true;
            editor.state.message = error.into();
        }
    });
    cx.notify();
}

fn field(label: &'static str, input: &Entity<InputState>) -> AnyElement {
    div()
        .min_w_0()
        .flex_1()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(color(TEXT_MUTED))
                .child(text!(label).with_id(label)),
        )
        .child(
            div()
                .min_w_0()
                .px_3()
                .py_2()
                .rounded(px(6.))
                .bg(color(APP))
                .border_1()
                .border_color(color(BORDER))
                .child(Input::new(input)),
        )
        .into_any_element()
}
fn note(id: impl Into<ElementId>, message: impl Into<SharedString>) -> AnyElement {
    div()
        .min_w_0()
        .whitespace_normal()
        .text_sm()
        .text_color(color(TEXT_MUTED))
        .child(text!(message.into()).with_id(id))
        .into_any_element()
}
/// A titled part of the editor, set apart by space and a rule above it.
fn section(id: &'static str, title: impl Into<SharedString>, hint: &'static str) -> Div {
    div()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .mt_4()
        .pt_4()
        .border_t_1()
        .border_color(color(BORDER))
        .child(
            div()
                .text_base()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color(TEXT))
                .child(text!(title.into()).with_id(id)),
        )
        .child(note(SharedString::from(format!("{id}-hint")), hint))
}
fn name(view: &ServerView, id: u32) -> String {
    view.user_names
        .get(&id)
        .map(|name| format!("{name} (#{id})"))
        .unwrap_or_else(|| format!("#{id}"))
}
fn resolve(this: &mut Desktop, token: u64, selection: LookupSelection, cx: &mut Context<Desktop>) {
    let member = selection.member;
    let (value, selection, channel) = {
        let Some(editor) = active_editor(this, token) else {
            return;
        };
        if editor.state.pending().is_some() {
            return;
        }
        if lookup_selection(editor, member) != Some(selection) {
            return;
        }
        let input = if member {
            &editor.member
        } else {
            &editor.target
        };
        (
            input.read(cx).value().trim().to_string(),
            selection,
            editor.state.draft().unwrap().channel(),
        )
    };
    if value.is_empty() || value.len() > 512 {
        fail(
            this,
            token,
            "Enter a registered account name or literal #id (at most 512 bytes).",
        );
        return;
    }
    if value
        .strip_prefix('#')
        .and_then(|id| id.parse::<u32>().ok())
        .is_some()
    {
        if let Some(editor) = active_editor(this, token) {
            editor.state.error = false;
            editor.state.message="A literal #id selects that registered account ID. Its cached name is only a label.".into();
        }
        return;
    }
    let request = request_id();
    if let Some(editor) = active_editor(this, token) {
        editor.resolved = None;
        editor.lookup = Some(Lookup {
            request,
            selection,
            input: value.clone(),
        });
        editor.state.error = false;
        editor.state.message = "Resolving a fresh registered account through the server…".into();
    }
    if let Err(error) = send(
        this,
        token,
        Command::LookupAccount {
            channel,
            request,
            name: value,
        },
    ) {
        if let Some(editor) = active_editor(this, token) {
            editor.lookup = None;
            editor.state.error = true;
            editor.state.message = error;
        }
        return;
    }
    let task=cx.spawn(async move|this,cx| {
        cx.background_executor().timer(std::time::Duration::from_secs(15)).await;
        let _=this.update(cx,|this,cx| {
            if let Some(editor)=active_editor(this,token) && editor.lookup.as_ref().is_some_and(|lookup|lookup.request==request) {
                editor.lookup=None;editor.state.error=true;editor.state.message="Account lookup timed out. Your draft is retained; wait for the earlier reply or reconnect before resolving again.".into();
            }cx.notify();
        });
    });
    if let Some(editor) = active_editor(this, token) {
        editor.lookup_timeout = Some(task);
    }
}
fn selected_rule(
    this: &mut Desktop,
    token: u64,
    index: usize,
    window: &mut Window,
    cx: &mut Context<Desktop>,
) {
    if active_editor(this, token).is_some_and(|editor| {
        editor
            .selected_rule
            .is_some_and(|(current, _)| current == index)
    }) {
        return;
    }
    if !require_applied_target(this, token, cx) {
        return;
    }
    if let Some(editor) = active_editor(this, token) {
        let Some(row) = editor
            .state
            .draft()
            .and_then(|draft| draft.entries().get(index))
        else {
            return;
        };
        let id = row.id;
        let target = row
            .value
            .user_id
            .map(|id| format!("#{id}"))
            .unwrap_or_else(|| row.value.group.clone().unwrap_or_default());
        let target_user = row.value.user_id.is_some();
        invalidate_lookup(editor, false);
        editor.target_user = target_user;
        editor.selected_rule = Some((index, id));
        editor
            .target
            .update(cx, |input, cx| input.set_value(target, window, cx));
        editor
            .rule_scroll
            .scroll_to_item(index, ScrollStrategy::Nearest);
    }
}
fn list_key(event: &KeyDownEvent, index: usize, count: usize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(match event.keystroke.key.as_str() {
        "up" => index.saturating_sub(1),
        "down" => (index + 1).min(count - 1),
        "pageup" => index.saturating_sub(5),
        "pagedown" => (index + 5).min(count - 1),
        "home" => 0,
        "end" => count - 1,
        _ => return None,
    })
}
fn row_button(
    id: String,
    label: String,
    selected: bool,
    action: impl Fn(&mut Desktop, &mut Window, &mut Context<Desktop>) + 'static,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let id = SharedString::from(id);
    let action = Rc::new(action);
    let click = action.clone();
    let foreground = if selected { TEXT } else { TEXT_MUTED };
    let control = div()
        .id(id.clone())
        .role(Role::Button)
        .aria_label(label.clone())
        .tab_index(0)
        .h(px(ROW_HEIGHT))
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .px_3()
        .rounded(px(6.))
        .cursor_pointer()
        .bg(color(if selected { ELEVATED } else { MAIN }))
        .hover(|style| style.bg(color(HOVER)))
        .text_sm()
        .text_color(color(foreground))
        .on_click(cx.listener(move |this, _, window, cx| click(this, window, cx)))
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                action(this, window, cx);
                cx.stop_propagation();
            }
        }))
        .child(layout::label(
            SharedString::from(format!("{id}-label")),
            label,
        ));
    layout::focusable(id, control)
}

pub fn render(view: &ServerView, editor: &Editor, cx: &mut Context<Desktop>) -> AnyElement {
    // Only three fixed UI properties are observed here. No draft/member scan,
    // task or notification occurs when the already-bound mode is unchanged.
    inputs_readonly(editor, editor.state.pending().is_some(), cx);
    let token = editor.token;
    if editor.leaving {
        return leave_prompt(token, cx);
    }
    let pending = editor.state.pending().is_some();
    let unapplied = target_pending(editor, cx);
    let header = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(styled_button(
            SharedString::from(format!("acl-reload-{token}")),
            if editor.dirty || unapplied {
                "Reload and discard draft"
            } else {
                "Reload"
            },
            Tone::Secondary,
            move |this, _, cx| {
                query(this, token, DraftRequest::Reload, cx);
                cx.notify();
            },
            cx,
        ))
        .child(styled_button(
            SharedString::from(format!("acl-save-{token}")),
            "Save permissions",
            Tone::Primary,
            move |this, _, cx| {
                query(this, token, DraftRequest::Check, cx);
                cx.notify();
            },
            cx,
        ));
    let mut body = div()
        .px_6()
        .pb_6()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_3()
        .child(header)
        // The status line holds "Unsaved changes" at its end, so edits
        // leave the layout in place.
        .child(
            div()
                .flex()
                .items_start()
                .gap_3()
                .text_sm()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .whitespace_normal()
                        .text_color(color(if editor.state.error {
                            DANGER
                        } else {
                            TEXT_MUTED
                        }))
                        .child(text!(editor.state.message.clone()).with_id("acl-status")),
                )
                .when(editor.dirty || unapplied, |line| {
                    line.child(
                        div()
                            .flex_shrink_0()
                            .text_color(color(WARNING))
                            .child(text!("Unsaved changes").with_id("acl-dirty")),
                    )
                }),
        );
    let Some(draft) = editor.state.draft() else {
        return body.into_any_element();
    };
    let inherit = draft.inherit_acls();
    body = body.child(with_help(
        format!("acl-inherit-help-{token}"),
        "Rules of the parent channel apply here too. Without it, only this channel's rules count.",
        styled_button(
            SharedString::from(format!("acl-inherit-{token}")),
            format!(
                "{} Inherit parent permissions",
                if inherit { "✓" } else { "○" }
            ),
            Tone::Secondary,
            move |this, _, cx| {
                change(this, token, |editor| {
                    if let Some(draft) = editor.state.draft_mut() {
                        draft.set_inherit_acls(!inherit);
                    }
                });
                cx.notify();
            },
            cx,
        ),
    ));
    let password = draft.password().map(str::to_owned);
    let mut password_section = section(
        "acl-password-title",
        "Channel password",
        "People who enter it may join, speak and write here.",
    )
    .child(note(
        "acl-password",
        match &password {
            Some(password) => format!("Channel password: {password}"),
            None => "No channel password".into(),
        },
    ))
    .child(field("New channel password", &editor.password))
    .child(styled_button(
        SharedString::from(format!("acl-set-password-{token}")),
        "Set password",
        Tone::Secondary,
        move |this, _, cx| set_password(this, token, false, cx),
        cx,
    ));
    if password.is_some() {
        password_section = password_section.child(styled_button(
            SharedString::from(format!("acl-remove-password-{token}")),
            "Remove password",
            Tone::Danger,
            move |this, _, cx| set_password(this, token, true, cx),
            cx,
        ));
    }
    body = body
        .child(access_table::kinds(view, editor, cx))
        .child(password_section)
        .child(access_table::render(view, editor, cx));
    let count = draft.entries().len();
    let scroll = editor.rule_scroll.clone();
    let rules = uniform_list(
        SharedString::from(format!("acl-rules-{token}")),
        count,
        cx.processor(move |this, range: Range<usize>, _, cx| {
            let mut rows = Vec::new();
            for index in range {
                let Some((id, label, selected)) = (|| {
                    let Panel::Acl { origin, editor, .. } = this.panel.as_ref()? else {
                        return None;
                    };
                    if editor.token != token {
                        return None;
                    }
                    let view = this.client.servers.get(origin.live_index(this)?)?;
                    let draft = editor.state.draft()?;
                    let row = draft.entries().get(index)?;
                    let label = match row.value.user_id {
                        Some(id) => name(view, id),
                        None => group_target(row.value.group.as_deref().unwrap_or("?")),
                    };
                    Some((
                        row.id,
                        format!(
                            "{}{} · {}",
                            if row.value.inherited {
                                if draft.inherit_acls() {
                                    "Inherited · "
                                } else {
                                    "Inactive inherited · "
                                }
                            } else {
                                ""
                            },
                            label,
                            effect(&row.value, true)
                        ),
                        editor.selected_rule == Some((index, row.id)),
                    ))
                })() else {
                    continue;
                };
                rows.push(row_button(
                    format!("acl-rule-{token}-{id}"),
                    label,
                    selected,
                    move |this, window, cx| {
                        // A retained visible-row callback must validate both its index
                        // AND stable key before selecting (reorder/reload can change it).
                        let valid = active_editor(this, token)
                            .and_then(|editor| editor.state.draft())
                            .and_then(|draft| draft.entries().get(index))
                            .is_some_and(|row| row.id == id);
                        if valid {
                            selected_rule(this, token, index, window, cx);
                            cx.notify();
                        }
                    },
                    cx,
                ));
            }
            rows
        }),
    )
    .track_scroll(&scroll)
    .h(px(ROW_HEIGHT * count.clamp(1, 6) as f32))
    .w_full();
    let rules = crate::dialogs::nested_virtual_list(
        "acl-rules",
        token,
        &scroll,
        rules,
        move |this| {
            matches!(this.panel.as_ref(), Some(Panel::Acl { editor, .. })
            if editor.token == token && !editor.stale)
        },
        cx,
    );
    let mut rules_section = section(
        "acl-rule-title",
        format!("Advanced rules · {count}"),
        "What the server applies, top to bottom; a later rule overrides an earlier one. The table above writes these rules.",
    )
    .child(layout::focusable_with_handle(
            SharedString::from(format!("acl-rule-focus-{token}")),
            editor.rule_focus.clone(),
            div()
                .id(SharedString::from(format!("acl-rule-list-{token}")))
                .role(Role::ListBox)
                .aria_label("ACL rules")
                .min_w_0()
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    let Some(editor) = active_editor(this, token) else {
                        return;
                    };
                    let count = editor
                        .state
                        .draft()
                        .map_or(0, |draft| draft.entries().len());
                    let current = editor.selected_rule.map_or(0, |(index, _)| index);
                    if let Some(next) = list_key(event, current, count) {
                        window.focus(&editor.rule_focus, cx);
                        selected_rule(this, token, next, window, cx);
                        this.cancel_modal_scroll();
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
                .child(rules),
        ))
        .child(styled_button(
            SharedString::from(format!("acl-add-rule-{token}")),
            "Add rule",
            Tone::Secondary,
            move |this, window, cx| {
                if !require_applied_target(this, token, cx) {
                    cx.notify();
                    return;
                }
                let mut selected = None;
                change(this, token, |editor| {
                    if let Some(draft) = editor.state.draft_mut() {
                        match draft.add_entry() {
                            Ok(id) => {
                                selected = Some((draft.entries().len() - 1, id));
                            }
                            Err(error) => {
                                editor.state.error = true;
                                editor.state.message = error.into();
                            }
                        }
                    }
                });
                if let Some((index, _)) = selected {
                    selected_rule(this, token, index, window, cx);
                }
                cx.notify();
            },
            cx,
        ));
    if let Some((id, entry)) = editor.rule() {
        rules_section = rules_section.child(rule_detail(view, editor, id, entry, cx));
    } else {
        rules_section = rules_section.child(note(
            "acl-select-rule",
            "Select a rule to edit its target, scope, and permissions.",
        ));
    }
    let table_fits = draft.table().is_some();
    let toggle = |label: String, show: bool, cx: &mut Context<Desktop>| {
        styled_button(
            SharedString::from(format!("acl-show-rules-{token}")),
            label,
            Tone::Ghost,
            move |this, _, cx| {
                if let Some(editor) = active_editor(this, token) {
                    editor.show_rules = show;
                }
                cx.notify();
            },
            cx,
        )
    };
    body = if !table_fits {
        body.child(rules_section)
    } else if editor.show_rules {
        body.child(rules_section.child(toggle("Hide advanced rules".into(), false, cx)))
    } else {
        body.child(
            div()
                .mt_4()
                .child(toggle(format!("Show advanced rules ({count})"), true, cx)),
        )
    };
    body = body
        .child(groups(view, editor, cx))
        .child(check_access::render(view, editor, cx));
    if pending {
        body = body.child(note(
            "acl-waiting",
            "Editing is paused while the server request is pending.",
        ));
    }
    body.into_any_element()
}

/// A rule's setting for one permission: allowed, denied (deny wins when a
/// rule sets both) or neither.
fn setting(entry: &AclEntry, bit: u32) -> Option<bool> {
    if entry.deny & bit != 0 {
        Some(false)
    } else if entry.grant & bit != 0 {
        Some(true)
    } else {
        None
    }
}

pub(crate) fn set_permission(entry: &mut AclEntry, bit: u32, setting: Option<bool>) {
    entry.grant &= !bit;
    entry.deny &= !bit;
    match setting {
        Some(true) => entry.grant |= bit,
        Some(false) => entry.deny |= bit,
        None => {}
    }
}

const CHOICES: [(Option<bool>, &str); 3] = [
    (Some(true), "Allow"),
    (None, "Not set"),
    (Some(false), "Deny"),
];

/// One row per permission: its name, explained on hover, and Allow, Not set
/// and Deny, which an editable rule can choose. Server-wide permissions
/// appear in the root channel only.
fn permission_grid(
    token: u64,
    id: RowId,
    entry: &AclEntry,
    editable: bool,
    root: bool,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let mut grid = div().flex().flex_col().gap_1();
    for &(bit, label) in permission::NAMED
        .iter()
        .filter(|(bit, _)| root || *bit < permission::KICK)
    {
        let current = setting(entry, bit);
        let mut row = div().flex().flex_wrap().items_center().gap_2().child(
            div()
                .id(SharedString::from(format!("acl-help-{token}-{id}-{bit}")))
                .w(px(150.))
                .flex_shrink_0()
                .tooltip(move |_, cx| {
                    cx.new(|_| Tooltip(permission::description(bit).into()))
                        .into()
                })
                .child(SharedString::from(label)),
        );
        if editable {
            for (choice, text) in CHOICES {
                let tone = match (current == choice, choice) {
                    (true, Some(false)) => Tone::Danger,
                    (true, _) => Tone::Primary,
                    (false, _) => Tone::Ghost,
                };
                row = row.child(named_button(
                    SharedString::from(format!("acl-bit-{token}-{id}-{bit}-{text}")),
                    text,
                    format!("{label}: {text}"),
                    tone,
                    move |this, _, cx| {
                        change_rule(this, token, id, |editor| {
                            if let Some(entry) = editor
                                .state
                                .draft_mut()
                                .and_then(|draft| draft.entry_mut(id))
                            {
                                set_permission(entry, bit, choice);
                            }
                        });
                        cx.notify();
                    },
                    cx,
                ));
            }
        } else {
            let text = CHOICES
                .iter()
                .find(|(choice, _)| *choice == current)
                .map_or("", |(_, text)| text);
            row = row.child(note(
                SharedString::from(format!("acl-setting-{token}-{id}-{bit}")),
                text,
            ));
        }
        if entry.grant & entry.deny & bit != 0 {
            row = row.child(note(
                SharedString::from(format!("acl-both-{token}-{id}-{bit}")),
                "Allowed and denied: deny wins",
            ));
        }
        grid = grid.child(row);
    }
    grid.into_any_element()
}

/// Whom a group target means, in words for Mumble's built-in groups.
fn group_target(group: &str) -> String {
    match group {
        "all" => "Everyone".into(),
        "auth" => "Registered users".into(),
        "in" => "People in this channel".into(),
        "out" => "People outside this channel".into(),
        _ if group.starts_with('#') => format!("Password holders ({group})"),
        _ => format!("@{group}"),
    }
}

/// What a rule allows and denies, in words; `short` names at most three
/// permissions per list.
fn effect(entry: &AclEntry, short: bool) -> String {
    let names = |wanted: Option<bool>| {
        let names: Vec<_> = permission::NAMED
            .iter()
            .filter(|(bit, _)| setting(entry, *bit) == wanted)
            .map(|(_, label)| *label)
            .collect();
        if short && names.len() > 3 {
            format!("{}, {} and {} more", names[0], names[1], names.len() - 2)
        } else {
            names.join(", ")
        }
    };
    let (allowed, denied) = (names(Some(true)), names(Some(false)));
    match (allowed.is_empty(), denied.is_empty()) {
        (true, true) => "Sets no permissions".into(),
        (false, true) => format!("Allows {allowed}"),
        (true, false) => format!("Denies {denied}"),
        (false, false) => format!("Allows {allowed}; denies {denied}"),
    }
}

/// Mumble's built-in groups and the channel's groups, to pick as a rule's
/// target; a pick fills in the expression, which "Apply target" applies.
/// The search field that picks a rule's group, for `Editor::new`. A pick
/// fills the rule's target field, which Apply target then writes.
fn group_picker(token: u64, window: &mut Window, cx: &mut Context<Desktop>) -> Picker {
    Picker::new(
        "Group for this rule",
        "Pick a group",
        |this| match this.panel.as_mut() {
            Some(Panel::Acl { editor, .. }) => Some(&mut editor.group_picker),
            _ => None,
        },
        move |this, group, window, cx| {
            if let Some(editor) = active_editor(this, token)
                && editor.state.pending().is_none()
                && editor.rule().is_some()
            {
                editor.target.update(cx, |input, cx| {
                    input.set_value(group.to_owned(), window, cx)
                });
                editor.group_picker.clear(window, cx);
            }
            cx.notify();
        },
        window,
        cx,
    )
}

/// The built-in groups, then the channel's own and inherited ones.
fn group_choices(editor: &Editor) -> Vec<Choice> {
    ["all", "auth", "in", "out"]
        .into_iter()
        .map(|group| Choice {
            key: group.to_owned(),
            label: group_target(group),
            kind: "Built-in group",
        })
        .chain(editor.state.draft().into_iter().flat_map(|draft| {
            draft.groups().iter().map(|row| Choice {
                key: row.value.name.clone(),
                label: format!("@{}", row.value.name),
                kind: "Group",
            })
        }))
        .collect()
}

/// `control` with `help` shown on hover.
fn with_help(id: impl Into<SharedString>, help: &'static str, control: AnyElement) -> AnyElement {
    div()
        .id(id.into())
        .tooltip(move |_, cx| cx.new(|_| Tooltip(help.into())).into())
        .child(control)
        .into_any_element()
}

/// Where a rule applies, in words.
fn scope(entry: &AclEntry) -> &'static str {
    match (entry.apply_here, entry.apply_subs) {
        (true, true) => "Applies here and in subchannels",
        (true, false) => "Applies here only",
        (false, true) => "Applies in subchannels only",
        (false, false) => "Applies nowhere",
    }
}
fn unknown_permissions(entry: &AclEntry) -> Option<String> {
    let known = permission::NAMED
        .iter()
        .fold(0, |bits, (bit, _)| bits | bit);
    let unknown = (entry.grant | entry.deny) & !known;
    (unknown != 0).then(|| {
        format!(
            "Keeps {} permission settings from a newer server unchanged.",
            unknown.count_ones()
        )
    })
}

fn rule_detail(
    view: &ServerView,
    editor: &Editor,
    id: RowId,
    entry: &AclEntry,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let token = editor.token;
    if entry.inherited {
        let mut detail=div().min_w_0().flex().flex_col().gap_2()
            .child(note("acl-inherited-rule",if editor.state.draft().is_some_and(|draft|draft.inherit_acls()) {
                "This rule belongs to an ancestor channel and is read-only here."
            } else {"Parent ACL inheritance is disabled in this draft. This ancestor rule is inactive and read-only here."}))
            .child(note("acl-inherited-scope", scope(entry)))
            .child(permission_grid(token, id, entry, false, editor.state.draft().is_some_and(|draft| draft.channel() == 0), cx));
        if let Some(bits) = unknown_permissions(entry) {
            detail = detail.child(note("acl-inherited-unknown", bits));
        }
        return detail.into_any_element();
    }
    let user = editor.target_user;
    let target_value = editor.target.read(cx).value().to_string();
    let resolved = user
        .then(|| selected_account(editor, &target_value, false))
        .flatten();
    let mut detail = div()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_2()
        .when(target_pending(editor, cx), |detail| {
            detail
                .child(note(
                    "acl-unapplied-target",
                    "This target edit has not been applied to the rule.",
                ))
                .child(styled_button(
                    SharedString::from(format!("acl-discard-target-{token}-{id}")),
                    "Discard target edit",
                    Tone::Secondary,
                    move |this, window, cx| {
                        discard_target(this, token, id, window, cx);
                        cx.notify();
                    },
                    cx,
                ))
        })
        .child(
            div().flex().flex_wrap().gap_2().children(
                [(false, "Group expression"), (true, "Registered user")]
                    .into_iter()
                    .map(|(mode, label)| {
                        styled_button(
                            SharedString::from(format!("acl-mode-{token}-{id}-{mode}")),
                            label,
                            if mode == user {
                                Tone::Secondary
                            } else {
                                Tone::Ghost
                            },
                            move |this, window, cx| {
                                if let Some(editor) = active_editor(this, token) {
                                    if editor.state.pending().is_some()
                                        || editor.rule().is_none_or(|(selected, _)| selected != id)
                                    {
                                        return;
                                    }
                                    editor.target_user = mode;
                                    let value = editor
                                        .rule()
                                        .map(|(_, entry)| {
                                            if mode {
                                                entry
                                                    .user_id
                                                    .map(|id| format!("#{id}"))
                                                    .unwrap_or_default()
                                            } else {
                                                entry.group.clone().unwrap_or_default()
                                            }
                                        })
                                        .unwrap_or_default();
                                    editor
                                        .target
                                        .update(cx, |input, cx| input.set_value(value, window, cx));
                                }
                                cx.notify();
                            },
                            cx,
                        )
                    }),
            ),
        )
        .children((!user).then(|| {
            editor.group_picker.render(
                &format!("acl-pick-{}-{id}", editor.token),
                &group_choices(editor),
                cx,
            )
        }))
        .child(field(
            if user {
                "Registered account name or #id"
            } else {
                "Mumble group expression (all, auth, !group, $certificate, #token, …)"
            },
            &editor.target,
        ))
        .when(user, |detail| {
            detail.child(note(
                "acl-resolved-target",
                resolved
                    .map(|id| format!("Resolved: {}", name(view, id)))
                    .unwrap_or_else(|| {
                        "Resolve a registered account before applying this target.".into()
                    }),
            ))
        })
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .when(user, |row| {
                    row.child(styled_button(
                        SharedString::from(format!("acl-resolve-rule-{token}-{id}")),
                        "Resolve account",
                        Tone::Secondary,
                        move |this, _, cx| {
                            resolve(
                                this,
                                token,
                                LookupSelection {
                                    member: false,
                                    row: id,
                                },
                                cx,
                            );
                            cx.notify();
                        },
                        cx,
                    ))
                })
                .child(styled_button(
                    SharedString::from(format!("acl-apply-target-{token}-{id}-{user}")),
                    "Apply target",
                    Tone::Secondary,
                    move |this, _, cx| {
                        let Some(origin) = origin(this, token) else {
                            return;
                        };
                        let Some(_) = origin.live_index(this) else {
                            return;
                        };
                        let value = match this.panel.as_ref() {
                            Some(Panel::Acl { editor, .. }) => {
                                editor.target.read(cx).value().to_string()
                            }
                            _ => return,
                        };
                        let resolved = match this.panel.as_ref() {
                            Some(Panel::Acl { editor, .. })
                                if editor.rule().is_some_and(|(selected, _)| selected == id) =>
                            {
                                selected_account(editor, &value, false)
                            }
                            _ => return,
                        };
                        if user && resolved.is_none() {
                            fail(
                                this,
                                token,
                                "Resolve the registered account first. No rule was changed.",
                            );
                            cx.notify();
                            return;
                        }
                        if !user && (value.is_empty() || value.len() > 256) {
                            fail(this, token, "Enter a group expression of 1–256 bytes.");
                            cx.notify();
                            return;
                        }
                        change_rule(this, token, id, |editor| {
                            if let Some(entry) = editor
                                .state
                                .draft_mut()
                                .and_then(|draft| draft.entry_mut(id))
                            {
                                entry.user_id = if user { resolved } else { None };
                                entry.group = (!user).then_some(value);
                            }
                        });
                        cx.notify();
                    },
                    cx,
                )),
        );
    let scope = div().flex().flex_wrap().gap_2().children(
        [
            (true, entry.apply_here, "Apply to this channel"),
            (false, entry.apply_subs, "Apply to subchannels"),
        ]
        .into_iter()
        .map(|(here, on, label)| {
            let help = if here {
                "The rule counts in this channel."
            } else {
                "The rule counts in every channel below this one."
            };
            with_help(
                format!("acl-scope-help-{token}-{id}-{here}"),
                help,
                styled_button(
                    SharedString::from(format!("acl-scope-{token}-{id}-{here}")),
                    format!("{} {label}", if on { "✓" } else { "○" }),
                    Tone::Secondary,
                    move |this, _, cx| {
                        change_rule(this, token, id, |editor| {
                            if let Some(entry) = editor
                                .state
                                .draft_mut()
                                .and_then(|draft| draft.entry_mut(id))
                            {
                                if here {
                                    entry.apply_here = !entry.apply_here;
                                    if !entry.apply_here {
                                        entry.apply_subs = true;
                                    }
                                } else {
                                    entry.apply_subs = !entry.apply_subs;
                                    if !entry.apply_subs {
                                        entry.apply_here = true;
                                    }
                                }
                            }
                        });
                        cx.notify();
                    },
                    cx,
                ),
            )
        }),
    );
    let root = editor
        .state
        .draft()
        .is_some_and(|draft| draft.channel() == 0);
    detail = detail
        .child(scope)
        .child(permission_grid(token, id, entry, true, root, cx));
    if let Some(bits) = unknown_permissions(entry) {
        detail = detail.child(note("acl-unknown-bits", bits));
    }
    detail
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .children(
                    [(false, "Move rule up"), (true, "Move rule down")]
                        .into_iter()
                        .map(|(down, label)| {
                            styled_button(
                                SharedString::from(format!("acl-move-{token}-{id}-{down}")),
                                label,
                                Tone::Secondary,
                                move |this, _, cx| {
                                    change_rule(this, token, id, |editor| {
                                        if let Some(draft) = editor.state.draft_mut()
                                            && draft.move_entry(id, down)
                                        {
                                            editor.selected_rule = Some((
                                                if down {
                                                    editor.selected_rule.unwrap().0 + 1
                                                } else {
                                                    editor.selected_rule.unwrap().0 - 1
                                                },
                                                id,
                                            ));
                                            editor.rule_scroll.scroll_to_item(
                                                editor.selected_rule.unwrap().0,
                                                ScrollStrategy::Nearest,
                                            );
                                        }
                                    });
                                    cx.notify();
                                },
                                cx,
                            )
                        }),
                )
                .child(styled_button(
                    SharedString::from(format!("acl-remove-rule-{token}-{id}")),
                    "Remove rule",
                    Tone::Danger,
                    move |this, _, cx| {
                        change_rule(this, token, id, |editor| {
                            if let Some(draft) = editor.state.draft_mut()
                                && draft.remove_entry(id)
                            {
                                editor.selected_rule = None;
                            }
                        });
                        cx.notify();
                    },
                    cx,
                )),
        )
        .into_any_element()
}

fn groups(view: &ServerView, editor: &Editor, cx: &mut Context<Desktop>) -> AnyElement {
    let token = editor.token;
    let draft = editor.state.draft().unwrap();
    let count = draft.groups().len();
    let list = uniform_list(
        SharedString::from(format!("acl-groups-{token}")),
        count,
        cx.processor(move |this, range: Range<usize>, _, cx| {
            let mut rows = Vec::new();
            for index in range {
                let Some((id, label, selected)) = (|| {
                    let Panel::Acl { editor, .. } = this.panel.as_ref()? else {
                        return None;
                    };
                    if editor.token != token {
                        return None;
                    }
                    let row = editor.state.draft()?.groups().get(index)?;
                    Some((
                        row.id,
                        format!(
                            "@{}{}",
                            row.value.name,
                            if row.value.inherited {
                                " · inherited"
                            } else {
                                ""
                            }
                        ),
                        editor.selected_group == Some((index, row.id)),
                    ))
                })() else {
                    continue;
                };
                rows.push(row_button(
                    format!("acl-group-{token}-{id}"),
                    label,
                    selected,
                    move |this, _, cx| {
                        if let Some(editor) = active_editor(this, token)
                            && editor
                                .state
                                .draft()
                                .and_then(|draft| draft.groups().get(index))
                                .is_some_and(|row| row.id == id)
                        {
                            invalidate_lookup(editor, true);
                            editor.selected_group = Some((index, id));
                            for scroll in &editor.member_scroll {
                                scroll.scroll_to_item(0, ScrollStrategy::Top);
                            }
                        }
                        cx.notify();
                    },
                    cx,
                ));
            }
            rows
        }),
    )
    .track_scroll(&editor.group_scroll)
    .h(px(ROW_HEIGHT * count.clamp(1, 5) as f32))
    .w_full();
    let list = crate::dialogs::nested_virtual_list(
        "acl-groups",
        token,
        &editor.group_scroll,
        list,
        move |this| {
            matches!(this.panel.as_ref(), Some(Panel::Acl { editor, .. })
                if editor.token == token && !editor.stale)
        },
        cx,
    );
    let mut body = section(
        "acl-groups-title",
        format!("Groups · {count}"),
        "Named sets of people that rules and the table can name.",
    )
    .child(layout::focusable_with_handle(
        SharedString::from(format!("acl-group-focus-{token}")),
        editor.group_focus.clone(),
        div()
            .id(SharedString::from(format!("acl-group-list-{token}")))
            .role(Role::ListBox)
            .aria_label("ACL groups")
            .min_w_0()
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                let Some(editor) = active_editor(this, token) else {
                    return;
                };
                let count = editor.state.draft().map_or(0, |draft| draft.groups().len());
                let current = editor.selected_group.map_or(0, |(index, _)| index);
                if let Some(next) = list_key(event, current, count) {
                    window.focus(&editor.group_focus, cx);
                    select_group(this, token, next);
                    this.cancel_modal_scroll();
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(list),
    ))
    .child(field("New group name", &editor.group_name))
    .child(styled_button(
        SharedString::from(format!("acl-add-group-{token}")),
        "Create group",
        Tone::Secondary,
        move |this, _, cx| {
            let value = match active_editor(this, token) {
                Some(editor) => editor.group_name.read(cx).value().to_string(),
                None => return,
            };
            change(this, token, |editor| {
                if let Some(draft) = editor.state.draft_mut() {
                    match draft.add_group(&value) {
                        Ok(id) => {
                            editor.selected_group = Some((draft.groups().len() - 1, id));
                            editor
                                .group_scroll
                                .scroll_to_item(draft.groups().len() - 1, ScrollStrategy::Nearest);
                        }
                        Err(error) => {
                            editor.state.error = true;
                            editor.state.message = error.into();
                        }
                    }
                }
            });
            cx.notify();
        },
        cx,
    ));
    let Some((index, id)) = editor.selected_group else {
        return body.into_any_element();
    };
    let Some(group) = draft
        .groups()
        .get(index)
        .filter(|row| row.id == id)
        .map(|row| &row.value)
    else {
        return body.into_any_element();
    };
    body = body
        .child(note(
            "acl-selected-group",
            format!(
                "Editing @{}{}",
                group.name,
                if group.inherited {
                    " · local overrides of an inherited group"
                } else {
                    ""
                }
            ),
        ))
        .child(
            div().flex().flex_wrap().gap_2().children(
                [
                    (true, group.inherit, "Inherit parent members"),
                    (false, group.inheritable, "Inheritable by subchannels"),
                ]
                .into_iter()
                .map(|(inherit, on, label)| {
                    let help = if inherit {
                        "Members of the parent channel's group of this name are members here too."
                    } else {
                        "Subchannels can use this group and its members."
                    };
                    with_help(
                        format!("acl-group-flag-help-{token}-{id}-{inherit}"),
                        help,
                        styled_button(
                            SharedString::from(format!("acl-group-flag-{token}-{id}-{inherit}")),
                            format!("{} {label}", if on { "✓" } else { "○" }),
                            Tone::Secondary,
                            move |this, _, cx| {
                                change_group(this, token, id, |editor| {
                                    if let Some(group) = editor
                                        .state
                                        .draft_mut()
                                        .and_then(|draft| draft.group_mut(id))
                                    {
                                        if inherit {
                                            group.inherit = !group.inherit;
                                        } else {
                                            group.inheritable = !group.inheritable;
                                        }
                                    }
                                });
                                cx.notify();
                            },
                            cx,
                        ),
                    )
                }),
            ),
        )
        .child(field(
            "Registered group member (name or #id)",
            &editor.member,
        ));
    let value = editor.member.read(cx).value().to_string();
    body=body.child(note("acl-resolved-member",selected_account(editor,&value,true).map(|id|format!("Resolved: {}",name(view,id))).unwrap_or_else(||"Resolve an offline or online registered account before adding it.".into())))
        .child(div().flex().flex_wrap().gap_2()
            .child(styled_button(SharedString::from(format!("acl-resolve-member-{token}-{id}")),"Resolve member",Tone::Secondary,move|this,_,cx|{resolve(this,token,LookupSelection {member:true,row:id},cx);cx.notify();},cx))
            .children([(false,"Add member"),(true,"Exclude inherited member")].into_iter().map(|(exclude,label)| {
                styled_button(SharedString::from(format!("acl-member-add-{token}-{id}-{exclude}")),label,Tone::Secondary,move|this,_,cx| {
                    let Some(origin)=origin(this,token) else{return;};let Some(_)=origin.live_index(this) else{return;};
                    let value=match this.panel.as_ref(){Some(Panel::Acl{editor,..})=>editor.member.read(cx).value().to_string(),_=>return};
                    let Some(account)=(match this.panel.as_ref(){Some(Panel::Acl{editor,..}) if editor.selected_group.is_some_and(|(_,selected)|selected==id)=>selected_account(editor,&value,true),_=>return}) else{fail(this,token,"Resolve the registered member first. No membership was changed.");cx.notify();return;};
                    change_group(this,token,id,|editor|if let Some(group)=editor.state.draft_mut().and_then(|draft|draft.group_mut(id)) {
                        if group.add.len()+group.remove.len()+group.inherited_members.len()>=8192 {editor.state.error=true;editor.state.message="This group has reached the safe membership limit.".into();return;}
                        if exclude && !group.inherit {editor.state.error=true;editor.state.message="Enable inheritance before excluding an inherited member.".into();return;}
                        let members=if exclude{&mut group.remove}else{&mut group.add};if !members.contains(&account){members.push(account);}
                    });cx.notify();
                },cx)
            })))
        .children([("Local members",0,group.add.len()),("Local exclusions",1,group.remove.len()),("Inherited members (read-only)",2,group.inherited_members.len())].into_iter().map(|(title,kind,count)| {
            div().min_w_0().flex().flex_col().gap_1().child(note(SharedString::from(format!("acl-members-title-{kind}")),format!("{title} · {count}")))
                .child(member_list(editor,kind,count,cx)).into_any_element()
        }))
        .child(styled_button(SharedString::from(format!("acl-remove-group-{token}-{id}")),if group.inherited{"Reset local group overrides"}else{"Remove group"},Tone::Danger,move|this,_,cx| {
            change_group(this,token,id,|editor|if let Some(draft)=editor.state.draft_mut() && draft.remove_group(id){editor.selected_group=None;});cx.notify();
        },cx));
    body.into_any_element()
}
fn member_action(this: &mut Desktop, token: u64, group: RowId, kind: usize, account: u32) {
    change_group(this, token, group, |editor| {
        let Some(group) = editor
            .state
            .draft_mut()
            .and_then(|draft| draft.group_mut(group))
        else {
            return;
        };
        match kind {
            0 => group.add.retain(|id| *id != account),
            1 => group.remove.retain(|id| *id != account),
            _ => {
                if !group.inherit {
                    editor.state.error = true;
                    editor.state.message =
                        "Enable inheritance before excluding an inherited member.".into();
                    return;
                }
                if group.add.len() + group.remove.len() + group.inherited_members.len() >= 8192 {
                    editor.state.error = true;
                    editor.state.message =
                        "This group has reached the safe membership limit.".into();
                    return;
                }
                if !group.remove.contains(&account) {
                    group.remove.push(account);
                }
            }
        }
        editor.member_selected[kind] = None;
    });
}
fn member_list(
    editor: &Editor,
    kind: usize,
    count: usize,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let token = editor.token;
    let group_key = editor.selected_group.unwrap();
    let list = uniform_list(
        SharedString::from(format!("acl-members-{token}-{}-{kind}", group_key.1)),
        count,
        cx.processor(move |this, range: Range<usize>, _, cx| {
            let mut rows = Vec::new();
            for index in range {
                let Some((account, label, selected)) = (|| {
                    let Panel::Acl { origin, editor, .. } = this.panel.as_ref()? else {
                        return None;
                    };
                    if editor.token != token || editor.selected_group != Some(group_key) {
                        return None;
                    }
                    let group = &editor
                        .state
                        .draft()?
                        .groups()
                        .get(group_key.0)
                        .filter(|row| row.id == group_key.1)?
                        .value;
                    let members = match kind {
                        0 => &group.add,
                        1 => &group.remove,
                        _ => &group.inherited_members,
                    };
                    let account = *members.get(index)?;
                    let view = this.client.servers.get(origin.live_index(this)?)?;
                    Some((
                        account,
                        format!(
                            "{} · {}",
                            name(view, account),
                            if kind == 2 { "Exclude" } else { "Remove" }
                        ),
                        editor.member_selected[kind] == Some((index, account)),
                    ))
                })() else {
                    continue;
                };
                rows.push(row_button(
                    format!(
                        "acl-member-{token}-{}-{kind}-{account}-{index}",
                        group_key.1
                    ),
                    label,
                    selected,
                    move |this, _, cx| {
                        member_action(this, token, group_key.1, kind, account);
                        cx.notify();
                    },
                    cx,
                ));
            }
            rows
        }),
    )
    .track_scroll(&editor.member_scroll[kind])
    .h(px(ROW_HEIGHT * count.clamp(1, 4) as f32))
    .w_full();
    let id = match kind {
        0 => "acl-members",
        1 => "acl-exclusions",
        _ => "acl-inherited",
    };
    let list = crate::dialogs::nested_virtual_list(
        id,
        token,
        &editor.member_scroll[kind],
        list,
        move |this| {
            matches!(this.panel.as_ref(), Some(Panel::Acl { editor, .. })
                if editor.token == token && !editor.stale && editor.selected_group == Some(group_key))
        },
        cx,
    );
    let label = match kind {
        0 => "Local group members",
        1 => "Local group exclusions",
        _ => "Inherited group members",
    };
    layout::focusable_with_handle(
        SharedString::from(format!("acl-member-focus-{token}-{}-{kind}", group_key.1)),
        editor.member_focus[kind].clone(),
        div()
            .id(SharedString::from(format!(
                "acl-member-list-{token}-{}-{kind}",
                group_key.1
            )))
            .role(Role::ListBox)
            .aria_label(label)
            .min_w_0()
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                let Some(editor) = active_editor(this, token) else {
                    return;
                };
                if editor.selected_group != Some(group_key) {
                    return;
                }
                let Some(group) = editor
                    .state
                    .draft()
                    .and_then(|draft| draft.groups().get(group_key.0))
                    .filter(|row| row.id == group_key.1)
                else {
                    return;
                };
                let members = match kind {
                    0 => &group.value.add,
                    1 => &group.value.remove,
                    _ => &group.value.inherited_members,
                };
                let current = editor.member_selected[kind]
                    .filter(|(index, id)| members.get(*index) == Some(id))
                    .map_or(0, |(index, _)| index);
                if let Some(next) = list_key(event, current, members.len()) {
                    window.focus(&editor.member_focus[kind], cx);
                    editor.member_selected[kind] = Some((next, members[next]));
                    editor.member_scroll[kind].scroll_to_item(next, ScrollStrategy::Nearest);
                    this.cancel_modal_scroll();
                    cx.stop_propagation();
                    cx.notify();
                } else if matches!(event.keystroke.key.as_str(), "enter" | "space")
                    && let Some((index, account)) = editor.member_selected[kind]
                    && members.get(index) == Some(&account)
                {
                    member_action(this, token, group_key.1, kind, account);
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(list),
    )
}
