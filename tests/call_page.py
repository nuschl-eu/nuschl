#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""The call page in headless Chromium and Firefox (nix/call-page-vm.nix).

Three synthetic publishers (tests/call_publisher.js, Chromium) join a call on the
stream host: the creator and a second participant publish a camera each, a third
a shared screen. A Chromium subscriber and a relay-only Firefox subscriber show
every source decrypted, in its colour, the screen in the large view, and the full
roster. The creator and first Chromium subscriber move from the SFU to direct
video and back as the publisher's bandwidth estimate changes. The screen exposes
receiver video statistics. A pinned camera takes
the large view; a publisher leaving takes its tile
and roster row along. A native publisher shares its screen and a tone as shared
audio before Chromium joins: the screen tile's speaker plays the tone, and muting
it silences the player while the track goes on. Its camera, a 1080p picture in a
1920 × 1082 frame as VCN 4 codes it, shows at 1920 × 1080. A page user then publishes
Chromium's fake camera and screen with Start camera and Share screen. Captures
start at the buttons. The subscribers' tile of that screen shows "No shared
audio" until an explicit audio opt-in shares a synthetic tab tone over the SFU,
which then plays and mutes as the native one. The subscribers show both videos, a participant without
AV1 switches the call to VP9 and back while the streams keep going, and Stop
ends the captures and the tiles. A Chromium without encoded transforms and a
broken link get their refusals. Screenshots land in the directory given as the
only argument.

