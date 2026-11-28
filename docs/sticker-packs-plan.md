# Sticker packs: signed versions, content-hashed, hosted by home servers

Planning document, 2026-11-17. Nothing here is implemented. Part of #253; it
grants no approval. Identity, hosting and moderation follow
[the identity plan](identity-plan.md) (#366). The importer follows the clean-room
sticker interface spec, kept outside the repository and cited as "the spec".
The plan holds the required features and the formats that are hard to change
later; everything else is in §10.

## 1. Decisions

- **Owner 2026-11-17, reference:** a sticker message carries a reference:
  `pack_id`, `pack_hash` of one version, `sticker_id`, the sticker's key and the
  pack key. Deduplication is global. `pack_hash` is a content hash over the
  version and its stickers, and recipients verify it.
- **Owner 2026-11-17, versions:** a pack is a chain of versions, each signed by
  the author's identity. A message names one version, so a later compromise of
  the author's key adds versions and leaves earlier messages unchanged. Editing
  comes later in the UI; the formats support it now.
- **Owner 2026-11-17, order:** identity (#366) is built before stickers, since
  signed versions and updates depend on it (§8).
- **Owner 2026-11-17, signer reference:** a version carries a reference to its
  signing key and no certificate; the verifier fetches the key from id.nuschl.eu
  (§2.1).
- **Owner 2026-11-17, versions in the UI:** the library pins one version per pack,
  and the user can update to a newer one or switch back to an earlier one (§3.4).
- **Owner 2026-11-17, encoding:** "no protobuf is ok; it's a standard
  technology". Manifests and version records are protobuf messages (prost);
  hashes and signatures cover the exact stored bytes (§2.1).
- **Owner 2026-11-17, keys:** an AEAD encrypts each blob under its own derived
  key, so a moderation report reveals one sticker (#274).
- **Owner 2026-11-17, visibility:** whoever sees one sticker or holds a link can
  view the pack.
- **Owner 2026-11-17, hosting (identity plan):** home servers serve stickers.
  Here the author's home server hosts the author's packs, and the servers that
  keep a conversation hold the versions its messages reference (§3.2).
- **Owner 2026-11-14, deduplication:** each device keeps one copy of each image,
  each server one encrypted copy of each version.
- **Owner 2026-11-13/14, flow:** tapping a sticker shows the whole pack with an
  Install button (spec §9); packs spread through the stickers people send.
  Showing one sticker fetches only that sticker.
- **Owner 2026-11-13/14, import:** the desktop imports packs from external pack
  links. The external service's trademark appears nowhere, and the client checks
  for an earlier import of that pack before any request.
- **Owner 2026-11-13/14 and 2026-11-17, rights:** one general Terms of Service,
  accepted once, grant the licence and require users to keep the service out of
  risk (§5).
- **Owner, moderation:** no deniability; stickers are reportable through message
  franking (#274).
- **Owner, crypto:** ChaCha20-Poly1305 from ring, as files/1, so the format adds
  no crate.

## 2. Packs

### 2.1 Versions

A pack is a `pack_id` and a chain of versions. Each version lists its objects:
object 0 is the manifest, object `1 + j` is file `j`. Files `0..N-1` are the
stickers (`1 <= N <= 200`), and file `N` is an optional cover.

```proto
message PackVersion {           // pack_hash = SHA-256("nuschl stickers/1
  bytes body = 1;               //   version" || record bytes as stored)
  bytes sealed_author = 2;      // format 1: a blob (§2.2) of AuthorSignature
}
message VersionBody {
  uint32 format = 1;            // 1 = Nuschl, 2 = external (§2.4)
  bytes pack_id = 2;            // 16 bytes
  uint32 version = 3;           // 1, 2, ...
  bytes prev = 4;               // pack_hash of version - 1; empty for 1
  bytes key_commit = 5;         // SHA-256("nuschl stickers/1 key" || pack_key)
  repeated bytes objects = 6;   // SHA-256 of each blob, object 0 first
}
message AuthorSignature {
  bytes pack_nonce = 1;         // 16 random bytes, fixed for the pack
  bytes identity = 2;           // the author's identity fingerprint
  uint32 generation = 3;        // the device-signing key that signed
  bytes signature = 4;          // over "nuschl/sticker-pack-version/v1"
}                               //   || SHA-256(body)
```

- **Integrity:** `pack_hash` fixes the body, every blob, the key commitment, the
  chain position and the sealed signature; the message fixes `pack_hash`. So
  `(pack_hash, sticker_id)` names one image for every recipient, server and
  moderator. Neither `pack_id` nor `pack_hash` names a server, so a pack keeps
  its identity wherever it is stored.
- **Author:** the device-signing key (e2ee.md §3.1) signs with the composite
  ML-DSA-65 + Ed25519 signature. `pack_id = SHA-256("nuschl stickers/1 pack id"
  || identity || pack_nonce)[..16]` binds the pack to the identity.
- **Signer reference:** `identity` and `generation` name the signing key; the
  version carries no certificate. The verifier fetches that generation's
  device-signing certificate from id.nuschl.eu's directory, checked against its
  KT log under the pinned log key (identity plan).
- **Logging and rotation:** the author's device publishes each version's
  `pack_hash` under the label `pack:<pack_id>` in id.nuschl.eu's log, and the
  service accepts it only from the identity's current generation. A version is
  valid when the log recorded it while its signing generation was current, so a
  rotation (e2ee.md §3.6) leaves earlier versions valid, and a stolen old key
  gets nothing logged. This works on every server, stock Mumble included;
  without a fresh lookup the pack shows "author unchecked" and updates wait.
  The service learns which identity publishes which `pack_id`.
- **Encoding:** records are protobuf messages (prost, Owner 2026-11-17), like
  the wire protocol, with evolution through field numbers. `pack_hash` and the
  signature cover the exact stored bytes, and verifiers check the bytes they
  received, so the encoding needs no canonical form. Parsing is strict: §7's
  limits apply, and signed records with unknown fields are refused; prost skips
  unknown fields, so the parser compares the decoded record's encoded length
  with the input.
- **Sealed:** the signature sits in a blob under the pack key, so a server
  learns at most who uploaded a version; viewers verify the signature. Clients
  drop versions whose signature or `pack_id` check fails.

### 2.2 Blobs and per-sticker keys

```
k_blob = HKDF-SHA256(IKM = pack_key[32], salt = "nuschl stickers/1 blob",
                     info = pack_id[16] || blob_salt[16], 32)
inner  = u32 len || bytes || zeros to Padmé(4 + len)          (as files/1)
blob   = "NST1" || blob_salt[16] || ChaCha20-Poly1305(k_blob, nonce = 0^12,
                                       inner, aad = "NST1" || blob_salt)
```

Every blob draws a fresh random `blob_salt`, so each key encrypts one plaintext
once, which the zero nonce relies on. An unchanged sticker keeps its blob, salt
and key in later versions, so versions share blobs. `sticker_key` is one
sticker's `k_blob` and opens that blob only.

```proto
message PackManifest {
  string title = 1;               // <= 64 graphemes
  repeated Sticker stickers = 2;  // display order; entry j is file j
  bool has_cover = 3;             // file N is the cover
}
message Sticker {
  string emoji = 1;               // one emoji of #250's table
  string alt = 2;                 // optional, <= 120 bytes
}
```

The author's name comes from id.nuschl.eu (§3.2). The crate `nuschl-stickers`
(no I/O) holds both formats, versions and the link parser; HKDF,
ChaCha20-Poly1305 and SHA-256 come from ring, signatures from `nuschl-identity`.

### 2.3 Updates and the pack key

- **A new version** keeps `pack_id`, `pack_key` and `pack_nonce`, takes the next
  number, sets `prev`, is signed from any linked device, and is uploaded to the
  author's home server.
- **One pack key per pack** for every version (D12): a message's version stays
  readable to everyone who received it, so rotating the key hides nothing.
- **Discovery** (follows from the identity plan): a client learns of a newer
  version from a received reference with the same `pack_id`, and, when the pack
  view opens, from the `pack:<pack_id>` lookup at id.nuschl.eu. It fetches the
  versions from the author's home server, found through id.nuschl.eu, and
  verifies them.
- **Forks:** a logged version whose `prev` differs from the previously logged one
  stops updates for that pack and shows both, the sign of racing devices.
- **Identity:** passkey and recovery-key recovery keep the identity, and the
  library comes back with the user's device-sync data (§3.4), so packs continue.
  An email reset makes a new identity, which the KT log shows; it publishes a new
  pack whose metadata says "replaces pack X", shown as unverified, and installing
  it is the user's choice.

### 2.4 External format

An imported pack is one unsigned version: format 2, version 1, empty `prev` and
`sealed_author`, `pack_id` the external pack id, `pack_key` the external key,
and the objects the external manifest and files byte for byte. Clients decrypt
them per the spec's §3 (HKDF and HMAC from ring, AES-256-CBC from RustCrypto
`aes` and `cbc`, already in the lockfile). Every importer computes the same
`pack_hash`, so each server stores it once. Upstream one key pair encrypts every
file, so an imported sticker's `sticker_key` is that pair and a report opens the
whole imported pack, as its link does (D16).

## 3. Sending, showing and installing

### 3.1 The reference

```proto
message StickerRef {
  bytes pack_id = 1;      // 16 bytes
  bytes pack_hash = 2;    // 32 bytes, one version
  uint32 sticker_id = 3;  // file index, < N
  bytes sticker_key = 4;  // the sticker's k_blob; format 2: the key pair
  bytes pack_key = 5;     // 32 bytes; left out of franking (§6)
  string emoji = 6;       // one emoji of #250's table
  string alt = 7;         // optional, <= 120 bytes
}
```

A sticker message is one `StickerRef`. Receivers validate every field as the
spec's §4.3 does, and refuse a `sticker_key` that differs from the key derived
from `pack_key`.

| | Nuschl compatible (`history/1`) | E2EE (`e2ee/1`) | Stock Mumble |
| --- | --- | --- | --- |
| Carrier | `sticker` in `ChatSend` | `sticker` in kind 13's `ChatBody` (#216), group feature `stickers/1` | `nuschl/sticker/1` over message 26 (#93) |
| Image | the conversation's servers | the same | packs on the device (D6); stock clients see the first frame as an inline `<img>` and the emoji |

### 3.2 Hosting, sending and receiving

- **Author's home server:** holds every version of the author's packs and their
  blobs, uploaded with each version. When the author moves home servers, the
  client uploads the packs to the new one from its devices, or the new server
  fetches them by hash from the old.
- **Sending:** `StickerHave {pack_hash}` asks the conversation's server which
  objects of the version it holds, or whether it is blocked. The client uploads
  what is missing of the version record, the earlier records of the chain and the
  sent sticker's blob (`StickerPut`, content-addressed, write-once), with the
  terms record (§5); the message follows once they are stored, and the rest of
  the version uploads in the background. For a direct conversation the target is
  the recipient's home server, and the sender's home server fetches by content
  hash, as for an attachment's second copy in the identity plan.
- **Checks:** each server checks a blob's hash against a version record it
  holds, its size and its header shape, and each record's `prev`. Pack objects
  travel on files/1's second TLS connection (docs/files.md).
- **Receiving:** the client keeps images by SHA-256 of the image and maps
  `(pack_hash, sticker_id)` to it. Otherwise it fetches the version record and
  object `1 + sticker_id` from its server, checks hashes, `key_commit` and
  `sticker_key`, decrypts, and has the content worker check the image.
- **Pack view:** tapping fetches the manifest and the author signature (§2.1)
  and shows the title, the author's username and display name from
  id.nuschl.eu, the contact name when known, and the stickers as they scroll
  into view. Install fetches the rest and adds the pack at that version.
- **Moving and history:** old messages keep working by hash on every server that
  keeps their conversation.

The content worker decodes every image, shown at 160 logical px; animation plays
while visible, and holds its first frame under reduced motion until hover or
focus. The accessible name is "Sticker: <alt text, or the emoji's name>".

### 3.3 Visibility

A server serves a record or blob to whoever names its hash, and a pack's version
list to whoever names its `pack_id`; it offers no search. Pack ids, hashes and
keys appear only in sticker messages and pack links. A server learns pack ids,
version chains, blob hashes and sizes, who uploaded and who fetched what and
when; the author's home server also learns who asks for its packs' versions. A
`history/1` server reads the messages, keys included.

### 3.4 Library and versions

The library pins one version per pack. The pack view lists the versions known
from the log (number and logged date) and offers "Update" to the newest, and
switching back to any earlier version the device still holds or the server
keeps. Received messages always show the version they reference, whatever the
pinned version. A server keeps the versions its messages reference, plus each
user's pinned and previous version, so a switch back works.

The library (packs, their keys, pinned and previous versions, and for one's own
packs the `pack_nonce`) is part of the user's device-sync data on the home
server (e2ee.md §3.7), sealed as the identity plan describes, so linked devices
share it and recovery restores it. Locally the desktop keeps it in the storage
worker and images under `stickers/<digest>`; the web client uses IndexedDB.
Received stickers outside the library share that store, bounded to 256 MiB, least
recently used first.

## 4. Importer for external pack links

1. "Add pack" takes a pasted link in either form of the spec's §8 and validates
   it; the client registers no URL handler. The help line reads "Nuschl is
   independent of the apps pack links come from."
2. A library pack with that external id and key commitment ends the import with
   zero requests.
3. Otherwise the external manifest and files are fetched (D1), decrypted, MAC
   checked, parsed, checked by the content worker and shown for review. Missing
   emoji are chosen in #250's picker.
4. The client builds the version record (§2.4) and adds the pack.

**Who fetches (D1).** Recommended: the home server fetches and caches with
`StickerExternal {ext_pack_id, file}`, one request in flight per object however
many users ask. It holds no key and checks size and the length rule; clients
check the MAC and the content. The external service sees one client per server
with a `Nuschl/<version> (+<operator contact>)` User-Agent, and users' addresses
stay with their server. Cached objects expire 30 days after their last request
and go on a 403 or 404 (D4), in line with the caching service of DSA Art. 5;
copies kept for days exceed InfoSoc Art. 5(1) (UrhG §44a), so this needs the
legal check. The alternative is each client fetching, which shows each
importer's address to the external CDN and lets any link holder upload altered
blobs under that pack id.

**Polite fetching.** Only on a user's import; HTTPS to the configured CDN host
without redirects to other hosts; two requests in flight, 100 ms between starts;
15 s connect, 30 s per file, reads capped at 10 KiB and 301 KiB; 429 and 503
wait for `Retry-After` (at most 10 minutes), other errors back off 1–32 s with
jitter, four attempts per file, three failed files in a row stop the import. The
link and CDN hosts are two constants with a switch that turns the importer off
(D5).

**Untrusted input.** The external manifest is parsed after its MAC check, within
10 KiB: invalid UTF-8 becomes U+FFFD, Cc and Cf characters go, and duplicate or
out-of-range indexes reject the pack. Images go to the content worker one at a
time through `StickerCheck` (kind, size, frames, duration, a 96 px preview) and
`StickerFrames`, for imports, created packs and every received sticker; results
are bound to their import or message, request and generation.

| Bound | Value |
| --- | --- |
| Input | <= 300 KiB; PNG, APNG or WebP by magic bytes |
| Header dimensions | <= 512 x 512, checked before decoding |
| Frames | <= 150 and <= 3 s, >= 20 ms each, two resident |
| Time | 2 s wall per image, under the worker's memory limit |
| Reply | frames at <= 160 px, under the 12 MiB IPC frame |

## 5. Terms of Service

Accepted once: in the first-run dialog, and on existing installs at the first
sticker sent or pack added (D8). Until then the Stickers tab shows:

> **Nuschl terms**
> Stickers you share must be yours to share. People you send them to can
> use them in Nuschl. [Read the terms]
> [Accept] [Not now]

The text is `terms/nuschl-terms-1.en.txt`, id `nuschl-terms/1`, with the SHA-256
of its canonical bytes (UTF-8, NFC, LF). The client sends the accepted id, hash
and time with its first upload to each server, which records them with the
account and its own time. The stickers section, version 1:

1. **Your confirmation.** By adding or sending stickers you confirm that you
   created them, or that everyone holding rights in them allows you to license
   them on these terms, and that their use under these terms infringes no rights
   and breaks no law.
2. **Keep the service safe.** You use Nuschl in ways that keep its servers and
   operators out of risk: you share only legal content you may share, and you
   keep automated or bulk use from overloading Nuschl or the services packs come
   from.
3. **Servers.** You grant the operator of every Nuschl server that stores or
   serves a pack or sticker of yours a simple (non-exclusive), worldwide,
   royalty-free, perpetual and irrevocable right to host, store, reproduce,
   distribute, publicly display and make it available through that server,
   including format conversion, resizing, compression, padding and encryption.
4. **Recipients.** Everyone who receives a pack or its stickers gets the same
   kind of right to use, display, install and send them on within Nuschl.
5. **Irrevocable.** You can delete a pack from your library and ask an operator
   to stop hosting it. Copies people received, and their rights, stay.
6. **Removal.** Each operator decides what its server hosts and may refuse or
   remove any pack or sticker, for example after a report or a legal notice.
7. **Other sources.** Imported packs come from third-party sources. Nuschl and
   its importer are independent of them and carry no endorsement by them.
8. **Moral rights.** You remain the author, with the rights EU and German law
   keep with you. You agree that stickers appear in messages without an author
   line, and that a pack shows your Nuschl username and display name.
9. **What is recorded.** A server you upload a pack to records your account, the
   terms version and the time, as proof of the licence while it hosts your packs
   and for three years after (GDPR Art. 6(1)(b) and (f)). You can ask its
   operator for a copy.

Best-effort wording by a non-lawyer; it needs a legal check before launch.

## 6. Moderation

- **Franking:** the franked envelope (#274 §3) carries the `StickerRef`; its
  opening `env'` leaves `pack_key` empty, so `com` covers `pack_id`,
  `pack_hash`, `sticker_id`, `sticker_key`, emoji and alt text, and a report
  reveals one `sticker_key` (D15). The moderator's client fetches the version
  record by `pack_hash`, then `objects[1 + sticker_id]`, checks both hashes and
  opens the blob with `sticker_key`, so it sees the image every recipient saw.
  Reports rest on `com`, `tag` and hashes; a stolen author key changes none.
- **Removal:** each server decides what it serves. Removing a version deletes
  the blobs only it references and blocks its `pack_hash`; removing a pack does
  so for every version and blocks its `pack_id` (for format 2 also the external
  id), and messages referencing it show the emoji and "Sticker unavailable" on
  that server. Other servers keep their copies until their moderators act.
  Uploaders get reasons and one appeal (DSA Art. 17, 20). Anyone can hide a pack
  locally.
- **Invite tree:** when id.nuschl.eu cuts the tree at an author, the author's
  username and invite rights go. Messages referencing existing packs keep
  working by hash, and servers may refuse to serve the author's packs.

## 7. Storage and limits

Each blob is stored once per server and counts toward the quota of the first
uploader of its pack. Versions stay as long as the server keeps history, and
pinned and previous versions as long as a user pins them (§3.4).

| Limit | Default | Reason |
| --- | --- | --- |
| Stickers per pack | 1–200, plus one cover | every conforming external pack fits |
| Image | <= 300 KiB, <= 512 x 512 px | as the import source |
| Manifest | <= 64 KiB (format 1), <= 10 KiB (format 2) | 200 entries with alt text |
| Version record | <= 16 KiB | 202 hashes and the sealed signature (signer reference, no certificate) |
| Version | <= 64 MiB of blobs | 201 x 301 KiB padded |
| Versions per pack | <= 100 per server | bounded chains |
| Packs | 1 GiB and 100 packs per user, 20 GiB per server | operator cost |
| External cache | 2 GiB per server | import dedupe |

`services.nuschl.stickers` in `nix/module.nix` sets `enable` (default true), the
quotas, `externalFetch` and the cache bounds.

## 8. Picker and slices

The Stickers tab (#250) shows the library: pack tabs (Recent, the packs, "+" Add
pack), arrow-key cells with §3.2's names, and search over emoji names, keywords
and alt text. Click sends and closes; Shift+click sends and stays open. A
received sticker opens its pack with "Install" and "Report".

Identity comes first (Owner 2026-11-17). The identity plan's slices are I1 (the
identity service with its directory, KT log and witnesses), I2 (the account
record with home server, lookups and moves) and I3 (direct conversations on both home servers);
device-signing keys exist (e2ee.md §3).

| # | Slice | Needs |
| --- | --- | --- |
| S1 | `nuschl-stickers`: versions, signer references, keys, both formats, links, vectors | — |
| S2 | Content worker `StickerCheck` and `StickerFrames` | — |
| S3 | Desktop library in device-sync data, picker tab, create from files, terms | S1, S2, I1 |
| S4 | nuschld pack store on home servers, upload, fetch by hash, NixOS options | S1, I2, I3 |
| S5 | `pack:` label at the identity service, publishing and verifying versions | S1, I1 |
| S6 | Importer with the external fetch (D1) | S3, S4 |
| S7 | Sending, showing, pack view and Install on the three paths | S3, S4, S5, #93, #216 |
| S8 | Version list, Update and switching back in the pack view | S7 |
| S9 | Reports through #274, removal and blocks | S4, #274 |
| S10 | Web: library, picker, rendering, sending, pack view | S1, S7 |

## 9. Tests

- Unit: vectors for `k_blob`, `key_commit`, `pack_id`, version records and
  `pack_hash` in both formats; a blob outside the version, under a key that
  misses `key_commit`, or with a `sticker_key` that misses the derivation is
  refused; a version signed by another identity, by a generation other than the
  current one when logged, with a broken `prev`, or forked is refused or
  flagged; a version logged before a rotation stays valid; a signed record with
  an unknown field is refused; a revealed `sticker_key` opens its blob only;
  the content worker refuses a declared 65535 x 65535 PNG, a 10,000-frame APNG,
  truncated files and MIME mismatches.
- Integration through `nuschl-testkit`: a synthetic external pack on a loopback
  CDN fixture; a library hit makes 0 requests; two importers through one server
  make one request per file and one stored `pack_hash`; a recipient's home
  server fetches a blob by hash from the sender's.
- VM with two home servers and an identity service: a sticker in a direct
  conversation reaches both servers; a later member sees old stickers; version 2
  is offered while version 1 messages keep showing, and switching back to
  version 1 works; an author's move keeps old
  messages working; a report verifies with the redacted pack key; a removed pack
  shows unavailable.
- Local only (GUI): desktop smoke with AT-SPI names; stock Mumble shows the
  inline image and Nuschl one sticker.

## 10. Later

- Editing packs in the UI.
- Nuschl pack links (`pack_id`, `pack_hash`, key and author; no server needed).
- Adding packs from links and files on the web (D7: desktop only in v1).
- Fetching packs on stock Mumble servers through the home server (D6: packs on
  the device in v1).
- Periodic update checks, and Oblivious HTTP (#285) to hide fetch timing.

## 11. Open questions

**Settled by the identity plan (#366):**

- **D2 `pack_id`:** derived from the identity fingerprint and `pack_nonce`. The
  identity survives passkey and recovery-key recovery, so the binding holds.
- **D3 Pack links:** v1 accepts external links; Nuschl links need no server and
  wait for §10.
- **D11 Author signature:** sealed under the pack key. Uploads to the author's
  home server come from the author's account, so public signatures add nothing
  for hosting and would show every server who authored what.
- **D13 Update discovery:** received references, and the `pack:` lookup at
  id.nuschl.eu when the pack view opens; lookup privacy is deferred there too.
- **D14 Applying updates** (Owner 2026-11-17): offered with Update, and switching
  back to earlier versions (§3.4).

**Open (recommended default first):**

- **D1 External fetch:** the home server fetches and caches (§4); or each client.
- **D4 Cache lifetime:** 30 days after the last request, removed on 403/404.
- **D5 External hosts:** two constants and an off switch.
- **D8 Terms:** at first run and, on existing installs, at first sticker use;
  binding language; three years' record retention.
- **D9 Server defaults:** 1 GiB and 100 packs per user, 20 GiB per server, 2 GiB
  of external cache, on; nuschl.eu's servers review reports in triage.nuschl.eu
  (#205).
- **D10 External CDN facts** before S6: certificate chain, rate limits and
  `Retry-After`, and whether its terms allow third-party fetching.
- **D12 Pack key:** one per pack; or one per version, which hides nothing from
  earlier readers.
- **D15 #274 alignment:** `env'` leaves `pack_key` empty and a report reveals
  `sticker_key`; #274's §3 and §6 change with it. The identity plan leaves this
  unchanged.
- **D16 Imported packs:** one unsigned version whose reports open that pack; or
  imports re-encrypted per sticker, at one copy per importer.
