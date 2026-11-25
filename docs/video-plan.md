# Video calls and screen sharing plan

Status: plan, 2026-11-18. Owner target: video calls and screen sharing this
week, in the web app and the native client, on stock Mumble servers first.
Implemented so far: the K1 key exchange in #267 (in review). Effort figures are
rough. It covers #35, #135 and #139–#147 and extends the frozen #139 signalling
design (`83a5b5a7:docs/legacy-signalling-plan.md`). **Owner** marks the owner's
decisions (§11).

## 0. Summary

- **One call per channel or direct conversation.** A camera and a shared
  screen are two sources a participant publishes into the same call. A
  Nuschl user starts the call; stock Mumble users join through the link in
  the message and publish their own camera and screen from the page. Camera
  and microphone open only on the user's own Start camera or Unmute.
- **Voice stays on Mumble.** Calls carry video and screen audio; talking uses
  the existing voice path, so stock Mumble users hear the call. Video playout
  follows the voice playout delay for lip sync (§6).
- **Topology:** a forward-only companion SFU, stream.nuschl.eu by default
  (§9), relays every source. Direct paths come later, chosen by upload
  bandwidth outside relay-only mode (§5.4). TURN at turn.nuschl.eu relays
  only to the SFU.
- **Keys on stock servers:** a call link secret S admits participants; every
  publisher hands its SFrame keys to every subscriber over a fresh X-Wing
  HPKE-PSK exchange (§4). S travels through the Mumble server, which can read
  it and join the call; the roster lists everyone who joins. Phase 1 has no
  identity verification.
- **Phases:** browser pages first (camera, screen and watching), launched from
  Nuschl or opened from the link, then native Linux capture, encoding and
  viewing; end-to-end-mode servers last (§3).
- **Codec (Owner):** royalty-free codecs only, one per call: AV1, or VP9 when
  a participant lacks AV1 decoding, later VP8 as the third choice. The pages
  use the browser's own encoding and decoding; native clients encode and
  decode in hardware first (VA-API on Linux), with software AV1 encoding and
  rav1d decoding as the fallback. The SFU forwards encoded frames as they
  are.

## 1. What exists

- `nuschl-rtc` negotiates Opus and the call's video codec (AV1 or VP9).
  Its encoded-media loopback probe also supports VP8. The SFU uses Dependency
  Descriptors to select layers and rewrites sequence numbers for each viewer.
- `crates/stream` and `crates/sframe` implement call links, key exchange, media
  rekeying and RFC 9605 SFrame. The native crypto worker supports SFU
  admission and sealed pairwise signaling. The Linux RTC worker receives media
  epochs directly from the crypto worker. Two native SDK participants can send
  and receive authenticated AV1 keyframes and interframes through the SFU.
- On Linux the desktop can publish camera and screen together or play the
  selected incoming AV1 source. Starting a room can leave both devices off.
  Each fresh portal grant supplies one PipeWire FD to a capture worker.
  VA-API handles encoding and decoding where available. The software fallbacks
  are rav1e and rav1d. The decoder returns bounded BGRA to GPUI.
- A native publisher whose video flows one way can move it from the SFU to
  direct connections with one or two native viewers and fall back to the SFU.
  Relay-only native connections use UDP TURN with the SFU as their only media
  peer.
- The companion browser page handles group calls, captures camera and screen,
  and can share the selected browser tab's audio in supported browsers. It
  sets receive quality per tile and switches two-person video between the SFU
  and a direct connection. The chat web app uses the companion page for video.
- The SFU's three-source default fits camera, screen and shared audio.
  WebRTC voice would add a fourth simultaneous source. Reserve a separate
  microphone purpose and preserve mute, deafen and per-conversation routing
  before raising the bound. See [screen-share audio](screen-share-audio.md)
  for native audio capture scope and playback exclusion.