Usage: call_page.py OUTPUT_DIRECTORY  (CHROMIUM, FIREFOX in the environment)
"""

import base64
import hashlib
import hmac
import json
import os
import secrets
import shutil
import subprocess
import sys
import time
from pathlib import Path

from selenium import webdriver
from selenium.webdriver.common.action_chains import ActionChains
from selenium.webdriver.common.by import By
from selenium.webdriver.support.ui import Select

ORIGIN = "https://stream.test"
OUT = Path(sys.argv[1])
OUT.mkdir(parents=True, exist_ok=True)
COLOURS = {"a": (220, 40, 40), "b": (40, 190, 70), "c": (40, 80, 220)}


def hkdf(secret: bytes, label: str, length: int) -> bytes:
    """The key schedule of apps/stream-sfu/src/protocol.rs."""
    key = hmac.new(b"nuschl/stream/v2", secret, hashlib.sha256).digest()
    return hmac.new(key, f"nuschl/stream/v2 {label}".encode() + b"\x01", hashlib.sha256).digest()[:length]


def b64(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def fragment(secret, publisher):
    return f"#v=1&k={b64(secret)}&h={b64(hashlib.sha256(hkdf(publisher, 'host', 32)).digest())}"


S, P = secrets.token_bytes(32), secrets.token_bytes(32)
LINK = fragment(S, P)
CREATOR = f"{LINK}&p={b64(P)}"


# The page's peer connections, for the receivers' statistics.
RECORD_PEERS = """
window.peers = [];
window.RTCPeerConnection = class extends RTCPeerConnection {
  constructor(...args) { super(...args); window.peers.push(this); }
};
"""
# The page user's capture calls, with whether each asked for audio and the surface
# it preselected, and their tracks. In the screen picker the user picks a monitor.
RECORD_CAPTURE = """
window.captures = []; window.capturedTracks = [];
for (const name of ['getUserMedia', 'getDisplayMedia']) {
  const original = navigator.mediaDevices[name].bind(navigator.mediaDevices);
  navigator.mediaDevices[name] = async (constraints) => {
    window.captures.push([name, Boolean(constraints.audio), constraints.video.displaySurface || null]);
    const stream = await original(name === 'getDisplayMedia' ? {video: true} : constraints);
    if (name === 'getDisplayMedia' && constraints.audio) {
      const [video] = stream.getVideoTracks();
      const settings = video.getSettings.bind(video);
      video.getSettings = () => ({...settings(), displaySurface: 'browser'});
      video.getCaptureHandle = () => null;
      const context = new AudioContext();
      const output = context.createMediaStreamDestination();
      const oscillator = context.createOscillator();
      oscillator.frequency.value = 660;
      oscillator.connect(output);
      oscillator.start();
      const [track] = output.stream.getAudioTracks();
      const stop = track.stop.bind(track);
      track.stop = () => {
        if (track.readyState === 'ended') return;
        stop();
        oscillator.disconnect();
        context.close();
      };
      stream.addTrack(track);
    }
    window.capturedTracks.push(...stream.getTracks());
    return stream;
  };
}
"""
# Record quality messages sent by a viewer to the real SFU.
RECORD_QUALITY = """
window.qualityRequests = [];
const send = WebSocket.prototype.send;
WebSocket.prototype.send = function (data) {
  const message = JSON.parse(data);
  if (message.type === 'quality') window.qualityRequests.push(message.request);
  send.call(this, data);
};
"""
# A browser that decodes VP9 and no AV1.
WITHOUT_AV1 = """
const capabilities = RTCRtpReceiver.getCapabilities;
RTCRtpReceiver.getCapabilities = (kind) => {
  const found = capabilities.call(RTCRtpReceiver, kind);
  if (found) found.codecs = found.codecs.filter((codec) => !/av1/i.test(codec.mimeType));
  return found;
};
"""

# Keep the browser's real candidate-pair identities while controlling its
# upload estimate so both paths can be exercised on any VM host.
DIRECT_UPLOAD = """
window.uploadEstimate = 1;
const getStats = RTCPeerConnection.prototype.getStats;
RTCPeerConnection.prototype.getStats = async function (...args) {
  const stats = await getStats.apply(this, args);
  return new Map([...stats].map(([id, entry]) => [id, entry.type === 'candidate-pair'
    ? {...entry, availableOutgoingBitrate: window.uploadEstimate} : entry]));
};
"""


def chromium(*, script=None, devices=False, direct=False):
    """Headless Chromium; `script` runs before every page's own, `devices` gives it
    Chromium's fake camera and screen without permission prompts."""
    options = webdriver.ChromeOptions()
    options.binary_location = os.environ["CHROMIUM"]
    options.set_capability("goog:loggingPrefs", {"browser": "ALL"})
    for argument in ["--headless=new", "--no-sandbox", "--ignore-certificate-errors", "--window-size=1280,800",
                     "--autoplay-policy=no-user-gesture-required", "--disable-dev-shm-usage"]:
        options.add_argument(argument)
    if devices:
        options.add_argument("--use-fake-device-for-media-stream")
        options.add_argument("--use-fake-ui-for-media-stream")
    if direct:
        # These two profiles exchange host IP candidates inside the isolated VM network.
        options.add_argument("--disable-features=WebRtcHideLocalIpsWithMdns")
    driver = webdriver.Chrome(options=options, service=webdriver.ChromeService(shutil.which("chromedriver")))
    if script:
        driver.execute_cdp_cmd("Page.addScriptToEvaluateOnNewDocument", {"source": script})
    return driver


def firefox():
    options = webdriver.FirefoxOptions()
    options.binary_location = os.environ["FIREFOX"]
    options.add_argument("-headless")
    options.accept_insecure_certs = True
    options.set_preference("media.autoplay.default", 0)
    driver = webdriver.Firefox(options=options, service=webdriver.FirefoxService(shutil.which("geckodriver")))
    driver.set_window_size(1280, 800)
    return driver


def wait(driver, script, what, timeout=90):
    """Polls `script` (returning a value) until it is truthy; returns it."""
    deadline = time.monotonic() + timeout
    while True:
        value = driver.execute_script(script)
        if value:
            return value
        if time.monotonic() > deadline:
            logs = driver.execute_script("return document.body.innerText")
            raise AssertionError(f"{what}: timed out; page text: {logs!r}")
        time.sleep(0.5)


