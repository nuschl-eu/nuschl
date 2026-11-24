// SPDX-License-Identifier: EUPL-1.2
//! UI-independent ACL drafts. Remote snapshots are never edited in place.
//!
//! Row keys belong to one draft lifetime and are never reused, including after
//! explicit reload. Account IDs refer to the registered-user database, not to
//! online sessions. Wire correlation and connection authority belong to adapters.
use crate::model::{Acl, AclEntry, AclGroup, permission};

/// What Mumble's channel password admits: `#<password>` is granted these and
/// `@all` denied them, in two local rules that apply in the channel only.
const PASSWORD: u32 = permission::ENTER
    | permission::SPEAK
    | permission::WHISPER
    | permission::TEXT_MESSAGE
    | permission::LINK_CHANNEL
    | permission::TRAVERSE;

/// The local rule that holds the password (`ChanACL::isPassword`). Before
/// Mumble 1.4 the rules left out Traverse.
fn is_password(entry: &AclEntry) -> bool {
    entry
        .group
        .as_deref()
        .is_some_and(|group| group.starts_with('#'))
        && entry.apply_here
        && !entry.inherited
        && entry.deny == 0
        && (entry.grant == PASSWORD || entry.grant == PASSWORD & !permission::TRAVERSE)
}

/// The `@all` rule that keeps everyone else out of a password channel.
fn is_password_lock(entry: &AclEntry) -> bool {
    entry.group.as_deref() == Some("all")
        && entry.apply_here
        && !entry.inherited
        && entry.grant == 0
        && (entry.deny == PASSWORD || entry.deny == PASSWORD & !permission::TRAVERSE)
}

pub type RowId = u64;

