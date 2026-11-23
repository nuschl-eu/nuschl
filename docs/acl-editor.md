# ACL and group editor

The native client provides a local ACL/group draft editor. Right-click a channel
and choose **Edit permissions…**. Web and TUI editor parity remain queued; the
shared draft/API boundary below is intended for those frontends too.

Select a rule to edit its target, scope and permissions. A target is a
registered user or a group: search for one of Mumble's built-in groups
(Everyone, Registered users, People in this channel, People outside this
channel) or the channel's groups in **Group for this rule**, or type a group
expression (`!group`, `~group`, `sub,1`, `#token`, `$certificate`). Each
permission is a row with Allow, Not set and Deny,
explained on hover; rule rows summarise whom they target and what they allow and
deny. Apply or explicitly discard a typed target before saving,
creating another rule, or changing selection. Rules can be created, removed and
reordered; inherited rules are inspectable but read-only. Channel inheritance
and group inherit/inheritable flags are separate controls. Select a group to add
or remove local members, exclude inherited members, or reset a local override.
Names resolve freshly to registered account IDs; `#id` explicitly selects a
numeric account. Missing labels never discard membership.

**Channel password** sets, changes or removes a password the way Mumble's channel
dialog does: two local rules that apply in the channel only, `@all` denied and
`#<password>` granted Enter, Speak, Whisper, Text message, Link channel and
Traverse. People with the password as an access token may enter. Everyone sees
the channel: nuschld lists a channel that an access token would open, so the
password can be entered.

**Channel type** sets who may join, speak and write in one step: Open, Members
only, Password, Announcements or Private. A type is a template for the table's
first three columns (See & join, Speak, Chat); the other columns and
permissions stay as they are. A channel whose table matches a type shows it;
otherwise none is selected and the editor says **Custom**. Moderators are
`@admin`: Murmur and nuschld create that group in the root and add a new
channel's creator to its own `admin` group. Members are registered users
(`auth`). Each type stores these standard Mumble rules, in this order, applying
here and in subchannels unless noted:

| Type | Rules |
| --- | --- |
| Open | none: Mumble's defaults and the parent channels decide |
| Members only | `@all` deny Enter, Listen, Speak, Whisper, Text message; `@auth` allow Traverse, Enter, Listen, Speak, Whisper, Text message |
| Password | the channel password's two rules below (here only) |
| Announcements | `@all` deny Speak, Whisper; `@admin` allow Speak, Whisper |
| Private | `@all` deny Traverse, Enter, Listen, Speak, Whisper, Text message; `@admin` allow the same |

Members only keeps Traverse, so a subchannel can admit everyone again. Private
denies Traverse: only `@admin`, other holders of Write and the rows you add
enter, listen, whisper or write here and in every subchannel. nuschld sends a
client only the channels it may traverse or an access token would open, and the
people in them ([server](server.md), "What each client sees"), so on nuschld
nobody else sees a private channel, its subchannels or the people in them, as
long as no rule there grants an access token (`#token`). Stock Murmur sends every
channel to every client, so there others still see a private channel and the
people in it. The description comes from `Kind::help`, which takes whether the
connection negotiated `hidden-channels/1`: "Others don't see the channel" on
nuschld, "Others see the channel" elsewhere. Password uses the typed password, or
keeps the current one; the other types remove it. Mumble's password rules let `@admin`
enter through Write; speaking there takes the password. Murmur checks Text
message in the receiver's channel for private messages too, so Members only and
Private also stop everyone else's private messages to people inside. Rules the
table cannot show leave the type unset.

**Create sub-channel…** offers the same types. Once the server shows the new
channel on the same connection, its permissions open, take the type and save.
The type applies only when the channel's rules name this account or certificate
as its creator: its own `admin` group, or Murmur's Write rule for a creator
without Write. A refusal, a reconnect or 15 seconds without the channel drop the
request. Murmur and nuschld refuse a name a sibling already has.