# Each tile's peer, placement, state, size and colour below the label and the bar.
TILES = """
return [...document.querySelectorAll('.tile')].map(tile => {
  const video = tile.querySelector('video');
  const canvas = Object.assign(document.createElement('canvas'), {width: 16, height: 9});
  const context = canvas.getContext('2d');
  context.drawImage(video, 0, 0, 16, 9);
  const [r, g, b] = context.getImageData(8, 6, 1, 1).data;
  return {peer: Number(tile.dataset.peer), screen: 'screen' in tile.dataset, state: tile.dataset.state,
          large: tile.parentNode.id === 'large', width: video.videoWidth, colour: [r, g, b],
          label: tile.querySelector('figcaption').textContent};
});
"""


def close(colour, expected):
    if callable(expected):
        return expected(colour)
    return all(abs(a - b) <= 45 for a, b in zip(colour, expected))


def green(colour):
    """Chromium's fake camera and screen: a green picture."""
    r, g, b = colour
    return g > 48 and g > r + 30 and g > b + 30


def showing(driver, expected, what):
    """Waits until the tiles are exactly `expected` ({(peer, screen): (colour, large)}),
    playing; a large of None takes either place."""
    def matches(tile):
        colour, large = expected.get((tile["peer"], tile["screen"]), (None, None))
        return (colour is not None and tile["state"] == "playing" and tile["width"] > 0
                and close(tile["colour"], colour) and large in (None, tile["large"]))

    def check():
        tiles = driver.execute_script(TILES)
        ok = len(tiles) == len(expected) and all(matches(tile) for tile in tiles)
        return tiles if ok else None
    deadline = time.monotonic() + 120
    while (tiles := check()) is None:
        if time.monotonic() > deadline:
            raise AssertionError(f"{what}: {json.dumps(driver.execute_script(TILES))}")
        time.sleep(0.5)
    print(what, json.dumps(tiles), flush=True)
    return tiles


def roster(driver):
    return driver.execute_script("return [...document.querySelectorAll('#roster li')].map(li => li.innerText)")


def heading(driver):
    return wait(driver, "const h = document.querySelector('#ending:not([hidden]) h1'); return h && h.textContent",
                "refusal")


def direct_video(publisher, viewer, peer):
    publisher.execute_script("publish([{color: 'rgb(220,40,40)', label: 'Camera A'}])")
    tracks = {(peer, False): (COLOURS["a"], False)}
    direct = "document.getElementById('status').textContent.includes('Direct video')"
    showing(viewer, tracks, "initial SFU video decoded")
    publisher.execute_script("window.uploadEstimate = 20_000_000")
    wait(viewer, f"return {direct}", "direct video connected", timeout=40)
    showing(viewer, tracks, "direct video decoded")
    assert publisher.execute_script("return call.sources.get('camera').transceiver.sender.track === null")

    publisher.execute_script("window.uploadEstimate = 1")
    wait(viewer, f"return !{direct}", "direct video falls back on reduced upload", timeout=20)
    wait(publisher, "return Boolean(call.sources.get('camera').transceiver.sender.track)",
         "the SFU sender regained the capture", timeout=20)
    showing(viewer, tracks, "SFU video decoded after fallback")