/// A locally correlated response. The legacy wire carries no request ID.
#[derive(Clone, Debug)]
pub struct AclReply {
    pub request: u64,
    pub result: Result<Acl, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RegisteredAccount {
    pub id: u32,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct AccountReply {
    pub request: u64,
    pub result: Result<RegisteredAccount, String>,
}

#[derive(Clone, Debug)]
pub struct Row<T> {
    pub id: RowId,
    pub value: T,
}

#[derive(Clone, Debug)]
pub struct AclDraft {
    baseline: Acl,
    inherit_acls: bool,
    entries: Vec<Row<AclEntry>>,
    groups: Vec<Row<AclGroup>>,
    next_row: RowId,
}

impl AclDraft {
    pub fn new(acl: Acl) -> Self {
        let mut draft = Self {
            baseline: acl.clone(),
            inherit_acls: acl.inherit_acls,
            entries: Vec::new(),
            groups: Vec::new(),
            next_row: 1,
        };
        draft.reload(acl);
        draft
    }

    /// Explicit discard/reload. Previously rendered row callbacks remain invalid.
    pub fn reload(&mut self, acl: Acl) {
        self.entries.clear();
        self.groups.clear();
        for entry in &acl.entries {
            let id = self.allocate();
            self.entries.push(Row {
                id,
                value: entry.clone(),
            });
        }
        for group in &acl.groups {
            let id = self.allocate();
            self.groups.push(Row {
                id,
                value: group.clone(),
            });
        }
        self.inherit_acls = acl.inherit_acls;
        self.baseline = acl;
    }

    fn allocate(&mut self) -> RowId {
        let id = self.next_row;
        self.next_row = self.next_row.checked_add(1).expect("ACL row ID exhausted");
        id
    }

    pub fn channel(&self) -> u32 {
        self.baseline.channel
    }
    pub fn inherit_acls(&self) -> bool {
        self.inherit_acls
    }
    pub fn set_inherit_acls(&mut self, inherit: bool) {
        self.inherit_acls = inherit;
    }
    pub fn entries(&self) -> &[Row<AclEntry>] {
        &self.entries
    }
    pub fn groups(&self) -> &[Row<AclGroup>] {
        &self.groups
    }
    pub fn baseline_matches(&self, current: &Acl) -> bool {
        canonical_acl(&self.baseline) == canonical_acl(current)
    }

    pub fn dirty(&self) -> bool {
        self.inherit_acls != self.baseline.inherit_acls
            || self.entries.len() != self.baseline.entries.len()
            || self.groups.len() != self.baseline.groups.len()
            || self
                .entries
                .iter()
                .map(|row| &row.value)
                .ne(&self.baseline.entries)
            || self
                .groups
                .iter()
                .map(|row| &row.value)
                .ne(&self.baseline.groups)
    }

    pub fn snapshot(&self) -> Acl {
        Acl {
            channel: self.channel(),
            inherit_acls: self.inherit_acls,
            entries: self.entries.iter().map(|row| row.value.clone()).collect(),
            groups: self.groups.iter().map(|row| row.value.clone()).collect(),
        }
    }

    pub fn entry_mut(&mut self, id: RowId) -> Option<&mut AclEntry> {
        self.entries
            .iter_mut()
            .find(|row| row.id == id && !row.value.inherited)
            .map(|row| &mut row.value)
    }

    pub fn add_entry(&mut self) -> Result<RowId, &'static str> {
        if self.entries.len() >= 4096 {
            return Err("The editor supports at most 4096 rules.");
        }
        let id = self.allocate();
        self.entries.push(Row {
            id,
            value: AclEntry {
                apply_here: true,
                apply_subs: true,
                inherited: false,
                user_id: None,
                group: Some("all".into()),
                grant: 0,
                deny: 0,
            },
        });
        Ok(id)
    }

    pub fn remove_entry(&mut self, id: RowId) -> bool {
        if let Some(index) = self
            .entries
            .iter()
            .position(|row| row.id == id && !row.value.inherited)
        {
            self.entries.remove(index);
            true
        } else {
            false
        }
    }

    /// Move only local rules. Inherited rules retain their position and authority.
    pub fn move_entry(&mut self, id: RowId, down: bool) -> bool {
        let Some(index) = self
            .entries
            .iter()
            .position(|row| row.id == id && !row.value.inherited)
        else {
            return false;
        };
        let Some(other) = (if down {
            index.checked_add(1)
        } else {
            index.checked_sub(1)
        }) else {
            return false;
        };
        if self
            .entries
            .get(other)
            .is_none_or(|row| row.value.inherited)
        {
            return false;
        }
        self.entries.swap(index, other);
        true
    }

    pub fn group_mut(&mut self, id: RowId) -> Option<&mut AclGroup> {
        self.groups
            .iter_mut()
            .find(|row| row.id == id)
            .map(|row| &mut row.value)
    }

    pub fn add_group(&mut self, name: &str) -> Result<RowId, &'static str> {
        if self.groups.len() >= 1024 {
            return Err("The editor supports at most 1024 groups.");
        }
        let name = name.trim().to_lowercase();
        if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
            return Err("Use a group name between 1 and 256 bytes without control characters.");
        }
        if self
            .groups
            .iter()
            .any(|row| row.value.name.eq_ignore_ascii_case(&name))
        {
            return Err("That group already exists. Select it to edit its local overrides.");
        }
        let id = self.allocate();
        self.groups.push(Row {
            id,
            value: AclGroup {
                name,
                inherited: false,
                inherit: true,
                inheritable: true,
                add: Vec::new(),
                remove: Vec::new(),
                inherited_members: Vec::new(),
            },
        });
        Ok(id)
    }

    /// The channel password, as Mumble's channel dialog shows it.
    pub fn password(&self) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|row| is_password(&row.value))
            .and_then(|row| row.value.group.as_deref()?.strip_prefix('#'))
    }

    /// Sets, changes or (with an empty `password`) removes the channel
    /// password the way Mumble's channel dialog does (`updatePasswordACL`).
    pub fn set_password(&mut self, password: &str) -> Result<(), &'static str> {
        let current = self.entries.iter().rposition(|row| is_password(&row.value));
        if password.is_empty() {
            if let Some(index) = current {
                self.entries.remove(index);
                if let Some(lock) = self
                    .entries
                    .iter()
                    .rposition(|row| is_password_lock(&row.value))
                {
                    self.entries.remove(lock);
                }
            }
            return Ok(());
        }
        if password.len() > 255 || password.chars().any(char::is_control) {
            return Err("Use a password of at most 255 bytes without control characters.");
        }
        if let Some(index) = current {
            self.entries[index].value.group = Some(format!("#{password}"));
            return Ok(());
        }
        if self.entries.len() + 2 > 4096 {
            return Err("The editor supports at most 4096 rules.");
        }
        for (group, grant, deny) in [
            ("all".to_owned(), 0, PASSWORD),
            (format!("#{password}"), PASSWORD, 0),
        ] {
            let id = self.allocate();
            self.entries.push(Row {
                id,
                value: AclEntry {
                    apply_here: true,
                    apply_subs: false,
                    inherited: false,
                    user_id: None,
                    group: Some(group),
                    grant,
                    deny,
                },
            });
        }
        Ok(())
    }

    /// The channel's own rules as a who-may-do-what table, password rules
    /// aside; `None` when the table cannot express them.
    pub fn table(&self) -> Option<crate::access::Table> {
        let rules: Vec<_> = self
            .entries
            .iter()
            .map(|row| &row.value)
            .filter(|entry| !entry.inherited && !is_password(entry) && !is_password_lock(entry))
            .cloned()
            .collect();
        crate::access::read(&rules)
    }

    /// Replaces the channel's own rules with `table`'s, keeping inherited rules
    /// first and the password rules last.
    pub fn set_table(&mut self, table: &crate::access::Table) -> Result<(), &'static str> {
        let rules = crate::access::compile(table);
        let password = |row: &Row<AclEntry>| {
            !row.value.inherited && (is_password(&row.value) || is_password_lock(&row.value))
        };
        let kept = self
            .entries
            .iter()
            .filter(|row| row.value.inherited || password(row))
            .count();
        if kept + rules.len() > 4096 {
            return Err("The editor supports at most 4096 rules.");
        }
        let (inherited, own): (Vec<_>, Vec<_>) = std::mem::take(&mut self.entries)
            .into_iter()
            .partition(|row| row.value.inherited);
        let password: Vec<_> = own.into_iter().filter(password).collect();
        self.entries = inherited;
        for value in rules {
            let id = self.allocate();
            self.entries.push(Row { id, value });
        }
        self.entries.extend(password);
        Ok(())
    }

    /// The channel's type, if its own rules match one.
    pub fn kind(&self) -> Option<crate::access::Kind> {
        crate::access::kind(&self.table()?, self.password().is_some())
    }

    /// Gives the channel `kind`'s rules. Password sets `password`, or keeps
    /// the current password when it is empty; other types remove it.
    pub fn set_kind(
        &mut self,
        kind: crate::access::Kind,
        password: &str,
    ) -> Result<(), &'static str> {
        let mut table = self
            .table()
            .ok_or("This channel uses advanced rules. Edit them below.")?;
        if kind != crate::access::Kind::Password {
            self.set_password("")?;
        } else if !password.is_empty() {
            self.set_password(password)?;
        } else if self.password().is_none() {
            return Err("Type a password first.");
        }
        crate::access::apply(&mut table, kind);
        self.set_table(&table)
    }

    /// An inherited group cannot be deleted at its parent. Removing its local
    /// definition restores the inherited defaults, exactly as stock Mumble does.
    pub fn remove_group(&mut self, id: RowId) -> bool {
        let Some(index) = self.groups.iter().position(|row| row.id == id) else {
            return false;
        };
        if self.groups[index].value.inherited {
            let group = &mut self.groups[index].value;
            group.inherit = true;
            group.inheritable = true;
            group.add.clear();
            group.remove.clear();
        } else {
            self.groups.remove(index);
        }
        true
    }
}

