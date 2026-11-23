// SPDX-License-Identifier: EUPL-1.2
//! Channel permissions (#341): the channel's own rules as a table of rows
//! (Everyone, groups, people) and five columns. A cell cycles Not set, Allow
//! and Deny and rewrites the rules in the table's order. A channel type sets
//! the first three columns at once.
use super::{
    Editor, FLASH, Jump, MEMBER, active_editor, change, check_access, group_target, name, note,
    parents::{self, Parents},
    section, show_source,
};
use crate::{
    dialogs::{Tone, Tooltip, named_button},
    menus::MenuTarget,
    panels::{Panel, channel_name},
    picker::Picker,
    state::{Desktop, ServerView},
    theme::*,
};
use gpui_kit::{prelude::*, *};
use nuschl_core::{
    access::{self, Bits, COLUMNS, Cell, Kind, Who},
    acl::{self, Reason},
    admin,
    model::{Acl, permission},
};
use std::{collections::HashMap, ops::Range, rc::Rc, time::Instant};

fn label(view: &ServerView, who: &Who) -> String {
    match who {
        Who::Everyone => "Everyone".into(),
        Who::Group(group) => group_target(group),
        Who::Person(id) => name(view, *id),
    }
}

fn cycle(this: &mut Desktop, token: u64, who: Who, column: usize) {
    change(this, token, |editor| {
        let Some(draft) = editor.state.draft_mut() else {
            return;
        };
        let Some(mut table) = draft.table() else {
            return;
        };
        let bits = table.row(who);
        let next = match bits.cell(column) {
            Cell::NotSet | Cell::Custom => Cell::Allow,
            Cell::Allow => Cell::Deny,
            Cell::Deny => Cell::NotSet,
        };
        bits.set(column, next);
        if let Err(error) = draft.set_table(&table) {
            editor.state.error = true;
            editor.state.message = error.into();
        }
        editor.selected_rule = None;
    });
}

/// Gives the channel `kind`'s rules; Password takes the typed password.
fn pick(this: &mut Desktop, token: u64, kind: Kind, cx: &mut Context<Desktop>) {
    let Some(password) =
        active_editor(this, token).map(|editor| editor.password.read(cx).value().to_string())
    else {
        return;
    };
    change(this, token, |editor| {
        let Some(draft) = editor.state.draft_mut() else {
            return;
        };
        if let Err(error) = draft.set_kind(kind, &password) {
            editor.state.error = true;
            editor.state.message = error.into();
        }
        editor.selected_rule = None;
    });
}

/// The channel type: Open, Members only, Password, Announcements or Private,
/// or none for a custom table. Left out when the table cannot show the rules.
pub(super) fn kinds(view: &ServerView, editor: &Editor, cx: &mut Context<Desktop>) -> AnyElement {
    let token = editor.token;
    let Some(draft) = editor.state.draft().filter(|draft| draft.table().is_some()) else {
        return div().into_any_element();
    };
    let current = draft.kind();
    let options = Kind::ALL
        .into_iter()
        .map(|kind| {
            let action: crate::settings::Action =
                Rc::new(move |this, _, cx| pick(this, token, kind, cx));
            (kind.label(), current == Some(kind), action)
        })
        .collect();
    section(
        "acl-kind-title",
        "Channel type",
        "Who may join, speak and write. The table shows the details.",
    )
    .child(crate::settings::segmented(
        "acl-kind",
        "Channel type",
        options,
        cx,
    ))
    .child(note(
        "acl-kind-help",
        current.map_or("Custom", |kind| kind.help(!view.capabilities.is_empty())),
    ))
    .into_any_element()
}

/// The search field that adds a row, for `Editor::new`.
pub(super) fn picker(token: u64, window: &mut Window, cx: &mut Context<Desktop>) -> Picker {
    Picker::new(
        "Add a row",
        "Add someone: a group or a person",
        |this| match this.panel.as_mut() {
            Some(Panel::Acl { editor, .. }) => Some(&mut editor.table_picker),
            _ => None,
        },
        move |this, key, window, cx| {
            change(this, token, |editor| {
                if let Some(who) = check_access::from_key(key)
                    && !editor.table_rows.contains(&who)
                {
                    editor.table_rows.push(who);
                }
            });
            if let Some(editor) = active_editor(this, token) {
                editor.table_picker.clear(window, cx);
            }
            cx.notify();
        },
        window,
        cx,
    )
}