RECORDED_TONE = """
const [selector, done] = arguments;
const stream = document.querySelector(selector).srcObject;
const [track] = stream.getAudioTracks();
// The receiver's jitter buffer over the recording, where the page lists its connections.
const receiver = async () => {
  for (const pc of window.peers ?? []) {
    const stats = await pc.getStats(track).catch(() => new Map());
    for (const s of stats.values()) {
      if (s.type === 'inbound-rtp') return {packets: s.packetsReceived, lost: s.packetsLost, samples: s.totalSamplesReceived,
        concealed: s.concealedSamples, silent: s.silentConcealedSamples, events: s.concealmentEvents,
        inserted: s.insertedSamplesForDeceleration, removed: s.removedSamplesForAcceleration,
        delay: s.jitterBufferDelay, emitted: s.jitterBufferEmittedCount, target: s.jitterBufferTargetDelay};
    }
  }
  return null;
};
const before = receiver();
const recorder = new MediaRecorder(stream);
const chunks = [];
const stop = () => { if (recorder.state === 'recording') recorder.stop(); };
const deadline = setTimeout(stop, 20000);
recorder.ondataavailable = ({ data }) => {
  if (data.size) chunks.push(data);
  if (chunks.length >= 2) stop();
};
recorder.onerror = event => {
  clearTimeout(deadline);
  done({error: String(event.error)});
};
recorder.onstop = async () => {
  clearTimeout(deadline);
  const recording = new Blob(chunks, {type: recorder.mimeType});
  try {
    const data = await recording.arrayBuffer();
    const context = new OfflineAudioContext(1, 48000, 48000);
    const decoded = await context.decodeAudioData(data);
    const samples = decoded.getChannelData(0);
    if (samples.length < decoded.sampleRate) throw new Error('recorded less than a second of audio');
    const middle = samples.subarray(samples.length / 4, 3 * samples.length / 4);
    let power = 0;
    let crossings = 0;
    let positive = false;
    for (const sample of middle) {
      power += sample * sample;
      if (sample > 0.1 && !positive) {
        positive = true;
        crossings++;
      }
      if (sample < -0.1) positive = false;
    }
    const [start, end] = await Promise.all([before, receiver()]);
    const receiving = start && end && Object.fromEntries(Object.keys(end).map((key) => [key, end[key] - start[key]]));
    done({hertz: crossings * decoded.sampleRate / middle.length, rms: Math.sqrt(power / middle.length), receiving});
  } catch (error) {
    done({error: String(error), bytes: recording.size, type: recording.type, chunks: chunks.length});
  }
};
recorder.start(1000);
"""


def speaker(peer):
    return f'.tile[data-peer="{peer}"][data-screen] .shared-audio button'


def player(peer):
    return f'#shared-audio audio[data-peer="{peer}"]'


def label(driver, peer):
    return driver.execute_script(f"return document.querySelector('{speaker(peer)}')?.getAttribute('aria-label')")


def without_audio(driver, peer, what):
    wait(driver, f"""const button = document.querySelector('{speaker(peer)}');
                     return button?.disabled && button.getAttribute('aria-label') === 'No shared audio'""",
         f"{what}: the screen tile shows no shared audio")


def tone_of(driver, peer, hertz, minimum, what):
    # Wait for actual recorder output. A negotiated track can precede its
    # first packets, and under VM load the recorder can begin late.
    tone = driver.execute_async_script(RECORDED_TONE, player(peer))
    assert "error" not in tone and abs(tone["hertz"] - hertz) < 25 and tone["rms"] > minimum, (driver.name, what, tone)
    return tone


def shared_audio(driver, peer, hertz, minimum, what, screenshot=None):
    """`peer`'s screen tile plays their shared audio from its speaker, which mutes
    only this page's player: the track goes on."""
    wait(driver, f"return document.querySelector('{speaker(peer)}')?.disabled === false",
         f"{what}: the screen tile's speaker")
    # A browser that waits for a click before playing shows the audio muted.
    if label(driver, peer) == "Unmute shared audio":
        driver.find_element(By.CSS_SELECTOR, speaker(peer)).click()
    wait(driver, f"""const audio = document.querySelector('{player(peer)}');
                     return audio && !audio.paused && !audio.muted && audio.srcObject.getAudioTracks()[0]?.readyState === 'live'
                       && document.querySelector('{speaker(peer)}').getAttribute('aria-label') === 'Mute shared audio'""",
         f"{what}: shared audio playing")
    tone = tone_of(driver, peer, hertz, minimum, what)
    print(f"{driver.name}: {what}: received the encrypted tone {tone}", flush=True)
    if screenshot:
        ActionChains(driver).move_to_element(driver.find_element(By.CSS_SELECTOR, speaker(peer))).perform()
        # On every tile, the speaker and its volume stay clear of the name.
        wait(driver, f"""const apart = (a, b) => a.right <= b.left || b.right <= a.left || a.bottom <= b.top || b.bottom <= a.top;
                         const clear = [...document.querySelectorAll('.tile .shared-audio')].every(control => {{
                           const name = control.closest('.tile').querySelector('figcaption').getBoundingClientRect();
                           return [...control.children].every(part => apart(part.getBoundingClientRect(), name));
                         }});
                         const volume = document.querySelector('{speaker(peer)}').nextElementSibling;
                         return clear && getComputedStyle(volume).opacity === '1'""",
             f"{what}: the volume opens on hover, clear of the names")
        driver.save_screenshot(str(OUT / screenshot))
    driver.find_element(By.CSS_SELECTOR, speaker(peer)).click()
    wait(driver, f"""return document.querySelector('{player(peer)}').muted
                     && document.querySelector('{speaker(peer)}').getAttribute('aria-label') === 'Unmute shared audio'""",
         f"{what}: muted")
    tone_of(driver, peer, hertz, minimum, f"{what}, muted here")
    driver.find_element(By.CSS_SELECTOR, speaker(peer)).click()
    wait(driver, f"return !document.querySelector('{player(peer)}').muted", f"{what}: unmuted")


