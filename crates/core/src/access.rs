// SPDX-License-Identifier: EUPL-1.2
//! Who may do what (#341): a channel's own rules as a table. Rows are
//! Everyone, groups and people; five columns show sets of Mumble permissions as
//! Allow, Deny, Not set or Custom. The table compiles to one rule order
//! (Everyone, group denies, group allows, people), so a person beats their
//! groups, groups beat Everyone, and among groups allow beats deny.
use crate::{acl::perm::ROOT_ONLY, model::AclEntry, model::permission::*};

/// Column label, permissions and hover help.
pub const COLUMNS: [(&str, u32, &str); 5] = [
    (
        "See & join",
        TRAVERSE | ENTER | LISTEN,
        "See the channel, enter it and listen to it.",
    ),
    ("Speak", SPEAK | WHISPER, "Talk and whisper in the channel."),
    ("Chat", TEXT_MESSAGE, "Write messages in the channel."),
    (
        "Moderate",
        MUTE_DEAFEN | MOVE,
        "Mute, deafen and move people in the channel.",
    ),
    (
        "Manage channel",
        WRITE | MAKE_CHANNEL | MAKE_TEMP_CHANNEL | LINK_CHANNEL,
        "Change the channel and its permissions, add and link channels.",
    ),
];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Who {
    Everyone,
    /// A group name, including Mumble's `auth` (registered users).
    Group(String),
    /// A registered account.
    Person(u32),
}

/// What a row allows and denies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bits {
    pub grant: u32,
    pub deny: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cell {
    NotSet,
    Allow,
    Deny,
    /// Some of the column's permissions are set, or set differently.
    Custom,
}

impl Bits {
    pub fn cell(self, column: usize) -> Cell {
        let bits = COLUMNS[column].1;
        match (self.grant & bits, self.deny & bits) {
            (0, 0) => Cell::NotSet,
            (granted, 0) if granted == bits => Cell::Allow,
            (0, denied) if denied == bits => Cell::Deny,
            _ => Cell::Custom,
        }
    }