**Channel permissions** shows the channel's permissions as a table: rows for
Everyone, groups and people, columns See & join (Traverse, Enter, Listen),
Speak (Speak, Whisper), Chat (Text message), Moderate (Mute/deafen, Move) and
Manage channel (Write ACL, Make channel, Make temporary, Link channel). A click
on a cell cycles this channel's own setting for the row: not set, allow, deny.
Each edit rewrites the channel's own rules in one order:
Everyone, group denies, group allows, people; the password rules stay last. So
a person beats their groups, groups beat Everyone, and among groups allow beats
deny. The table reads existing rules when that order decides the same as
theirs, as with Murmur's default root rules; otherwise it says the channel's
rules depend on their order and leaves them to **Advanced rules**, the
rule list the server applies top to bottom. While the table shows the channel,
that list stays collapsed behind **Show advanced rules**. Every cell shows
the result for a member of its row in this channel (Yes, No or Partly),
evaluated with `nuschl_core::acl` over the channel's whole rule list. Cells set
in this channel are solid; the others are dimmed, and hovering names what
decided them: Mumble's default, another row, a rule in a parent channel or
Write ACL. Hovering a Partly cell lists which permissions are allowed. So a
click on one row shows at once which other rows it changes. **Remove** clears
everything a row sets here; rows are added with the search field below the
table. The table shows eight rows at a time and scrolls; it draws only the rows
in view. Permissions a row sets outside the five columns are listed under the
table. Once the parent channels' rules are read, a dimmed cell set in a parent
names that channel on hover, and its context menu (or Alt-click) offers
**Show in <channel>**: that channel's permissions open, scrolled to the cell,
which lights up and fades out (ease-out; at once with reduced motion). Unsaved
changes ask Save or Discard first.

Closing the editor with unsaved changes asks first: **Save** (closes once the
save is confirmed), **Discard** or **Keep editing**.

**Check access** lists what Everyone, Registered users, a member of one of
the channel's groups, or a registered account may do in the channel, and what
decided each permission: a rule and its channel, Mumble's defaults, Write ACL,
SuperUser, or a parent channel they cannot pass. Whom to check is picked in a
search field (`picker.rs`, a reusable searchable choice): Everyone and the
groups first, then every account the client knows, including offline ones
when the user may register accounts. It reads the parents' rules once per
editor and evaluates them with the channel's draft through `nuschl_core::acl`,
the evaluation nuschld runs, so unsaved changes show before saving. A person
is checked with their account, groups and certificate, in the edited channel;
access tokens other people hold stay unknown to the client.

Each open editor has its own lifetime token, in addition to stable row IDs and
original server/connection/channel authority. Dirty drafts survive disconnect as
read-only inspectors; obsolete callbacks cannot edit a replacement panel. Inputs
are read-only throughout a pending load/save/verification, and only become
editable again when bound to a current local row. Explicit reload discards local
changes after a successful response. The editor does not mutate remote cached
ACL vectors.

Rules, groups and memberships use fixed-height virtual lists with one selected
item's detail editor. Only visible rows instantiate controls. Compact list labels
ellipsize with full accessibility names and hover text; detail fields retain the
full source. Home/End, arrows and Page Up/Down navigate retained list focus, and
the shared scroll contract reveals that focus once without idle timers or
per-frame model scans. Text input keeps its native editing keys.

`nuschl_core::admin::AclDraft` owns an immutable server baseline and private local
rows. Callbacks use row IDs, never vector positions. Removal and explicit reload
retire old IDs; inherited rules remain read-only. Group provenance is separate
from local overrides: an inherited group can have local members, exclusions, or
inherit/inheritable flags. Removing its local definition resets those overrides;
it cannot remove the parent's group. The legacy query cannot distinguish a
pristine inherited-only group from an explicit empty local definition with the
same default flags. Like stock Mumble, saving omits that ambiguous definition;
its independent lifetime after an ancestor removes the group cannot be preserved
from this wire representation. Account IDs identify registered users,
including offline users, and must never be populated from presence/session IDs.
Unedited model fields and unknown permission bits survive edits.

`AclEditorState::new(channel)` manages load, explicit reload, pre-save conflict
checks, and post-save verification. A frontend supplies distinct request tickets
and preserves its original connection/channel authority. It must forward only
matching active-editor responses. Late or unsolicited replies cannot replace a
draft. Reload discards changes only after an explicit request succeeds. Save
checks a fresh snapshot against the baseline before returning a write effect;
the submitted draft remains frozen through verification. A different readback
retains the draft and reports the difference. Failed/rejected/oversized replies
also retain it. A UI timeout stops waiting, but does not free the actor's pending
wire reservation.

The native session's `QueryAclDraft` has at most one unresolved query per channel
ID and 128 across a connection. Each reservation retains its channel instance
and local ticket until the actual response or connection teardown. Closing an
editor, timing out, removing a channel, or reusing its ID must not purge that
reservation and associate the old response with a new query. A retry reports the
unresolved request instead. Legacy `QueryAcl` keeps its existing event shape;
untagged/cached ACL events cannot initialize the new editor. `QueryUserIds`
resolves names through Mumble's `QueryUsers`, independently of online presence.