def native_video(viewer):
    secret, publisher = secrets.token_bytes(32), secrets.token_bytes(32)
    posted = fragment(secret, publisher)
    state = OUT / "native"
    state.mkdir()
    log = state / "publisher.log"
    env = dict(os.environ, NUSCHL_NATIVE_CALL_HOST=ORIGIN.removeprefix("https://"),
               NUSCHL_NATIVE_CALL_STATE=str(state),
               NUSCHL_NATIVE_CALL_LINK=f"{ORIGIN}/call{posted}&p={b64(publisher)}")
    with log.open("w") as output:
        native = subprocess.Popen([os.environ["NUSCHL_NATIVE_CALL_TEST"], "--ignored", "--exact",
                                   "sfu::tests::native_publisher_in_call_page_vm"], env=env,
                                  stdout=output, stderr=subprocess.STDOUT)
        peer_file = state / "peer"
        try:
            for _ in range(150):
                if peer_file.exists() and (peer := peer_file.read_text()).isdigit():
                    break
                assert native.poll() is None, log.read_text()
                time.sleep(0.1)
            else:
                raise AssertionError(f"native publisher did not join: {log.read_text()}")
            viewer.get(f"{ORIGIN}/call{posted}")
            showing(viewer, {(int(peer), True): ((255, 0, 0), True)}, "native screen decoded in Chromium")
            shared_audio(viewer, int(peer), 600, 0.05, "native shared audio, joined late",
                         screenshot="native-shared-audio.png")
            (state / "switch").touch()
            showing(viewer, {(int(peer), False): ((255, 0, 0), None)}, "native camera replaced screen in Chromium")
            # The native camera codes 1920 × 1080 in a 1920 × 1082 frame, as VCN 4
            # does; the tile shows the descriptor's 1920 × 1080.
            shown = wait(viewer, f"""
              const tile = document.querySelector('.tile[data-peer="{peer}"]:not([data-screen])');
              const video = tile?.querySelector('video');
              const box = video && getComputedStyle(video).objectViewBox;
              return video?.videoHeight === 1082 && box !== 'none' && [video.videoWidth, video.videoHeight, box];
            """, "native 1080p camera cropped in Chromium")
            print("native camera", shown, flush=True)
            assert shown == [1920, 1082, "inset(0px 0px 2px)"], shown
        finally:
            (state / "stop").touch()
            try:
                assert native.wait(timeout=15) == 0, log.read_text()
            except subprocess.TimeoutExpired:
                native.kill()
                native.wait()
                raise
    viewer.get("about:blank")