    /// Sets every permission of `column`.
    pub fn set(&mut self, column: usize, cell: Cell) {
        let bits = COLUMNS[column].1;
        self.grant &= !bits;
        self.deny &= !bits;
        match cell {
            Cell::Allow => self.grant |= bits,
            Cell::Deny => self.deny |= bits,
            Cell::NotSet | Cell::Custom => {}
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    pub rows: Vec<(Who, Bits)>,
}

impl Table {
    /// The row for `who`, added with nothing set if missing.
    pub fn row(&mut self, who: Who) -> &mut Bits {
        let index = match self.rows.iter().position(|(row, _)| *row == who) {
            Some(index) => index,
            None => {
                self.rows.push((who, Bits::default()));
                self.rows.len() - 1
            }
        };
        &mut self.rows[index].1
    }
}

fn rule(who: &Who, grant: u32, deny: u32) -> AclEntry {
    AclEntry {
        apply_here: true,
        apply_subs: true,
        inherited: false,
        user_id: match who {
            Who::Person(id) => Some(*id),
            _ => None,
        },
        group: match who {
            Who::Everyone => Some("all".into()),
            Who::Group(name) => Some(name.clone()),
            Who::Person(_) => None,
        },
        grant,
        deny,
    }
}

/// The table's rules in their one order; rows with nothing set drop out.
pub fn compile(table: &Table) -> Vec<AclEntry> {
    let mut rows: Vec<_> = table.rows.iter().collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut rules = Vec::new();
    for (who, bits) in &rows {
        if *who == Who::Everyone {
            rules.push(rule(who, bits.grant, bits.deny));
        }
    }
    for allow in [false, true] {
        for (who, bits) in &rows {
            if matches!(who, Who::Group(_)) {
                rules.push(if allow {
                    rule(who, bits.grant, 0)
                } else {
                    rule(who, 0, bits.deny)
                });
            }
        }
    }
    for (who, bits) in &rows {
        if matches!(who, Who::Person(_)) {
            rules.push(rule(who, bits.grant, bits.deny));
        }
    }
    rules.retain(|rule| rule.grant | rule.deny != 0);
    rules
}

/// The row a rule belongs to, if the table can show its target.
pub fn who(entry: &AclEntry) -> Option<Who> {
    match (entry.user_id, entry.group.as_deref()) {
        (Some(id), _) => Some(Who::Person(id)),
        (None, Some("all")) => Some(Who::Everyone),
        (None, Some(name))
            if !name.is_empty()
                && !name.starts_with(['!', '~', '#', '$'])
                && !["in", "out", "none", "strong", "sub"].contains(&name)
                && !name.starts_with("sub,") =>
        {
            Some(Who::Group(name.to_owned()))
        }
        _ => None,
    }
}

/// A rule's place in the table's order for a permission it sets: Everyone,
/// group denies, group allows, people.
fn rank(who: &Who, allow: bool) -> u8 {
    match who {
        Who::Everyone => 0,
        Who::Group(_) => 1 + u8::from(allow),
        Who::Person(_) => 3,
    }
}

/// Reads `rules` (a channel's own, password rules left out) into a table, or
/// `None` when the table's order would change what they decide. Rules apply
/// here and below; a here-only rule may hold root-only permissions, which
/// apply only where they are set.
pub fn read(rules: &[AclEntry]) -> Option<Table> {
    let mut table = Table::default();
    let mut parsed = Vec::with_capacity(rules.len());
    for entry in rules {
        let who = who(entry)?;
        let reach = entry.apply_subs || (entry.grant | entry.deny) & !ROOT_ONLY == 0;
        if entry.inherited || !entry.apply_here || !reach || entry.grant & entry.deny != 0 {
            return None;
        }
        let row = table.row(who.clone());
        if row.grant & entry.deny != 0 || row.deny & entry.grant != 0 {
            return None;
        }
        row.grant |= entry.grant;
        row.deny |= entry.deny;
        parsed.push((who, entry.grant, entry.deny));
    }
    // A later opposite setting for the same permission must also come later in
    // the table's order; people never match each other's rules.
    for bit in (0..32).map(|shift| 1u32 << shift) {
        let mut latest = [None::<u8>; 2];
        for (who, grant, deny) in &parsed {
            let allow = match (grant & bit, deny & bit) {
                (0, 0) => continue,
                (0, _) => false,
                _ => true,
            };
            let rank = rank(who, allow);
            if latest[usize::from(!allow)]
                .is_some_and(|earlier| earlier > rank || (earlier == rank && rank != 3))
            {
                return None;
            }
            let slot = &mut latest[usize::from(allow)];
            *slot = Some(slot.map_or(rank, |seen| seen.max(rank)));
        }
    }
    Some(table)
}

/// The permissions channel types set: the See & join, Speak and Chat columns.
pub const ACCESS: u32 = COLUMNS[0].1 | COLUMNS[1].1 | COLUMNS[2].1;

/// Channel types (#341): templates of the access columns. Moderators are
/// `@admin`, which Murmur creates in the root and, with the creator, in each
/// new channel; members are registered users (`auth`). Password also holds
/// Mumble's password rules, which the draft keeps outside the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Open,
    Members,
    Password,
    Announcements,
    Private,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Open,
        Kind::Members,
        Kind::Password,
        Kind::Announcements,
        Kind::Private,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Open => "Open",
            Kind::Members => "Members only",
            Kind::Password => "Password",
            Kind::Announcements => "Announcements",
            Kind::Private => "Private",
        }
    }

    /// What the type does, on a server that sends clients only the channels
    /// they may traverse or an access token would open (`hidden`, nuschld's
    /// `hidden-channels/1`) or on one that sends every channel to everyone,
    /// as Murmur does.
    pub fn help(self, hidden: bool) -> &'static str {
        match self {
            Kind::Open => "Anyone may join, speak and write.",
            Kind::Members => "Registered users may join, speak and write.",
            Kind::Password => {
                "People with the password may join, speak and write. Others see the channel."
            }
            Kind::Announcements => "Only @admin may speak. Everyone may listen and write.",
            Kind::Private if hidden => {
                "Only @admin and people you add may join. Others don't see the channel."
            }
            Kind::Private => "Only @admin and people you add may join. Others see the channel.",
        }
    }

    /// The access columns the type sets, by row, in the table's order.
    fn rows(self) -> Vec<(Who, Bits)> {
        let speak = COLUMNS[1].1;
        let admin = Who::Group("admin".into());
        let deny = |deny| Bits { grant: 0, deny };
        let grant = |grant| Bits { grant, deny: 0 };
        match self {
            Kind::Open | Kind::Password => Vec::new(),
            // Traverse stays, so a subchannel may admit everyone again.
            Kind::Members => vec![
                (Who::Everyone, deny(ACCESS & !TRAVERSE)),
                (Who::Group("auth".into()), grant(ACCESS)),
            ],
            Kind::Announcements => vec![(Who::Everyone, deny(speak)), (admin, grant(speak))],
            Kind::Private => vec![(Who::Everyone, deny(ACCESS)), (admin, grant(ACCESS))],
        }
    }
}