/// Query provenance (`inherited`) does not mean there is no local definition.
/// Preserve inherited groups whose local flags or membership override a parent.
pub fn group_has_local_definition(group: &AclGroup) -> bool {
    !group.inherited
        || !group.inherit
        || !group.inheritable
        || !group.add.is_empty()
        || !group.remove.is_empty()
}

/// Whether a new channel's `acl` names this account or certificate as its
/// creator: Murmur and nuschld add a registered creator to the channel's own
/// `admin` group, and give a creator without Write a rule granting it.
pub fn created_by(acl: &Acl, user_id: Option<u32>, hash: Option<&str>) -> bool {
    let admin = acl
        .groups
        .iter()
        .any(|group| group.name == "admin" && user_id.is_some_and(|id| group.add.contains(&id)));
    let hash = hash.map(|hash| format!("${hash}"));
    admin
        || acl.entries.iter().any(|entry| {
            !entry.inherited
                && entry.grant & permission::WRITE != 0
                && ((entry.user_id.is_some() && entry.user_id == user_id)
                    || (hash.is_some() && entry.group == hash))
        })
}

/// What `acl`'s own channel defines, as the permission model reads it: the
/// channel's own rules and groups. Inherited copies in a query reply come from
/// the parents' nodes instead.
pub fn node(acl: &Acl, parent: Option<u32>) -> crate::acl::Node {
    crate::acl::Node {
        parent,
        inherit_acl: acl.inherit_acls,
        acls: acl
            .entries
            .iter()
            .filter(|entry| !entry.inherited)
            .map(|entry| crate::acl::AclEntry {
                apply_here: entry.apply_here,
                apply_subs: entry.apply_subs,
                user_id: entry.user_id,
                group: entry.group.clone().unwrap_or_default(),
                grant: entry.grant,
                deny: entry.deny,
            })
            .collect(),
        groups: acl
            .groups
            .iter()
            .filter(|group| group_has_local_definition(group))
            .map(|group| {
                (
                    group.name.clone(),
                    crate::acl::Group {
                        inherit: group.inherit,
                        inheritable: group.inheritable,
                        add: group.add.iter().copied().collect(),
                        remove: group.remove.iter().copied().collect(),
                    },
                )
            })
            .collect(),
    }
}

