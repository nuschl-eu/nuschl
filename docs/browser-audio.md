# Browser voice

Browser voice uses the existing Mumble connection, with Opus packets carried in
TLS `UDPTunnel` messages through the configured WebSocket relay. It does not
establish a peer-to-peer connection or expose the browser's address to other
participants. The configured relay and Mumble server remain trusted network
endpoints. This transport is separate from the newer RTC transport work.

## Consent and routing

Joining voice enables playback following an explicit user gesture and starts
muted. Unmute requests microphone permission explicitly. The browser requests
its native echo cancellation, noise suppression and automatic gain control;
the status reports the settings the browser actually exposes, with missing
settings recorded as unknown. These settings do not prove measured echo
cancellation quality or detect processing elsewhere in an operating system's
audio graph.

Only one explicitly selected server/channel receives microphone packets. Text
navigation does not move that route. Microphone consent is bound to the server,
connection and channel. Disconnecting, changing that destination, or stopping
audio releases capture and invalidates old results. A late permission result
is stopped after a newer mute. Unmute also waits for the server's matching
self-state acknowledgment; local intent alone does not permit transmission.

When the browser ends or mutes the capture track or suspends the AudioContext
while capture is open (a hidden page on a phone, another app taking the
microphone, an unplugged device), Nuschl mutes and shows "Microphone stopped".
Capture stays closed until the user's next Unmute, also when the page returns.

Mute closes capture immediately. Deafen closes capture and flushes playback.
An explicit playback gesture can reopen the local playback gate, while the
SDK's effective deafen state continues to gate output pending acknowledgment.
Undeafening does not restore microphone consent. Suspended audio contexts need
another user gesture. UI controls distinguish the SDK's local mute/deafen
intent from effective restrictions while a server acknowledgment is pending.

## Media boundary and bounds

`BrowserAudio` in `web/app/src/audio.js` handles permission, Web Audio nodes and
routing gates. It exchanges bounded Opus jobs and control messages with the
codec Worker. The Worker shares fixed PCM rings with the capture and playback
AudioWorklets. TLS, certificate identity and session state stay in the SDK
Worker. A browser Worker uses a separate JavaScript/WASM realm.

`nuschl-audio-wasm` uses the memory-safe `opus-rs` implementation and forbids
first-party unsafe Rust. Its WASM imports contain no network, storage or clock
capability. The fixed same-origin JavaScript loader fetches the codec module;
the media protocol provides no caller-selected URL. AudioWorklet modules are
also fixed same-origin resources. The browser's audio and permission services
are system-service boundaries.

Capture is mono 48 kHz in 20 ms frames. The capture Worklet writes up to ten
frames (200 ms); the codec Worker encodes frames from the newest 120 ms when
they match the current TX epoch. Encoded replies expire at capture-clock
granularity (20 ms). Its six outstanding encoded replies and
BrowserAudio's six acknowledged SDK sends each have an independent limit.
Frames beyond the six SDK sends are dropped; a send keeps its slot until a
genuine reply or lane retirement. See [the SDK protocol](sdk-rpc.md).

Receive validates full Opus framing before admitting speaker, timing and gain
state. It accepts 10–120 ms packets in 10 ms units. Up to sixteen stereo
decoders preserve reordered packet gains, conceal lost spans with FEC or PLC,
and retain cached samples at talkspurt end. Each speaker has up to 64 encoded
packets of 1024 bytes. Admission allows 2000 packets/s globally and 200 per
speaker, with up to 16 new decoders per second.

The playback Worklet consumes fixed 10 ms stereo blocks from a twelve-slot
ring (120 ms). Every rendered 480 frames, the Worklet makes one coalesced wake
available to the codec Worker over a direct MessageChannel. The Worker fills
the ring ahead by the 95th percentile of recent one-second peak wake gaps,
with a 30–120 ms bound and exponential forgetting. The lead grows at most
10 ms per 400 ms, giving the network buffer time to adapt. The receiver adapts
its network jitter target using packet arrivals recorded before the codec
queue. Time stretching moves the receiver towards its target while preserving
pitch. Wide f64 sums stay in the ring; playback clips after mixing and folds
stereo before clipping for mono output. Each callback reads a bounded number
of slots using preallocated PCM storage.