/// Gives `table` `kind`'s access columns; other permissions stay.
pub fn apply(table: &mut Table, kind: Kind) {
    for (_, bits) in &mut table.rows {
        bits.grant &= !ACCESS;
        bits.deny &= !ACCESS;
    }
    for (who, set) in kind.rows() {
        let bits = table.row(who);
        bits.grant |= set.grant;
        bits.deny |= set.deny;
    }
}

/// The type whose access columns `table` holds, for a channel with or
/// without a password; `None` is a custom setup.
pub fn kind(table: &Table, password: bool) -> Option<Kind> {
    let mut access: Vec<_> = table
        .rows
        .iter()
        .map(|(who, bits)| {
            let bits = Bits {
                grant: bits.grant & ACCESS,
                deny: bits.deny & ACCESS,
            };
            (who.clone(), bits)
        })
        .filter(|(_, bits)| *bits != Bits::default())
        .collect();
    access.sort_by(|a, b| a.0.cmp(&b.0));
    Kind::ALL
        .into_iter()
        .find(|kind| (*kind == Kind::Password) == password && kind.rows() == access)
}

/// What the columns let people do, for the effect summary.
const VERBS: [&str; 5] = ["join", "speak", "write", "moderate", "manage"];

/// How a change alters one column for one row (#341): `who` may now, or no
/// longer, do all of the column's permissions, here and in `below`
/// subchannels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effect {
    pub who: Who,
    pub column: usize,
    pub allowed: bool,
    pub here: bool,
    pub below: usize,
}

/// What changes for each of `rows` in `channel` and its subchannels `below`;
/// `permissions` gives a row's effective permissions in a channel before and
/// after the change.
pub fn effects(
    rows: &[Who],
    channel: u32,
    below: &[u32],
    permissions: impl Fn(&Who, u32) -> (u32, u32),
) -> Vec<Effect> {
    let mut effects: Vec<Effect> = Vec::new();
    for who in rows {
        for id in std::iter::once(channel).chain(below.iter().copied()) {
            let (before, after) = permissions(who, id);
            for (column, (_, bits, _)) in COLUMNS.iter().enumerate() {
                let allowed = after & bits == *bits;
                if allowed == (before & bits == *bits) {
                    continue;
                }
                let index = effects
                    .iter()
                    .position(|effect| {
                        effect.who == *who && effect.column == column && effect.allowed == allowed
                    })
                    .unwrap_or_else(|| {
                        effects.push(Effect {
                            who: who.clone(),
                            column,
                            allowed,
                            here: false,
                            below: 0,
                        });
                        effects.len() - 1
                    });
                if id == channel {
                    effects[index].here = true;
                } else {
                    effects[index].below += 1;
                }
            }
        }
    }
    effects
}

/// `effects` in short lines, one per row, direction and place, at most
/// `limit` of them and then "and N more".
pub fn summary(effects: &[Effect], label: impl Fn(&Who) -> String, limit: usize) -> Vec<String> {
    let mut lines: Vec<(&Effect, Vec<&str>)> = Vec::new();
    for effect in effects {
        let same = |(first, _): &&mut (&Effect, Vec<&str>)| {
            (&first.who, first.allowed, first.here, first.below)
                == (&effect.who, effect.allowed, effect.here, effect.below)
        };
        match lines.iter_mut().find(same) {
            Some((_, verbs)) => verbs.push(VERBS[effect.column]),
            None => lines.push((effect, vec![VERBS[effect.column]])),
        }
    }
    let subchannels = |count| match count {
        1 => "1 subchannel".to_owned(),
        _ => format!("{count} subchannels"),
    };
    let mut text: Vec<String> = lines
        .iter()
        .take(limit)
        .map(|(effect, verbs)| {
            let verbs = match verbs.split_last() {
                Some((last, [])) => (*last).to_owned(),
                Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
                None => String::new(),
            };
            let place = match (effect.here, effect.below) {
                (true, 0) => "here".to_owned(),
                (true, below) => format!("here and in {}", subchannels(below)),
                (false, below) => format!("in {}", subchannels(below)),
            };
            let may = if effect.allowed {
                "may now"
            } else {
                "may no longer"
            };
            format!("{} {may} {verbs} {place}", label(&effect.who))
        })
        .collect();
    if lines.len() > limit {
        text.push(format!("and {} more", lines.len() - limit));
    }
    text
}