The draft path refuses snapshots above 4096 rules, 1024 groups, 256-byte group
names/expressions, or 8192 total member references in one group. Duplicate group
names are also refused. These are safe editing limits, not an instruction to
truncate before saving. Different servers can impose lower write limits; the
frontend must show failures/readback differences without dropping the draft.

Name labels have a separate budget from ACL data. Both legacy and tagged SDK
responses inspect at most 4096 references, issue at most one deduplicated 128-ID
lookup per response, and spend at most 1024 ID attempts per connection. Failed
enqueues spend attempt budget, preventing repeated snapshots from hammering a
full command queue. Successful outstanding lookups are deduplicated. The label
cache retains at most 4096 account IDs. Missing names display as `#id`; they must
not make an ACL partial or cause membership to be discarded. Explicit user name
lookup remains available independently of automatic labeling.

The legacy wire has no query IDs, ACL revision, acknowledgement, or atomic
compare-and-swap. Local tickets correlate the session's serialized normal query
lane; they cannot authenticate the intent of an unsolicited same-channel reply
sent by a server while a query is outstanding. A concurrent operator can change
permissions between the preflight read and write. Post-write readback verifies
the observed local settings; it does not make that sequence atomic. Protobuf
unknown fields are already discarded by the current generated decoder; this
work does not claim opaque unknown-field roundtripping.

Fresh typed-name lookup uses a separate bounded `LookupAccount` transaction,
matched to the original editor, row and input. It first sends a Ping barrier,
then a single name query, holding ordinary name queries until completion. The
barrier preserves all current transport counters and is handled outside ordinary
RTT sampling. This relies on the honest server's ordered TLS handling; Mumble
has no authenticated query intent or immutable name-to-account transaction.
Timeouts retain the unresolved wire reservation until the actual reply or
reconnect. A changed input cannot apply an earlier result.

Native ACL reads/writes are paced against shared conservative control credit so
Murmur's default 1/s, burst-5 limit does not silently drop a load/check/write/verify
sequence. At most 16 semantic commands and 16 MiB wait; original authority is
rechecked at actual dispatch. Serialized frames are never held after a one-time
authorization. Urgent mute/deafen/privacy controls bypass the admin queue and
still spend shared credit. Queue refusals and timeouts terminate the matching UI
request while preserving its draft; writes are not blindly retried.

ACL editing follows the server's child-Write **or root-Write** administrative
rule. Current known denials are checked locally; an unknown alternative remains
subject to authoritative server rejection. Opening a menu requests missing
current permission information with bounded lifetime-aware deduplication. This
exception does not grant unrelated channel-edit/remove actions.

Focused tests cover core row lifetime, inherited-group overrides, immutable
baseline/conflict behavior, dirty/late replies, wrong-channel/lossy replies,
explicit reload, frozen saves, actor query reservations after reopening and
channel ID reuse, incomplete snapshots, unresolved-query bounds, inherited-group
wire serialization, unknown permission bits, and SDK label budgets under large
groups, duplicates, and command-queue pressure. Actual native editor/server UI
proof is exercised by [acl_editor.py](../tests/acl_editor.py) against owned stock
Murmur and nuschld at their default 1/s, burst-5 rate with no reconnect settling
or raised-limit override. It checks native offline-member editing, inherited
local overrides, ordered user/group rules, subchannel permission denial with an
unrelated control, dirty conflict retention, root-admin recovery of a child that
denies Write, stale callbacks, reconnect and restart persistence, a channel
password that admits only a client holding it as an access token, and Check
access explaining an unsaved password, and an access-table edit saved as a
stock Murmur rule.

The same fixture's `--geometry` mode loads 512 rules and 512 groups, checking
pointer/keyboard endpoints, bounded instantiated rows, focus reveal and text
editing keys at 1280×800, 900×600 and 900×800 at 200% zoom. `--pending-input`
attempts a real edit during a naturally paced save and verifies the original
field value is retained, then editable after confirmation. `--channel-types`
checks that each cell starts under its column header, reads a custom table as
Custom, saves Members only, Password and Open and compares the server's rules,
then creates a channel as Announcements and checks the rules saved for it.
[acl_e2ee_bootstrap.py](../tests/acl_e2ee_bootstrap.py) separately starts nuschld
in E2EE-only mode and uses the native SuperUser password form and group editor
to promote an offline account, without changing security policy or using a raw
legacy operator. These are loopback protocol/admin proofs, not MLS chat/video
coverage. Both scripts use private GNOME, null audio and temporary profiles.