Main owns independent, exact capture and playback epochs and grants each
Worklet its own shared authorization word. The codec Worker sees the ring
buffers and their tagged blocks. Mute, bitrate changes and capture failure
invalidate TX while preserving authorized RX. Deafen, destination or
connection changes, and suspension revoke RX. Worklets discard stale blocks
and play silence when the ring is empty or playback is disabled. The sixteen
global packet inbox slots stay occupied until matching acknowledgments or
Worker termination, including across route transitions. The Worker heartbeat
bounds failure detection.

These application bounds complement the browser's own Worker and WASM
isolation.

## Browser support and deployment

The baseline uses Rust WASM Opus rather than requiring WebCodecs AudioEncoder.
WebCodecs codec support is optional and older Safari/iOS releases cannot be
assumed to provide Opus encoding. Voice requires a secure, cross-origin isolated context,
SharedArrayBuffer, AudioWorklet, a 48 kHz AudioContext and user permission. AudioContext resume
and permission acquisition begin synchronously with the activating gesture to
support autoplay policies; there is no physical Safari/iOS or Android audio
hardware proof yet.

The modules operate with same-origin scripts/workers/connections and
`script-src 'self' 'wasm-unsafe-eval'`, without inline scripts, blob workers or
JavaScript `unsafe-eval`. Deployment must permit microphone access for the
application origin and serve the page with COOP `same-origin` and COEP
`require-corp`. Voice uses microphone permission.

## Screen off

