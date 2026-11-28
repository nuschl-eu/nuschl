# Identity on id.nuschl.eu and light federation

Status: plan for review (2026-11-17). It builds on the identity model, key
custody and key transparency of [e2ee.md](e2ee.md) §3 to §5, the witnesses of
[witness.md](witness.md), "MLS and encrypted history" in
[architecture.md](architecture.md) and "Where secrets live" in
[SECURITY.md](../SECURITY.md). PR #363 gives every identity a recovery key that
the device keeps; PR #273 signs sticker pack versions with the author's identity.

The plan holds the required features and the choices that are hard to change later
(see "Lock-in guards"); everything else is in "Later".

## Decisions (Owner, 2026-11-17)

- **Minimal design.** "lets try to keep the design as minmimal as possible while
  putting in the required features or making sure we don't lock ourselves into a
  wrong design that can't be easily changed later."
- **Migratable identities.** "Ideally I'd want to keep identities fully migratable
  so a disappearing or malicious home-server can't impact much."
- **id.nuschl.eu is the identity service and the passkey rpId**, renamed from
  auth.nuschl.eu (decided 2026-11-13; passkeys are unimplemented). It runs passkey
  ceremonies with PRF, keeps encrypted backups, handles recovery and email, and
  keeps a directory and a key-transparency log with witnesses.
- **Safeguards.** The identity service keeps public data and sealed blobs only; if
  it disappears, existing identities, contacts and conversations keep working.
- **One identity service.** id.nuschl.eu is the only identity service, with one
  username namespace, to avoid ecosystem fragmentation; "the code and design should
  also be simpler with just one id service." Self-hosting covers home servers.
- **Light federation.** Home servers carry the SFU, voice, direct messages,
  attachments and stickers. The identity service tells where a user's home server
  is and sends all email. A user whom a home server blocks moves to another one.
- **Moving home servers** updates the account's home server in the identity
  service's directory and log (see "Home server location").
- **Direct conversations live on both participants' home servers.** Either one
  suffices to restore the conversation, also when every device of both users has
  lost its state and only the recovery key remains.
- **Usernames** are globally unique and managed by id.nuschl.eu, with free display
  names. Each account has a stable account id, so usernames can change.
- **Lookups** go straight to the identity service.
- **Recovery** works with a synced passkey, with the recovery key on a later login
  screen, or by email. An email reset makes a new identity that the identity service
  vouches for: public in KT and unverified for contacts. It takes effect at once;
  users can opt in to a 7-day delay open to objection. Email at registration is
  offered and optional. Linux desktops recover with a passkey through a page on
  id.nuschl.eu.
- **Invite tree.** "an invite tree like lobste.rs so spam or sockpuppet accounts can
  be identified more easily (and a later public registration phase which can be more
  heavily moderated)". id.nuschl.eu keeps one tree. Invites are available at once,
  and operator identities get unlimited invites. id.nuschl.eu moderates obvious spam
  and abuse only, by cutting the tree at the abusing account. Later public
  registration: email maybe, no proof of work, no probation for now.
