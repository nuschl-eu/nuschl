# Music bot

`nuschl-musicbot` plays music from Subsonic servers in Mumble channels and web
browsers. People control what plays on a web page; they get their link by writing
`!web` to the bot in Mumble. It works with stock Murmur and `nuschld`.

## What it does

- **Libraries.** Any number of Subsonic-compatible servers: Navidrome, Gonic,
  Ampache, Airsonic and others. With OpenSubsonic servers the bot logs in with an
  API key (`apiKeyAuthentication`) and, if the server has the `transcoding`
  extension, tells it which formats it decodes, so the server transcodes only what
  the bot cannot play. Plain Subsonic servers get a salted password token and send
  the original file, or always a transcoded one when `transcode` is set.
  Use HTTPS for libraries on another machine. Local libraries also accept HTTP
  on a loopback address.
- **Browsing.** The web page browses each library much like Navidrome's own: album
  lists (recently added, recently played, most played, random, by name, starred)
  with covers, the artist index, an artist's albums, an album's songs, and
  playlists. The artist index has a bar of initials that stays at the top while
  scrolling; each initial scrolls to its artists. A search asks all libraries at
  once for artists, albums and songs. While typing two or more characters in the
  search field, a list under it suggests a few artists, albums and songs with
  their covers; arrow keys pick one, Enter opens its page and Enter without a
  pick runs the full search; Escape closes the list. Every song, album and
  playlist has *Add* and *Play next*. Song titles open the song's page: its
  details (track, disc, length, year, genre, format, play count), rating, the
  albums it appears on and, with sonic similarity, similar songs. The genre
  there links to the genre's page.
  Choose the destination queue in the player bar; the browser remembers the
  selection for the page (per public address and path).
- **Genres.** The Genres page lists a library's genres with their album and
  song counts, sorted by name or by size, and a field that narrows the list by
  name. A genre's page shows its counts, its albums 48 per page and its songs
  50 per page; *Add all* and *Play all next* queue the page of songs on screen.
  Links to genre pages carry the genre's name, so they work when shared.
