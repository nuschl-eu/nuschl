# Developing Nuschl

How to build and check Nuschl, and how its build is put together. What the tests
cover is in [tests/README.md](../tests/README.md); the rules for code and dependencies
are in [SECURITY.md](../SECURITY.md) and [architecture.md](architecture.md).

## The environment

Everything comes from the Nix flake's development shell:

```sh
direnv allow
# Or enter the same pinned environment explicitly:
nix develop
```

`cargo add` is built into the pinned Cargo. `Cargo.lock` and `flake.lock` are tracked.
The Nix environment uses Rust's latest stable release from the locked rust-overlay
revision; `nix flake update rust-overlay` advances it. Outside Nix, rustup follows
the `stable` channel independently of `flake.lock`. Commits are made with `jj`.

## Recipes

| Command | What it does |
| --- | --- |
| `just build` | `cargo build --workspace` |
| `just desktop` | build and start the desktop app |
| `just check` | formatting, Clippy, the SDK for wasm32, tests, `cargo deny check` |
| `just workers` | the worker executables against their protocol (`tests/workers.py`) |
| `just vm` | the Linux integration tests in NixOS VMs: packaging, confinement, SQLx/PostgreSQL, service preflight, the desktop against stock Mumble servers |
| `just vm-driver --root ROOT DRIVER` | a built NixOS test driver run by hand, with all VM state on disk (below) |
| `just vm-driver-check` | the hand runner's and its pre-boot guard's tests, without a VM |
| `just test-postgres` | the storage worker on a throwaway PostgreSQL |
| `just fuzz <target> [seconds]` | a libFuzzer target from `fuzz/` ([below](#fuzzing)) |
| `just miri` | the Windows sandbox adapter's unit tests under Miri |
| `just rtc` | the WebRTC trial with the worker's Opus (below) |
| `just readme-screenshot` | the picture at the top of the README (`tests/readme_screenshot.py`) |
| `just crate2nix` | regenerate the per-crate Nix build after dependency changes |
| `just emoji-assets` | link the emoji images and picker data for builds outside Nix ([emoji.md](emoji.md)) |
| `just fmt` | format Rust, Nix and TOML |

Other VM checks run with `nix build .#checks.x86_64-linux.<name>`:
`vm-server` (the server module), `vm-home-manager`, `home-manager`. UI scenes for
looking at changes run in a private headless GNOME Shell (`tests/screenshots.py`,
tests/README.md).

A test's built `.driver` can also run outside the Nix daemon. The pinned driver
keeps each machine's disk image, sockets and shared directory below an inherited
`XDG_RUNTIME_DIR` (normally the tmpfs `/run/user/<uid>`) and uses `TMPDIR` only
when that is unset, so setting `TMPDIR` alone leaves VM state in RAM. Run drivers
through `just vm-driver --root ~/.cache/nuschl-vm-<run> DRIVER`
(`tests/vm_driver.py`). It points both variables into a fresh mode-0700 root inside
the cache directory, below an existing parent only you can write, with a clean
environment; `--env` adds only `NUSCHL_*` fixture inputs. Node names such as
`machine` then name state private to the run. Before the driver's own script,
`tests/vm_driver_scratch.py` runs inside the actual driver. It refuses to boot if
any machine's real state, shared or VLAN directory, or a socket its start script
creates, is RAM-backed, outside `ROOT/run` or too long for a Unix socket. Keep the
root short (about 50 bytes). `--probe` runs only that check and boots no guest.
The runner stops its own process group below 8 GiB free `/tmp`, scratch space or
available memory, or after `--timeout`. It leaves `driver.log`,
`scratch-proof.json` and `status.json` in the root and deletes nothing. Flake
checks such as `just vm` still run inside the Nix daemon; client variables do not
move its build scratch.

The worker exchange diagnostic has its own `main` (`harness = false`). Run:

```sh
nix build \
  .#checks.x86_64-linux.ipc-exchange \
  .#checks.x86_64-linux.ipc-exchange-negative-controls
```

This exercises crash, malformed reply, timeout and lingering-worker recovery
with real bubblewrap/prlimit. The
ordinary IPC unit-test leaf compiles this fixture under libtest and reports zero
tests for it. This check requires a working sandbox and rejects a substituted
successful executable; it checks eventual cleanup, not atomic worker-tree exit
or platform-wide confinement.

The Linux video encoder diagnostic uses the fixed `--encode` sandbox role
with generated I420 frames. Run `nix run .#video-probe` for a summary or
`nix run .#video-probe -- --obu` for elementary AV1 on standard output.
Use `--screen` or `--camera` to request one live frame through the portal.
The probe prints the frame dimensions and encoded byte count. Nix builds try
the isolated VA-API pipeline first and fall back to the software worker if
hardware setup fails before opening the granted connection. Starting again
after a running PipeWire screen stream fails opens a new PipeWire connection
for the existing screen-cast portal session, while screen sharing stays enabled.
The native capture adapter applies live bitrate targets through its bounded
socket protocol and coalesces pending targets to the latest value.
The same capture supplies the local preview on a separate socket with bounded
frames. The preview scales to fit 1280 × 720 and pauses when hidden.
The VA-API path emits keyframes and interframes. The software path encodes
independent frames at up to 15 fps. The capture integration test opens one
connection to a private PipeWire daemon with a synthetic video node. The local
GPU variant checks interframes, 90 interframes in a row on a screen, and
recovery after a requested keyframe.
The native SFU test encodes, sends and decodes AV1 in confined workers and
checks the resulting pixels. Compositor DMA-BUF verification is tracked in #418.

The VA-API decoder writes mapped BGRA directly into the bounded worker pipe.
The client reads those bytes into the frame allocation, and compatible BGRA
atlases stage them directly in wgpu. The vendored atlas GPU readback tests
run locally because they require a Vulkan device. The NixOS desktop tests
exercise image rendering in the packaged app.

## Continuous integration

Pull requests to `main` and pushes to `main` run one command on the self-hosted
runner, the `ci` check: `nix build -f ci.nix`, every flake check including the
NixOS VM tests. Run the same command on the
working copy before pushing with `scripts/pre-pr`; Nix
reads only tracked files, so commit new files first. jj runs no git hooks: push
bookmarks with `scripts/pr-push --bookmark NAME`. With plain git,
`git config core.hooksPath .githooks` runs it before each push.

Unit tests run as one `<crate>-unit` check per workspace member, generated
from the member list in flake.nix. `unitExtras` there gives a leaf the inputs
its tests need (bubblewrap, PostgreSQL, features); a member whose tests run
elsewhere appears in `unitOptOut` with its reason.

Changes reach `main` only through pull requests from this repository's branches
whose required checks pass. The `ci` check fails pull requests from forks
before checking them out; a maintainer pushes a reviewed fork branch here instead.
Every workflow has a read-only token; CodeQL's `security-events: write`, to
upload results, is the only exception. Unit tests with test keys or passwords
live in a `tests.rs` file, which CodeQL's SARIF filter treats as test code.

Reviews check that a change is:

- minimal: no code, option or abstraction that the change does not need;
- readable and correct, including its failure paths;
- built on standard, widely used crates only;
- tested through the real code paths, not only mocks;
- checked by CI with a single command that also runs locally;
- described plainly, without flattery or overstated results.

## Benchmarks

`nix build -f benchmarks.nix` measures performance locally;
`ci` does not run it. Its parts are attributes:

- `instructions`: `nuschl-media-worker --bench STAGE` runs a fixed synthetic minute
  (far end, double talk, near end, through a simulated room) through the capture
  chain (echo cancellation, noise suppression, speech detection, gain) and the Opus
  encoder. `perf stat -e instructions:u` counts user-space instructions per second
  of audio for each stage; the result also has the offline echo reduction. Repeated
  runs of one build differ by less than 0.0001%, so the build fails when a stage
  costs more than 5% over `bench/baseline.json`. Counts depend on the CPU, so the
  build runs locally and refuses a baseline from another CPU model. To refresh the
  baseline, copy `cpu` and `per_audio_second` from the printed result.
- `one-core`, `one-core-musicbot`: existing VM tests (media_live's jitter, echo,
  double-talk and sounds checks, rtc_probe, desktop_recovery, the music bot's
  playback) on a guest with one core, each `--arg runs N` times (default 5). A busy
  loop runs in the tested processes' slice. For each test: the pass rate, the
  failed quarantined steps (`tests/quarantine.txt`) and the workers' CPU seconds
  per second alive. Where the test starts the media worker
  itself (media_live), `NUSCHL_AUDIO_TIMING=1` makes it print `audio-stats` lines:
  capture gaps (late or missing capture callback intervals) and playback underruns
  per minute, packets sent a period or more after their deadline, and
  capture-to-send latency percentiles (the sounds check transmits continuously).
  media_live's jitter check adds silent 20 ms playback windows per minute.
  Sandboxed workers do not receive that variable. The suite prints its results
  and writes them to `one-core.json` beside each run's log. It reports them
  without a recorded baseline: they depend on host contention. desktop_recovery's
  reconnect after the server restart fails often on one core (#263).

## Packages and cross builds

The Linux package (`nix build .#default`, also used by the VM tests) is built per
crate with crate2nix, so a change rebuilds only the crates that depend on it; after
changing Cargo.lock or a manifest run `just crate2nix`. Builds import the committed
`nix/crate2nix/Cargo.nix`. The `crate2nix-current` check runs the same crate2nix
command offline on crane's vendored crates and fails with a diff when its output
differs from `nix/crate2nix`. Clippy, tests and the Windows/macOS cross builds use
crane (`.#default-crane` builds the package the same way).

Windows and macOS packages are cross-built from Linux: `.#windows`, `.#macos-app`,
`.#macos-universal` and `.#macos-aarch64` ([install.md](install.md#how-releases-are-made)).

`nix flake check` runs the native build, tests, Clippy, formatting,
dependency policy, and a vulnerability check using a pinned advisory database.
`cargo deny check` also checks the current advisory database and registry yanks
outside the Nix sandbox.

### Reusing Cargo build artifacts

Prefer Crane's dependency-only artifacts when reusing a build. A full workspace
check also contains first-party metadata. With Nix's fixed source timestamps,
Cargo can incorrectly reuse that metadata after a source change, producing stale
API errors or skipping the code that a check should exercise.

If a coordinated check reuses full workspace artifacts, run
`cargo clean --release --workspace --offline` inside that check's owned build
directory before the release-profile check. Match the profile if checking another
one. This removes workspace outputs while retaining third-party dependencies;
do not clean another agent's build or the shared artifact source. Confirm from the
build log that the intended workspace packages were checked or compiled. Record
the source revision, seed artifact, command and result with the evidence.

## Fuzzing

`fuzz/` is a cargo-fuzz workspace of its own; its committed lock is the main lock
cut down. `just fuzz <target> [seconds] [libFuzzer flags]` runs one target in the
flake's fuzz shell (`nix develop .#fuzz`: the newest nightly in the pinned
rust-overlay, and cargo-fuzz) from `fuzz/seeds/<target>` into `fuzz/corpus/<target>`,
with debug assertions, 10 seconds per input and 2 GiB of memory. Findings land in
`fuzz/artifacts/<target>`; corpus and artifacts stay uncommitted. Targets build
without a sanitizer, which safe Rust does not need and which costs three quarters
of the speed; `FUZZ_SANITIZER=address` checks the unsafe code in dependencies
(html5ever, the image decoders), for example over a corpus with
`just fuzz <target> 0 -runs=0`. Rerun a finding with `nix develop .#fuzz -c cargo
fuzz run --fuzz-dir fuzz -s none -a <target> fuzz/artifacts/<target>/<file>`, and
minimise it with `cargo fuzz tmin`. [SECURITY.md](../SECURITY.md#fuzzing) has the
rules for findings.

| Targets | Input |
| --- | --- |
| `html`, `legacy_html`, `markdown` | legacy HTML, Markdown and plain messages into documents; text into the copy for stock clients and back |
| `images`, `favicon` | avatars, website icons, `data:` and pasted images through the content worker's decoders; a website's page through the icon lookup |
| `control_frames`, `server_messages`, `extension_messages`, `kt_messages` | frames and Mumble, extension and `kt/1` messages from several connections into nuschld's state; the crypto worker's IPC |
| `client_messages` | a hostile server's frames into the client session's state |
| `relay`, `crypt`, `voice`, `voice_playback` | voice: the server's relay, OCB2 decryption, the client's decoding, the media worker's jitter buffer and Opus decoder |
| `identity`, `kt` | identity and key-transparency encodings and verifiers |
| `mls` | MLS input from hostile peers: structural checks before OpenMLS, key packages, Welcome routing, `mls/1` messages |
| `tunnel` | the web relay's tunnel frames and TLS shape checks |
| `kt_witness` | witnessing's formats: C2SP signed notes, checkpoints and verifier keys, `WitnessedHead` through the client's check, the auditor's `AddEpochs` through its check and its state files |
| `transfer_offer`, `mumble_url`, `policy_file` | file offers in messages, `mumble://` links and invites, `nuschl-policy.json` |
| `math`, `math_tokens` | LaTeX through the converter, its check, Typst and the rasteriser; message text through the formula grammar |

## Emoji assets

The desktop compiles in, and the web client serves, Twemoji images and the picker's
data built from sources pinned together in `nix/emoji.nix` (`nix build .#emojiAssets`,
[emoji.md](emoji.md)). Nix builds and the development shell set
`NUSCHL_EMOJI_ASSETS`; without it, the desktop's build script reads `emoji-assets/`,
which `just emoji-assets` links (with `web/app/public/emoji` for `npm run dev` and
`npm test`). Both are ignored by version control.

## The icon

The icon's source is `apps/desktop/assets/nuschl.svg`. The Linux package installs
the desktop entry and icons and registers `mumble://` links; after changing the icon,
regenerate the committed Windows `.ico` with the command at the top of
`nix/desktop-files.nix`.

## The WebRTC trial

The modern media transport is a str0m trial in `nuschl-rtc`. Run
`nix run .#rtc-probe` for the headless audio/video loopback probe, or
`just rtc` inside the development shell to include the worker's Opus encoding and
decoding. Both send Opus and a generated VP8 test pattern between two loopback UDP
peers; no microphone, camera, public STUN service or external server is used. The
probe reports encoded-frame delivery delay and checks received bytes; it is not a
microphone-to-speaker latency measurement. Video admission follows the bandwidth
estimate, reserves room for audio and drops frames rather than accumulating a stale
queue. The transport test details are in [tests/README.md](../tests/README.md#webrtc-transport-trial).

The native call tests use a loopback TLS terminator and a real SFU. On Linux
each participant starts a confined crypto worker and RTC worker. The tests send
synthetic AV1 through the SFU and through real coturn. They check authenticated
frame bytes, source identity, simultaneous publishing and receiving, the relay
address and worker shutdown. Run the Nix `nuschl-session-unit` check, which supplies coturn
and the crypto worker, RTC worker, video worker and SFU executables.
The standalone `nuschl-rtc-worker-unit` check exercises SFrame and two UDP legs
using a synthetic AV1 frame in the worker's confined role.

The native media API accepts local camera and screen frames while it receives
video from the call's other publishers. A publisher and purpose identify each
remote video source. Both media directions use the authenticated WSS peer
address and the SFU's configured UDP port, which defaults to 3479. Two native
peers with video flowing one way can switch to a direct route and keep the SFU
leg available. The direct leg tests its host address and can obtain a reflexive
address from the WSS-advertised UDP TURN service with STUN. In relay-only mode,
the broker forwards the SFU legs through allocations on that service.