/// `acl`'s whole rule list as one node, to evaluate permissions in its
/// channel: inherited rules apply here (while the channel inherits), and each
/// group holds its effective members. `acls[i]` is `acl.entries[i]`.
pub fn flat_node(acl: &Acl) -> crate::acl::Node {
    crate::acl::Node {
        parent: None,
        inherit_acl: true,
        acls: acl
            .entries
            .iter()
            .map(|entry| {
                let active = !entry.inherited || acl.inherit_acls;
                crate::acl::AclEntry {
                    apply_here: active && (entry.apply_here || entry.inherited),
                    apply_subs: active && entry.apply_subs,
                    user_id: entry.user_id,
                    group: entry.group.clone().unwrap_or_default(),
                    grant: entry.grant,
                    deny: entry.deny,
                }
            })
            .collect(),
        groups: acl
            .groups
            .iter()
            .map(|group| {
                let mut members: std::collections::BTreeSet<u32> = if group.inherit {
                    group.inherited_members.iter().copied().collect()
                } else {
                    Default::default()
                };
                members.extend(&group.add);
                for removed in &group.remove {
                    members.remove(removed);
                }
                (
                    group.name.clone(),
                    crate::acl::Group {
                        inherit: false,
                        inheritable: true,
                        add: members,
                        remove: Default::default(),
                    },
                )
            })
            .collect(),
    }
}

/// A UI chooses globally unique tickets and validates its original connection
/// before issuing effects. Pending requests never permit editing the draft.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftRequest {
    Load,
    Reload,
    Check,
    Verify,
}

#[derive(Clone, Debug, Default)]
pub struct AclEditorState {
    channel: u32,
    draft: Option<AclDraft>,
    pending: Option<(u64, DraftRequest)>,
    submitted: Option<Acl>,
    pub message: String,
    pub error: bool,
}