- `privacy/1` relay-only and `PeerPolicy` exist; `rtc/1` is still to build
  (#141).
- Voice playout knows its per-talker delay, which video playout can follow.
- Screen audio has its own plan, `docs/screen-share-audio.md` (#146).

## 2. The call

- **Room:** one SFU room per call, named by `stream_id` from S. A call belongs
  to a channel or a direct conversation, each with at most one call; a DM call
  is a two-person call. Nuschl clients show the call as a card with the
  participants and their sources (plugin data, #93), and the message carries
  the call link as readable text for stock Mumble clients: in the channel, or
  for a DM call as a private text message to that user (D2).
- **Sources:** everyone with the link, stock Mumble users included, watches and
  publishes at most one camera and one screen; publishing takes S and the
  user's own action.
- **Size (project instance):** 16 participants per call, all of whom may
  publish a camera, and one screen at a time per call. Self-hosted operators
  set their own caps.
- **Views:** a grid of camera tiles; a shared screen or a pinned camera takes
  the large view. The active speaker comes from the Mumble talking state.
- **Start and end:** a Nuschl user's **Start call** or **Share screen**
  creates the call and posts its card and link. The call ends when the last
  publisher leaves or after 4 h. The creator, holding P, can **End call**,
  **Rotate link** (a new S and room) and remove a publisher. Removing ends
  that connection; since everyone shares S, Rotate link keeps the person out.

## 3. Phases

**Phase 1, the first release** (Owner: browser-launched capture first):
- Nuschl desktop opens `https://<SFU origin>/call#v=1&k=S&p=P` in the
  system browser and posts the card. The page captures the camera
  (getUserMedia) and the screen (getDisplayMedia), encodes AV1 with the
  browser's encoder, and shows the grid. P, the owner secret, stays with the
  creator.
- Everyone else opens `https://<SFU origin>/call#v=1&k=S`, stock Mumble users
  from the channel message, and gets the same page without the owner actions.
- The web app embeds the same call view, so web users join in place.
- Servers (Owner): stock Mumble servers (Murmur 1.4 and 1.5) and
  compatible-mode nuschld. End-to-end-mode nuschld shows "not available yet".
- Realistic quality: cameras up to 720p30, screens up to 1080p30 with software
  AV1; more where the browser encodes AV1 in hardware.

**Phase 2, in parallel:** native Linux capture through the portal (camera and
screen both arrive as PipeWire streams), an isolated video worker with VA-API
AV1 and a software fallback, str0m in the SDK driver, the native call view in
GPUI (#145) with the isolated decoder (#35), identity-bound keys (#139 B), then
direct paths.

**Later:** Windows and macOS capture, scalable layers everywhere (#140),
nuschld's own SFU and `rtc/1` (#141, #142), end-to-end servers over MLS (#135),
long-call validation (#147).

**Target on Nuschl servers (Owner):** every voice channel is already a WebRTC
session, and that session is the call. **Start camera** and **Share screen**
add camera and screen tracks to it directly, and the channel's keys cover them
(M1). The explicit call with link secret and card is the stock-Mumble path
only (#141, #142, M1). On stock servers every participant shares one link, so
room slots apply per call (per-person links would post one message per
participant); on Nuschl servers admission runs per participant through the
session and identity, which allows per-person limits and removal.

## 4. Keys on stock servers

**Schedule.** S is 32 bytes from the OS RNG. PRK = HKDF-Extract("nuschl/stream/
v2", S) yields `stream_id` (16 B), `admit_key`, `mac_key`, `psk` and
`signal_key`.

**Pairwise exchange (K1).** A subscriber makes a fresh X-Wing key pair per join
and sends `KeyRequest{version, suite, stream_id, viewer_id, nonce_v, not_after,
pk_v, [binding]}`, MACed under `mac_key`. The publisher answers `KeyGrant{nonce_s,
enc, ciphertext, confirm_s}`, an HPKE PSK-mode seal of {epoch, K_epoch, kid,
not_after} with the transcript hash as `info`; the subscriber confirms with
`KeyAck`. The HPKE context exports a pairwise channel key that carries rekeys
and quality reports. Replays, reflections, other suites, expired requests and
bad confirmations fail closed; the suite is fixed to X-Wing.

**Calls use sender keys.** Each publisher has its own K_epoch per source (one
KID per encoding) and runs the pairwise exchange with every subscriber. A
newcomer runs one exchange per publisher; once it publishes, every participant
runs one with it. For 16 participants that is at most 15 + 15 exchanges of
about 5 KB each.

**Rekeying.** A publisher rekeys on every join (before the newcomer's first
frame, batched within 2 s), within 2 s of a leave or kick, and every 15
minutes. Receivers keep the previous epoch for 2 s.

**Passive holders.** Someone with S and every recording still lacks the keys:
every sk_v and K_epoch is fresh randomness, independent of S.

**Credentials.** A binding names the identity root key and the
`DeviceSigningCertificate` by SHA-256; a receiver without them in its bounded
cache answers `NeedCredentials` and receives them inline (Owner). They travel
inside `signal_key` encryption, so identities stay hidden from the SFU. Key messages
are at most 16 KiB (Owner).

**SFrame.** RFC 9605 `AES_128_GCM_SHA256_128`, implemented in `crates/sframe`
on the locked RustCrypto crates and checked against the RFC's vectors; those
crates have a far larger user base than the `sframe` crate, which would be a
new dependency. AV1 frames get a one-byte envelope (`0x08` on key frames) that browsers'
packetizers accept; it relies on libwebrtc behaviour and has a regression test
per supported browser. A failing sender transform stops sending.

**SFU admission.** The SFU holds `stream_id`, `admit_key` and a verifier of P;
it sees HPKE and SFrame ciphertext only. Participants prove `admit_key` with an
HMAC over an SFU nonce; that admits them to watch and to publish. Creating the
room, ending the call, rotating the link and removing a publisher need P.

**Who can do what.** S is a link secret the Mumble server can see, and Phase 1
has no identity verification. The server can join and publish as an extra
participant, which the roster shows; with the stream host's help it can
intercept participants unseen. Identity-bound keys (Phase 2, #139 B) detect a
substituted pk_v ("Identity check failed").

**Where keys live.** Natively the crypto worker runs the exchange and sends
media epochs over a private socket to one confined RTC worker per call. The RTC
worker runs SFrame and str0m. The SDK brokers UDP outside that process. In the
browser a dedicated stream worker, which is also the encoded-transform worker,
holds the keys using libcrux's portable wasm32 build (SECURITY.md).

## 5. Quality, bandwidth and paths

Owner target: up to 4K60 for screens where devices and the network sustain it;
each receiver gets min(request, decode capacity, network capacity, policy).
Each source has two settings, chosen by its sender (Owner, #534): Sharp or
Smooth, and Low latency. Sharp keeps the resolution and lowers the frame rate
when bandwidth or CPU is short; Smooth keeps the frame rate and lowers the
resolution. Screens start Sharp, cameras Smooth. Low latency bounds each
viewer's queue at the SFU to 50 ms, and receivers render its frames at once
(`jitterBufferTarget` 0 in browsers). Resolution and frame rate follow the
bandwidth and the source; a publisher may limit a source to a §5.1 tier under
Advanced, and each viewer automatically requests a tier that fits its tile.
Quality and path changes leave keys and relay-only untouched.

### 5.1 Tiers

Realtime AV1 estimates from published guidance (±50 %), to be measured by S0f:

| Source | Tier | Bitrate | Key frame |
| --- | --- | --- | --- |
| Screen | 4K60 | 6–15 Mbit/s (motion 20–40) | 0.4–2.5 MB |
| Screen | 1440p60 | 3–8 (motion 10–20) | 0.2–1 MB |
| Screen | 1080p30 | 1.5–3 (motion 3–6) | 0.1–0.6 MB |
| Screen | 720p15 (floor) | 0.4–0.8 | 40–250 KB |
| Camera | 1080p30 (top) | 2–4 | 100–300 KB |
| Camera | 720p30 (speaker) | 1–2 | 50–150 KB |
| Camera | 360p30 (tile) | 0.3–0.6 | 15–50 KB |
| Camera | 180p15 (floor) | 0.1–0.2 | 5–20 KB |

Grid tiles request 360p, the large view 720p or more. Cameras need several
resolutions at once, so cameras publish three spatial layers (SVC) or three
simulcast encodings, chosen by S0f; screens use temporal layers (`L1T3`).
Chromium's AV1 encoder in screen-content mode writes key pictures with an
undecodable inter-layer predicted frame (dav1d and libaom fail in its tile
data), so screens keep one resolution.

Local S0f throughput sample on a Radeon RX 7900 XTX with Mesa 26.3.0 and
FFmpeg 9.0.2. The input is ten seconds of moving `testsrc2` with one
key frame per second. The encoding rate includes generating and uploading
NV12 frames. The decoding rate measures VA-API output surfaces.

| Input | Encoder target | AV1 payload | Encode | Decode | Largest key frame |
| --- | ---: | ---: | ---: | ---: | ---: |
| 3440×1440 at 100 fps | 20 Mbit/s | 23.74 Mbit/s | 319 fps | — | 75 KiB |
| 3440×1440 at 100 fps | 25 Mbit/s | 29.68 Mbit/s | 320 fps | 448 fps | 86 KiB |
| 3840×2160 at 60 fps | 15 Mbit/s | 17.92 Mbit/s | 198 fps | 293 fps | 112 KiB |

These figures measure the codec on one device. The browser and SFU use the
tiers above, and end-to-end tests measure capture, transport and playout
separately. Payload exceeded the encoder target in each sample. Admission
and pacing use measured payload rates.

### 5.2 Bounds

- Encoder caps: screen frames ≤8 MiB, key frames and deltas; camera key frames
  ≤256 KiB, deltas ≤128 KiB. The largest measured screen key frame is 4.45 MB
  (3840×2160 noise, VA-API QVBR). An oversized frame is dropped and a key frame
  requested.
- SFU queues hold about 300 ms at the leg's estimate, 50 ms for a Low latency
  source, clamped to 32 KiB–2 MiB; RTX caches about 1 s. A layered source's
  queue measures how long a frame starting now waits, less the time the last
  key picture takes: past the bound it lowers the receiver's target and leaves
  out whole frames the target can do without, past twice the bound every frame
  but a key picture until the next switch point. A temporal unit with a frame
  out keeps its later frames. A key picture's temporal unit goes out whole up
  to 8 MiB.
- Key-frame requests: one per receiver and source per 2 s, coalesced per
  source; native senders repair loss with intra refresh. For a layered source
  the SFU asks only when a receiver lacks a chain of the decode target it
  should get; lost packets come back from the SFU's retransmission buffer.
- Per room: ≤150 Mbit/s egress, ≤25 Mbit/s per receiver, ≤70 % of the host's
  upload. Available capacity is shared max-min by dropping layers; a join
  fails when the base layers do not fit.
- Traffic budget (Owner, #443): each SFU has a configurable egress budget per
  period (module options `trafficBudget` and `period`). The shared rate is the
  remaining bits divided by the seconds left in the period, plus an hourly
  draw on traffic banked so far. Banked traffic is the elapsed share of
  the budget minus bytes already used, with a floor of zero. The host upload
  cap further bounds this rate. The SFU reserves outgoing UDP datagrams and
  their IP headers against the hard byte limit before sending. Reservations
  survive restarts and reset at the next UTC period. The reservation file can
  be up to 64 MiB ahead of the live count, which the SFU writes to
  `traffic.usage` once a second. HTTP and separately relayed TURN traffic
  have their own limits.

### 5.3 Adaptation

- Receivers send a fixed-size `QualityRequest` per source when the tile changes
  size, at most once a second. The SFU also accepts decode-capacity reports.
  Automatic feedback from the decoder remains planned.
- The SFU picks layers per receiver from the DD (below), so a slow receiver
  lowers only its own quality; it tells each publisher the largest demand per
  source. A demand below the tier receivers want keeps that tier's height for a
  Sharp source and its frame rate for a Smooth one. Browser senders set the
  track's `contentHint` (`detail` or `motion`) and the sender's
  `degradationPreference` (`maintain-resolution` or `maintain-framerate`).
- Congestion control is GCC/TWCC on every leg. The native publisher budgets
  72% of its upload estimate for video. When both sources run, the camera gets
  up to its encoder ceiling or half the budget, whichever is lower. The
  screen gets the rest. Rate changes reach the running encoder directly.
- Step-down order: bitrate, then what the mode gives up, then what it keeps (at
  most once per 10 s): frame rate before resolution for Sharp, resolution before
  frame rate for Smooth. Step-up needs 1.3× the next point's floor for 10 s
  (what the mode gives up) or 30 s (what it keeps).

Screen publishers use the selected tier for dimensions and cadence. With the
screen-rate capability, the SFU supplies a separate encoder ceiling. The
ceiling leaves packet overhead and a share of the host, room and viewer
budgets for each forwarded screen alongside camera and audio. A single-layer
source uses the tightest viewer path; layered sources can keep higher rates for
viewers that can receive them. A backed-up forwarding queue caps the encoder at
the viewer's reduced tier until the queue recovers. The SFU probes within
configured limits while actual payload varies with screen motion. The
negotiated screen target can reach 100 Mb/s of video payload with sufficient
egress and receiver capacity. Publishers using the original screen-rate
capability retain their 50 Mb/s ceiling. The shared traffic rate also limits
screen bitrate. With steady dimensions and frame rate, the SFU sends a new
target for changes of at least 5% and 100 kb/s, or for the encoder's minimum
rate, at most once per second. Direct screen publishers use their direct
congestion controller for bitrate.

#### Layer selection

The SFU chooses a decode target for each viewer of each layered source
(`crates/stream-quality/src/dd.rs`, `apps/stream-sfu/src/media.rs`).

Inputs. Per source, from the Dependency Descriptor: the decode targets with
their spatial and temporal layers, the layer sizes, the active targets, and
each frame's decode target indications, references and chains; and the RTP
timestamps of its temporal units. Per viewer: the tier it asks for and decodes,
lowered by the traffic policy (each tier caps the height and the frame rate),
its bandwidth estimate and the SFU's budgets, the source's mode, and the queue
bound (300 ms, 50 ms for Low latency).

Measurements, per source:

- The frame rate is the median temporal-unit interval of the latest active
  window: 9 consecutive temporal units within one second of media time, so a
  short pause inside a window leaves the median. A slower window, from a
  static or slow source, keeps the measurement. Until the first active window,
  the latest slow window's median counts, and before 9 units the source counts
  as 60 fps, the highest rate any tier asks for. A jump back in media time of
  over a second, as from a new stream, starts the units over. A target whose
  highest temporal layer is t of the active targets' top T has 2^(t−T) of the
  source's frame rate: WebRTC's scalability modes double the frame rate with
  each temporal layer.
- The bitrate of each target is its bytes over one-second windows from the
  source's first frame. A window with a pause of over a second between two
  packets keeps the previous rates, and gives a source without rates its
  first ones. A new set of targets starts the windows over.

Choices, per viewer: a target is eligible when its spatial layer's height
belongs to the viewer's tier or a lower one and its frame rate is at most 1.25
times the tier's. The lowest active target is always eligible, and until a
bitrate window ends, after the source's first frame or a new set of targets,
it is the only one; meanwhile the viewer's stepper waits. A viewer that gets
a source in its first second therefore starts on the lowest target, and its
move up a spatial layer when the second ends takes one more key picture. A Sharp source's eligible
targets go by spatial, then temporal layer; a Smooth source's go along its
lowest spatial layer by temporal layer, then along its top temporal layer by
spatial layer. Each step of a late queue removes the top choice, down to the
lowest choice of the top group: the top spatial layer for Sharp, the top
temporal layer for Smooth.

State, per viewer and source, updated once a second:

- The budgets allocate each viewer the highest choice whose rate fits
  (1.3 times the rate above the current choice).
- A stepper starts at that choice once the source's bitrates are measured and
  moves toward the allocation: down at once within a group (spatial layer for
  Sharp, temporal for Smooth) and to another group at most once per 10 s; up
  after the allocation has allowed the next choice for 10 s within a group or
  30 s to another group. Its choice is the wanted target. When the choices
  change (a new mode, tier, frame rate or set of active targets), it keeps its
  layer, or takes the highest choice at or below it in both layers, and the
  hold toward a higher choice starts over.
- The selector forwards each temporal unit for one target: the current target
  when the unit's first packet goes out. It switches to the wanted target at
  the first frame that starts a temporal unit with no packet out, comes in
  order (the first copy of its start, before any packet of a later frame), is
  a switch point for the wanted target and whose chain the viewer holds;
  when the viewer lacks that chain, the SFU asks the publisher for a key
  frame, at most once per 2 s. While a switch to a lower spatial layer waits
  for that key frame, the selector forwards the current spatial layer's lowest
  temporal layer. A frame the current target needs and the viewer misses
  leaves it without a target until the next such switch point.
- Late and repeated packets: the rest of a frame whose start went out goes
  out, and so does a late frame that forwarded frames reference, for its
  unit's target or the referencing frame's, with its unit's marker when the
  unit has none out. A frame stays out, with its later and repeated packets,
  when its start came and stayed out, when a packet of it came before its
  start and stayed out, when a frame it references came and stayed out, or
  when it is older than the current target's first frame in a unit with no
  packet out.
- A new set of targets gives target indices other layers: each viewer takes
  the lowest target until the next second's choice.
- A frame behind the newest one, in a temporal unit up to five seconds later,
  starts a new stream that numbers its frames anew, as from a reopened
  encoder: the selectors start over, and late packets of the old stream from
  the five seconds before are unreadable. Nuschl's native encoder numbers its
  frames on across reopens.
- The queue leaves out a frame the target can do without once the queue's
  delay passes the bound, every frame but a key picture past twice the bound,
  and keeps a temporal unit with a packet out whole up to MAX_KEY_BYTES: its
  byte cap passes the rest, and the budgets lend it up to one burst, down to
  minus one burst. Past that the queue sheds the unit and releases its target.
  The time the last key picture's unit takes extends the bound until the delay
  falls to half the bound; a packet of a later unit, or of a new stream, ends
  that unit. A shed frame takes the rest of its unit and the second before it.
  Each overloaded second adds a step; steps recover one per 10 s.

Invariants:

- The wanted target is eligible: it never exceeds the viewer's tier in height
  or frame rate.
- Every forwarded frame belongs to its temporal unit's target, arrives whole
  with the frames it references, and the current target changes only at a
  switch point whose chain the viewer holds.
- After a viewer's tier falls, frames above it stop at the first fitting
  switch point that comes in order: the key picture the SFU asks for, or the
  next base-layer frame on an intact chain.
- No target above the lowest comes from an incomplete bitrate window, and no
  frame rate from a window that is not active.
- A static or slow source keeps its measured frame rate, so it makes no target
  eligible that was not eligible while it was active.
- Moves follow the stepper's holds; when the old layer leaves the choices, the
  new choice is at most the highest one at or below it in both layers. Each
  forwarded temporal unit ends with one marker.
- One temporal unit takes at most MAX_KEY_BYTES past the queue's cap, and
  each budget lends at most one burst: other sources and audio keep their
  share.

### 5.4 Direct or relayed

Owner rule (2026-11-18):
- A direct path reveals IP addresses, as WebRTC usually does; that is
  acceptable outside relay-only mode. Relay-only users always relay.
- Outside relay-only mode, the path depends solely on upload bandwidth: a
  publisher goes direct when 80 % of its probed upload sustains the desired
  quality for the direct receivers, and moves receivers to the SFU when its
  legs back off. "When my upload is too slow: relay automatically (default) /
  lower the quality" (Owner).
- Path choice is independent of identity verification; `CallBinding`
  authenticates keys (§7). A Friends Direct mode, direct only with friends or
  verified contacts, is a proposal (#444).
- Candidates travel to the peer sealed under the pairwise channel key.
- The browser call starts on the SFU. Two-person calls can move video to a
  direct path. The SFU connection remains available for fallback.
- Path switches (Owner) are quick and keep the keys: the new path is set up
  and ICE-connected in the background without media, media moves in one step
  with a key frame on the new path, and the old path closes at once. Media
  runs on one path at a time.

## 6. Capture, codecs and rendering

- **Capture.** Linux: the xdg-desktop-portal ScreenCast and Camera interfaces
  each hand over a PipeWire FD, so one capture path serves both sources. Every
  capture needs a fresh portal grant.
- **Hardware codecs (Owner).** Native clients encode and decode in hardware
  first: VA-API on Linux, Media Foundation with D3D11 video acceleration
  (DXVA) on Windows, VideoToolbox on macOS. AV1 comes first, VP9 when the call
  negotiated VP9.
- **Encoding.** One `nuschl-video-worker --encode` role per source: PipeWire
  DMA-BUF into the hardware encoder without a CPU copy, the software AV1
  encoder S0f picks otherwise (rav1e preferred), live rate control and at
  most 2 queued frames. Encoders have had exploited bugs (libvpx's VP8
  encoder, [CVE-2023-5217](https://nvd.nist.gov/vuln/detail/CVE-2023-5217)),
  so encoding runs isolated too, including any native software VP8.
- **Hardware AV1 encoder (#558).** On Linux, GStreamer imports, scales and
  converts the PipeWire DMA-BUFs to NV12 DMA-BUFs, and the encoder core
  (`nuschl-video-encode`) codes them over VA-API in constant-quantizer mode.
  Its rate controller picks each frame's quantizer from the sizes of earlier
  frames, applies a bitrate change on the next frame and drops frames under a
  tight budget; a still picture is refined step by step. Keyframes come on
  request only. Screens send L1T3 and cameras L1T1, each frame with the
  Dependency Descriptor the SFU picks layers by.
- **Software AV1 encoder (#558).** Where no GPU encodes AV1, the encoder core
  codes the captured I420 pictures with libaom in real-time mode
  (`nuschl-aom`, feature `aom`): the same rate controller sets each frame's
  quantizer, libaom's SVC controls carry the temporal layers and screens use
  its screen tools. libaom's reference slots follow every frame it codes, so
  the core codes each picture once and follows a frame it cannot send with a
  keyframe. The Nix-built Linux packages enable it; the portable and macOS
  builds keep rav1e's independent frames until they link libaom.
- **Decoding.** A `--decode` role per call: the hardware decoder first, rav1d
  (Owner) as the AV1 software fallback, with pixel, memory and CPU caps.
  Decoded frames stay on the device.
- **Software VP9 (Owner).** The fallback for VP9 calls without a hardware
  codec: a libvpx encoder and decoder, or FFmpeg's VP9 decoder, in its own
  video-worker role, one process per incoming stream (or per call), with
  access to its pipes only and the existing sandboxes' memory and CPU
  limits. That native code
  needs a narrow SECURITY.md exception for that role, agreed with the owner
  when the slice starts.
- **Frame transport (Owner).** In three steps:
  1. The existing bounded framed pipes (`F_SETPIPE_SZ` 1 MiB) with copies;
     1080p30 I420 is about 93 MB/s. Measure `instructions:u` per frame before
     optimising.
  2. If copying dominates (4K60 is about 750 MB/s): slots in one `memfd`,
     created once and passed with `SCM_RIGHTS`, carry pixel payloads only.
     Bounded pipe messages ("frame seq in slot k, length, format, size") hand
     a slot over; the trusted side checks slot and length against its own
     table, uploads the pixels and returns "slot k free". Ordering,
     backpressure and crash handling stay on the pipe, so a hostile worker
     can at most produce wrong pixels.

     Unsafe stays minimal (Owner): the only `unsafe` is one block in the
     sandbox adapter that maps the `memfd` with `memmap2::MmapRaw` and views
     it as `&[AtomicU8]`, with a `SAFETY:` comment and one narrow SECURITY.md
     exception. Slot bookkeeping, length checks, `SCM_RIGHTS` fd passing
     (`rustix` or `nix`, both in Cargo.lock) and dmabuf fds are safe Rust. The
     worker may write the mapping at any time, and `AtomicU8` permits that:
     the trusted side copies pixels out by atomic loads into an owned or GPU
     staging buffer, and no `&[u8]` over the mapping exists.
     If the step 2 measurement shows atomic byte copies are too slow, one
     more `unsafe` call is allowed (Owner): a bulk copy out of the mapping
     into an owned buffer, for example volatile or `ptr::copy_nonoverlapping`.
     It needs a thorough `SAFETY:` argument (why a concurrent writer yields
     only arbitrary bytes and no undefined behaviour in the trusted process,
     including under Rust's data-race rules; the source and destination
     guarantees; why the destination is never aliased), a Miri or loom test
     where feasible, and the owner's review of that block.
  3. Hardware frames go zero-copy: VA-API exports decoded surfaces as dmabuf
     fds, passed over the Unix socket in the same ordered protocol and
     imported by the GPU; shared D3D11 textures on Windows and IOSurface on
     macOS.

  `vmsplice` is set aside: it is Linux only, its gift semantics fall outside
  Rust's types, and the reader still copies.
- **Workers.** Hardware encoding and decoding run in the isolated
  `nuschl-video-worker` roles (#35, V1, N1), each with only the devices it
  needs (the DRM render node on Linux); untrusted bitstreams are parsed only
  there. Each platform video API needs a scoped FFI row for its role in
  SECURITY.md, agreed with the owner.
- **Rendering.** GPUI surfaces for the grid and the large view (#145).
- **Lip sync.** Video frames carry RTP timestamps; the receiver holds each
  camera's frames for its talker's voice playout delay, aiming for audio
  ahead by at most 45 ms and behind by at most 125 ms (ITU-R BT.1359).
- **Browser (Owner).** The pages use the browser's own video encoding and
  decoding: WebRTC, and WebCodecs where needed. A browser with neither AV1 nor
  VP9 decoding gets a clear message. The page goes by `getStats`. The native
  path above stays as it is.
- **One codec per call (Owner).** Participants state AV1 and VP9 decoding
  support at admission; the SFU sets the call codec, AV1 when every
  participant decodes it and VP9 otherwise (WebKit today), and tells the
  publishers. Publishers encode each source once. When a participant without
  AV1 joins, the call switches to VP9; when the last one leaves, it switches
  back. The switch rides on the rekey that the join or leave triggers anyway
  (within 2 s): each publisher restarts its encoder in the new codec, whose
  first frame is a key frame, and receivers change decoders there.
- **VP8, later (Owner, #442).** The third choice in the order AV1 → VP9 → VP8,
  for devices whose hardware or browser lacks the other two. VP8 is
  royalty-free (Google's irrevocable patent grant and the 2013 Google–MPEG LA
  licence) and mandatory in WebRTC browsers (RFC 7742). It has temporal
  layers only, no spatial SVC layers, and lower efficiency per bit.

## 7. Consent, privacy and bounds

- **Camera:** only the user's explicit **Start camera** opens the camera, in
  the app and on the page alike, as with the microphone. Joining,
  reconnecting, restarting and server events leave it off. The camera stops
  on **Stop camera**, leaving the call, disconnect, channel change and app
  exit, and Nuschl shows its own indicator.
- **Screen:** every share needs a fresh picker and shows a banner; window
  sharing is preselected. The browser shows a monitor-share warning. The native
  client starts capture after the system picker and shows the monitor-share
  warning alongside the preview. Sharing stops on Stop, disconnect, channel
  change, a portal or track end and app exit.
- **Phase 1 lease.** For a page it opened, Nuschl keeps a control connection
  to the SFU and renews that page's publisher lease (about 10 s); any stop
  event revokes it, and a crash lets it expire. Pages opened from the link
  stop on Stop, a track end and page close.
- **Relay-only** keeps the `privacy/1` and `PeerPolicy` meaning: configured
  servers and relays only. Native clients connect to the SFU or TURN; browsers
  use `iceTransportPolicy: 'relay'` through turn.nuschl.eu (Owner). Relay-only
  users stay relayed whatever their bandwidth.
- **Authenticity:** `CallBinding` signs the transcript including pk_v, enc and
  the DTLS fingerprints (#139 §5.5); results bind to {connection generation,
  stream id, epoch, request}.
- **Isolation:** codecs run in `nuschl-video-worker` roles, encoding and
  decoding in separate roles; decoding happens in the decode role and the
  browser; keys stay in the crypto worker, the native RTC worker and the browser stream
  worker. Worker credit is held until the process is reaped.
- **Bounds:** §5.2; key messages ≤16 KiB, ≤32 pending key requests, ≥1 s
  between epochs, bounded replay windows, fixed-size quality messages.

## 8. Infrastructure

- `stream.nuschl.eu`, the default SFU origin: the static `/call` page, the
  `/sfu` WebSocket on the same origin, SFU media on one UDP port; access logs
  off.
- `turn.nuschl.eu` on its own IP: coturn with `use-auth-secret` from
  `LoadCredential`, peers limited to the SFU's addresses, 10-minute credentials
  issued by the SFU after admission, per-allocation `max-bps` at the tier cap
  plus 20 %.
- Headers (CSP, COOP/COEP, Permissions-Policy denying WebAuthn) live in
  `nix/stream-module.nix` (#227). The passkey rpId is id.nuschl.eu (Owner).
- Project caps: §2 and §5.2, 4 h room lifetime, limited room creations per IP
  (IPv6 per /64). Operators self-host with the same NixOS module.
- The stream host learns IP addresses, timing, room ids, participant counts and
  bitrates; S, keys, media, the Mumble server and names stay with clients, as
  long as the page code is honest. Native clients run their own code only.

## 9. Distribution

1. Phase 1 uses the companion SFU at stream.nuschl.eu. The call link and the
   card carry the SFU origin, and clients take it from there; this is the guard
   against lock-in to one host.
2. Operators run their own SFU and TURN with the NixOS modules; the desktop
   uses the server's configured SFU, with stream.nuschl.eu as the default.
3. nuschld hosts the SFU itself (#141, #142, `rtc/1`): channel calls on the
   channel's server, direct calls on a participant's home server, matching
   `docs/identity-plan.md` (#366).
4. The SFU forwards SFrame-encrypted frames only: content confidentiality is
   the same whoever runs it, and moving a call to another SFU changes only the
   origin.
5. Later, SFUs forward to each other (cascading) for calls across several
   servers.

## 10. Slices

Phase 1: S0f, K1 → K2 → W1 → B1 → T1, with E1 (the long pole), E2, Q1, D1,
D2 and I1 alongside.

| ID | Goal | Depends on | Size | Issue |
| --- | --- | --- | --- | --- |
| S0 | Spikes: HPKE on wasm32, browser AV1 and transform matrix, str0m forwarding | — | done | #139 |
| S0f | Measure tiers, key frames, VA-API AV1, camera SVC or simulcast, 4K decoding | — | M | #412 |
| K1 | `crates/stream` and `crates/sframe` | S0 | in review | #267 |
| K2 | `crates/stream-wasm` for the browser stream worker | K1 | S | #413 |
| Q1 | `crates/stream-quality`: requests, reports, tier ladders, layer choice, budgets | S0 | M | #414 |
| E1 | Companion SFU: rooms with many publishers, admission, per-receiver layers, key relay, TURN credentials, NixOS unit | S0 | L | #422 |
| E2 | Traffic budget per period with paced throttling (§5.2) | E1, Q1 | S–M | #443 |
| W1 | Call page: subscribe, decrypt, grid and large view, roster | K2, E1 | M | #416 |
| B1 | Publishing from the page for everyone with the link: camera and screen, key grants and rekeys, Stop | W1 | M | #417 |
| D1 | Desktop: Start call and Share screen, S and P, the card, the lease, Rotate link | K1, #93 | S–M | #423 |
| D2 | DM calls: Start call and Share screen in the DM view, link and card to that user | D1 | S | #441 |
| I1 | Hosts, DNS and modules (§8) | owner hosts | S–M | #415 |
| T1 | End-to-end tests and documentation | Phase 1 | M | #147 (part) |
| V1 | `nuschl-video-worker --encode`: portal PipeWire, VA-API AV1, software fallback | S0f | M–L | #418 |
| R1 | SDK driver RTC: UDP in the network process, `PeerPolicy`, SFrame, bitrate | K1, E1, V1 | L | #424 |
| U1 | Native call controls: camera and screen pickers, indicators, curtain | V1, R1 | M | #419 |
| N1 | Native call view: `--decode` role, GPUI grid, lip sync, `CallBinding` | R1, #93 | L | #145, #35 |
| X1 | Direct paths by upload bandwidth (§5.4), two-person calls first | R1, N1, Q1 | L | #420 |
| M1 | End-to-end servers: S in an MLS `Signal` envelope, then exporter keys | N1 | M–L | #135 |

Tests belong in the slice issues; each slice ships with its unit, testkit or
VM tests (#313). T1 covers headless Chromium and Firefox in a VM against owned
Murmur 1.4 and 1.5 with a stock Mumble client, and relay-only by packet capture.

## 11. Decisions

**Made by the owner (2026-11-13 and 2026-11-17):**
- Video calls and screen sharing are one feature, planned and built together.
- AV1: hardware first on every platform, software AV1 as the fallback; rav1d
  for native software decoding. Since 2026-11-18 native clients also decode
  in hardware first, through VA-API on Linux and the platform equivalents
  (§6), and VP9 is allowed too:
  royalty-free codecs only, so H.264 and HEVC stay out, and each call
  negotiates one codec for all participants (§6).
- Phase 1 captures in the browser, launched from Nuschl; native Linux follows.
- Up to 4K60 screens; each receiver gets min(request, capacity, network,
  policy); relaying is the publisher's opt-in when its upload falls short.
- Relay-only users are always relayed, in a browser through TURN.
- libcrux with hpke-rs for the X-Wing HPKE-PSK exchange on both targets.
- Key messages ≤16 KiB, credentials by fingerprint and cached.
- Domains: stream.nuschl.eu, turn.nuschl.eu, id.nuschl.eu as the passkey rpId.

**Made by the owner (2026-11-18):**
- Target: video calls and screen sharing this week, in the web app and the
  native client. Phase 1 targets stock Mumble servers (Murmur 1.4 and 1.5) and
  compatible-mode nuschld.
- In a call a Nuschl user starts, stock Mumble users join through the link
  in the channel message and share their own camera and screen from the page.
  P covers the owner actions only.
- The pages use the browser's own video encoding and decoding; a browser with
  neither AV1 nor VP9 gets a clear message.
- One codec per call: AV1 when every participant decodes it, VP9 otherwise,
  later VP8 as the third choice (#442); publishers encode each source once.
- Native software VP9: libvpx or FFmpeg's VP9 decoder in its own isolated
  video-worker role, as the fallback to hardware codecs (§6).
- Frame transport: framed pipes first, then `memfd` slots for pixels with
  control on the pipe, and dmabuf (or the platform equivalent) for hardware
  frames (§6). The only `unsafe` is the one `memmap2` mapping of the `memfd`,
  plus one reviewed bulk copy if atomic copies measure too slow.
- On Nuschl servers, later: the voice channel's WebRTC session is the call,
  and Start camera and Share screen add tracks to it, with per-participant
  admission, limits and removal (§3).
- Each source is Sharp or Smooth and may be Low latency; resolution and frame
  rate limits sit under Advanced (§5).
- Call model: one call per channel or direct conversation with camera and
  screen as sources (§2).
- Call keys: per-publisher sender keys over the K1 exchange (§4); a call-wide
  group key needs MLS and comes with end-to-end servers (M1).
- Camera consent: the microphone rule applies to the camera (§7).
- Call size and camera tiers: 16 participants; 360p tiles, 720p speaker,
  1080p30 top (§5.1).
- Phase 1 cameras: cameras in the browser pages from the start, alongside
  screens.
- Audio: talking stays on Mumble voice; video follows its playout delay.
- SFrame: our RFC 9605 layer on the locked RustCrypto crates (K1).
- Key code stays plain and easy to review; zeroizing secrets is optional.
- Direct paths: outside relay-only mode the path depends solely on upload
  bandwidth, and IP disclosure there is acceptable; relay-only users always
  relay; two-person calls first (§5.4).
- Sharers without layers (Firefox today): one stream for the slowest
  receiver, with a hint to use Chromium or the app for 4K.
- Hosting: coturn limited to SFU peers now, a Rust TURN server later
  (#143); SFU and pages on one host.
- A call label is optional for now; Phase 1 ships the roster and the
  functionality first.

## 12. Open questions

libwebrtc's future handling of the AV1 envelope; spatial layers and simulcast
with the envelope; hardware AV1 encoding per browser and per GPU vendor; the
§5.1 figures on real content; camera SVC in hardware encoders; native 4K60
decoding; the publisher-lease stop deadline; the SFrame envelope for VP9; all
effort estimates.
