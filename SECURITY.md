# Memory safety and isolation

Native code handles untrusted data with a memory-safe implementation. When none is
suitable, that processing runs in a worker with only the capabilities its role
needs. Memory-safe code can still panic, over-allocate or loop, so workers are
bounded as a whole by their OS limits and restarted by their supervisor.

The web app uses the browser's own facilities for untrusted formats, such as image
decoding and media codecs, and ships Rust compiled to WebAssembly where that is
simpler, such as logic shared with the client SDK.

First-party crates inherit `unsafe_code = "forbid"`. The Win32 launcher in
`nuschl-sandbox-windows` and the native macOS module in `nuschl-capture-macos`
have the narrow exceptions below. Every dependency change includes a review of the
dependency's implementation, enabled features and transitive crates: safe wrappers
around C libraries and Rust code with unchecked SIMD or raw pointers need that
review too.

Prefer RustCrypto crates for cryptographic primitives that meet the protocol's
requirements and have a sound security and maintenance history. Keep native or SIMD
crypto behind the boundaries below. Keep and document an existing provider when
replacing it would weaken security, interoperability or support. A dependency
change alone is no reason to change a cryptographic protocol.

## Explicit exceptions and boundaries

| Component | Needed for | Boundary |
| --- | --- | --- |
| GPUI and its graphics, font and window backends | Desktop rendering and accessibility | Desktop shell only; message content arrives from workers as typed documents and validated pixels. Its SVG renderer draws only compiled-in icons and pinned Twemoji ([emoji](docs/emoji.md)). |
| Terminal graphics and clipboard adapters | Terminal output, OS clipboard | TUI only; draws bounded, validated worker bitmaps. Clipboard helpers get fixed argv, byte and time bounds, and run only on explicit local paste; SSH sessions skip the host clipboard. |
| CPAL and OS audio bindings | PipeWire, PulseAudio, CoreAudio, WASAPI | Media and call audio workers; device handles, audio and the worker's own PipeWire objects only. |
| Codec and DSP crates with SIMD internals | Voice processing | Native media worker or browser codec Worker. The browser Worker shares fixed PCM rings with AudioWorklets. Capture and playback authorization stay in separate page-to-worklet buffers. |
| SQLx SQLite | Local history, server state | Storage worker; one database directory, fixed parameterized statements, extension loading and deserialization disabled. `nuschld --import-murmur` reads the untrusted Murmur file read-only, immutable and query-only ([server](docs/server.md)). |
| SQLx PostgreSQL | Server persistence | `nuschld`'s storage worker; it holds the database credential and one scoped connection. |
| TLS provider (rustls with ring) | TLS and CPU acceleration | Session and server authority; bounded protocol framing. |
| `aes` | Mumble's OCB2 voice encryption (AES-NI) | Session authority; fixed-size blocks, keys from the TLS control channel. |
| `rsa` | The Mumble client key (stock Mumble imports only RSA certificates) | Key generation in `nuschl-session` only (cargo-deny); TLS signing uses ring. |
| `image` codecs | Images and avatars from untrusted peers, music-bot covers | Content worker only (native process, or the web client's content-worker WASM), with dimension and allocation limits. Nuschl's own UIs receive validated RGBA; pages served to other browsers receive PNGs the content worker re-encoded. |
| `akd` | Key transparency with VRF and audit proofs | Through `nuschl-kt` only, in the crypto worker, `nuschld` and `nuschl-witness`. akd_core enables ed25519-dalek's `legacy_compatibility` binary-wide, so `nuschl-kt` checks `s < ℓ` itself and `nuschl-identity` verifies with libcrux. Verifiers run under `catch_unwind` behind a strict decoder ([e2ee §5.2](docs/e2ee.md)). |
| libcrux and OpenMLS (with `rayon`) | Post-quantum MLS and identity signatures (X-Wing HPKE, ML-KEM, ML-DSA); no memory-safe Rust alternative | In the crypto worker, `nuschld` and `nuschl-witness`; in the desktop, `nuschl-stream` only parses call links and derives `stream_id` and `host_key` (HKDF and SHA-256) from secrets the desktop made itself. OpenMLS only through `nuschl-e2ee` (crypto worker, `nuschld`): every key package, tree and message is checked structurally first, and calls run under `catch_unwind`. Patched versions: `vendor/*/NUSCHEL-PATCH.md`. |
| libcrux's portable wasm32 build with hpke-rs (X-Wing, HKDF, HMAC, SHA-256, ChaCha20-Poly1305), and `aes-gcm` through `nuschl-sframe` | Call keys and SFrame in the browser (docs/video-plan.md §4) | `nuschl-stream-wasm` only, in the call page's dedicated stream worker, which is also its encoded-transform worker. It holds S, P and every key and draws randomness from `crypto.getRandomValues`; the page receives the SFU's join fields and proofs, key messages and frames, the last two encrypted. Requests are bounded before parsing (key messages 16 KiB, 256 peers); `nuschl-stream` and `nuschl-sframe` bound pending requests and replay windows. |
| str0m and `aes-gcm` through `nuschl-sframe` | Native call transport and media encryption | RTC worker. The worker gets current call media epochs, bounded encoded frames and brokered UDP datagrams. A crypto worker holds key exchange and pairwise signaling keys. |
| `turn-client-proto` | UDP STUN discovery and TURN allocation, permission and channel framing | Session broker only. The broker pins the configured TURN address and admits media only for the authenticated SFU endpoint. |
| PipeWire video bindings | Capture Linux portal video buffers | Capture worker only, through an inherited file descriptor from a fresh screen or camera grant. The desktop and diagnostic probe share the ashpd grant adapter. |
| `windows-capture` and `cameras` | Windows Graphics Capture picker and Media Foundation webcam capture | Video capture worker only. Screen and camera run as separate AppContainer roles. The picker selects the screen; the camera uses bounded top-down raw frames. Both yield bounded BGRA to the worker's AV1 encoder. |
| `nuschl-capture-macos` (`objc2` bindings) | ScreenCaptureKit picker and AVFoundation camera capture | Video capture worker only. Unsafe calls stay in its macOS module, which runs the system consent flow, validates pixel buffer metadata, and copies one locked BGRA frame into a bounded slot. The adapter receives one source class. |
| Global shortcuts (`ashpd`; `global-hotkey` on Windows and macOS) | System-wide push-to-talk | Desktop shell; press and release of one bound key. |
| Typst (`typst`, `typst-render`) | LaTeX formulas; Typst scripts can reach files, packages, plugins and data | Math worker only. It receives code-free math markup from Nuschl's converter, in a one-file world without packages, clock or system fonts, with a library stripped of file, plugin, image, data, `eval` and `std` access, under per-formula limits ([math](docs/math.md)). |
| `psm` (assembly, through `stacker`) | Stack growth in Typst's evaluator | Through `stacker` and `typst-eval` only, in the math worker. |
| Symphonia, rubato, audioadapter-buffers, opus-rs (with `fearless_simd`), nnnoiseless in `nuschl-musicbot-codec-worker` | Music decoding, resampling, Opus encoding, voice ducking | That executable's fixed decode, encode and duck roles, sandboxed on Linux only; elsewhere and without confinement it fails to start. cargo-deny keeps these crates out of `nuschl-musicbot`, which starts the worker from its fixed libexec path, moves opaque bytes, validates bounded replies and makes no codec or DSP call ([music bot](docs/musicbot.md#security-and-codec-roles)). |
| GStreamer and VA-API | AV1 hardware encoding, decoding and DMA-BUF video import | Confined Linux video worker only. Loads fixed core, app, PipeWire, VA and AV1 parser plugins. VA gets one verified DRM render device, its read-only sysfs directory and read-only system video driver libraries. |
| `cros-libva` (chromeos/cros-libva main at `59384456ac2ae78c0c3e5515f41ef1efd9b802cf`), libva and the system VA driver | AV1 hardware encoding by the encoder core (`nuschl-video-encode`) over VA-API, feature `va-encoder` | Confined Linux video worker only (cargo-deny), with the GStreamer row's render node and read-only driver grants. A safe wrapper over libva's C API: 59 unsafe blocks, each an FFI call or a read of a structure libva returns. It trusts the driver's coded-segment list, mapped image size and query counts; the worker copies at most its coded-buffer size (3 × width × height + 64 KiB), keeps each mapping and image within one call, and maps only surfaces no pending picture holds. The worker implements none of its external-descriptor traits, which forward raw pointers and fds to the driver. Bindgen generates its bindings from libva's headers at build time; it links libva and libva-drm, which load the system VA driver. The pin carries the packed-header buffers Mesa's AV1 encoder reads its frame state from; it moves to the first release that has them. |
| rav1e with defaults disabled | Local AV1 encoding of I420 frames, alongside its existing AVIF use through ravif | Video encoder worker for video, content worker for image codecs. The video roles use the scalar Rust kernels. |
| rav1d with defaults and assembly disabled | Decode AV1 in software through the upstream Rust API | Confined video decoder. Its AV1 frame and pixel limits apply before the UI receives packed BGRA. Cargo.lock pins the revision with the Rust API. |
| OS sandbox and process APIs (`nuschl-sandbox`) | Starting confined workers | Supervisor adapter; prefer safe wrappers (`win32job` for job objects). |
| `nuschl-sandbox-windows` (`windows-sys`) | AppContainer launch with an explicit handle list and job limits, which no maintained safe wrapper offers | The worker executables' launch stage, before any input is read; cargo-deny admits only the worker crates. One trait method per Win32 function with Microsoft's documented contract as its `# Safety` (cross-checked with ReactOS); unsafe calls only in `os.rs`, one per block with a `SAFETY:` comment; guards release Windows allocations exactly once; no `as` casts; launch steps tested under Miri against a mock that fails each call in turn ([Windows sandbox](docs/windows-sandbox.md)). |

cargo-deny's wrapper rules confine the named crates only. Classify every new
dependency, transitive crate and system library before allowing it, keep each
allowance narrow, and name the safer alternative that is missing.

## Worker contract

Trusted supervisors start workers from fixed paths with fixed arguments. Workers
exchange versioned, size-bounded messages over inherited pipes and socket pairs.
Worker roles use the credentials listed below. Server TLS identity and passwords
stay with the session authority. `nuschld`'s storage worker holds the database
credential. A worker cannot name an executable, choose a database,
request a URL fetch or send SQL. Content replies carry only inert text, style data and validated
pixels or masks. Supervisors kill and reap failed and stuck workers. Startup fails
closed when a sandbox helper or platform policy is unavailable.

On Linux (`crates/sandbox/src/linux.rs`) every worker runs under prlimit and
bubblewrap with all namespaces unshared, read-only system libraries, private
`/proc`, `/dev` and `/tmp`, an empty environment, and dies with its parent.
bubblewrap's `--disable-userns` (0.8.0 and later) leaves the worker no user
namespace to create, and bubblewrap fails when one could still be created. Roles
add only:

| Role | Grants | Limits |
| --- | --- | --- |
| content, one-shot (also link-preview parsing) | none | 512 MiB address space; 5 s CPU and wall time per request (15 s for image re-encoding) |
| math, long-lived, stops after a minute idle | none | 512 MiB; 3 s wall per formula, then killed; 120 s CPU over its life |
| update verifiers (Ed25519, ML-DSA-44), one-shot | none | 256 MiB; 5 s CPU |
| storage | its history directory, writable | 1 GiB |
| crypto | its `keys/` directory, writable | 1 GiB |
| media | PipeWire and PulseAudio sockets, ALSA devices, sound sysfs and procfs, `/usr/share/pipewire`, `/usr/share/alsa` | 1 GiB |
| call audio, Linux only | the media role's grants | 1 GiB |
| transfer | network and resolver files; the offered file, read-only | 1 GiB |
| music-bot codec, Linux only | none | 1 GiB |
| video encode, Linux only | none | 1 GiB, 120 s CPU over its life, 5 s wall time per exchange |
| video capture, Linux only | one portal-granted PipeWire connection inherited as standard input, a private socket for AV1 and versioned control, an optional connected Unix socket for local BGRA preview pixels, read-only system PipeWire configuration, one optional DRM render device with read-only sysfs metadata and the hardware codec's driver libraries | 1 GiB, 1 h CPU over its life, 15 s to first AV1 frame, 5 s to read a started frame, preview bounds of 1280 × 720 |
| video decode, Linux only | bounded encoded AV1 input and BGRA output pipes, one optional DRM render device with read-only sysfs metadata and the hardware codec's driver libraries | 1 GiB, 1 h CPU over its life, bounded exchange deadline |
| RTC, Linux only | private IPC sockets for call media, UDP datagrams and media epoch updates | 1 GiB, 1 h CPU over its life |

The sandbox runs `prlimit` and `bwrap` from the executable's directory when they
are there, else from the `PATH`. The Debian and Arch packages install
`/usr/lib/nuschl/bwrap`, which runs `/usr/bin/bwrap`, and the AppArmor profile
`nuschl-bwrap` for that path. Where AppArmor restricts unprivileged user
namespaces (Ubuntu 23.10 and later), the profile lets the launcher and
bubblewrap create them. The workers run under the same profile and cannot
create any.

Rules for every platform:

- Confinement restricts filesystem and network access, environment, inherited
  handles, process creation, memory, CPU and wall time.
- Content needs no user files, network, devices or secrets; media needs scoped
  audio access; storage needs only its database directory.
- Call an adapter sandboxed only after its platform policy is implemented and
  tested. A worker executable started directly (for development) is unconfined.
  Native Windows, macOS and mobile each need their own capability review.
- An audio service socket grants that service's whole API. Nuschl code creates
  and changes only its own streams and links. While the user shares audio in a
  call, the call audio worker links other applications' playback streams into
  its own capture node ([screen-share audio](docs/screen-share-audio.md)); the
  links belong to its connection and end with the share.
- A link-preview worker's slot is released only after the whole worker has
  exited: on Linux the namespace init named by bwrap's receipt, on Windows when
  the supervisor's job lists no process, on macOS when the worker is reaped
  (`sandbox-exec` execs it and its profile denies `process-fork`).

Three processes run outside `nuschl-sandbox`:

- `nuschld`'s storage worker holds the database credential and may reach
  PostgreSQL; its hardened systemd unit confines it, and the NixOS storage
  preflight inherits that unit.
- `nuschld --read-murmur` confines itself with Landlock to reading one file,
  without network, and refuses to run where Landlock cannot fully enforce that
  (Linux 6.12 or later); under the NixOS service its systemd unit applies too.
- The TUI clipboard helpers are OS clipboard programs that need the display
  socket; they run under prlimit.

### macOS (implemented, untested on macOS)

Workers start under `sandbox-exec` with Seatbelt profiles
(`crates/sandbox/src/macos.rs`), without memory or CPU limits; supervisor deadlines
bound them. Content, link-preview parsing, math and update use a deny-by-default
profile on top of `system.sb`, as do the software video encoder and decoder;
storage and crypto add reads and writes below their
directory; transfer adds network and the offered file. Media uses a deny list, since CoreAudio
needs undocumented Mach services: no network or process execution, writes only to
`/dev` and temporary directories, home reads only for audio preferences. Narrow it
to deny-by-default once it is traced on hardware.
The camera and screen workers extend the deny-by-default profile with the
system consent service and the Mach services for their source class. Full
capture, permission prompt attribution, and the service allowlists still need
validation on a Mac.

### Windows (tested in a Windows 11 VM)

Each worker runs in an AppContainer inside a job object of its own, started by its
own launch stage, which reads no untrusted input and exits 125 when it cannot
apply the policy ([Windows sandbox](docs/windows-sandbox.md)). Content, link-preview
parsing, math, update, storage, crypto, software video encoding and software
video decoding are less privileged AppContainers without capabilities. Media
gets `microphone`; transfer gets `internetClient` and `privateNetworkClientServer`.
Video camera capture gets `webcam` and screen capture gets `graphicsCapture`.
RTC and the ephemeral call key worker use less privileged AppContainers.
Storage and crypto may modify only their own directory's contents, each in a
container per directory; junctions are refused. The offered file reaches transfer
as an inherited read-only handle; only it and the standard handles are inherited.
The job admits one process, ends the worker with the launch stage, limits
committed memory and CPU time as on Linux, and blocks the
clipboard, global atoms, other processes' USER handles, desktop switching and
system settings. Extension-point DLLs, non-system fonts and remote or low-label
images are refused. Known gaps:
[what remains](docs/windows-sandbox.md#what-remains).

## Component rules

**Session.** Per connection, against a hostile server: at most 4,096 channels,
16,384 users, 64 MiB of comments, descriptions and avatars, and 256 listened
channels per user; past these, new items are dropped with one visible notice.
Exported PKCS#12 identities are written atomically and owner-only (0600).

**Microphone and voice.**

- The client opens a microphone stream only while a talk server is unmuted.
- A mute mid-sentence ends that server's transmission with a constant Opus
  silence packet.
- Deafen implies mute. Sparse server snapshots are normalized; stale self-state
  echoes cannot overwrite newer local privacy choices.
- Capture epochs and per-connection routing revisions invalidate queued PCM and
  Opus across mute and deafen changes, new speaking destinations and media-worker
  restarts.
- A mute closes the microphone's streams inside the media process; its audio stops
  being processed at once. Native stream teardown can block: the client replaces a
  media process that has not confirmed the streams closed within 100 ms.
- A deafened server receives and plays no voice, even while another unmuted server
  keeps capture running.
- An `e2ee/1` server gets no ordinary Mumble voice until encrypted voice (SFrame)
  exists: it receives no shared microphone packets, the SDK refuses voice, the
  desktop reports voice unavailable, and the session rejects raw voice commands
  independently of the media supervisor. The MLS lock covers chat only.

**Updater** ([update worker](apps/update-worker/README.md#confinement-and-actual-proof),
[local release](docs/local-release.md)). Two separately confined fixed-algorithm
workers (Ed25519, ML-DSA-44) must both accept a manifest. The trust roots are
embedded; there is no key file or minisign fallback. The updater refuses a
manifest whose release time or version is older than the newest it accepted.
Signed v2 metadata expires within 30 days. A newer signed release still installs
from expired metadata; without one, the client reports that it could not confirm
the newest version.
The hybrid signing pair stays on the local signing machine; hosted CI holds no
signing secret or publication authority.

**APT publication.** The signed release ledger binds the exact Nix-built Debian
package. The optional APT publication uses a separate local OpenPGP key pinned by
`packaging/nuschl-archive-keyring.asc`. The download host receives package bytes,
signed indices and the archive public key over SSH.

**Server.** `nuschld` is the network-facing process. It holds the TLS key (ring)
and hashes passwords with argon2id under a concurrency limit and a per-account
limit on failed guesses. It accepts self-signed client certificates (Mumble's
identity is the certificate hash) and always verifies the TLS proof of key
possession. UDP ping replies are no larger than the request
(`allow_ping_amplification` permits up to 2×, rate-limited). Its state lives in
its storage worker. Secrets reach the service only through `LoadCredential`; the
NixOS module refuses Nix store paths. `nuschld admin` works offline through the
storage worker; an operator lock (a `flock` beside SQLite, an advisory lock on
PostgreSQL) keeps it and a running server off the same database.

**Extension** ([extension](docs/extension.md)). Logs contain no message text or
idempotency keys. Old text of edited, deleted, expired or erased messages is
overwritten in SQLite (`secure_delete`); PostgreSQL keeps old row versions until
VACUUM. Retention is forever unless the server or a channel sets less; shortening
it needs explicit confirmation and applies to existing messages. Live-only
(`none`) conversations store only their highest `seq`. Erasing a user removes their
messages on the server; copies clients hold remain. Stock clients are told that
channels are archived, and their direct messages are unstored by default.
Unregistered users read no history by default. The client's sticky record
(`nuschl-policy.json`, per host and port) refuses any weaker answer from a server
that once negotiated the extension or a capability, until an explicit reset.

**Crypto worker and key transparency** ([e2ee](docs/e2ee.md)). The crypto worker,
one per profile and without network, owns the identity secrets and logs none of
them; the desktop and session see only public keys, statements and verification
results. Its keystore and state are sealed under a store key kept in a 0600 file in
`keys/` until OS keychain support exists. Before reading or creating the store key
it takes an exclusive OS lock on `keys/store.lock` and holds it until exit;
contention or an unavailable lock fails closed, and the lock file stays in place on
release. On the server, `kt.signing_key_file` and `kt.vrf_secret_file` come through
`LoadCredential` (`services.nuschl.kt.*`) or are generated once in the state
directory. Back both up: a lost VRF secret starts a new log, which clients that
pinned the old one treat as misbehaviour until reset. Records behind published
bindings are kept permanently.

**Witness** ([witness](docs/witness.md)). `nuschl-witness` holds the ML-DSA-44 seed
clients trust to vouch for servers' logs, from `LoadCredential` or generated once
(0600) in its state directory; back it up. It parses submissions with nuschl-kt's
strict decoder, with bounded bodies and per-origin and enrolment rate limits, and
runs akd's verifiers as clients do. Its only outbound connections are enrolment
checks to the submitted origin's `host:port`, to private or loopback addresses only
when configured. `nuschld` verifies every cosignature against the configured key
before serving it.

**Transfer** ([files](docs/files.md)). The transfer worker (magic-wormhole) is the
only sandboxed worker with network access: rendezvous, transit relay and peers,
plus the offered file, read-only. Downloads stream to the desktop, which writes
at most the announced size to a staging file in its cache folder and then only the
path the user chose (the offered name is a sanitized suggestion). With `relay_only`, the worker skips direct sockets,
interface enumeration, STUN and peer-address advertising and uses only the locally
configured rendezvous and relay; peer-provided relay addresses are ignored, and a
relay failure has no peer fallback. Enabling it revokes active direct-capable
workers. Configured servers and relays still see the client's address, and legacy
Mumble servers can show it to other clients. RTC adapters apply
`nuschl_core::network::PeerPolicy` before gathering or dialing and advertise only
relay candidates in relay-only mode, on every application call path.

**Native call signaling.** The session's crypto worker holds the link's ephemeral
key-exchange state in memory. It exposes room metadata, SFU challenge proofs, opaque
key messages and direct signaling authenticated with the pairwise key. Replies bind
to the current call generation and its selected peer. Relay-only sessions reject
direct signals. Pairwise keys stay in the crypto worker. The RTC process receives
active call media epochs through a private worker-to-worker socket. The desktop
is the trusted launcher and handle broker. It assigns the endpoints to the two
workers and releases its copies. The creator's reply
carries the two credentials the configured SFU needs to create its room.
The session authenticates that host over WSS using WebPKI roots before requesting
the proofs and sending the join. The WebSocket handshake uses the configured host
and `/sfu`, and caps frames and messages at 256 KiB. Key messages share the SFU's
16 KiB binary limit.

**Native call media.** Each RTC worker handles one call. It keeps the
SFrame counter and replay state for the call and receives only the media epochs
the call needs. The video workers encode and decode media. The capture worker
scales frames from the authorized source into a bounded local preview. The
Linux preview uses a dedicated socket. The macOS and Windows capture workers
use the capture reply pipe. The desktop validates the version, dimensions and
length before allocating a preview.
The SDK applies
`PeerPolicy` before network setup, then validates the destination and length of
datagrams it routes. Outside relay-only mode, a direct leg can probe the
configured UDP TURN service for one reflexive address. It sends and receives
media only on the host and reflexive addresses from sealed pairwise signaling.
Relay-only mode uses the TURN address advertised by the authenticated WSS
connection and permits only the SFU's IP and configured media port. It sends
only the relay candidate. The RTC worker uses inherited IPC and a sandbox
without network access. On Windows and macOS an ephemeral key worker with no
profile access writes epochs to the RTC worker's inherited one-way pipe.
Ending the call closes its workers and releases their process slots after reaping.

**Companion SFU** (`nuschl-stream-sfu`, [protocol](crates/protocol/src/sfu.rs)).
A network service under its hardened systemd unit; its state lives in memory,
except its traffic count per billing period, kept in its state directory. A
room holds the `admit_key` and `host_key` its creator hands over. Media keys stay
with the participants, and RTP payloads and key messages pass through unread. Its
configuration bounds every network input; key messages are at most 16 KiB and UDP
datagrams 2 KiB. The TURN REST secret arrives through `LoadCredential`.

## Where secrets live

Nuschl's folders (`crates/session/src/layout.rs`; on Linux the XDG Base Directory
ones, see [desktop](docs/desktop.md)) keep secrets out of the configuration folder,
which people keep in dotfiles repositories, and give each persistent worker one
folder of its own. Folders are created 0700, files 0600.

| What | Where (Linux) | Reached by |
| --- | --- | --- |
| Mumble certificate and private key, with lock and import backups | `$XDG_DATA_HOME/nuschl/identity/` | the main process only (TLS client authentication is in-process) |
| End-to-end keys: sealed keystore, state, store key | `$XDG_DATA_HOME/nuschl/keys/` | the crypto worker only; the main process sees public keys only |
| Local history (SQLite) | `$XDG_DATA_HOME/nuschl/history/` | the storage worker only |
| Servers' security records (`nuschl-policy.json`) | `$XDG_DATA_HOME/nuschl/` | the main process only |
| Settings, bookmarks with accepted certificate pins and access tokens, `managed.json` | `$XDG_CONFIG_HOME/nuschl/` | the main process only |
| Last channels, update check state, shared-audio volumes by certificate hash | `$XDG_STATE_HOME/nuschl/` | the main process only |
| Channel trees and server icons (server-controlled, bounded when read) | `$XDG_CACHE_HOME/nuschl/` | the main process only |
| Music bot certificates, links, library keys, queues with playback history, cover cache, the import requester secret | `/var/lib/nuschl-musicbot/` under the NixOS module; each browser's link in a cookie | the bot only (private systemd state directory). Every valid link controls all queues, reads history, streams audio, sets the shared library account's ratings and, with an import service configured, requests albums from it ([links](docs/musicbot.md#links), [adding music](docs/musicbot.md#adding-music)). The public Mumble comment carries only the public address and instructions. |

Only the storage and crypto workers get one of these folders, each its own: bwrap
binds it at `/data`, Seatbelt allows reads and writes below it, and on Windows the
folder's own AppContainer is granted it. The two folders are siblings, so neither
worker reaches the other's folder, the identity, the records or the configuration.
A profile whose XDG variables would put another folder inside a worker's folder is
refused. Server passwords are not saved. Access tokens are shared channel
passwords and stay in `servers.json` with their bookmarks; keep them out of public
dotfiles repositories.

When an earlier version's history in the data folder cannot be moved, the storage
worker gets the whole data folder and nothing else moves into it. On Windows,
migration first retires the old `nuschel.*` containers and their path grants, in a
short-lived storage worker, and aborts if that fails.

## Fuzzing

The libFuzzer targets in `fuzz/` cover the hostile-input paths
([targets and how to run them](docs/development.md#fuzzing)). Fix each finding at
its root and add a regression test with the minimised input next to the code. Where
small inputs can take super-linear time (link detection, the icon page scan, HTML
nesting), tests time a hostile input at the largest accepted size.

## Release gates

- Reject malformed/oversized frames before allocating the advertised body.
- Bound pending requests and terminate/reap stuck workers; never silently restart forever.
- Exercise content parsing with hostile inputs and test native codec interoperability.
- Verify workers cannot read an unrelated file or make a network connection.
- Test route/device churn and ensure recovery touches only Nuschl-owned resources.
- Run dependency license, source, vulnerability, and unsoundness checks.
- Preserve third-party notices and review dependency exceptions before distribution.