`CallSession` (`web/app/src/call-session.js`) holds a screen wake lock while the
page is joining or in voice, so the screen stays on. The browser releases the
lock when the page is hidden; the page requests it again each time it becomes
visible and releases it on leave. When the browser refuses the lock (hidden page,
low battery, policy), voice runs on without it. Chrome on Android has the Screen
Wake Lock API since 84 and Safari on iOS since 16.4, in Home Screen web apps since
18.4 ([MDN compatibility data](https://github.com/mdn/browser-compat-data/blob/main/api/WakeLock.json)).

In voice the page is also a media session: the channel and server as metadata,
`playing` as its state, `setMicrophoneActive` with the microphone's state, and
action handlers where `togglemicrophone` and `pause` mute and `hangup` and `stop`
stop audio. The handlers only mute; only the user's Unmute button opens the
microphone. A silent ten-second WAV from a `blob:` URL loops in an `<audio>`
element while in voice (CSP `media-src blob:`); the Join click starts it once so
Safari lets it play later. Chrome shows a media notification only for a playing
media element of more than five seconds ([web.dev](https://web.dev/articles/media-session)).
Chromium's Android notification (`MediaSessionHelper`) needs a controllable
session, and `MediaSessionImpl` on Android counts one as controllable with full
audio focus, which such an element requests; an `AudioContext` joins the session
as ambient sound and a `MediaStream` element as one-shot. The notification shows
the channel as title and the server below it. Its buttons
(`MediaNotificationController`) cover play or pause, seek, previous, next and
stop, so on Android, pause mutes and stop stops audio.

| | Chrome on Android | Safari on iOS |
| --- | --- | --- |
| Screen Wake Lock | 84 | 16.4; Home Screen web apps 18.4 |
| Media Session metadata, `play`, `pause` | 57 | 15 |
| `togglemicrophone`, `setMicrophoneActive` | 93 per MDN; buttons only in desktop Picture-in-Picture | 18.4 |
| `hangup` | 93 per MDN; as above | absent (WebKit's `MediaSessionExtendedActionsEnabled` is off) |
| Microphone with the page hidden | unverified | WebKit interrupts capture (`InterruptAudioOnPageVisibilityChangeEnabled` is on for iOS) |

Versions come from MDN's compatibility data. Whether a phone keeps voice
running with the screen off is the manual check in #447.

When the browser freezes the page (`freeze`, Chrome 68) or hides it for good
(`pagehide`), the call ends at once: the client plays the native client's
Disconnected sound unless deafened and closes the SDK worker. Closing the worker
sends the WebSocket close frame within milliseconds, and the relay then ends the
Mumble connection, so the server sees the user leave. The page then shows "Voice
ended in the background. Reload to reconnect." Chrome pauses a frozen page's Web
Audio output (Blink `BaseAudioContext`), so the sound plays when the page
resumes. Safari has no `freeze` event; there `pagehide` and the server's timeout
end the call.

`web/app/src/call-session.test.js` covers the lock and media session state;
`tests/web_call_session.browser.mjs <repository> <chromium> <playwright-core/index.mjs>`
runs `CallSession` in Chromium with the production CSP and the browser's Wake
Lock, Media Session and media element, then a freeze: the call ends, a terminated
worker's WebSocket sends its close frame, and an offline render of the
disconnect sound is audible (silent while deafened).
Headless Chromium keeps every page visible, so that test sets the page's
visibility itself.

## Verification scope

`node --test tests/web_audio.unit.mjs tests/web_playout_clock.mjs` exercises
the production worklet rings, mixing, policy epochs, capture after a 90 ms
Worker stall, the adaptive wake lead and acknowledged-send credit behavior.
`vm-web` connects two packaged browser profiles through stock Murmur and
measures playback from a synthetic microphone. It also verifies each
capture-stop path and cross-origin isolation.
The native codec tests check actual tone encode/decode with spectral validation
and reject malformed/non-finite/unbounded inputs without poisoning subsequent
codec use.

`tests/web_audio.py` runs the actual production audio modules, generated Rust
WASM codec and SDK Worker against an owned loopback Murmur and relay under
Chromium. Its wrapper exposes only the Nix store and owned scratch, constructs
a fresh device tree without sound/camera devices, and hides host session
sockets. Input is an owned 997 Hz WAV supplied through Chromium's fake capture
device. Output uses a null AudioContext sink, with an ALSA null fallback and
unavailable private Pulse/PipeWire sockets. Worklet PCM is measured in the
browser; an independent libopus decoder verifies the wire data's 997 Hz
carrier. No physical microphone or speaker is part of this fixture. For browser
text/layout tests, pass `--font-dir` with an explicit Nix store font directory.
The wrapper builds an owned fontconfig and font cache; it never mounts host
font configuration. This is necessary for meaningful textarea/layout behavior
in the otherwise minimal filesystem.

The controller fixture and the separate production App fixture prove different
layers. Controller success alone does not establish UI control correctness,
mobile audio compatibility, real-device echo cancellation, packet-loss quality,
or long-running audio performance. Exact run evidence and any incomplete
assertions are recorded in the integration handoff rather than inferred from a
green unit suite.

The extended Chromium 156 controller run passed against stock Murmur 1.5.939
with its default control rate limits: actual server Opus interchange and worklet
PCM, startup mute, explicit synthetic capture, deafen silence, playback-only
undeafen, transmission stopping on mute, disposal of a late permission result,
four rapid deafen toggles and muted reconnect with a new connection generation.
An independent libopus 1.6.1 decoder verified 27 wire packets (25,920 finite
samples) against the owned 997 Hz input. No CSP violations, foreign-origin
requests or browser errors were observed. This is a controller integration
result; production App control tests are recorded separately.

The separate packaged App run also passed real Join/Unmute/Mute/Deafen/Enable
sound/Stop audio controls with the same confined synthetic input and null
output. It measured nonzero playback and exact silence after deafen, preserved
chat after stopping audio, verified multiline messages in browser storage, and
cleared the composer after the SDK send acknowledgment. A 400 ms audio-only
window delivered 20 packets without replacing the full Svelte state snapshot.
These Chromium results do not establish mobile Safari audio or background-tab
behavior.


The requested default is 128,000 bit/s, mono VoIP with 20 ms packets. The
session selects the lower of that target and its authenticated server bandwidth
minus a conservative 24,000 bit/s Mumble framing/transport reserve; an absent
limit initially uses the conservative 72,000 bit/s server budget. This reserve
covers Mumble server bandwidth accounting, not the extra WebSocket relay hop's
entire network cost. ServerSync and later ServerConfig limits update the active
route, revoke queued frames/readiness, and pass only a bounded effective bitrate
to the independent codec worker. Encoded packet buffers are also limited to that
20 ms payload allowance, so VBR cannot emit a packet above it. A budget below the
6,000 bit/s codec minimum plus the reserve disables capture while preserving
playback. No unmute or microphone consent is created by a budget change.

This changes the default; persistent bitrate controls and a separate stereo music
preset remain future work. Existing echo/noise processing and mono VoIP behavior
are unchanged. Mumble's upstream accounting distinguishes audio bitrate from
framing/transport overhead; see its [AudioInput implementation](https://github.com/mumble-voip/mumble/blob/v1.5.939/src/mumble/AudioInput.cpp).


## Clocked playout evidence and limits

`tests/opus_playout.py` compares explicit native and actual-WASM actions with an
independent libopus decoder at fixed gain and unfitted delay. All six scenarios
pass the unchanged 35 dB gate, including reorder/FEC/PLC/restart, every 10–120 ms
duration, a 120 ms packet followed by 10/20 ms gaps, and malformed packing before
a queued valid packet. The latest malformed case measures 71.26 dB native and
70.18 dB WASM; the 20 ms loss measures 81.59/83.68 dB. Earlier failing fixtures
remain retained rather than overwritten.

`tests/web_playout_worker.mjs CODEC_DIRECTORY` executes production worker source,
actual Rust WASM and actual PlaybackProcessor with a deterministic sample clock.
Twelve checks cover configure-order permutations with interleaved fresh packets,
TX-only continuity, RX flush, delayed old disabled barriers after re-enable,
packet gain, bounded admission, exact stereo PCM, stalls and measured buffering. The separate main/Worklet suite has 30 passing
checks, including 20 rapid TX/RX changes under a stopped worker and rejection of
unsolicited capture acknowledgments.

Realized startup is measured separately from target buffer size and transport
reserve. On an explicit 480-frame clock, regular 10/20/120 ms packets first yield
scheduled PCM at 40/30/130 ms after the first arrival; a single ended 120 ms
packet begins at 10 ms, retains all twelve blocks, then restarts correctly.
These numbers exclude sender packetization, decoder lookahead, browser scheduling
and device latency. The inherited policy initially estimates unknown packet
duration as 20 ms; a non-ended single 120 ms packet therefore waits for another
packet. Improving that first-packet estimate is a separate policy review, not a
hidden retuning in this refactor.

On this host, actual-WASM memory stayed at 5,832,704 bytes in the 16-speaker
stress runs. The non-silent independent libopus 10 ms packet and its 20/120 ms
aggregates were decoded in aligned bursts by all sixteen speakers. Test-only
wrappers count actual packet/FEC/PLC calls (cached blocks are excluded below).
Across 240 renders per duration, costs were:

| Packet duration | Aligned decode bursts | Decode p95 | Decode max | Renders over 10/20 ms |
| --- | ---: | ---: | ---: | ---: |
| 10 ms | 240 | 0.69 ms | 0.96 ms | 0 / 0 |
| 20 ms | 120 | 1.05 ms | 1.66 ms | 0 / 0 |
| 120 ms | 25 | 4.55 ms | 4.57 ms | 0 / 0 |

A separate cold simultaneous 16 × 120 ms tone decode took 5.44 ms. This compares
bounded work with the 10 ms cadence/30 ms initial lead; it is not a physical-device,
adversarial-entropy, or browser scheduling guarantee. Independent clock tests
also deliberately expire deadlines: they remain silent with their requests pending,
trim partial lateness, and require the ready barrier after fully late replies.

The updated confined Chromium 156 controller proof uses the actual packaged
codec, current SDK, private Murmur1.5.939 and same-origin CSP. It verifies
continuous measured output through a microphone-only mute with an unchanged
RX epoch, exact deafen silence, late permission disposal, rapid mute/deaf privacy
and muted reconnect. Evidence is under
`~/.cache/nuschl-output-quality/jitter-browser-exact/`. Its final source is
`9827d211` (documentation-only descendants do not change the runtime). The exact
packaged App at `/nix/store/sgh58hpc4x2r78sdqnb53y3sj9qrnkgj-nuschl-web-app-with-notices`
also passes the production-CSP UI/private-server test: RMS 0.06762, deafen zero,
20 audio-only packets with zero timeline replacements, live-call reorder retaining
microphone/generation/draft, text and mute/stop controls. Its proof is
`~/.cache/nuschl-output-quality/jitter-packaged-app-final/proof.json`.
This does not establish
background-tab/Safari/Firefox scheduling, physical device AEC, every malformed
entropy stream, or full reference-Mumble jitter/PLC perceptual equivalence.
See `browser-playout-port.md` for the internal clock/epoch/budget contract.