- **Attachment deduplication** (review of #366): clients reuse attachment
  references, and servers count them, accepting that a server learns which
  messages share an attachment. No convergent encryption (Owner 2026-11-17).

## Roles and data

| Role | Holds |
| --- | --- |
| Identity service (id.nuschl.eu) | Directory and KT log, backups, passkey credentials, email addresses, invite tree |
| Home server (a `nuschld` the user chooses) | The user's account, an inbox for conversation invites, direct conversations and calls, attachments, stickers, the user's sealed server-side keys |
| Channel server (any `nuschld`) | Channels, ACLs and accounts, as in e2ee.md |
| Witness (`nuschl-witness`) | Cosignatures over the identity service's and every server's log |
| Device (crypto worker) | All private keys (e2ee.md §3.5), the recovery key (#363), contacts' cached records |

One `nuschld` can be a home server and a channel server at once. Like Murmur, one
`nuschld` hosts several virtual servers; the operator decides whether its users
may create them, with quotas (#379). That leaves identity unchanged. A client
connects to the identity service and to every server that hosts one of its
conversations.

**Identity service.** Each account has a random 128-bit account id. A device logs
in with its device key, certified under the account's identity, and every
directory change is authenticated by that login. The KT log, built from
`nuschl-kt` and witnessed as in witness.md, has these labels:

- `account:<account id>`: the current identity fingerprint, the home server, the
  display name (signed by the identity, like its keys), the email binding's salted
  hash, and reset entries;
- `id:<fingerprint>` (reserved in e2ee.md §5.1): the identity record of e2ee.md
  §3.1 (root key, device-signing certificate, device certificates, revocations);
- `user:<username>`: the account id; a rename maps the new username, and a
  forwarding entry for the old one comes later;
- `pack:<pack_id>`: sticker pack versions ([sticker plan](sticker-packs-plan.md)
  §2.1, #273).

The root seed is sealed under a backup key; the `RecoveryBlob` holds it under the
recovery key, and each passkey wraps the backup key with its own PRF output. The
email address stays with the service to send codes. The invite tree stays outside
the log. Clients pin the log's key as they pin a server's (e2ee.md §5.1); a new
key is announced in the log, signed by the old one.

**Home server location.** The home server in `account:` is authenticated by the
user's login, and the log shows every change to the user's devices, whose
self-monitoring raises the alarm of e2ee.md §4 for a change from elsewhere. A
separate signature would only stop the identity service, or someone holding its
database, from redirecting contacts' invites to another server for a while; that
costs availability and metadata, and MLS keeps the content safe. The identity
service can deny lookups anyway, so the plan signs keys and the profile and
leaves the home server to the login and the log.

**Home server.** The account is bound by an `AccountStatement` (e2ee.md §3.8). The
server stores ciphertext and delivery metadata only: who talks to whom on it, sizes,
times and IP addresses.

## Flows

### Register

1. On first connect the device creates the identity and its recovery key (#363).
2. The user redeems a registration invite and picks a username at the identity
   service, which creates the account id; the device publishes the identity record.
3. The home server is the server the user registers with. The device binds the
   account there and sets it as the home server.
4. Optional on the same screen: one or more passkeys or FIDO keys, and an email
   address confirmed by a code.

**Passkeys.** Each passkey wraps the backup key with its own PRF output (CTAP2
`hmac-secret` on FIDO keys), so several passkeys and password managers work side
by side. Credentials are non-discoverable by default, so FIDO keys spend no
resident-key slot: the login names the username first, and the service answers
with the account's `allowCredentials`. Discoverable credentials are an opt-in for
login without a username. The WebAuthn user handle and the PRF salt derive from
the account id, so a rename leaves passkeys working.

### Add a device

Linking as in e2ee.md §3.4; the new device's certificate also goes to the identity
service's record.

### Look up a user

The client asks the identity service by username, account id or fingerprint (from
an invite link, a QR code or a channel server's `acct:` binding), checks the
proofs and the witnesses' cosignatures (e2ee.md §5.4, witness.md), and pins the
identity on first use (e2ee.md §4). To start a conversation it puts a sealed MLS
invite into the contact's inbox on their home server.

### Direct conversations

Both participants' home servers keep the conversation. Requirements:

- Each of the two servers holds the full encrypted history and what a member needs
  to rejoin the group or to start a successor with the same members (e2ee.md §6).
- Every key needed to read the history (history keys, e2ee.md §6.6, and the
  device-sync key of §3.7) is also stored on the user's home server, sealed under a
  key derived from the root seed. The recovery key and the passkey backup open the
  root seed, so either restores the conversations without any device.
- If one server disappears, the other keeps serving, and the participants continue
  there or on a new home server.

Recommended for keeping the copies in step (open question 1):

- MLS messages are small and always go to both home servers, and both
  deduplicate application messages by epoch, sender and generation, so either one
  holds the full history. Application messages need no ordering across servers,
  so either server takes them during an outage.
- Commits: each server accepts one commit per epoch. The initiator's server orders
  them; a client that gets no answer from it sends its commit to the other server,
  which orders until the first returns and catches up from it. If a partition made
  the two servers accept different commits for one epoch, the initiator's server's
  commit wins; members on the other branch rejoin with an MLS external commit and
  send their messages from that branch again. Commits in a two-person conversation
  are rare, so this stays a corner case.
- An attachment is uploaded once, to the recipient's home server, which returns a
  signed receipt. Without a receipt within a short timeout, or when the upload
  stalls, the client also uploads to the sender's home server.
- The other home server fetches the encrypted blob by its content hash from the
  first, in the background. The hash verifies the blob, so this narrow
  server-to-server fetch needs no trust between servers.

This fits MLS: each attachment's key and hash travel inside MLS messages, and the
blobs are opaque ciphertext, so where a blob is stored is independent of the group.

**Deduplication.** Each upload encrypts with a fresh random key, and servers and
attachment references address a blob by the hash of its ciphertext. A blob is
shared only when an existing reference (ciphertext hash and key) is reused, as
when forwarding or sending an attachment again while a server still holds it.
No convergent encryption (Owner 2026-11-17), so equal files from different
uploads stay unlinkable. The client's local store may key decrypted files by
their reference, on the device only. Each message names the blobs it references
in the MLS message's authenticated data, so servers count references and delete a
blob once no stored message names it. A server thereby learns which messages
share an attachment (accepted in the review of #366).

### Move home server

1. The user registers at server Y, binds the account, and sets Y as the home
   server at the identity service, which logs the change.
2. Contacts see the move on their next lookup or monitoring pass (e2ee.md §5.4).
3. The user's client copies its conversations to Y from the other participants' home
   servers or from its devices.

Every step works without the old server.

### Recover with a passkey or the recovery key

A synced passkey (for example in a password manager) runs a WebAuthn assertion for
rpId `id.nuschl.eu` with PRF; the PRF output, through HKDF, unwraps the backup key
and with it the root seed. The later login screen (#363) does the same with the
56-character recovery key and the `RecoveryBlob`. With the root seed, the device
rotates the device-signing key, certifies itself, revokes the devices the user
marks as lost (e2ee.md §3.5), and opens its sealed keys on the home server.
Identity, verification and history survive.

Native apps run the ceremony through the platform's passkey API where it offers
PRF or `hmac-secret` and the app may claim `id.nuschl.eu`: Windows Hello and the
Windows WebAuthn API, Apple platforms through associated domains, Android
Credential Manager through Digital Asset Links, and password managers that
provide passkeys to them. So the root seed stays in the crypto worker. On Linux
desktops and elsewhere, a page on id.nuschl.eu unwraps the root, certifies the new
device over the linking protocol of e2ee.md §3.4 and discards the root; that page's
code is trusted while it runs.

### Recover by email

Email proves control of an address only, so an email reset creates a new identity,
and the identity service vouches for it.

1. A code goes to the bound address; the new device submits a new root with it.
2. The service records in `account:` "replaced <old fingerprint> by <new
   fingerprint>, vouched by id.nuschl.eu (email)" and mails the address. The
   account id, username, home server, email binding and place in the invite tree
   stay; only the identity changes. Contacts see a key change
   labelled "reset, vouched by id.nuschl.eu"; verified contacts drop to unverified.
   The old identity's devices see the entry through self-monitoring and raise the
   blocking alarm of e2ee.md §4.
3. With the opt-in, step 2 takes effect after 7 days. Until then a
   `ResetObjection` signed by the old device-signing key, or by the root opened
   with the recovery key, cancels the reset in the log.

Without the opt-in, control of the email address is enough to take over the
username, and the reset is visible afterwards. Signal binds an identity to a phone
number: without Registration Lock the SMS code re-registers at once; with it, the
user's PIN is needed, or with a forgotten PIN a wait of 7 days after the account's
last activity. A new identity key shows contacts "safety number changed", and
contacts rely on Signal's server for the binding until they compare safety numbers.

The new identity starts with an empty history. Sticker pack versions are signed by
the author's identity and `pack_id` derives from its fingerprint (#273), so only the
old identity adds versions to its packs. The new identity publishes a new pack whose
metadata says "replaces pack X"; clients show that note as unverified and leave
installing it to the user.

## Invite tree

Registration at the identity service starts with an invite, as on lobste.rs. An
invite is a single-use code signed by the inviter's device-signing key over the
code's hash and an expiry; the invitee's first publication signs it too. Each
identity's inviter is recorded, so the identities form one tree.

- **Visibility.** The service's moderators see the tree; each user sees their own
  inviter and invitees.
- **Moderation.** Invites are available at once and unlimited by default.
  id.nuschl.eu acts on obvious spam and abuse only: a moderator cuts the tree at
  the abusing account: that account and its subtree lose their usernames and
  invite rights, and the service stops publishing their new records. Signed
  revocations stay accepted, so users keep control of their keys. Moderators can
  limit one account's invites and re-admit individual accounts of a cut subtree.
- **Operators.** An operator identity has unlimited invites and admits whoever the
  operator wants; the community is one subtree, cut at the operator if needed. Each
  home server decides which registered identities it hosts.
- **Interplay.** The tree links account ids, so an email reset keeps the place in
  the tree. Moving home servers leaves the tree unchanged.

## When a party is malicious or gone

| Party | If it disappears | If it is malicious, it can | Limited by |
| --- | --- | --- | --- |
| Identity service | Existing identities, contacts and conversations keep working from cached records and home servers; new registrations, recoveries, lookups and moves wait for its return | Deny service; delay updates; see registrations, emails and lookups; vouch for an email reset; serve a hostile browser ceremony page | Signatures on keys and profiles; the log shows every change to the user's devices; witnesses bound split views and freezes to 24 h; backups need the PRF output or recovery key; resets are logged, and opted-in identities can object for 7 days |
| Home server | The other participant's home server keeps each direct conversation; the user moves | Deny service; drop invites; see metadata | End-to-end encryption; moves need the user's login and show in the log |
| Channel server | Its channels are gone | As in e2ee.md §2 | As in e2ee.md §2 and §5 |
| Stolen unlocked device | — | Add devices, move the home server, object to a reset | Self-monitoring alarms on the other devices; revocation and a move back |

## Lock-in guards

- Every new record and statement uses the KT value framing of e2ee.md §5.1
  (`uint16 format`), so later formats, including a successor service or a migration,
  arrive as new format versions without breaking proofs.
- Moderation decisions are signed records kept apart from the tree, so other
  moderators can publish their own later.
- Lookup requests are self-contained and leave the asker anonymous, so a home server
  can later forward them unchanged for privacy.
- Server-side keys derive from the root seed, so every recovery path that opens the
  root restores history.
- Accounts have stable ids, so usernames and identities can change under them.

## Bounds

Email limits (Owner 2026-11-17), defaults of options in the identity service's
NixOS module `services.nuschl-id` (`nix/id-module.nix`):

| Option | Default |
| --- | --- |
| `email.rateLimits.perRecipient` | 5 per hour per recipient address |
| `email.rateLimits.perDomain` | 100 per hour per destination domain and mail server |
| `email.rateLimits.perRequester` | 10 per hour per requesting account and IP address |

Proposals: KT values at most 4096 bytes (e2ee.md §5.1); a backup blob at most 16
KiB and 8 passkeys per account; one move per hour; one bound email address,
8-digit codes valid 15 minutes with 5 attempts, one pending reset per account;
invite codes expire after 7 days; an attachment receipt within 10 s of the
upload's end, and 30 s without progress counts as a stall. Clients validate every
reply against these limits before allocating.

## Slices

1. Identity service: a `nuschl-id` role in `nuschld` with the directory, the
   `account:`, `id:` and `user:` labels, invites and the tree, witness submission
   and the NixOS module `services.nuschl-id`.
2. Home server location, lookups, inboxes and the move flow.
3. Direct conversations on both home servers, with root-derived server-side keys.
4. Recovery-key login screen (#363), then passkey backup and recovery.
5. Email binding and resets.

Each slice has unit tests and loopback integration tests through `nuschl-testkit`;
the move, restore and reset flows also get a NixOS VM test with two home servers and
an identity service.

## Later

- Lookup privacy through a home-server proxy or batched queries.
- Public registration (email maybe; no proof of work; no probation for now).
- Social recovery: k of n chosen contacts confirm a reset, with the opt-in
  delay and objection.
- Moderation communities: users and servers subscribe to moderators they trust.
- Forwarding from an old username to the renamed account.
- Sending very long messages as an attachment (#377).
- Reducing channel servers' `acct:` roster commitment once `id:` records exist.

## Open question (recommended default first)

1. **Keeping both copies of a direct conversation:** MLS messages to both home
   servers; attachments once to the recipient's server with a signed receipt, to
   the sender's server as fallback; the other server fetches blobs by content hash
   ("Direct conversations"). Alternative: the sender's client uploads the second
   copy later at low priority.