drivers = []
try:
    # Every browser starts first, before the media load: a busy host slows startup most.
    user = chromium(script=RECORD_PEERS + RECORD_CAPTURE, devices=True)
    vp9 = chromium(script=WITHOUT_AV1)
    plain = chromium(script="delete window.RTCRtpScriptTransform")
    drivers += [user, vp9, plain]
    native_video(user)
    publishers, pages = {}, {}
    for name, fragment in [("a", CREATOR), ("b", LINK), ("c", LINK)]:
        driver = pages[name] = chromium(script=DIRECT_UPLOAD if name == "a" else None, direct=name == "a")
        drivers.append(driver)
        driver.get(f"{ORIGIN}/call_publisher.html{fragment}")
        publishers[name] = wait(driver, "return window.peer && window.peer()", f"publisher {name} joined")
        if name == "a":
            chrome, fox = chromium(script=RECORD_QUALITY, direct=True), firefox()
            drivers += [chrome, fox]
            chrome.get(f"{ORIGIN}/call{LINK}")
            direct_video(driver, chrome, publishers[name])
            fox.get(f"{ORIGIN}/call?relay{LINK}")
    a, b, c = (publishers[name] for name in "abc")
    for driver in [chrome, fox]:
        status = wait(driver, "const s = document.getElementById('status').textContent;"
                              "return s !== 'Connecting' && s", "subscriber joined")
        print("status", status, flush=True)
    assert "Relay only" in fox.find_element(By.ID, "status").text
    assert "Relay only" not in chrome.find_element(By.ID, "status").text
    codec = chrome.find_element(By.ID, "status").text.split(" · ")[0]
    assert codec in ("AV1", "VP9") and fox.find_element(By.ID, "status").text.startswith(codec), codec

    pages["b"].execute_script("publish([{color: 'rgb(40,190,70)', label: 'Camera B'}])")
    cameras = {(a, False): (COLOURS["a"], False), (b, False): (COLOURS["b"], False)}
    for driver in [chrome, fox]:
        showing(driver, cameras, "two cameras in the grid")
    chrome.save_screenshot(str(OUT / "grid.png"))

    pages["c"].execute_script("publish([{color: 'rgb(40,80,220)', label: 'Screen C', screen: true}])")
    shared = {**cameras, (c, True): (COLOURS["c"], True)}
    for driver in [chrome, fox]:
        showing(driver, shared, "the screen in the large view")
    chrome.save_screenshot(str(OUT / "large-screen.png"))
    fox.save_screenshot(str(OUT / "large-screen-firefox.png"))

    # Strip tiles grow with the window, and Auto asks for the camera tier they show.
    camera_tier = f"return window.qualityRequests.filter(r => r.startsWith('01{b:016x}00')).at(-1)"
    wait(chrome, f"{camera_tier} === '01{b:016x}0000'", "180p for a strip tile in a small window")
    chrome.set_window_size(2560, 1440)
    wait(chrome, f"{camera_tier} === '01{b:016x}0001'", "360p for a strip tile in a large window")
    chrome.set_window_size(1280, 800)
    wait(chrome, f"{camera_tier} === '01{b:016x}0000'", "180p again in a small window")

    # Actual receiver measurements become available on the selected tile.
    screen_stats = chrome.find_element(By.CSS_SELECTOR, "#large .video-stats")
    screen_stats.find_element(By.TAG_NAME, "summary").click()
    quality = Select(screen_stats.find_element(By.TAG_NAME, "select"))
    assert quality.first_selected_option.text == "Auto"
    # In a larger window Auto fits the large tile above 720p15; a limit caps it.
    chrome.set_window_size(2560, 1600)
    lowest = f"01{c:016x}0100"
    latest = f"return window.qualityRequests.filter(r => r.startsWith('{lowest[:-2]}')).at(-1)"
    wait(chrome, f"{latest} !== '{lowest}'", "receive quality fits the tile")
    quality.select_by_value("0")
    wait(chrome, f"{latest} === '{lowest}'", "receive limit")
    quality.select_by_value("")
    chrome.set_window_size(1280, 800)
    wait(chrome, """
      const panel = document.querySelector('#large .video-stats[open]');
      if (!panel) return false;
      const video = panel.parentElement.querySelector('video');
      const [resolution, decoded, presented, bitrate, jitter] =
        Array.from(panel.querySelectorAll('dd'), node => node.textContent);
      return resolution === [video.videoWidth, video.videoHeight].join(' × ') &&
        [decoded, presented, bitrate].every(value => parseFloat(value) > 0) && parseFloat(jitter) >= 0;
    """, "received screen stats", timeout=25)
    chrome.find_element(By.CSS_SELECTOR, "#grid .video-stats > summary").click()
    wait(chrome, "return document.querySelectorAll('.video-stats[open]').length === 1 && "
                 "document.querySelector('#grid .video-stats[open]')", "one stats panel at a time")
    chrome.find_element(By.CSS_SELECTOR, "#grid .video-stats[open] > summary").click()

    # The full roster on both subscribers: themselves, the other subscriber and the publishers.
    for driver in [chrome, fox]:
        rows = roster(driver)
        print("roster", rows, flush=True)
        assert len(rows) == 5 and rows[0].startswith("You"), rows
        named = {row.split("\n")[0]: row for row in rows[1:]}
        assert "Host" in named[f"Participant {a}"] and "Camera" in named[f"Participant {a}"], rows
        assert "Screen" in named[f"Participant {c}"], rows
        assert driver.find_element(By.ID, "count").text == "5"
        assert not driver.find_element(By.ID, "host-actions").is_displayed()

    # A pinned camera takes the large view; the screen joins the grid.
    chrome.find_element(By.CSS_SELECTOR, f".tile[data-peer='{a}'] .pin").click()
    pinned = {(a, False): (COLOURS["a"], True), (c, True): (COLOURS["c"], False)}
    showing(chrome, {**pinned, (b, False): cameras[b, False]}, "pinned camera")
    chrome.save_screenshot(str(OUT / "large-pinned.png"))

    # A publisher that leaves takes its tile and roster row along.
    pages["b"].quit()
    drivers.remove(pages["b"])
    showing(chrome, pinned, "after a publisher left")
    wait(chrome, "return document.querySelectorAll('#roster li').length === 4", "roster after a leave")

    # A page user publishes Chromium's fake camera and screen from the page's own
    # buttons. Nothing is captured before a button, and nothing asks for audio.
    user.get(f"{ORIGIN}/call{LINK}")
    wait(user, "return !document.getElementById('camera').disabled", "Start camera ready")
    time.sleep(3)
    assert user.execute_script("return window.captures") == [], "a capture before the button"
    user.find_element(By.ID, "camera").click()
    wait(user, "return document.getElementById('camera').textContent === 'Stop camera'", "camera on")
    user.find_element(By.ID, "screen").click()
    wait(user, "return document.getElementById('screen').textContent === 'Stop sharing'", "screen on")
    assert user.execute_script("return window.captures") == [["getUserMedia", False, None],
                                                              ["getDisplayMedia", False, "window"]]
    assert user.find_element(By.ID, "notice").text == "Sharing your whole screen"
    u = user.execute_script("return Number(document.querySelector('.tile[data-own]').dataset.peer)")
    published = {(u, False): (green, False), (u, True): (green, None)}
    everyone = {chrome: {**pinned, **published},
                fox: {(a, False): (COLOURS["a"], None), (c, True): (COLOURS["c"], None), **published}}
    for driver, expected in everyone.items():
        showing(driver, expected, "a page user's camera and screen")
    showing(user, {(a, False): (COLOURS["a"], False), (c, True): (COLOURS["c"], True),
                   (u, False): (green, False), (u, True): (green, False)}, "the page user's own view")

    for driver in [chrome, fox]:
        without_audio(driver, u, "a page user's screen")
    chrome.save_screenshot(str(OUT / "screen-without-audio.png"))
    user.find_element(By.ID, "screen").click()
    wait(user, "return document.getElementById('screen').textContent === 'Share screen'", "screen stopped")
    user.find_element(By.ID, "screen-audio").click()
    user.find_element(By.ID, "screen").click()
    wait(user, "return document.getElementById('notice').textContent.includes('Sharing tab audio')",
         "tab audio announced")
    assert user.execute_script("return window.captures.at(-1)") == ["getDisplayMedia", True, "browser"]
    selector = player(u)
    for driver in [chrome, fox]:
        shared_audio(driver, u, 660, 0.2, "tab audio", screenshot="shared-audio-tile.png" if driver is chrome else None)

    user.save_screenshot(str(OUT / "publishing.png"))
    chrome.save_screenshot(str(OUT / "publishing-subscriber.png"))

    # A participant without AV1 switches the call to VP9: every publisher re-encodes,
    # and the streams keep going; its leave switches back to AV1.
    def codec_everywhere(codec):
        for driver in [chrome, fox, user]:
            wait(driver, f"return document.getElementById('status').textContent.startsWith('{codec}')", codec)

    def advancing(driver, peer, what):
        """The peer's tiles play and their pictures change (the fake devices animate)."""
        showing(driver, everyone[driver], what)
        pictures = f"""
        const canvas = Object.assign(document.createElement('canvas'), {{width: 32, height: 18}});
        const context = canvas.getContext('2d');
        const pictures = {{}};
        for (const tile of document.querySelectorAll('.tile[data-peer="{peer}"]')) {{
          const video = tile.querySelector('video');
          if (!video.videoWidth || video.readyState < video.HAVE_CURRENT_DATA) continue;
          context.drawImage(video, 0, 0, 32, 18);
          const kind = 'screen' in tile.dataset ? 'screen' : 'camera';
          pictures[kind] = [video.srcObject.id, video.videoWidth, video.videoHeight,
            Array.from(context.getImageData(0, 0, 32, 18).data).join()];
        }}
        return pictures;
        """
        before = {}
        waiting = {"camera", "screen"}
        deadline = time.monotonic() + 15
        while waiting:
            current = driver.execute_script(pictures)
            for kind in waiting & current.keys():
                frame = current[kind]
                # Compare pictures at the same size and on the same stream.
                if kind not in before or before[kind][:3] != frame[:3]:
                    before[kind] = frame
                elif before[kind][3] != frame[3]:
                    waiting.remove(kind)
            if not waiting:
                return
            if time.monotonic() >= deadline:
                sizes = {kind: frame[1:3] for kind, frame in current.items()}
                raise AssertionError(f"{what} ({driver.name}): no picture change for {sorted(waiting)}. "
                                     f"Latest video sizes {sizes}")
            time.sleep(0.5)

    vp9.get(f"{ORIGIN}/call{LINK}")
    codec_everywhere("VP9")
    showing(vp9, {(a, False): (COLOURS["a"], False), (c, True): (COLOURS["c"], None), **published},
            "VP9 from every publisher")
    for driver in [chrome, fox]:
        advancing(driver, u, "after the switch to VP9")
    vp9.quit()
    drivers.remove(vp9)
    codec_everywhere("AV1")
    for driver in [chrome, fox]:
        advancing(driver, u, "after the switch back to AV1")

    user.find_element(By.ID, "screen-audio").click()
    wait(user, "return document.getElementById('screen').textContent === 'Stop sharing'", "screen kept sharing")
    for driver in [chrome, fox]:
        wait(driver, f"return !document.querySelector('{selector}')", "tab audio after turning it off")
        showing(driver, everyone[driver], "video without tab audio")

    # Stop ends both captures and takes the tiles off every page.
    user.find_element(By.ID, "camera").click()
    user.find_element(By.ID, "screen").click()
    for driver in [chrome, fox]:
        wait(driver, f"return !document.querySelector('.tile[data-peer=\"{u}\"]')", "tiles after Stop")
        wait(driver, f"return !document.querySelector('{selector}')", "tab audio after Stop")
    assert user.execute_script("return window.capturedTracks.every(track => track.readyState === 'ended')")
    assert user.find_element(By.ID, "camera").text == "Start camera"

    # Refusals: a browser without encoded transforms, and a link without its keys.
    plain.get(f"{ORIGIN}/call{LINK}")
    assert heading(plain) == "Browser not supported"
    assert plain.save_screenshot(str(OUT / "refusal.png"))
    chrome.get(f"{ORIGIN}/call?again#v=1&k={b64(S)}")
    assert heading(chrome) == "Link not valid"
    print(json.dumps({"passed": True, "codec": codec}), flush=True)
except BaseException:
    for index, driver in enumerate(drivers):
        driver.save_screenshot(str(OUT / f"failure-{index}.png"))
        print(f"page {index}:", driver.current_url.split("#")[0], driver.execute_script(
            "return document.body.innerText"), flush=True)
        if driver.capabilities["browserName"] != "firefox":
            for entry in driver.get_log("browser"):
                print(f"page {index} console:", entry["message"], flush=True)
    raise
finally:
    for driver in drivers:
        driver.quit()