impl AclEditorState {
    pub fn new(channel: u32) -> Self {
        Self {
            channel,
            ..Default::default()
        }
    }
    pub fn draft(&self) -> Option<&AclDraft> {
        self.draft.as_ref()
    }
    pub fn draft_mut(&mut self) -> Option<&mut AclDraft> {
        if self.pending.is_some() || self.submitted.is_some() {
            None
        } else {
            self.draft.as_mut()
        }
    }
    pub fn pending(&self) -> Option<u64> {
        self.pending.map(|(request, _)| request)
    }
    pub fn begin(&mut self, request: u64, kind: DraftRequest) -> bool {
        if self.pending.is_some()
            || (self.submitted.is_some() && kind != DraftRequest::Verify)
            || (kind == DraftRequest::Load && self.draft.is_some())
            || (kind == DraftRequest::Check && self.draft.is_none())
            || (kind == DraftRequest::Verify && self.submitted.is_none())
        {
            return false;
        }
        self.pending = Some((request, kind));
        self.error = false;
        self.message = match kind {
            DraftRequest::Load => "Loading permissions…",
            DraftRequest::Reload => "Reloading permissions; waiting before discarding the draft…",
            DraftRequest::Check => "Checking for conflicting server changes…",
            DraftRequest::Verify => "Verifying the server's saved permissions…",
        }
        .into();
        true
    }
    /// A timed-out legacy query remains reserved in the actor. This only stops
    /// waiting in the UI; an eventual old reply is ignored, never rebound.
    pub fn timeout(&mut self, request: u64) {
        if self.pending() == Some(request) {
            self.pending = None;
            self.submitted = None;
            self.error = true;
            self.message = "No ACL response arrived. Your draft is retained. Wait for the earlier reply or reconnect before reloading.".into();
        }
    }
    /// Return a write effect only after the fresh snapshot matches the immutable
    /// baseline. The wire has no atomic compare-and-swap; a second writer can
    /// still race after this check. Only an exact subsequent read verifies save.
    pub fn receive(&mut self, reply: &AclReply) -> Option<Acl> {
        let (request, kind) = self.pending?;
        if reply.request != request {
            return None;
        }
        self.pending = None;
        let acl = match &reply.result {
            Ok(acl) => acl,
            Err(error) => {
                self.submitted = None;
                self.error = true;
                self.message = error.clone();
                return None;
            }
        };
        if acl.channel != self.channel {
            self.submitted = None;
            self.error = true;
            self.message = "The response belongs to a different channel. Reopen the editor.".into();
            return None;
        }
        if let Err(error) = validate_snapshot(acl) {
            self.submitted = None;
            self.error = true;
            self.message = error.into();
            return None;
        }
        match kind {
            DraftRequest::Check => {
                let draft = self.draft.as_ref()?;
                if !draft.baseline_matches(acl) {
                    self.error = true;
                    self.message="Permissions changed on the server. Your draft is retained; reload before saving.".into();
                    return None;
                }
                let update = draft.snapshot();
                self.submitted = Some(update.clone());
                self.message = "Sending permissions…".into();
                Some(update)
            }
            DraftRequest::Verify => {
                if self
                    .submitted
                    .take()
                    .is_none_or(|submitted| local_acl(&submitted) != local_acl(acl))
                {
                    self.error = true;
                    self.message="The server returned different permissions. Your draft is retained; review or reload before retrying.".into();
                    return None;
                }
                self.replace(acl.clone());
                self.submitted = None;
                self.message = "Permissions saved and confirmed by the server.".into();
                None
            }
            DraftRequest::Load | DraftRequest::Reload => {
                self.replace(acl.clone());
                self.submitted = None;
                self.message = "Edit a local rule or group. Inherited rules are read-only.".into();
                None
            }
        }
    }
    fn replace(&mut self, acl: Acl) {
        if let Some(draft) = self.draft.as_mut() {
            draft.reload(acl);
        } else {
            self.draft = Some(AclDraft::new(acl));
        }
    }
}

// Wire semantics omit query-only inherited rules/members. Group/member ordering
// is not meaningful, while local rule order and all permission bits are.
fn canonical_acl(acl: &Acl) -> Acl {
    let mut acl = acl.clone();
    for group in &mut acl.groups {
        group.add.sort_unstable();
        group.add.dedup();
        group.remove.sort_unstable();
        group.remove.dedup();
        group.inherited_members.sort_unstable();
        group.inherited_members.dedup();
    }
    acl.groups.sort_by(|a, b| a.name.cmp(&b.name));
    acl
}

/// Defense in depth for every frontend. Never offer to save a lossy snapshot.
pub fn validate_snapshot(acl: &Acl) -> Result<(), &'static str> {
    if acl.entries.len() > 4096
        || acl.groups.len() > 1024
        || acl
            .entries
            .iter()
            .any(|entry| entry.group.as_ref().is_some_and(|group| group.len() > 256))
        || acl.groups.iter().any(|group| {
            group.name.len() > 256
                || group
                    .add
                    .len()
                    .saturating_add(group.remove.len())
                    .saturating_add(group.inherited_members.len())
                    > 8192
        })
    {
        return Err(
            "This ACL exceeds the safe editor limits; it cannot be edited without losing data.",
        );
    }
    let mut names = std::collections::BTreeSet::new();
    if !acl.groups.iter().all(|group| names.insert(&group.name)) {
        return Err(
            "The server returned duplicate group names. Reload a valid ACL before editing.",
        );
    }
    Ok(())
}

fn local_acl(acl: &Acl) -> Acl {
    let mut acl = canonical_acl(acl);
    acl.entries.retain(|entry| !entry.inherited);
    acl.groups.retain(group_has_local_definition);
    for group in &mut acl.groups {
        group.inherited = false;
        group.inherited_members.clear();
        group.add.sort_unstable();
        group.add.dedup();
        group.remove.sort_unstable();
        group.remove.dedup();
    }
    acl.groups.sort_by(|a, b| a.name.cmp(&b.name));
    acl
}

#[cfg(test)]
mod tests;