- **Sonic similarity.** Libraries with the OpenSubsonic `sonicSimilarity` extension
  offer **Similar songs** for library tracks and the playing song, plus **Blend
  into this song**: songs that lead from the playing song to a chosen one by how
  they sound (the library's sonic path). The library analyses the audio, for
  example Navidrome or Ampache with AudioMuse-AI. Results show the library's
  similarity score; add songs one by one, or use **Add all** / **Play all next**.
- **Covers.** The bot fetches at most 8 MiB of encoded art, then a credential-free,
  confined content worker decodes PNG, JPEG, WebP or the first GIF frame. Source
  dimensions are capped at 4096 per edge with a 64 MiB decoder allocation budget;
  the worker shrinks the image to at most 400×400 pixels (300×300 by default) and
  encodes those pixels as a new 8-bit RGBA PNG without metadata. The bot checks
  its structure (only `IHDR`, `IDAT` and `IEND`, correct CRCs, the expected size)
  and serves it; the original art never reaches the browser. Unsupported formats
  and failed helpers show a placeholder.
  Finished covers are kept on disk in the state directory (`web.cover_cache_mib`,
  64 MiB by default, each for at most a day), so the library and the worker see
  each cover and size once a day at most, also across restarts. Covers with
  byte-identical images, such as an album's songs, share one worker job per
  size. The browser keeps them for a day as well.
- **Channels.** One Mumble connection per configured channel, each with its own
  client certificate. Every channel starts with its own queue. On the web page a
  channel can play another channel's queue instead: then both hear the same song,
  encoded once and sent to both. Channels may be on different servers.
  People may also subscribe to listen to the bot's channel while staying in
  another channel. These subscriptions count as listeners and keep the queue
  playing; others in their current channel do not receive the music.
- **Browser listening.** The headphones button next to play hears the selected
  queue in the browser; while it is on, a small slider beside it sets the browser
  volume. Its volume is local and independent of Mumble volume and speech ducking.
  Browser listeners keep the queue playing even with nobody in its Mumble channel.
  Pressing the button again releases the listening connection.
- **Big screen.** The arrows button in the player bar opens the playing song over
  the whole window: large cover, scrubber, play/pause and skip, browser listening,
  Mumble volume, rating and the next songs. It can also switch to full screen;
  Esc closes it.
- **DJ mode.** A queue switches into DJ mode on the DJ page: two decks with
  their own decoders, mixed live into the queue's Mumble and browser output,
  with crossfader, one-button fades, EQ, filter, tempo with BPM sync,
  headphone cue in the browser and MIDI controllers. Details below.
- **Queues.** Drag songs, albums or playlists from the library into the queue,
  then drag entries to reorder them. The move handles also accept Alt+Up/Down.
  Remove entries, clear upcoming tracks, skip, pause and set the volume. The player
  bar has a live progress display: click or drag to seek, or use Left/Right for
  five-second steps and Home/End. Space toggles playback outside controls.
  Playback status has a reserved row below the progress bar, so speech activity,
  pauses and buffering keep the surrounding controls in place.
  A queue waits when it has neither Mumble recipients nor browser listeners.
  Deafened Mumble users do not count as recipients. Changes appear for every
  viewer through a live stream.
- **History.** The History view keeps the latest 1,000 finished plays or failed
  attempts per queue, newest first. It shows track details, who queued the track,
  local start/end times, transmitted duration, and whether it completed, was
  skipped, or failed. History survives restarts and loads in pages of 50.
  In DJ mode, deck songs the room heard enter it the same way (see DJ mode).
- **Library history.** Playback also appears in the configured Subsonic account's
  history and play counts. Shared channels count as one play; independent queues
  count separately. The submission rules are described below.
- **Ratings.** Click one of the five stars on the playing song, a library track
  or a History entry to save its rating. Click the star for the current rating
  again to clear it. Listeners share the configured library account.
- **Lyrics.** The song page shows a song's lyrics from the library, and the
  lyrics button in the player bar opens them on the big screen (where the same
  button switches between lyrics and the next songs). Timed lyrics follow the
  music while that song plays in the selected queue; clicking a line plays from
  there.
- **Playlists.** Everyone with a Mumble certificate owns personal playlists:
  **New playlist** on the Playlists page, an *Add to playlist* menu on songs,
  albums, the song page and the playing song, and a heart in the player bar, the
  big screen and the song page for their Favorites. Everyone sees every playlist
  and queues one song or all of it; the owner renames, shares and deletes, and a
  shared playlist takes songs from everyone with a certificate. Playlists made in
  the library itself stay read-only. Details below.
- **Ducking.** While somebody in a channel speaks, its Mumble music output gets
  quieter (by default to 30 % of its volume) and comes back 0.8 s after the last
  speech. It fades down in about 60 ms and up in about half a second, so nothing
  clicks. It reacts to speech, not to a microphone that is merely open: the bot
  decodes what each person sends and judges every 10 ms. A frame counts as speech
  when RNNoise's speech probability passes the threshold and the frame is at least
  9 dB above that person's background level, the quietest frame of the last 1.5 s.
  Speech has gaps that quiet between syllables; steady hiss, hum, fans and silence
  do not. 30 ms of speech in a row are needed, so a click does not duck. Each queue
  has its own settings on the web page: on or off, how quiet, for how long, and how
  sensitive the speech detection is (the same scale as Nuschl's voice activation).
  The page notes when a queue is ducked. The bot decodes voice only while a queue
  with ducking plays to that channel.
- **Formats.** MP3, FLAC, Ogg Vorbis, Opus, WAV, AAC and ALAC (MP4 is downloaded
  before it plays, up to 512 MiB). Everything is resampled to 48 kHz stereo and
  sent in 20 ms Opus packets (960 samples per channel), at 96 kbit/s by default.
  Increasing bitrate changes packet size rather than audio duration: 320 kbit/s
  averages 800 payload bytes per packet. The encoder caps each payload at 992 bytes
  to leave room for Mumble headers within its 1 KiB packet limit. This prevents
  large VBR peaks from being discarded. The bot also reserves transport overhead
  when the server allows less bandwidth; a 320 kbit/s server budget permits up to
  294.4 kbit/s of Opus with the current overhead allowance.

## Links

Every bot connection publishes a Mumble comment with the configured public web
address and instructions for requesting a personal link. The public comment
contains no authenticated URL and is republished after reconnecting.

Write `!web` to the bot, in its channel or privately. It answers privately with a
link like `https://music.example.org/u/5f0c…/`. That link is your key, so keep it to
yourself. Opening it stores the key in an HttpOnly cookie for 90 days and moves
the browser to the public address, so the key never stays in the address bar.
The cookie is `SameSite=Lax` and scoped to the public address's path. Over HTTPS
it is `Secure` and named `__Host-musicbot_link` (at `/`) or
`__Secure-musicbot_link` (below a path); over plain HTTP it is `musicbot_link`.
**Forget this browser** at the bottom of every page deletes it; the link itself stays valid.
A person with a Mumble certificate keeps the same link; `!web new` replaces
it, and the old one stops working. Without a certificate anybody could use the
same name later, so every `!web` gives a new link. Links are kept in `tokens.json`
in the state directory.

Requests that use the cookie must come from the page itself. The bot refuses them
with 403 when the browser's `Sec-Fetch-Site` is anything but `same-origin` or
`none`, or when an `Origin` header names another origin than the public address.
Open the page at the public address you configured; another host name for the
same server is a different origin. Requests with the link in the path
(`/u/<token>/…`, for scripts) do not use the cookie and are not checked this way.

The address bar can be copied and pasted anywhere: it contains only the public
address and the open view, such as `https://music.example.org/#/album/0/a1?library_key=0b6f3c1e9d2a4f5b8c7e6d5a4b3c2d1e`.
**Copy link** on library pages copies the same URL. Somebody who has opened their
own link in that browser sees the view as themselves. `library_key` is the
library's key from the state: a random value the bot keeps for each library in
`library_keys.json` in its state directory and replaces when the library's name
or URL changes. So the view finds the same library even when the configuration
lists it at another position.
Without a remembered link the page explains how to get one. A replaced or
revoked link is forgotten the next time the page loads.

A link grants everything the page does: controlling every queue, reading playback
history, and streaming any queue's audio in a browser (only the server-wide cap of
16 browser streams applies, about 3 Mbit/s each). The bot also writes to each
configured library account: it scrobbles every qualifying play to that account's
history, and any valid link can set or clear that account's song ratings. The bot
uses one account per library, so every link holder shares those ratings.

At most 4,096 links are retained. When the limit is reached, issuance fails
without replacing another person's link. Token files are capped at 4 MiB.
Link changes use synced atomic replacement; a failed write rolls back the in-memory
change, and the next issuance confirms that rollback on disk before returning a link.

Anyone who can write to the bot on its servers can get a link, and every link
controls all queues of the bot.

Library detail links and drag actions belong to the current browser connection.
A reconnect or library change clears old browse data and returns detail pages to
a safe list. Expired detail URLs cannot send old IDs to a replacement backend:
a shared view names its library by key, and a key no current library has (its
name or URL changed) opens the album list instead of the item.

## Running it

On NixOS:

```nix
imports = [ inputs.nuschl.nixosModules.musicbot ];
services.nuschl-musicbot = {
  enable = true;
  virtualHost = "music.example.org";   # nginx in front, links use https://
  enableACME = true;
  credentials.navidrome = "/run/secrets/navidrome-api-key";
  settings = {
    subsonic = [
      { name = "Navidrome"; url = "https://navidrome.example.org"; api_key_file = "navidrome"; }
    ];
    channel = [
      { name = "lounge"; host = "voice.example.org"; username = "Music"; channel = "Lounge"; }
    ];
  };
};
```

`credentials` are passed with systemd's `LoadCredential`; a relative `*_file` in the
settings names one of them. The service runs as a dynamic user with its state in
`/var/lib/nuschl-musicbot` (certificates in `identities/<channel>/`, links in
`tokens.json`, playback state in `queues.json`, library keys in `library_keys.json`,
decoded covers in `covers/`). The package includes the Svelte
frontend. nginx keeps no access log for the page, since the links are secrets.

The NixOS module validates the generated TOML during the build with the selected
package's `--check-config` command. Unknown keys, missing or mistyped fields,
invalid authentication combinations, ranges and endpoint syntax fail the build.
The service depends on the checked file, so it cannot deploy an unchecked config.
The checker uses the same Rust schema and validation rules as normal startup;
it reads no credentials, creates no state and makes no network connections.
To check a configuration manually:

```sh
direnv exec . cargo run -p nuschl-musicbot -- --config musicbot.toml --check-config
```

Credential availability and correctness, server reachability, certificates, channel
existence and permissions still depend on the running deployment.


For a source build, run `direnv exec . just musicbot-web`. The bot plays only
with its codec worker in place beside it, so lay out a prefix as the package does:

```sh
direnv exec . cargo build -p nuschl-musicbot -p nuschl-musicbot-codec-worker
mkdir -p prefix/bin prefix/libexec/nuschl-musicbot
cp target/debug/nuschl-musicbot prefix/bin/
cp target/debug/nuschl-musicbot-codec-worker prefix/libexec/nuschl-musicbot/
prefix/bin/nuschl-musicbot \
  --config musicbot.toml --state-directory DIR --web-assets apps/musicbot/web/dist
```

`apps/musicbot/nuschl-musicbot.example.toml` explains every setting. Relative secret
file names resolve beside the configuration, or in systemd's credentials directory.
The page speaks HTTP; put a TLS proxy in front of it. Assets and API URLs are
relative to the public address, including when a proxy serves it under a path.
For frontend development, run `npm run dev` in `apps/musicbot/web` through direnv,
and set the bot's `public_url` to the Vite address (`http://localhost:5173`), so
the links point there and the page's origin passes the bot's same-origin check.
Open the `/u/<token>/` link once. Vite proxies it and the API, art and licence
requests to a bot on :8474, so the bot's cookie works for the dev page.

Servers with a self-signed certificate need `certificate_sha256`, the SHA-256 of
the server certificate. `channel` is the path from the root channel, so a
subchannel needs its parents: `Games/Music`, not `Music`. A channel the bot cannot
find is reported on the web page, with the full paths of channels of that name if
there are any, and the bot stays in the root channel. When a connection drops, the
bot reconnects with growing pauses up to a minute.

## Security and codec roles

The parent owns Mumble TLS, Subsonic credentials, bearer links and queue metadata.
It transports opaque library bytes; it does not call a decoder, voice classifier
or encoder, and does not link one: the codec crates belong to the separate
`nuschl-musicbot-codec-worker`, which `cargo-deny` alone allows to depend on them.
The parent's own signal processing works on the validated 48 kHz PCM the decode
children return: the volume and ducking gain, and in DJ mode the mix, the tempo
resampler and a limiter in first-party safe Rust, and the EQ and filter as
direct form I biquads with cookbook coefficients from the safe `biquad` crate
(`apps/musicbot/src/player/dsp.rs`).
The bot starts that worker only from its fixed place beside itself
(`<prefix>/libexec/nuschl-musicbot/`; a missing worker fails the operation) as
`--codec-worker <role>`, in one fixed role, through versioned, size-capped IPC
(`nuschl_ipc::musicbot`):

- Decode: one child for the current track of each active player, and in DJ mode
  one per loaded deck (two at most). The parent streams
  bounded opaque chunks; the child returns exactly 20 ms of finite stereo PCM per
  reply. The retained parent queue is 250 frames (5 seconds, 1,920,000 PCM bytes), plus at most
  five 32 KiB pump/read/write buffers, one 64 KiB response and bounded pipe storage.
- Encode: one lazy persistent child per player, one 20 ms request at a time.
  It uses Opus Audio, constrained VBR, and at most 992 encoded bytes per reply.
- Duck: one lazy persistent child per Mumble connection, one packet at a time,
  at most 1 KiB per packet and 64 retained speaker states. A request that fails
  (a crash, a malformed reply, the 2-second deadline) costs only that packet once
  the child's exit is confirmed: the Mumble connection stays up, the talker whose
  packet was in flight is ignored for the rest of the session (poison input is not
  retried), and after three such failures ducking stops until the next Mumble
  session, with one notice on the page. Nothing else (no ducking or settings
  change) resets this. When ducking is off, the child is retired once and later
  packets send nothing. Only an exit that cannot be confirmed ends the session; the
  connection then stays down rather than start a second detector.

Each role also has bubblewrap namespace supervision processes. These are process
costs, not additional user audio streams. Children have no audio
socket or device access. The Linux launcher clears the environment, exposes only
read-only runtime files and private temporary storage, unshares networking and other
namespaces, and limits address space to 1 GiB per child. It fails closed if required
sandbox tools are unavailable. On Windows and macOS no confinement for these
persistent roles has been designed or proven, so codec launch fails closed there
and the bot plays nothing.

After clearing the inherited environment, [bubblewrap 0.12](https://github.com/containers/bubblewrap/blob/v0.12.0/bubblewrap.c#L3195)
sets only the fixed public `PWD=/tmp` for Rust codec children. No credential or
parent environment value is admitted by the service inspection fixture.

IPC frames are capped at 64 KiB before allocation. Encoder/duck exchanges have a
2-second exchange deadline and a separate two-second retirement grace; an error
returns when that grace expires, while the original child and containment remain
owned. A canceled-request preflight has the same retirement bound and cannot
reuse old pipes or start a replacement. A decode operation must produce its next
reply within 35 seconds
(excluding consumer backpressure). Network reads have the existing 30-second bound.
Before its first PCM, a decode child reports work that yields no audio in exact
steps: each MiB of an MP4 file it reads whole, and each 250 frames (5 seconds) it
skips towards a seek position. The parent accepts only increasing whole steps of
the stages the request allows, never more input than it actually sent, a bounded
number of them, and audio only after the whole skip. Progress resets only the
35-second wait: the first PCM must arrive within 300 seconds of starting the child,
a seek may start at most three hours (540,000 frames) into a song, an MP4 file may
be at most 512 MiB and other input before the first PCM at most 2 GiB. Exceeding a
limit fails the song with an explicit message, never a truncated decode, and progress
after the first PCM is a protocol violation. The skip happens after resampling, so a
seek plays exactly the frames an unskipped decode would play from that position.
Cancellation closes the consumer promptly but replacement awaits actual child exit
and the outstanding network read. Decoder terminal notification is separate from
cleanup and does not need space in the PCM queue. Errors follow already-buffered
PCM; a provisional decoder Done is not recorded as successful history until the
final network result and reaping are confirmed. Kill/reap failure retains the old
process slot; no replacement is created. A retiring encoder or decoder suspends
new playback jobs while pause/seek/queue controls remain responsive. A panicked
decoder supervisor does not confirm reaping and permanently retains that slot.
A song has no artificial total-duration limit: retained
queues and each operation are bounded. An HTTP sender can stream indefinitely only
while bounded decoding continues and the configured queue retains that track.

MP4 downloads are confined to the child and capped at 512 MiB. Decoder input rates
are 8–384 kHz with at most eight channels (Opus: one or two). Real FFT lengths are
capped at 16,384 samples; each probe/packet operation may read at most 4 MiB.
Replies are validated before use, and a seek/skip/pause/destination change is checked
again after asynchronous encoding, before broadcasting or advancing playback.
An encoded frame rejected by that fence resets the encoder before its next use,
so an untransmitted frame cannot become hidden predictive encoder history.
The NixOS service retains DynamicUser, ProtectSystem=strict, ProtectHome,
PrivateDevices, NoNewPrivileges, empty capabilities, MDWE and its syscall filter.
Namespace setup is restricted to the kinds used by bubblewrap; mount/unshare/setns/
capset are explicitly permitted. AF_NETLINK is needed to initialize the child's
private loopback. The parent uses ProcSubset=all so bwrap can read overflow IDs.
ProtectProc=invisible remains enabled. ProtectHostname=private preserves UTS
isolation. ProtectKernelTunables/ProtectKernelLogs are disabled because their
locked proc bind mounts prevent Linux from mounting a genuinely private proc in
an unprivileged nested namespace. Sysfs stays read-only; DynamicUser/empty
capabilities prevent host sysctl writes, kernel-log syscalls are explicitly denied,
and PrivateDevices excludes /dev/kmsg. No host proc is bound into a codec child.
These are measured service-policy changes, not an unrestricted worker fallback.

The owned Linux launcher and exact service-profile replica denied a synthetic
credential file, a loopback listener and a synthetic credential environment
variable. The final NixOS VM passed actual service playback, then inspected the
running encoder: fixed role arguments, only stdio file descriptors, private PID
and network namespaces, a 1 GiB address-space limit, and no profile/credential
mounts or inherited parent environment. Under the actual service UID and mount
namespace, non-mutating checks confirmed empty effective capabilities, unwritable
host sysctl/sysrq paths and writable service-owned state. The VM also passed the
full standalone stock-Murmur/fake-Subsonic suite. Its network was restricted and
all media was generated; it opened no host microphone or audio service.

These checks do not establish Windows/macOS confinement, a complete hostile-media
corpus for every supported format, physical audio quality or instantaneous
revocation of packets already submitted to the session. A canceled blocking
library read can delay replacement until its existing 30-second read bound.

Authenticated SSE streams share 32 lifetime permits; overload returns HTTP 503
without an admission wait queue. Artwork has two global jobs. Each owned task
retains its permit through the worker's bounded response, EOF and actual process
reaping, even if the HTTP client disconnects. A helper failure never falls back
to parent decoding or to the original art. The fixed packaged content helper
has no network, credentials, profile or audio devices.

The page loads covers as images from the bot itself (CSP `img-src 'self'`),
lazily as they come near the viewport. Every image it can load is a PNG the
content worker made, so browsers decode only those.

Use `tokens_file = "channel-tokens"` for private Mumble ACL tokens; its contents
are a JSON array of token strings supplied through `credentials.channel-tokens`.
The NixOS module rejects inline `channel.tokens`, which would otherwise enter the
world-readable Nix store. Standalone inline tokens remain supported, so protect
that configuration file like the other credentials. API keys/passwords retain
existing credential-file handling. Never put secret values directly in Nix settings.

## Tests

`apps/musicbot-codec-worker/tests/confined.rs` runs the decode and duck roles
confined by the production launcher, with real faults, and a canary child
started by the same launcher: files outside it are hidden, loopback connections
fail and its environment holds only `PWD`. The codec roles' framing, role
checks and encoder reset have unit tests in the worker. `vm-musicbot` decodes
the bot's Opus with the reference libopus, and its module evaluation checks
that inline ACL tokens are refused. `musicbot_service_isolation.py` runs inside
the owned NixOS VM and verifies actual service/child metadata and
non-mutating access checks.

The artwork/SSE follow-up passed 91 bot tests, 30 frontend tests, 20 IPC tests
(one existing ignored test), and one content-worker unit plus four integration
tests (one existing ignored test). `apps/content-worker/tests/thumbnail.rs`
drives the real thumbnail supervisor and sandbox against the content worker and
a faulty worker: malformed, duplicate and oversized replies, crashes, missing
sandbox programs, a worker that stays after its reply, a caller that stops
waiting while the job keeps the worker to its deadline, unreadable input and
an image wider than the decoder admits. `musicbot_artwork.py` runs the packaged UI and CSP at
320/390/1280 pixels: exact generated red/blue pixels, library changes, held stale
responses, malformed JSON, offscreen Canvas release and no image/media requests.
Since covers became worker-made PNGs, browser mode checks loaded `<img>` pixels,
lazy loading, held stale responses, a non-image answer and a CSP-refused foreign
image instead.
The browser is isolated from host devices and profiles; its non-local background
requests meet an owned rejecting proxy. This proves synthetic artwork behavior,
not full browser resource-exhaustion resistance or platform-wide confinement.
The final NixOS VM also passed the actual service artwork route, codec-child
capability checks and full standalone interoperability suite. An earlier run
failed because its best-effort event drain raced the actor's subscription-switch
end marker; the fixture now observes that specific bounded boundary before
starting new music, retaining the original packet-count/end assertions.

The bounded-retirement tests use test-only acknowledgement gates around real
owned subprocesses and a controlled blocking Read. They verify prompt terminal
notification, retained child/read slots, cancellation, buffered PCM ordering and
late network failures without claiming to reproduce an unkillable kernel task.
Those gates are absent from release code; ordinary real-worker confinement and
service fault checks remain separate evidence.

Rust tests cover queue ordering, seek boundaries, history lifecycle and retention,
durable state, library identity across reordering/removal, gain ramps, VAD,
and authenticated rating reads/writes, bounds, failures and live revisions.
DJ mode's tests cover crossfader curves, EQ and filter responses, resampler
length and continuity at five tempos, gain ramps, the limiter, the transition
timeline, position accounting, auto load, failure back-off, entering and leaving,
control ranges, cue admission, the SSE rate, and in the real playback loop with
controlled decoders that two decks hold exactly two decoders that retire before
the queue resumes. The deck queue, preload, BPM and loop tests cover deck-queue
auto load and requeueing, saved deck queues, cue jumps that play in the next
frame from memory and jumps past the decoded part that wait, preloading the next
deck-queue song, the memory budget, the 30-minute cap, decode children counted
and reaped, preload back-off, the beat grid within 10 ms on synthetic beats with
intro silence and swing, tempo within 1 BPM at 85 to 174 BPM, loop lengths exact
in samples at several tempos, the loop crossfade, quantised and free loop points,
beat loops at several BPMs, the cache's bound and restart, band energies on
tones and noise, column timing, and bounded spectrum ranges bound to the deck's
song.
Frontend tests cover progress interpolation, scrubbing, drag indices, route
encoding, shortcuts, and history pagination during live refreshes.
`tests/musicbot.py` drives the JSON API and two SSE viewers against stock Murmur
and a plain/OpenSubsonic fixture. It checks synchronized mutations, shared and
separate audio, speech-only ducking, seeking, public presence comments, completed,
skipped and failed history, pagination, restart recovery with libraries reordered
or removed, and sonic similarity endpoints on the OpenSubsonic library only.
Rust tests cover the `[import]` validation, pseudonym derivation, error mapping,
id checks, reply caps, refused redirects, the import poller and the `api/import`
endpoints against a fixture service. The VM check confirms that the module's bot
reaches its import service with the key from its credential.

The backend reporting check also ran against a real Navidrome 0.64.2 instance
with a six-second FLAC and stock Murmur. Navidrome's now-playing API identified
`nuschl-musicbot` under the configured account. Completed plays and qualifying
skips updated its history and play count; paused enqueues, short skips and a seek
to the end did not. Shared queues submitted once, independent queues separately,
and a deployment restart preserved the original play timestamp without backfill.

Sonic similarity was checked against real Navidrome 0.64.2 with a local
`SonicSimilarity` plugin over six fixture songs. Navidrome advertised
`sonicSimilarity` only after the plugin was enabled. In the browser, the similar
list showed Navidrome's order and scores, **Add all** queued it in that order, and
**Play all next** for a blend from the playing song queued the rest of the path.

```sh
cargo build -p nuschl-cli -p nuschl-musicbot -p nuschl-musicbot-codec-worker
mkdir -p prefix/bin prefix/libexec/nuschl-musicbot
cp target/debug/nuschl-cli target/debug/nuschl-musicbot prefix/bin/
cp target/debug/nuschl-musicbot-codec-worker prefix/libexec/nuschl-musicbot/
direnv exec . just musicbot-web
python3 tests/with_local_murmur.py "$(nix build --inputs-from . nixpkgs#murmur --print-out-paths)/bin/mumble-server" \
  tests/musicbot.py prefix/bin PORT --web-assets apps/musicbot/web/dist \
  --espeak "$(nix build --inputs-from . nixpkgs#espeak-ng --print-out-paths)/bin/espeak-ng" \
  --opus-library "$(nix build --inputs-from . nixpkgs#libopus --print-out-paths)/lib/libopus.so"
```

`nix build .#checks.x86_64-linux.vm-musicbot` runs the bot under its NixOS module
against Murmur in a VM, then the same script.

## Browser listening

The headphones button starts audio for the selected queue after a browser gesture. The
**Browser volume** slider changes only local playback; **Mumble volume** controls
the bot's channel output. Speech ducking applies to Mumble output. Browser audio
uses the original decoded stereo samples (at most full scale), so every format the
confined decode worker decodes can be heard here. Selecting another queue switches the listening stream while keeping
the browser's audio context and volume.

Pause, seek and skip follow the shared queue and clear buffered browser audio.
Browser subscriptions count as listeners. When the last receiving Mumble channel
and browser listener leave, playback waits. Listening connections and browser
audio state are temporary; the saved queue retains its ordinary playback state.

Use HTTPS, or localhost during development. Listening requires streaming fetch,
AudioWorklet and a 48 kHz AudioContext. Browser suspension, page closure, revoked
links and connection/library changes stop listening and release its subscription.
Press the headphones button again to restart after an interruption.

The live stream carries 48 kHz stereo float32 PCM, about **3.1 Mbit/s per listener**.
The server admits at most 16 browser streams, with bounded shared frame storage
and a one-record HTTP handoff. The worklet starts after 60 ms of audio and retains
at most 200 ms in its ring; slow readers discard old frames and resume at the live
edge. Network and browser scheduling add their own latency.

Verified in real Chromium with stock Murmur: browser-only playback with Mumble
volume zero, independent local mute/volume, pause/seek/skip, queue switching,
listener release and suspension cleanup. Reference-Opus Mumble output fell to
the configured speech-ducking level while measured browser output stayed steady.

## DJ mode

**DJ mode** on the DJ page (or **DJ mode** in the queue's heading) turns the
selected queue into two decks, A and B, mixed live into the same output: its
Mumble channels and its browser listeners hear the mix, with the queue's Mumble
volume and speech ducking applied as before. The playing song continues on deck A
at its position with its decoder; with auto load on, the queue's next song waits
on deck B, paused. **Leave DJ mode** continues the lead deck's song (the playing
one, else the louder one) as the queue's current song from its position.

Each deck has title, artist and cover; the song's BPM at the deck's tempo (see
BPM and beat grid); the position with a seek slider,
play/pause, **Cue** (go to the cue point), **Set cue**, load next and eject; a
3-band EQ (low shelf 250 Hz, mid peak 1 kHz, high shelf 2.5 kHz, −26 to +6 dB),
a filter knob (low-pass down to 60 Hz on the left, high-pass up to 8 kHz on the
right, neutral in the middle) and the channel fader; a tempo fader with a ±8 %
or ±16 % range that changes speed and pitch together, like a turntable, and
**Sync**, which sets the deck's tempo so its BPM matches the other deck's
tempo-adjusted BPM (widening the range to 16 % when needed; beat phase stays with
the DJ); and a headphones button: this browser alone hears the deck after EQ,
filter and tempo, before its channel fader and the crossfader, at its own volume.

The mixer has the crossfader with a **Smooth** curve (constant power, each deck
at −3 dB in the middle) or a **Cut** curve (both decks at full level, cut within
the last 4 % of the travel), **Fade to A/B**, which moves the crossfader to the
other deck over the chosen time and starts that deck, and **Auto load**: after a
fade the deck that went silent pauses and takes its next song, and a deck whose
song ends takes the next one too. Moving the crossfader stops a fade where it is.
Everyone with a link controls the decks.

**Deck queues.** In DJ mode each deck has its own queue under it (on phones: deck
A, its queue, the mixer, deck B, its queue). Its rows reorder by drag or by
Alt+Up/Down on their handle, and have **Load now**, moves to the other deck's
queue or the main queue, and remove. The **A** and **B** buttons on library songs,
albums, playlists and main-queue rows add to that deck's queue. A deck's next song
(for **Load next**, auto load and the keyboard) is the first of its own queue,
else the first of the main queue. A song that leaves a deck before the room heard
it goes back to the front of the queue it came from. Deck queues have the main
queue's cap (5,000 songs each), are saved with the queues in `queues.json`, and
stay while DJ mode is off, hidden until it is on again.

**Drag and drop.** Each deck takes the page's drags: library songs, albums and
playlists, main-queue rows and deck-queue rows. A song loads onto the deck; an
album or playlist puts its first song on the deck and the rest at the front of
the deck's queue. The deck shows a highlight and "Drop to load onto deck A"
while something is dragged over it. A deck queue takes drops at the gap the
pointer points at, the main queue takes deck-queue rows back. Keyboard users use
the **A**/**B** buttons and **Load now**.

**Songs in memory.** Each deck's song, and the song each deck loads next, is
decoded completely into memory as fast as its decode child delivers, as
interleaved 16-bit stereo at 48 kHz (11.5 MB a minute). Playback, cue jumps,
seeks and loops read from there: a jump inside the decoded part plays in the next
frame, after the one-frame fade; a jump past it waits for decoding to reach it.
The memory is one budget across the bot, `audio.preload_mib` (768 MiB by default,
at most 16,384; 0 lets decks stream their songs as in the queue). Songs longer
than 30 minutes or of unknown length and songs the budget cannot hold stream
from a decoder like a queue song (a seek then restarts the decoder); a deck that
streams a song (such as the queue's song carried onto deck A) keeps streaming it
until its next seek or load, then plays it from memory once it is there. At most four preload decode
children run at once across the bot, each keeping its process credit until it
has exited; a playing deck's song comes before a paused deck's, and both before
next songs, whose decoding stops for them and resumes later where it stopped.
Next songs give way first when memory runs short, and a song's memory returns
once it has left its deck and is no longer next. A preload failure fails that
song; preloading waits 0.5 seconds after a failure, doubling up to 8 seconds,
and stops with a notice after three failures in a row. The NixOS service sets no
memory limit of its own; size `preload_mib` to the host.

**BPM and beat grid.** A song's BPM comes from the library when it gives one
(OpenSubsonic `bpm`, which Navidrome reads from the file's tags); otherwise the
bot estimates it from the song in memory, and the deck shows "≈" and "BPM est."
(and "Analysing…" while it runs). The estimate reads 60 seconds from 30 seconds
into the song (from its start for songs under 90 seconds): a mono copy at 8 kHz,
an onset envelope at 100 Hz from a 256-point spectrum in 15 bands, then tempo by
a comb over the envelope's autocorrelation from 60 to 200 BPM, refined to 0.1
BPM, with octave correction towards 80–170 BPM. Below a confidence of 0.15 the
BPM stays unknown. The beat grid is the song's beats at a constant tempo: the
library's BPM or the estimate, corrected by up to 0.5 % from the whole decoded
song, and the first beat, found by matching the onset envelope against a pulse
train at that tempo; below a confidence of 0.15 the grid stays unknown. One
analysis runs at a time across the bot: a first one when the window is in
memory, a final grid when the whole song is. Results are kept per library and
song ID in `beats.json` in the state directory (private, replaced atomically,
at most 20,000 songs, the oldest leaving first), so each song is analysed once.
Sync uses the library's BPM or the estimate.

**Loops.** Each deck loops from its song in memory. **In** and **Out** set a
loop from the position, **Loop** turns it off or plays the last loop again from
its start, the beat buttons make a loop of 1/4 to 32 beats from the current beat,
½ and ×2 halve and double it, and ◀ ▶ move it by its length or by one beat (a
deck playing inside a moving loop moves along). With **Quantise** on (the
default) and a known beat grid, loop points snap to the nearest beat and beat
loops start on the beat at or before the position; without a grid loops use the
exact positions and the deck says the loop is free. A loop lasts a quarter beat to
64 beats, or 10 ms to 60 seconds when free, and lies within the song. The loop is
in song time, so tempo changes keep working inside it, and its end wraps to its
start within the frame, sample-accurate, with a 5 ms equal-power crossfade. A
seek or cue jump outside a playing loop turns it off; inside, it keeps looping.
Songs a deck streams cannot loop. The deck's position bar and its waveform show
the loop with handles to drag (snapping like the buttons) and, with a grid, beat
ticks with bars emphasised.

**Waveform.** Each deck draws its song around the playhead (±3, ±6 or ±12
seconds) as a three-band waveform (low, mid, high in three theme colours) or,
with **Spectrogram**, as a heat map of 24 bands, with the beat grid (bars every
four beats drawn stronger), the cue point and the loop, whose ends drag here too.
An overview strip above the position bar shows the whole song with the visible
part and the loop. The bot computes the band energies from the song in memory as
it decodes, one 20 ms frame of work at a time: a mono 12 kHz copy through biquad
filter banks, 100 columns per second of song time, 3 coarse bands (20–250 Hz,
250 Hz–2.5 kHz, 2.5–5.6 kHz) and 24 log-spaced bands from 40 Hz to 5.5 kHz, one
byte per band in dB (27 bytes per column, about 1.6 MB for a 10-minute song). They
live with the song's PCM and leave with it; `beats.json` keeps no waveforms. The
page asks for the columns around the visible window (up to 2,000 per request),
keeps them per song, and draws at the deck's tempo from the position it
interpolates between state events; it redraws every animation frame while the
deck plays and the page is visible, and only on changes otherwise. Songs a deck
streams show a flat line with a note.

**How it sounds.** Every control sets a target; the bot moves gains, EQ, filter
and tempo towards it one step per 20 ms frame (settling in about 60 ms), ramps
within the frame and recomputes filter coefficients every 32 samples. Play,
pause, seeks and cue jumps fade over one frame. A peak limiter keeps the sum of
both decks within full scale. The page sends a moving control about 30 times a
second, one request at a time per control.

**Latency.** A control reaches the mix after the network delay, typically 0.1 to
0.3 seconds, plus up to 20 ms; Mumble and browsers add their own buffering.

**Limits.** Two decks per queue. A deck that streams holds one decode child at
most and starts a new one only after the old one has exited; songs in memory use
the preload decode children (at most four across the bot). All of a player's
decode children exit when DJ mode ends. A decode failure fails that deck's
song; the deck's next decoder waits 0.5 seconds, doubling per failure in a row up
to 8 seconds, and after three failures in a row auto load stops with a notice.
Three encoder failures in a row pause DJ mode with a notice. Four headphone cue
streams across the bot. DJ mode itself is not saved: after a restart the lead
deck's song continues as the queue's current song.

**History.** A deck's song counts as heard while the room's mix reaches a Mumble
channel or a browser listener and the deck sounds in it at −20 dB or more
(channel fader times crossfader); the first heard frame starts the play and
announces it as now playing. When the song leaves the deck (end, eject, a new
song, a fade with auto load, leaving DJ mode) the play enters the history like a
queue play: completed at the end, skipped otherwise, failed on errors. Scrobbling
uses the queue's threshold on heard time. Heard time counts the room's mix only;
headphone cue audio stays out of it.

**Keyboard shortcuts.** The DJ page is playable from the keyboard: the left hand
plays deck A, the right hand deck B (the same keys six columns to the right), the
mixer sits in the middle. Keys match by their place on the keyboard
(`KeyboardEvent.code`), so they stay put on every layout; the panel shows each
key's printed character where the browser tells it. Shortcuts act only on the DJ
page and never while a text field, select or editable text has the focus; with
Ctrl, Alt or Meta held the browser keeps the key. Space stays the page's play and
pause for the queue (in DJ mode it holds and resumes the whole mix).

| Action | Deck A | Deck B |
| --- | --- | --- |
| Play / pause | D | L |
| Go to cue point / set it | F / Shift+F | ; / Shift+; |
| Seek back / forward 4 s | A / Q | J / U |
| Tempo down / up | S / W | K / I |
| Volume down / up | C / E | . / O |
| Filter towards low-pass / high-pass | Z / X | M / , |
| Cut or restore low / mid / high EQ | 1 / 2 / 3 | 8 / 9 / 0 |
| Sync | R | P |
| Load the deck's next song | T | Y |
| Headphone cue | V | / |
| Loop in / out | Shift+Z / Shift+X | Shift+M / Shift+, |
| Loop on/off (reloop) | Shift+D | Shift+L |
| 4-beat / 8-beat loop | 4 / 5 | - / = |
| Halve / double the loop | Shift+S / Shift+W | Shift+K / Shift+I |
| Move the loop back / forward | Shift+A / Shift+Q | Shift+J / Shift+U |

Mixer: G and H move the crossfader towards A and B (Shift+G and Shift+H to the
ends), B fades to the other deck, N turns auto load on or off, and ? opens
**Keyboard shortcuts**. A press moves a fader one step (volume 5 %, tempo 0.1 %,
filter 0.05, crossfader 0.05); holding the key for a quarter second glides it
(volume 50 % a second, tempo 1 %, filter 0.5, crossfader 1.0), and the browser's
own key repeat is ignored. The panel lists every action with its key; **Change**
binds the next key pressed (Escape cancels; a key bound elsewhere moves to the new
action and the panel says so), **Unbind** frees one and **Reset to defaults**
restores the layout. The bindings are kept in this browser's localStorage.

**MIDI controllers.** In browsers with Web MIDI (Chromium-based, over HTTPS or on
localhost), **Use a MIDI controller** lists the inputs. **Learn** beside a
control, or **Learn on the decks** and a click on it, binds the next fader, knob or
button moved; Escape cancels. Faders and knobs take control changes: absolute,
14-bit when the controller also sends controller + 32 while learning, or relative
encoders (1–63 up, 65–127 down); EQ, filter, tempo and crossfader knobs rest at
their middle. Buttons take note-on or a control change of 64 or more. The mapping
is kept in localStorage per device name and can be cleared. MIDI drives the queue
selected on the page; headphone cue needs a click. Web MIDI's default allowlist
(`self`) admits the page's own origin, and the bot sends no Permissions-Policy
header that narrows it. Every controller is set up through Learn.

## Lyrics

The bot asks the library that holds the song. Libraries with the OpenSubsonic
`songLyrics` extension (Navidrome among them) get `getLyricsBySongId` and may
answer with timed lines from embedded or sidecar lyrics; the first timed lyrics
in the answer win, otherwise the first untimed ones. Their `offset` moves every
line (positive shows lines sooner), and lines are ordered by their start. Other
libraries get the classic `getLyrics` with the song's artist and title and answer
plain text. Each line keeps at most 500 characters without control characters,
and at most 2,000 lines are kept. A library answer of "not found" (error 70)
means the song has no lyrics.

Timed lyrics highlight the line being sung while the selected queue plays that
song, using the same position as the progress bar, and keep it in the middle of
their box. Scrolling the box by hand pauses this for five seconds. Lines are
buttons that seek there. The highlight follows the position the bot reports;
Mumble and the browser add their own short delay.

## Song ratings

Click a star to save one to five stars immediately. Click the star for the current
rating again to clear it using the standard Subsonic
[`setRating`](https://opensubsonic.netlify.app/docs/endpoints/setrating) value 0.
Stars appear inline in the player bar, library song lists and playback History.

Navidrome stores ratings per account. The bot uses the configured library account,
including the account associated with an API key. All listeners and queues using
that account share its rating; the latest saved choice wins. Ratings remain in
the library server across bot restarts.

Visible stars load the current backend rating and follow changes from other
listeners. Offscreen rows wait until they are visible, and all star controls share
two client-side read slots. Playback progress does not reload ratings. Redisplaying
a track also picks up edits made directly in Navidrome.
Reconnecting disables stale controls until fresh ratings arrive; tracks from a
removed library cannot be rated. A failed write keeps the last confirmed rating
and shows **Retry**, which reloads the backend value before another star can be saved.

Verified against real Navidrome 0.64.2 with two independent browser listeners:
all five ratings and clearing, player/library/history controls, live updates,
unchanged request counts during playback, failed writes and explicit recovery,
and retained backend ratings after a bot restart. Narrow-screen controls were
also exercised in Chromium.

## Playlists

Personal playlists are real playlists of each library's configured account,
made with the Subsonic `createPlaylist`, `updatePlaylist` and `deletePlaylist`
calls and kept private in the library (`public=false`). The bot records who owns
each one in `playlists.json` in its state directory, a private file it replaces
atomically:

```json
{"version":1,"playlists":[{"library":"<library key>","id":"<playlist id>",
  "owner":"r<32 hex digits>","owner_name":"Ada","shared":false,"favorites":false}]}
```

`library` is the library's key from `library_keys.json`, `owner` the person's
pseudonym (see "Requester pseudonyms" under Adding music; the secret in
`import_requester.key` exists also without an import service) and `owner_name`
their Mumble name, renewed whenever they change a playlist. Entries of a library
whose key leaves `library_keys.json` go at the next start, and entries of
playlists the library no longer lists go when somebody opens the Playlists page.

A playlist lives in the library selected when it was made and takes songs of
that library only. The page shows your playlists first (Favorites first), then
other people's with their name and a **Shared** badge, then the library's own.

| Who | View and queue | Add and remove songs | Rename, share, delete |
| --- | --- | --- | --- |
| The owner | yes | yes | yes, except Favorites |
| Others with a certificate | yes | while it is shared | — |
| People without a certificate | yes | — | — |
| Anybody, for playlists made in the library | yes | — | — |

People whose Mumble client lacks a certificate share one pseudonym per server, so
owning playlists needs a certificate; the page explains why its controls are off.
**Favorites** is a personal playlist per person and library, made by the first
heart and named "Favorites of <name>". It stays private and keeps its name; its
owner adds and removes songs. Turning a heart off takes every copy of the song out.

Limits: names have 1 to 100 characters without control characters; a person owns
at most 50 personal playlists and the bot keeps at most 10,000; a playlist holds
at most 5,000 songs; one request adds 1 to 500 songs (an album with more is
refused); song ids have at most 128 bytes. Songs go to the library 100 at a time
and in at most 4 KiB of encoded ids per request, so request URLs stay short.

Changes of one playlist wait for each other, as do one person's new playlists
and Favorites; a change waits up to 30 seconds for its turn and then answers 503.
The lock then moves into the admitted library call, which holds it until the
library has answered, also when the browser has gone. Removing a song names its
position and id; the bot reads the playlist again and answers 409 when another
song is there now. After every change that reached the library, State/SSE
`playlists.revision` advances and every open playlist view reloads.

Tested against a loopback Subsonic fixture that keeps playlists the way
Navidrome's API does; a run against a real Navidrome is still open.

## Backend playback history

The bot uses the standard Subsonic `scrobble` endpoint. After the first successful
audio packet it sends `submission=false` to announce now-playing. When playback
ends, it sends `submission=true` if transmitted audio reached half the advertised
duration or four minutes, whichever is shorter. Tracks without a positive known
duration require four minutes. Completion, skip and decoder failure use the same
threshold; paused time, listenerless waits and audio skipped by seeking do not
count.

Reports use the library's configured account, including the account associated
with an API key. Mumble names identify who queued a track in the bot's own History
view; they do not create separate Subsonic users. Navidrome updates that account's
play count, last-played time and backend history. Any external scrobbling uses the
integrations configured for that account on the library server.

Each logical queue reports once, even when several Mumble channels hear it.
Pause and seek retain the ongoing play. Deployment restarts retain its first-audio
Unix timestamp in milliseconds and transmitted duration; the resumed play may
announce now-playing again, then submit once when it ends. Finished history
restored at startup is not backfilled.

One owned reporter serializes requests across queues. Failed requests are logged
without remote diagnostics and are not retried: Subsonic provides no idempotency
key for a submitted play. SIGTERM, Ctrl-C and web-server failure join the current
request and drain already-finished reports within a total 30-second grace. An
ongoing play remains in the saved queue for deployment recovery.


## Playback state

Deployment restarts retain the current song and position, upcoming entries and
their order, pause, volume, ducking settings, and channel-to-queue assignments.
There is a brief audio gap while the bot reconnects, then playback resumes.
Back up `queues.json` together with the bot's state directory. Keep the channel
names and library names/URLs stable: queues are matched by channel name, and
tracks by library name plus URL, rather than configuration order.

History starts with the first audio frame successfully broadcast to Mumble or a
browser. A shared queue counts each frame once across both outputs. Transmitted
duration excludes paused/buffering time, listenerless waits and skipped-over audio
after a seek. Reaching EOF marks a play completed even if the user sought past part
of it. Failed attempts that sent
no audio show **Never started**. Removing an upcoming track or skipping before
any audio does not enter history. Pause, seek and restart preserve one ongoing
play; its history entry appears when it finishes or an error is processed.

History lives in `queues.json`, including ongoing-play start and transmitted
duration. Older queue files load with empty history; past plays are not inferred.
Historical metadata remains readable if its library is removed; its browsing and
artwork links become unavailable. Queues removed from the configuration are dropped.

Queue changes are checkpointed after one second and playing positions every five
seconds. SIGTERM and Ctrl-C write a final checkpoint before exit. Writes are
serialized, synced and atomically replaced. A crash can lose changes since the
last checkpoint; a corrupt or unsupported file stops startup and remains untouched.
Upcoming tracks whose library was removed are dropped with a warning. If the
current track's library disappears during deployment, its ongoing play is retained
as failed before decoding resumes.

Server bandwidth changes apply live. A channel whose budget cannot support the
6 kbit/s codec minimum plus transport overhead stops receiving music; other
attached channels and browser listeners may continue. Position and history duration
stop advancing only when no eligible Mumble or browser output remains.

Seeking restarts decoding and discards audio before the requested position, which
works with all supported formats and server transcodes. On a slow connection or a
long track, seeking can take time; the player shows buffering while preparing it.

## Adding music

With an import service configured, the web page offers **Add music**: people
search the import service for albums missing from the libraries, open an album
page with its tracks and **Add to library**. When the library already has an album
of the same title and artist, the page says so, links to it and the button reads
**Add anyway**. **Requests** lists recent imports, newest first, with their
progress, a **Yours** marker and how many more tracks the viewer can add today.
Once the import service has handed an album over, the page says it shows up in the
library within a few minutes, while the library imports and scans it.

```toml
[import]
url = "https://import.example.org/" # https, or http on 127.0.0.0/8, ::1 or localhost
key_file = "import-key"             # bearer key, a credential like the other *_file entries
library = "Navidrome"               # the [[subsonic]] library added albums arrive in
```

The section is optional: without it `state.import` is `null` and the page hides
the feature. `--check-config` checks the URL (no user name, query or fragment;
https, or http only for loopback hosts) and that `library` names a configured
library, without reading the key. On NixOS the key is one of the module's
`credentials`.

**Requester pseudonyms.** The import service learns one stable pseudonym per
person: `"r"` and the first 32 hexadecimal digits of HMAC-SHA256
over the Mumble server, a zero byte and the person's certificate hash, keyed with
a 32-byte secret the bot makes at its first start in `import_requester.key` in its
state directory. The same pseudonyms name the owners of personal playlists. People
without a certificate share one pseudonym per server. The
bot watches the import list and fetches covers, which every viewer shares, as
`musicbot`.

**Limits.** The import service enforces them: lookups per requester and in total
per minute, and tracks added per requester and in total over a rolling day. Its
answers become fixed texts on the page:

| Cause | Status | Text |
| --- | --- | --- |
| `rate_limited` | 429 | Too many searches at once. Try again in a few seconds. |
| `limit_reached` for the requester | 409 | You have added as much as you can for today. Try again later. |
| `limit_reached` in total | 409 | The library has taken in as much as it can for today. Try again later. |
| `too_many_tracks` | 422 | This album is too large to add. |
| `not_available`, `not_found`, an invalid id | 404 | This album cannot be added. |
| any other answer, a network error, a timeout or a malformed reply | 503 | Adding music is not available right now. |
| the bot's four import calls are all in use | 503 | Too many requests at once. Try again in a moment. |

**Security.** The key is a credential file, read once at startup and sent only as
`Authorization: Bearer` to the configured URL; redirects are not followed, so the
key never goes anywhere else. Every call has its own admission (four at once,
none waiting), a 30-second deadline and a 1 MiB cap on its reply. Ids are exactly
16 lowercase hexadecimal characters before they enter a URL path; replies are
parsed into typed values and checked (bounded texts without control characters,
sane numbers, at most 50 albums or imports and 200 tracks) before a viewer sees
them; the import service's error texts become the fixed texts above. The confined
content worker re-encodes covers as PNGs like library covers; they are cached in
`covers/` under the key namespace `import`, which no library key (32 hexadecimal
digits) can take.

A background task asks for the import list once at start, every 3 seconds while
an import is queued or being added and every 30 seconds otherwise or after a
failure. When an import's status or progress changes, `state.import.revision`
advances and viewers receive the new state; the page then reloads its requests.
A request through the bot makes the task look again at once.

## Web API

The page uses the endpoints below `api/` next to the public address and
authenticates with the remembered link's cookie. Scripts can use the same
endpoints below `/u/<token>/api/` instead; a token in the path never falls back to
the cookie. Cookie requests from another origin or site get 403 (see Links).
POST bodies must be JSON, which cross-site forms cannot send. `POST forget` with
`{}` answers 204 and deletes this browser's cookie. `GET state` returns a
snapshot; `GET events` sends `event: state` SSE frames immediately, after changes,
and roughly every two seconds for progress correction, at most ten a second (DJ
faders change the state up to 30 times a second). Slow viewers receive the
latest state when they can read again. Revoked links lose access to the stream.
At most 32 streams may remain open across all tokens; excess viewers receive 503.
The permit belongs to the response stream, not merely its request handler.
Proxies must pass SSE through without buffering; the bot sends `X-Accel-Buffering: no`.

`GET listen?player=N` opens the authenticated live browser audio stream. Invalid
links return 404, unknown queues return 400, and exhausted browser-stream admission
returns 503. Responses use `application/octet-stream`, `Cache-Control: no-store`
and `X-Accel-Buffering: no`. Format headers are `X-Audio-Format: f32le`,
`X-Audio-Rate: 48000`, `X-Audio-Channels: 2` and `X-Audio-Frame-Samples: 960`.

The first body record is byte 0, which clears buffered audio. Byte 1 is followed
by exactly 7,680 bytes: 960 interleaved stereo frames of little-endian float32 PCM,
each sample within -1.0..1.0 (clients reject anything else). Reset records also
delimit pauses, seeks and song changes. Byte 2 is a keep-alive without audio: a
stream with nothing else to send (a paused or waiting queue) sends one at least every
15 seconds, so proxies with read timeouts (nginx defaults to 60 seconds) keep it
open. The stream remains open while the queue waits; disconnecting releases its
subscription. Revoked links are
checked during playback and at least once per second while idle or backpressured.

`GET cue?player=N&deck=D` streams one DJ deck before its channel fader in the
same format and records, for headphones in one browser. Invalid links return 404,
unknown queues and decks 400, and with all four cue streams in use 503; cue
streams have this admission of their own beside the 16 listening streams. A cue
stream stays open while DJ mode is off and then sends only keep-alives.
Seeking or loading the deck resets it.

The authenticated `GET licenses` route serves the installed, bounded
licence text. Nix packages are gated by the scoped Rust/npm notice inventory;
the frontend retains actual emitted-owner licence files and its source lock.
The musicbot scope is separate from the main web client's WASM build.

`GET history?player=N&before=ID&limit=N` returns newest-first `items`, an exclusive
`next_before` cursor, the oldest retained ID (`oldest_id`), and a `revision`.
The default page size is 50, capped at 100; zero is invalid. Each item includes a
stable history ID, the queue entry and track metadata, library name, nullable
`started_at_ms`, `ended_at_ms` (Unix milliseconds), `played_ms`, `position_ms`,
`outcome` (`completed`, `skipped`, `failed`) and a nullable safe error message.
The history ID orders finished plays independently of reordered queue-entry IDs.
A missing library has `entry.track.library: null`; backend URLs and credentials
are never returned. State/SSE snapshots include only each player's
`history_revision`, so open history views refresh when a record is added without
receiving the entire retained history every playback tick.

`GET rating?library=N&id=ID` reads the configured account's current song rating
and returns `{"rating":N}`. `POST rating` accepts JSON
`{"library":N,"id":"ID","rating":N}` and returns 204 after a successful backend
write. Ratings must be integers from 0 to 5; 0 clears the rating. These endpoints
require a valid viewer link and use the same bounded, authenticated library
requests as browsing. They are independent of queue selection.

State/SSE includes `rating_revision`, advanced only after a successful backend
write. Visible stars use it to reload the authoritative rating. The counter is
local to a bot run; ratings are fetched from the library rather than queue or
history snapshots. After a failed write, **Retry** reloads the current rating.

State/SSE includes `playlists`: `{"revision":N,"can_own":bool}`. `revision`
advances after every playlist change that reached the library and when owner
entries change; `can_own` says whether this viewer's Mumble client has a
certificate. `GET playlists?library=N` returns `{"playlists":[Playlist]}` and
`GET playlist?library=N&id=ID` returns `{"playlist":Playlist,"songs":[Track]}`,
where a Playlist has the library's fields (`library`, `id`, `name`, `song_count`,
`duration`, `owner` (the library account), `cover_art`) and, for this viewer,
`personal`, `owner_name` (null for library playlists), `mine`, `shared`,
`favorites`, `editable` (may add and remove songs) and `manageable` (may rename,
share and delete). The playlist changes are JSON POSTs:

| Route | Body | Answer |
| --- | --- | --- |
| `playlists/create` | `{"library":N,"name":"…","shared":false}` (`shared` optional) | the new Playlist |
| `playlists/rename` | `{"library":N,"id":"ID","name":"…"}` | 204 |
| `playlists/share` | `{"library":N,"id":"ID","shared":bool}` | 204 |
| `playlists/delete` | `{"library":N,"id":"ID"}` | 204 |
| `playlists/add` | `{"library":N,"id":"ID"}` with exactly one of `"songs":["ID",…]`, `"album":"ID"` or `"player":N` (the song that queue plays, from the same library) | `{"added":N}` |
| `playlists/remove` | `{"library":N,"id":"ID","index":N,"song":"ID"}` | 204, or 409 when another song is at `index` |
| `favorites` | `{"library":N,"id":"song ID","favorite":bool}` | `{"favorite":bool}` |

`GET favorites?library=N&id=ID` returns `{"favorite":bool}`: whether the song is
in the viewer's Favorites of that library, without a library call while they have
none. Refusals answer 403 with the reason (no certificate, a library playlist,
private to its owner, Favorites stays private), invalid names, ids or song counts
400, full playlists and reached caps 409, a playlist busy for 30 seconds 503,
and library failures the redacted errors of browsing.

`POST command` accepts JSON tagged by `command`: `pause`, `resume`, `skip`,
`clear`, `seek` (`position_ms`), `volume`, `ducking`, `remove`, `move`, `add`, and
`attach`. Commands include a player index; entries have stable IDs. `move.index`
is the index after removing the moved entry. `add.index` is an insertion position,
or null for the end. Successful commands return 204 and broadcast the new state.

`POST dj` accepts one DJ mode control per request, JSON tagged by `control`, with
`player` (the queue index) and, for deck controls, `deck` (0 for A, 1 for B):

| Control | Fields | Range |
| --- | --- | --- |
| `mode` | `on` | turns DJ mode on or off |
| `load` | `library`, `id` and `kind` (`song`, the default, `album` or `playlist`: the first song onto the deck, the rest to the front of its queue), or `entry` (an entry of the main queue or a deck queue, which leaves its queue) | ids of 1 to 128 bytes |
| `queue_add` | `library`, `id`, `kind`, `index` (null: the end) | adds to the deck's queue |
| `queue_move` | `entry`, `deck` (0, 1, or null for the main queue), `index` (in that queue without the entry) | |
| `queue_remove` | `entry` | |
| `queue_clear` | `deck` | |
| `loop_in`, `loop_out`, `loop_toggle` | — | |
| `beat_loop` | `beats` | 0.25, 0.5, 1, 2, 4, 8, 16 or 32 |
| `loop_resize` | `factor` | 0.5 or 2 |
| `loop_move` | `by` (`loop` or `beat`), `direction` | −1 or 1 |
| `loop_set` | `start_ms`, `end_ms` (song time) | within the first three hours; snapped to beats when quantising |
| `quantize` | `on` | |
| `eject`, `set_cue`, `jump_to_cue`, `sync` | — | |
| `play` | `playing` | |
| `seek` | `position_ms` (song time) | 0 to three hours, clamped to the song |
| `volume` | `volume` | 0 to 100 |
| `eq` | `band` (`low`, `mid`, `high`), `db` | −26 to 6 |
| `filter` | `value` | −1 (low-pass) to 1 (high-pass), 0 neutral |
| `tempo` | `tempo` in percent | −16 to 16, clamped to the deck's range |
| `tempo_range` | `range` | 8 or 16 |
| `crossfader` | `position` | −1 (deck A) to 1 (deck B) |
| `curve` | `curve` | `smooth` or `cut` |
| `transition` | `seconds` | 1 to 60 |
| `auto_load` | `on` | |

Values outside a range and unknown decks answer 400; a control the state refuses
answers 409 with the reason (DJ mode off, an empty deck, a queue entry gone, a
full queue, sync without both BPMs or beyond 16 %, a fade to an empty deck, a
loop without its start, a beat loop without beats, a loop on a song that streams).
Successful controls return 204. State/SSE gives each player `dj`: null, or
`{"decks":[Deck,Deck],"crossfader":N,"curve":"smooth"|"cut","transition":
{"to":N,"duration_ms":N,"elapsed_ms":N}|null,"auto_load":bool,"notice":"…"|null}`,
where a Deck has `entry` (the queue's entry format, or null), `position_ms` (song
time), `playing`, `buffering`, `advancing`, `cue_ms`, `volume`, `eq`
(`{"low":N,"mid":N,"high":N}`), `filter`, `tempo` (percent), `tempo_range`,
`bpm` (the library's BPM or the estimate, one decimal, or null), `bpm_source`
(`library`, `estimate` or null), `analysing`, `loaded_ms` (how much of the song
is decoded in memory, null while the deck streams), `preloaded` (all of it),
`loop` (`{"start_ms":N,"end_ms":N|null,"active":bool}` or null), `quantize`,
`grid` (`{"bpm":N,"first_beat_ms":N}` or null: beats at `first_beat_ms + k ·
60000 / bpm` in song time), `error` and `cue_listeners`. Song time advances
`tempo` percent faster than real time while `advancing`. In DJ mode the player's
`current` is null. Every player also has `deck_queues`: two lists of entries in
the queue's format, kept while DJ mode is off.

`GET spectrum?player=N&deck=D&entry=E&view=coarse|fine&from=C&count=K` returns
the band energies of deck D's song for the deck's waveform: `count` (1 to 2,000,
default 2,000) columns from column `from`, as `application/octet-stream` with one
byte per band and column, column after column. Column n covers song time n/100 s
to (n+1)/100 s. `coarse` has 3 bands (20–250 Hz, 250 Hz–2.5 kHz, 2.5–5.6 kHz),
`fine` 24 log-spaced bands from 40 Hz to 5.5 kHz, lowest first, scaled so white
noise reads flat. A byte b is `b · 72 / 255 − 72` dB (0 at −72 dB or below, 255
at 0 dB, a full-scale sine). Headers: `X-Spectrum-From`, `X-Spectrum-Columns`
(columns computed so far), `X-Spectrum-Total` (the song's columns, 0 when its
length is unknown) and `X-Spectrum-Bands`. `GET spectrum/overview` with the same
`player`, `deck` and `entry` returns the whole song in 1,000 coarse columns (the
loudest of each group; columns not yet computed read 0). Both answer 409 when
deck D plays another song than `entry`, 404 while the song streams instead of
being in memory, and 400 for other values. The page draws the numbers itself.

Browsing uses `albums`, `album`, `artists`, `artist`, `genres`, `genre/albums`,
`genre/songs`, `playlists`, `playlist`, `search` and `suggest` JSON endpoints.
Library identity in requests is its current config index.
Album lists accept `library`, `list`, and `page`; detail endpoints accept `library`
and `id`. The authenticated `art/<library>/<id>?size=…&v=…` route returns
`image/png`: a PNG the content worker made from the decoded art (8-bit RGBA,
only `IHDR`, `IDAT` and `IEND`). Covers are made in
edge lengths of 32, 64, 96, 128, 200, 300 and 400 pixels; a request gets the
largest of these that is no larger than its `size`, so the page's few sizes share
cache entries and worker jobs. Invalid images return 404. An uncached cover
waits up to 10 seconds for one of 4 cover fetches, which have their own
admission beside the 8 library calls for browsing; a cover another request
finished meanwhile comes from the cache, otherwise the bot fetches the encoded
image and takes its SHA-256. A PNG the worker already made from the same bytes
at the same size comes from memory and is cached under the requested ID too.
Navidrome gives every song its own cover ID and answers songs without art of
their own with the album's image, so an album's songs cost one fetch each and
one worker job together. Memory holds the last 512 such PNGs, at most 32 MiB,
and the least recently used leave first. Requests with an image that is being
made wait for that job; others wait up to 10 seconds for one of two jobs. Up to
64 requests wait for a fetch or a job at once. A request beyond those 64, one
still waiting after 10 seconds, or a missing worker gets 503, and a job that
runs longer than 10 seconds gets 504. The page loads a failed cover again after
0.25, 0.5, 1, 2, 4 and 5 seconds, then shows the placeholder. Library failures
preserve the existing redacted errors.

Successful answers are kept on disk in `covers/` below the state directory, up to
`web.cover_cache_mib` MiB (64 by default, at most 4096; 0 turns the cache off and
deletes the files) and at most 4,096 covers. Entries are keyed by library, cover
ID and size and are kept for at most 24 hours; the least recently used leave
first, and no single cover may take more than an eighth of the space. Every file
holds its key and is checked again when read; nothing past its indexed size is
read. A damaged file is deleted and fetched anew, and only well-formed PNGs are
served. A restart keeps the files and drops expired, unfinished or excess ones. If
the folder cannot be used the bot logs a warning and runs without the cache. A
cached cover needs no library call and no worker job. State/SSE includes
`library_keys`, the random key of each
library (see Links). With `v` set to that key the answer is
`private, max-age=86400`, so the browser keeps it for a day. Without `v` it stays
`private, no-store`. A `v` for another library (the configuration changed) gets
404, so a browser never keeps one server's cover under another's URL.
API errors carry an HTTP error status and `{"error":"message"}`.

`GET song?library=N&id=ID` returns `{"song":Track,"details":{…},"appearances":[Track],
"appearances_error":null}` for a song's page. `details` holds `year`, `genre`,
`bit_rate` (kbit/s), `disc_number` and `play_count`, each null when the library
does not say. `appearances` has one track per album with a song of the same title
and artist (compared without case or surrounding spaces), the song's own album
first. The bot finds them with `search3` on the title, which every server
supports, and looks at its first 40 songs. When that search fails the song still
shows, `appearances` is empty and `appearances_error` gives the redacted reason.

`GET similar?library=N&id=ID` asks the library's `getSonicSimilarTracks` for up to
25 songs and returns `{"song":Track,"matches":[{"track":Track,"similarity":N}]}`.
`GET path?library=N&from=A&to=B` asks `findSonicPath` for up to 25 songs and
returns `{"matches":[…]}` from the start to the target. `similarity` is null when
the library reports no usable score (OpenSubsonic uses -1 for that). State/SSE
includes `sonic`, one boolean per library, from the extensions learned at startup;
the web page offers these actions only where it is true. Libraries without the
extension answer these endpoints with an error instead of being queried.

`GET lyrics?library=N&id=ID` returns `{"lyrics":{"synced":bool,"lang":"eng"|null,
"lines":[{"start_ms":N|null,"text":"…"}]}}`, or `{"lyrics":null}` when the
library has none. `start_ms` is set on every line of synced lyrics and null
otherwise. Library failures return the redacted errors of browsing.

`GET genres?library=N` returns `{"genres":[{"name":"…","albums":N|null,
"songs":N|null}],"truncated":bool}` from the library's `getGenres`: sorted by
name ignoring case, each name once, at most 2,000, with `truncated` true when the
library has more. The list holds the genres whose names have 1 to 200
characters. Counts come from `albumCount` and `songCount`; whole numbers given
as text count as numbers, and every other value gives null.
`GET genre/albums?library=N&name=…&page=P` returns `{"albums":[Album],"more":bool}`,
48 albums per page from `getAlbumList2` with `type=byGenre`, and
`GET genre/songs?library=N&name=…&page=P` returns `{"songs":[Track],"more":bool}`,
50 songs per page from `getSongsByGenre`. `name` is the genre's name as a
percent-encoded query value, and the bot passes it to the library as one query
value too. An empty name or one over 200 characters answers 400 before any
library call. Pages run from 0 to 10,000; the bot keeps at most one page of
what the library sends. Library failures answer with the redacted errors of
browsing, and with all eight library calls in use the answer is 503 `the music
library is busy`. The web page's routes are `#/genres?library=N` and
`#/genre?library=N&name=…`.

`GET suggest?q=…` returns `{"items":[{"library":N,"kind":"artist"|"album"|"song",
"id":"ID","title":"…","subtitle":"…"|null,"cover_art":"ID"|null}]}` for the
suggestions under the search field. `q` is trimmed and cut to 200 characters;
fewer than 2 characters give no items without asking a library. Every library
gets one `search3` with `artistCount=3`, `albumCount=3` and `songCount=5`, and
the bot keeps at most that many of each kind per library. Items list the
artists of every library in library order, then the albums, then the songs, and
stop at 10. An item with the kind, title and artist of an earlier one (compared
without case or extra spaces) is left out, so the same artist in two libraries
or the same song on two albums shows once, from the first place it was found.
`subtitle` is the artist of an album or song. Libraries that fail
add no items. The calls share the admission of the other library calls: with
all eight in use the answer is 503 `the music library is busy`, and the page
then shows no suggestions. Subsonic and OpenSubsonic define no suggestion call;
Navidrome's `search3` matches the start of each word, which suits typing.

`add` also accepts `kind: "similar"` with the song in `id`, `kind: "path"` with
the start in `id` and the target in `to`, and `kind: "genre"` with the genre's
name in `id` and an optional `page` (from 0), which queues that page of the
genre's songs, at most 50. The bot asks the library again when
adding. A path skips its first song when that song is already playing in the queue.
Similar songs, listed and added, hold each recording once: the song itself, every
match with a similarity of 100% and every match with its title and artist are left
out, and of several copies of one recording (the same title and artist, compared
without case or extra spaces, at most 3 seconds apart in length) the closest match
stays.

With an import service configured, State/SSE includes `import`:
`{"library":N,"revision":N}`, the library added albums arrive in and a revision
that advances when the import list changes; it is `null` otherwise, and the
endpoints below answer 404. `GET api/import/search?q=…` returns `{"albums":[Album]}`
(`q` trimmed and cut to 200 characters; empty gives no albums).
`GET api/import/album?id=ID` returns `{"album":Album+{"tracks":[…]},
"in_library":{"library":N,"id":"ID"}|null}`; `in_library` comes from one search
for "artist title" in that library, comparing titles and artists without case or
extra spaces, and is null when that search fails. `GET api/import/requests`
returns `{"requests":[Import],"budget":{"mine":Budget,"total":Budget}}` and
`POST api/import/requests` with `{"album_id":"ID"}` returns
`{"request":Import,"existing":bool}`. Album, Import and Budget have the fields of
the import service's protocol. The authenticated `import-art/<id>?size=…` route
answers like `art/`, with `private, max-age=86400`. Errors use the texts of
"Adding music".