/// Clears everything `who`'s row sets here; inherited values stay.
fn remove(this: &mut Desktop, token: u64, who: Who) {
    change(this, token, |editor| {
        editor.table_rows.retain(|row| *row != who);
        let Some(draft) = editor.state.draft_mut() else {
            return;
        };
        let Some(mut table) = draft.table() else {
            return;
        };
        table.rows.retain(|(row, _)| *row != who);
        if let Err(error) = draft.set_table(&table) {
            editor.state.error = true;
            editor.state.message = error.into();
        }
        editor.selected_rule = None;
    });
}

/// What a member of `who`'s row may do in the channel, why, and the tree the
/// reasons point into. Until the parents' rules are read, the channel's own
/// rule list stands in for the path (`flat`).
fn evaluate(
    parents: &Parents,
    snapshot: &Acl,
    base: &acl::Node,
    who: &Who,
) -> (u32, [Reason; 32], HashMap<u32, acl::Node>, bool) {
    let channel = snapshot.channel;
    let (tree, flat) = if parents.done {
        (parents::tree(&parents.acls, snapshot, &[], who), false)
    } else {
        let mut node = base.clone();
        if let Who::Group(group) = who {
            node.groups
                .entry(group.clone())
                .or_default()
                .add
                .insert(MEMBER);
        }
        (HashMap::from([(channel, node)]), true)
    };
    let (granted, reasons) = acl::explain(&tree, &parents::subject(who, channel, None), channel);
    (granted, reasons, tree, flat)
}

/// The table row a rule belongs to.
fn row_of(entry: &acl::AclEntry) -> Who {
    match (entry.user_id, entry.group.as_str()) {
        (Some(id), _) => Who::Person(id),
        (None, "all") => Who::Everyone,
        (None, group) => Who::Group(group.to_owned()),
    }
}

/// What decided a permission, in words, and the other channel and row it was
/// set in.
fn source(
    reason: Reason,
    tree: &HashMap<u32, acl::Node>,
    flat: bool,
    snapshot: &Acl,
    who: &Who,
    view: &ServerView,
) -> (String, Option<(u32, Who)>) {
    match reason {
        Reason::Default => ("Mumble's default".into(), None),
        Reason::Rule { channel, index } => {
            let Some(entry) = tree.get(&channel).and_then(|node| node.acls.get(index)) else {
                return ("a rule".into(), None);
            };
            let target = match entry.user_id {
                Some(id) => name(view, id),
                None => group_target(&entry.group),
            };
            if flat
                && snapshot
                    .entries
                    .get(index)
                    .is_some_and(|entry| entry.inherited)
            {
                (format!("a {target} rule in a parent channel"), None)
            } else if channel != snapshot.channel {
                (
                    format!("the {target} row in {}", channel_name(view, channel)),
                    Some((channel, row_of(entry))),
                )
            } else if row_of(entry) == *who {
                ("this row".into(), None)
            } else {
                (format!("the {target} row"), None)
            }
        }
        Reason::Write => ("Write ACL".into(), None),
        Reason::Superuser => ("SuperUser".into(), None),
        Reason::Unreachable { channel } => (
            format!("Traverse denied in {}", channel_name(view, channel)),
            None,
        ),
    }
}

/// The server the open editor belongs to.
fn server_index(this: &Desktop) -> Option<usize> {
    match this.panel.as_ref()? {
        Panel::Acl { origin, .. } => origin.live_index(this),
        _ => None,
    }
}