/// What a permission lets people do, for descriptions of rules.
fn verb(bit: u32) -> &'static str {
    match bit {
        WRITE => "edit permissions",
        TRAVERSE => "pass through",
        ENTER => "enter",
        SPEAK => "speak",
        MUTE_DEAFEN => "mute and deafen people",
        MOVE => "move people",
        MAKE_CHANNEL => "make channels",
        LINK_CHANNEL => "link channels",
        WHISPER => "whisper",
        TEXT_MESSAGE => "write messages",
        MAKE_TEMP_CHANNEL => "make temporary channels",
        LISTEN => "listen",
        KICK => "kick people",
        BAN => "ban people",
        REGISTER => "register people",
        SELF_REGISTER => "register",
        RESET_USER_CONTENT => "reset people's content",
        _ => "use a newer permission",
    }
}

/// `items` as "a, b and c" (or "or c").
fn list(items: &[&str], last: &str) -> String {
    match items.split_last() {
        Some((only, [])) => (*only).to_owned(),
        Some((final_item, rest)) => format!("{} {last} {final_item}", rest.join(", ")),
        None => String::new(),
    }
}

/// Whom a rule's group expression means, as Mumble evaluates it
/// (`acl::group_applies`).
fn whom(specification: &str) -> String {
    let (mut name, mut invert, mut here, mut token, mut hash) =
        (specification, false, false, false, false);
    loop {
        let flag = match name.chars().next() {
            Some('!') => &mut invert,
            Some('~') => &mut here,
            Some('#') => &mut token,
            Some('$') => &mut hash,
            _ => break,
        };
        *flag = true;
        name = &name[1..];
    }
    if name.is_empty() {
        return "nobody".to_owned();
    }
    let people = if token {
        format!("people with the access token \u{201c}{name}\u{201d}")
    } else if hash {
        format!("certificate {}…", name.get(..8).unwrap_or(name))
    } else {
        match name {
            "all" => "everyone".to_owned(),
            "auth" => "registered users".to_owned(),
            "strong" => "people with a verified certificate".to_owned(),
            "none" => "nobody".to_owned(),
            "in" if here => "people in this channel".to_owned(),
            "in" => "people in the channel".to_owned(),
            "out" if here => "people outside this channel".to_owned(),
            "out" => "people outside the channel".to_owned(),
            "sub" if here => "people in this channel's subchannels".to_owned(),
            "sub" => "people in the channel's subchannels".to_owned(),
            _ if name.starts_with("sub,") => {
                format!(
                    "people in subchannels ({}{name})",
                    if here { "~" } else { "" }
                )
            }
            _ if here => format!("members of @{name} as defined here"),
            _ => format!("members of @{name}"),
        }
    };
    if invert {
        format!("everyone except {people}")
    } else {
        people
    }
}

/// A channel's own rules (#341) in short sentences, in their order. "But"
/// starts a rule that reverses an earlier one for the people both cover; a
/// deny for Everyone reads "Nobody may". `name` names an account; at most
/// `limit` lines, then "and N more".
pub fn describe(rules: &[AclEntry], name: impl Fn(u32) -> String, limit: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for (index, rule) in rules.iter().enumerate() {
        let deny = rule.deny;
        let grant = rule.grant & !deny;
        if grant | deny == 0 {
            continue;
        }
        let verbs = |bits: u32| {
            (0..32)
                .map(|shift| 1u32 << shift)
                .filter(|bit| bits & bit != 0)
                .map(verb)
                .collect::<Vec<_>>()
        };
        let everyone = rule.user_id.is_none() && rule.group.as_deref() == Some("all");
        let what = match (grant, deny) {
            (0, _) if everyone => format!("may {}", list(&verbs(deny), "or")),
            (0, _) => format!("may not {}", list(&verbs(deny), "or")),
            (_, 0) => format!("may {}", list(&verbs(grant), "and")),
            _ => format!(
                "may {} but not {}",
                list(&verbs(grant), "and"),
                list(&verbs(deny), "or")
            ),
        };
        let place = match (rule.apply_here, rule.apply_subs) {
            (true, true) => "here and in subchannels",
            (true, false) => "here",
            (false, true) => "in subchannels",
            (false, false) => "nowhere",
        };
        let who = match rule.user_id {
            Some(id) => name(id),
            None if everyone && grant == 0 => "nobody".to_owned(),
            None => whom(rule.group.as_deref().unwrap_or_default()),
        };
        let reverses = rules[..index]
            .iter()
            .any(|earlier| earlier.grant & deny != 0 || earlier.deny & grant != 0);
        let sentence = if reverses {
            format!("But {who} {what} {place}.")
        } else {
            let mut who = who;
            if let Some(first) = who.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            format!("{who} {what} {place}.")
        };
        lines.push(sentence);
    }
    if lines.len() > limit {
        let more = lines.len() - limit;
        lines.truncate(limit);
        lines.push(format!("and {more} more"));
    }
    lines
}