/// For a partly allowed column, which permissions are allowed.
fn partly(bits: u32, allowed: u32) -> String {
    if allowed == 0 || allowed == bits {
        return String::new();
    }
    let names = |wanted: bool| {
        permission::NAMED
            .iter()
            .filter(|(bit, _)| bits & bit != 0 && (allowed & bit != 0) == wanted)
            .map(|(_, label)| *label)
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(" Allows {}; not {}.", names(true), names(false))
}

fn row_help(who: &Who) -> String {
    match who {
        Who::Everyone => "Everyone, unless a group or person row sets the same column.".into(),
        Who::Group(group) if group == "auth" => {
            "Everyone with a registered account, unless a person row sets the same column.".into()
        }
        Who::Group(group) => {
            format!("Members of @{group}, unless a person row sets the same column.")
        }
        Who::Person(_) => "This registered account.".into(),
    }
}

/// Permissions outside the columns that a row sets, in words.
fn others(grant: u32, deny: u32) -> Option<String> {
    let shown = COLUMNS.iter().fold(0, |bits, (_, column, _)| bits | column);
    let names = |bits: u32| {
        permission::NAMED
            .iter()
            .filter(|(bit, _)| bits & bit & !shown != 0)
            .map(|(_, label)| *label)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let (allowed, denied) = (names(grant), names(deny));
    match (allowed.is_empty(), denied.is_empty()) {
        (true, true) => None,
        (false, true) => Some(format!("Also allows {allowed}")),
        (true, false) => Some(format!("Also denies {denied}")),
        (false, false) => Some(format!("Also allows {allowed}; denies {denied}")),
    }
}

/// Rows the table shows at most; the list scrolls beyond them.
const VISIBLE_ROWS: usize = 8;
const TABLE_ROW: f32 = 40.;

/// What the table's visible rows are built from, once per frame.
struct Rows {
    snapshot: Acl,
    base: acl::Node,
    own: Vec<(Who, Bits)>,
    targets: Vec<Who>,
}

fn bits_of(rows: &[(Who, Bits)], who: &Who) -> Bits {
    rows.iter()
        .find(|(row, _)| row == who)
        .map_or_else(Bits::default, |(_, bits)| *bits)
}

fn line() -> Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .px_3()
        .py_1()
        .border_t_1()
        .border_color(color(BORDER))
}

/// One row of the table: who, the five cells and Remove.
fn row(
    view: &ServerView,
    editor: &Editor,
    rows: &Rows,
    number: usize,
    cx: &mut Context<Desktop>,
) -> AnyElement {
    let token = editor.token;
    let Some(who) = rows.targets.get(number) else {
        return div().into_any_element();
    };
    let own = bits_of(&rows.own, who);
    let row_label = label(view, who);
    // The list lays rows out at their content's width unless they fill it;
    // filled, the cells line up under the header's columns.
    let mut row = line().w_full().h(px(TABLE_ROW)).child(
        div()
            .id(SharedString::from(format!("acl-row-{token}-{number}")))
            .w(px(130.))
            .flex_shrink_0()
            .truncate()
            .tooltip({
                let help = row_help(who);
                move |_, cx| cx.new(|_| Tooltip(help.clone().into())).into()
            })
            .child(SharedString::from(row_label.clone())),
    );
    let (granted, reasons, tree, flat) = evaluate(&editor.parents, &rows.snapshot, &rows.base, who);
    for (column, (heading, bits, _)) in COLUMNS.iter().enumerate() {
        let allowed = granted & bits;
        let (label, state) = match allowed {
            0 => ("No", "Not allowed"),
            _ if allowed == *bits => ("Yes", "Allowed"),
            _ => ("Partly", "Partly allowed"),
        };
        let detail = partly(*bits, allowed);
        let cell = own.cell(column);
        // Where an inherited value was set: (channel, row, menu label).
        let mut jumps: Vec<(u32, Who, String)> = Vec::new();
        let (tone, set_here, help) = if cell == Cell::NotSet {
            let mut sources: Vec<String> = Vec::new();
            for bit in (0..32).filter(|bit| bits & (1 << bit) != 0) {
                let (text, from) = source(reasons[bit], &tree, flat, &rows.snapshot, who, view);
                if !sources.contains(&text) {
                    sources.push(text);
                }
                if let Some((channel, row)) = from
                    && !jumps.iter().any(|(seen, _, _)| *seen == channel)
                {
                    jumps.push((
                        channel,
                        row,
                        format!("Show in {}", channel_name(view, channel)),
                    ));
                }
            }
            let menu = if jumps.is_empty() {
                ""
            } else {
                " Right-click to show where it is set."
            };
            (
                Tone::Ghost,
                false,
                format!(
                    "{state} by {}.{detail} Click to set it here.{menu}",
                    sources.join(" and ")
                ),
            )
        } else {
            let tone = match cell {
                Cell::Allow => Tone::Primary,
                Cell::Deny => Tone::Danger,
                Cell::Custom | Cell::NotSet => Tone::Secondary,
            };
            (
                tone,
                true,
                format!("{state}, set here.{detail} Click to change it."),
            )
        };
        let to_jump = move |server: usize, (channel, who, label): &(u32, Who, String)| {
            (
                Jump {
                    server,
                    channel: *channel,
                    who: who.clone(),
                    column,
                },
                label.clone(),
            )
        };
        let target = who.clone();
        let first = jumps.first().cloned();
        let click = move |this: &mut Desktop, window: &mut Window, cx: &mut Context<Desktop>| {
            // Alt-click goes where an inherited value is set.
            if window.modifiers().alt {
                if let (Some(server), Some(first)) = (server_index(this), first.as_ref()) {
                    show_source(this, to_jump(server, first).0);
                }
            } else {
                cycle(this, token, target.clone(), column);
            }
            cx.notify();
        };
        let button = named_button(
            SharedString::from(format!("acl-cell-{token}-{number}-{column}")),
            label,
            format!(
                "{row_label} · {heading}: {label}, {}",
                if set_here { "set here" } else { "not set here" }
            ),
            tone,
            click,
            cx,
        );
        let flash = editor
            .flash
            .as_ref()
            .filter(|flash| flash.who == *who && flash.column == column)
            .map(|flash| {
                let first = flash.shown.get().is_none();
                let shown = *flash.shown.get().get_or_insert_with(Instant::now);
                flash.shown.set(Some(shown));
                let t = (shown.elapsed().as_secs_f32() / FLASH.as_secs_f32()).min(1.);
                // Ease out: strong at first, settling gently.
                let strength = if cx.reduce_motion() {
                    f32::from(u8::from(t < 1.))
                } else {
                    (1. - t).powi(3)
                };
                (first, strength)
            });
        row = row.child(
            div()
                .id(SharedString::from(format!(
                    "acl-cell-help-{token}-{number}-{column}"
                )))
                .relative()
                .flex_1()
                .min_w_0()
                .rounded(px(6.))
                .when_some(flash, |cell, (first, strength)| {
                    cell.bg(color(ACCENT).opacity(0.55 * strength))
                        .when(first, |cell| {
                            // Bring the arrived-at cell into view once.
                            cell.child(
                                canvas(
                                    |bounds, window, _| window.request_autoscroll(bounds),
                                    |_, _, _, _| {},
                                )
                                .absolute()
                                .size_full(),
                            )
                        })
                })
                .tooltip(move |_, cx| cx.new(|_| Tooltip(help.clone().into())).into())
                .when(!jumps.is_empty(), |cell| {
                    cell.on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            let Some(server) = server_index(this) else {
                                return;
                            };
                            this.menu_acl =
                                jumps.iter().map(|jump| to_jump(server, jump)).collect();
                            this.open_menu(server, MenuTarget::AclSource, event.position);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                })
                .child(
                    div()
                        .when(!set_here, |cell| cell.opacity(0.6))
                        .child(button),
                ),
        );
    }
    let mut end = div().w(px(84.)).flex_shrink_0();
    if own != Bits::default() || editor.table_rows.contains(who) {
        let target = who.clone();
        end = end.child(named_button(
            SharedString::from(format!("acl-remove-row-{token}-{number}")),
            "Remove",
            format!("Remove {row_label}"),
            Tone::Ghost,
            move |this, _, cx| {
                remove(this, token, target.clone());
                cx.notify();
            },
            cx,
        ));
    }
    row.child(end).into_any_element()
}

pub(super) fn render(view: &ServerView, editor: &Editor, cx: &mut Context<Desktop>) -> AnyElement {
    let token = editor.token;
    let section = section(
        "acl-table-title",
        "Channel permissions",
        "What each person or group may do in this channel and its subchannels. A person's setting beats their groups'; a group's beats Everyone's. Dimmed cells come from defaults, other rows or parent channels.",
    );
    let Some(draft) = editor.state.draft() else {
        return section.into_any_element();
    };
    let Some(mut table) = draft.table() else {
        return section
            .child(note(
                "acl-table-advanced",
                "This channel's rules depend on their order in ways the table cannot show. Edit them under Advanced rules.",
            ))
            .into_any_element();
    };
    for who in &editor.table_rows {
        table.row(who.clone());
    }
    let snapshot = draft.snapshot();
    let base = admin::flat_node(&snapshot);
    let inherited = if draft.inherit_acls() {
        access::inherited(&snapshot.entries)
    } else {
        Vec::new()
    };
    let mut targets: Vec<Who> = table
        .rows
        .iter()
        .chain(&inherited)
        .map(|(who, _)| who.clone())
        .collect();
    targets.sort();
    targets.dedup();
    let mut header = div()
        .flex()
        .items_center()
        .gap_1()
        .px_3()
        // Over the rows' scrollbar gutter, so the columns line up.
        .pr(px(12. + crate::layout::SCROLLBAR_GUTTER))
        .py_2()
        .bg(color(ELEVATED))
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .child(
            div()
                .w(px(130.))
                .flex_shrink_0()
                .child(text!("Who").with_id("acl-column-who")),
        );
    for (column, (heading, _, help)) in COLUMNS.iter().enumerate() {
        header = header.child(
            div()
                .id(SharedString::from(format!("acl-column-{token}-{column}")))
                .flex_1()
                .min_w_0()
                .pl_3()
                .tooltip(move |_, cx| cx.new(|_| Tooltip((*help).into())).into())
                .child(text!(*heading).with_id(("acl-column-label", column))),
        );
    }
    let mut grid = div()
        .flex()
        .flex_col()
        .border_1()
        .border_color(color(BORDER))
        .rounded_md()
        .overflow_hidden()
        .child(header.child(div().w(px(84.)).flex_shrink_0()));
    if targets.is_empty() {
        grid = grid.child(line().py_2().child(note(
            "acl-table-empty",
            "Nothing set here or above. Add someone below.",
        )));
    } else {
        let count = targets.len();
        if let Some(index) = editor
            .flash
            .as_ref()
            .filter(|flash| flash.shown.get().is_none())
            .and_then(|flash| targets.iter().position(|who| *who == flash.who))
        {
            // The arrived-at row draws, and its cell then comes into view.
            editor
                .table_scroll
                .scroll_to_item(index, ScrollStrategy::Center);
        }
        let rows = Rc::new(Rows {
            snapshot,
            base,
            own: table.rows.clone(),
            targets: targets.clone(),
        });
        let list = uniform_list(
            SharedString::from(format!("acl-table-rows-{token}")),
            count,
            cx.processor(move |this, range: Range<usize>, _, cx| {
                let Some(Panel::Acl { origin, editor, .. }) = this.panel.as_ref() else {
                    return Vec::new();
                };
                let view = origin
                    .live_index(this)
                    .and_then(|index| this.client.servers.get(index));
                match view {
                    Some(view) if editor.token == token => range
                        .map(|number| row(view, editor, &rows, number, cx))
                        .collect(),
                    _ => Vec::new(),
                }
            }),
        )
        .track_scroll(&editor.table_scroll)
        .h(px(TABLE_ROW * count.min(VISIBLE_ROWS) as f32))
        .w_full();
        grid = grid.child(crate::dialogs::nested_virtual_list(
            "acl-table",
            token,
            &editor.table_scroll,
            list,
            move |this| {
                matches!(this.panel.as_ref(), Some(Panel::Acl { editor, .. })
                    if editor.token == token && !editor.stale)
            },
            cx,
        ));
    }
    let mut section = section.child(grid);
    // Permissions outside the columns, under the table: its rows are of one height.
    for (number, who) in targets.iter().enumerate() {
        let own = bits_of(&table.rows, who);
        if let Some(others) = others(own.grant, own.deny) {
            section = section.child(note(
                SharedString::from(format!("acl-table-others-{token}-{number}")),
                format!("{}: {others}", label(view, who)),
            ));
        }
    }
    let picker = editor.table_picker.render(
        &format!("acl-table-add-{token}"),
        &check_access::choices(view, draft, &targets),
        cx,
    );
    section.child(picker).into_any_element()
}