/// What each target inherits from the parent channels: `entries` is the
/// channel's rule list, inherited rules first; a later rule overrides an
/// earlier one for the permissions it sets. Rules with group expressions are
/// left to the rule list.
pub fn inherited(entries: &[AclEntry]) -> Vec<(Who, Bits)> {
    let mut rows = Table::default();
    for entry in entries.iter().filter(|entry| entry.inherited) {
        let Some(who) = who(entry) else { continue };
        let bits = rows.row(who);
        bits.grant = (bits.grant & !entry.deny) | entry.grant;
        bits.deny = (bits.deny & !entry.grant) | entry.deny;
    }
    rows.rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule_for(who: &Who, grant: u32, deny: u32) -> AclEntry {
        rule(who, grant, deny)
    }

    fn table() -> Table {
        let mut table = Table::default();
        table.row(Who::Person(7)).set(1, Cell::Allow);
        table.row(Who::Group("guests".into())).set(1, Cell::Deny);
        table.row(Who::Group("guests".into())).set(0, Cell::Allow);
        table.row(Who::Everyone).set(0, Cell::Deny);
        table.row(Who::Group("mods".into())).set(3, Cell::Allow);
        table
    }

    #[test]
    fn rules_follow_specificity_and_read_back() {
        let rules = compile(&table());
        let order: Vec<_> = rules
            .iter()
            .map(|rule| (rule.user_id, rule.group.clone(), rule.grant, rule.deny))
            .collect();
        let see = TRAVERSE | ENTER | LISTEN;
        assert_eq!(
            order,
            [
                (None, Some("all".into()), 0, see),
                (None, Some("guests".into()), 0, SPEAK | WHISPER),
                (None, Some("guests".into()), see, 0),
                (None, Some("mods".into()), MUTE_DEAFEN | MOVE, 0),
                (Some(7), None, SPEAK | WHISPER, 0),
            ]
        );
        let back = read(&rules).expect("table-shaped rules read back");
        assert_eq!(compile(&back), rules);
    }

    #[test]
    fn murmurs_default_root_rules_read_as_a_table() {
        let admin = Who::Group("admin".into());
        let auth = Who::Group("auth".into());
        let mut everyone = rule_for(&Who::Everyone, SELF_REGISTER, 0);
        everyone.apply_subs = false;
        let rules = [
            rule_for(&admin, WRITE, 0),
            rule_for(&auth, MAKE_TEMP_CHANNEL, 0),
            everyone,
        ];
        let table = read(&rules).expect("Murmur's defaults fit the table");
        let manage = table.rows.iter().find(|(who, _)| *who == admin).unwrap().1;
        assert_eq!(manage.cell(4), Cell::Custom);
        assert_eq!(manage.cell(0), Cell::NotSet);
    }

    #[test]
    fn rules_whose_order_matters_otherwise_stay_advanced() {
        let guests = Who::Group("guests".into());
        // Everyone's deny after a group's allow overrides the group.
        let late_deny = [
            rule_for(&guests, ENTER, 0),
            rule_for(&Who::Everyone, 0, ENTER),
        ];
        assert_eq!(read(&late_deny), None);
        // A group's deny after a person's allow overrides the person.
        let person_first = [
            rule_for(&Who::Person(3), SPEAK, 0),
            rule_for(&guests, 0, SPEAK),
        ];
        assert_eq!(read(&person_first), None);
        // Different permissions never conflict.
        let independent = [
            rule_for(&guests, ENTER, 0),
            rule_for(&Who::Everyone, 0, SPEAK),
        ];
        assert!(read(&independent).is_some());
        let mut here_only = rule_for(&guests, ENTER, 0);
        here_only.apply_subs = false;
        assert_eq!(read(&[here_only]), None);
        let mut expression = rule_for(&guests, ENTER, 0);
        expression.group = Some("~mods".into());
        assert_eq!(read(&[expression]), None);
        assert_eq!(read(&[]), Some(Table::default()));
    }

    #[test]
    fn inherited_rules_fold_per_target() {
        let auth = Who::Group("auth".into());
        let mut parent = rule_for(&auth, SPEAK | MAKE_TEMP_CHANNEL, 0);
        parent.inherited = true;
        let mut grandchild_override = rule_for(&auth, 0, SPEAK);
        grandchild_override.inherited = true;
        let own = rule_for(&auth, 0, ENTER);
        let mut expression = rule_for(&Who::Everyone, 0, ENTER);
        expression.group = Some("~admin".into());
        expression.inherited = true;
        let rows = inherited(&[parent, grandchild_override, expression, own]);
        assert_eq!(
            rows,
            [(
                auth,
                Bits {
                    grant: MAKE_TEMP_CHANNEL,
                    deny: SPEAK,
                }
            )]
        );
    }

    /// Root(0) ─ Edited(1) ─ Sub(2) ─ Deeper(3); Edited ─ Own(4), whose own
    /// rule lets registered users speak. Account 7 is in @guests.
    fn subtree(
        edited: Vec<crate::acl::AclEntry>,
    ) -> std::collections::HashMap<u32, crate::acl::Node> {
        use crate::acl::{AclEntry, Group, Node};
        let mut guests = Group::new();
        guests.add.insert(7);
        let node = |parent, acls| Node {
            parent,
            inherit_acl: true,
            acls,
            groups: Default::default(),
        };
        let mut root = node(None, Vec::new());
        root.groups.insert("guests".into(), guests);
        let own = AclEntry {
            apply_here: true,
            apply_subs: true,
            user_id: None,
            group: "auth".into(),
            grant: SPEAK | WHISPER,
            deny: 0,
        };
        [
            (0, root),
            (1, node(Some(0), edited)),
            (2, node(Some(1), Vec::new())),
            (3, node(Some(2), Vec::new())),
            (4, node(Some(1), vec![own])),
        ]
        .into()
    }

    fn compare(before: Vec<crate::acl::AclEntry>, after: Vec<crate::acl::AclEntry>) -> Vec<Effect> {
        use crate::acl::{Subject, effective_permissions};
        let (before, after) = (subtree(before), subtree(after));
        let rows = [
            Who::Everyone,
            Who::Group("auth".into()),
            Who::Group("guests".into()),
        ];
        effects(&rows, 1, &[2, 3, 4], |who, channel| {
            let subject = Subject {
                user_id: match who {
                    Who::Everyone => None,
                    Who::Group(group) if group == "auth" => Some(3),
                    _ => Some(7),
                },
                channel,
                hash: None,
                tokens: &[],
                strong: false,
            };
            (
                effective_permissions(&before, &subject, channel),
                effective_permissions(&after, &subject, channel),
            )
        })
    }

    fn entry(group: &str, grant: u32, deny: u32) -> crate::acl::AclEntry {
        crate::acl::AclEntry {
            apply_here: true,
            apply_subs: true,
            user_id: None,
            group: group.into(),
            grant,
            deny,
        }
    }

    #[test]
    fn effects_follow_inheritance_into_subchannels() {
        let label = |who: &Who| match who {
            Who::Everyone => "Everyone".to_owned(),
            Who::Group(group) => format!("@{group}"),
            Who::Person(id) => format!("#{id}"),
        };
        // Denying registered users Speak reaches Sub and Deeper; Own allows it.
        let denied = compare(vec![], vec![entry("auth", 0, SPEAK | WHISPER)]);
        assert_eq!(
            summary(&denied, label, 3),
            [
                "@auth may no longer speak here and in 2 subchannels",
                "@guests may no longer speak here and in 2 subchannels",
            ]
        );
        // Without Traverse, Everyone loses everything here and below; @guests
        // keep it.
        let everyone = entry("all", 0, ENTER | TRAVERSE | LISTEN | TEXT_MESSAGE);
        let closed = compare(
            vec![],
            vec![
                everyone.clone(),
                entry("guests", ENTER | TRAVERSE | LISTEN | TEXT_MESSAGE, 0),
            ],
        );
        assert_eq!(
            summary(&closed, label, 1),
            [
                "Everyone may no longer join, speak and write here and in 3 subchannels",
                "and 1 more",
            ]
        );
        let lines = summary(&closed, label, 3);
        assert_eq!(
            lines[1..],
            ["@auth may no longer join, speak and write here and in 3 subchannels"]
        );
        // A change confined to the channel itself names only it.
        let mut here_only = entry("guests", MUTE_DEAFEN | MOVE, 0);
        here_only.apply_subs = false;
        let moderators = compare(vec![], vec![here_only]);
        assert_eq!(
            summary(&moderators, label, 3),
            ["@guests may now moderate here"]
        );
        assert_eq!(compare(vec![everyone.clone()], vec![everyone]), []);
    }

    #[test]
    fn advanced_rules_read_as_sentences_in_their_order() {
        let mut admin = rule_for(&Who::Group("admin".into()), MOVE, 0);
        admin.apply_subs = false;
        let mut certificate = rule_for(&Who::Everyone, ENTER, 0);
        certificate.group = Some("$0123456789abcdef".into());
        let name = |id: u32| format!("Ada (#{id})");
        let lines = describe(
            &[admin, rule_for(&Who::Everyone, 0, ENTER), certificate],
            name,
            8,
        );
        assert_eq!(
            lines,
            [
                "Members of @admin may move people here.",
                "Nobody may enter here and in subchannels.",
                "But certificate 01234567… may enter here and in subchannels.",
            ]
        );
        // Dynamic groups, negation and `~`, a person, both directions.
        let expression = |group: &str, grant, deny| {
            let mut rule = rule_for(&Who::Everyone, grant, deny);
            rule.group = Some(group.into());
            rule
        };
        let rules = [
            expression("in", SPEAK, 0),
            expression("~out", 0, TEXT_MESSAGE),
            expression("!~admin", 0, MUTE_DEAFEN | MOVE),
            expression("~sub,0,1", LISTEN, 0),
            expression("#door", ENTER | SPEAK, LINK_CHANNEL),
            rule_for(&Who::Person(5), WRITE, 0),
        ];
        assert_eq!(
            describe(&rules, name, 8),
            [
                "People in the channel may speak here and in subchannels.",
                "People outside this channel may not write messages here and in subchannels.",
                "Everyone except members of @admin as defined here may not mute and deafen people or move people here and in subchannels.",
                "People in subchannels (~sub,0,1) may listen here and in subchannels.",
                "People with the access token \u{201c}door\u{201d} may enter and speak but not link channels here and in subchannels.",
                "Ada (#5) may edit permissions here and in subchannels.",
            ]
        );
        // A later rule that reverses an earlier one says so; the list is bounded.
        let order = [
            rule_for(&Who::Group("guests".into()), SPEAK, 0),
            rule_for(&Who::Everyone, 0, SPEAK),
            rule_for(&Who::Group("auth".into()), 0, 0),
        ];
        assert_eq!(
            describe(&order, name, 1),
            [
                "Members of @guests may speak here and in subchannels.",
                "and 1 more"
            ]
        );
        assert_eq!(
            describe(&order, name, 8)[1],
            "But nobody may speak here and in subchannels."
        );
    }

    #[test]
    fn only_private_reads_differently_where_hidden_channels_stay_hidden() {
        for kind in Kind::ALL {
            assert_eq!(kind.help(true) == kind.help(false), kind != Kind::Private);
        }
        assert!(
            Kind::Private
                .help(true)
                .ends_with("Others don't see the channel.")
        );
        assert!(
            Kind::Password
                .help(true)
                .ends_with("Others see the channel.")
        );
    }

    #[test]
    fn a_column_is_set_whole() {
        let mut bits = Bits {
            grant: ENTER,
            deny: 0,
        };
        assert_eq!(bits.cell(0), Cell::Custom);
        bits.set(0, Cell::Deny);
        assert_eq!(bits.cell(0), Cell::Deny);
        assert_eq!(bits.deny, TRAVERSE | ENTER | LISTEN);
        bits.set(0, Cell::NotSet);
        assert_eq!(bits, Bits::default());
    }
}
