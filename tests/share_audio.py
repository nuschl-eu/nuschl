#!/usr/bin/env python3
"""Desktop audio sharing against a private PipeWire session (no host devices).

Uses the private session of pipewire_recovery.py: PipeWire, WirePlumber and
pipewire-pulse with a virtual speaker and microphone only. Every sound is a
synthetic tone of its own pitch:

- an application playing into the speakers (pw-play): 550 Hz, and a second
  application that starts while the share runs: 660 Hz;
- Nuschl's voice, played by a live media worker: 1000 Hz;
- a stream that names itself Nuschl (`application.name`, pw-play): 1250 Hz;
- shared audio the call audio worker receives and plays itself: 1500 Hz;
- a microphone that is the session's default source (PipeWire's audio test
  source): 440 Hz.

The call audio worker shares the desktop's audio. Its Opus packets, decoded
with reference libopus, hold the applications' tones and none of Nuschl's or
the microphone's. Muting the received stream silences it on the speakers
while its packets keep arriving, and unmuting brings it back. The second application's tone arrives once it plays, and
the first one's leaves once it quits. After the share stops, its node and
links are gone and every other link is still in place. The run repeats with
the worker in the sandbox a call gives it (`nuschl media-sandbox
--call-audio`), where it runs as `/worker`.
"""
import argparse
import ctypes
import json
import math
import os
from pathlib import Path
import signal
import struct
import subprocess
import sys
import tempfile
import threading
import time
import wave

sys.path.insert(0, str(Path(__file__).resolve().parent))
from mumble_client import ReferenceOpus  # noqa: E402
from pipewire_recovery import PipeWire, Worker, defaults  # noqa: E402
from worker_ipc import WorkerCodec  # noqa: E402

CALL_AUDIO_FRAME_LIMIT = 8 * 1024

APPLICATION, SECOND, VOICE, NAMED, RECEIVED, MICROPHONE = 550, 660, 1000, 1250, 1500, 440
SHARE_NODE = "nuschl-shared-audio"
# A sine source at 440 Hz, the session's default microphone: a share that the
# session manager linked like a recording would hear it.
TEST_MICROPHONE = """context.spa-libs = { audiotestsrc = audiotestsrc/libspa-audiotestsrc }
context.objects = [
  { factory = adapter
    args = { factory.name = audiotestsrc node.name = "test-microphone" node.description = "Test microphone"
             media.class = "Audio/Source" audio.position = [ MONO ] priority.session = 3000
             object.linger = true } }
]
"""


def tone_wav(path, frequency, seconds=60, amplitude=0.2):
    with wave.open(str(path), "wb") as file:
        file.setnchannels(2)
        file.setsampwidth(2)
        file.setframerate(48000)
        frames = bytearray()
        for n in range(48000 * seconds):
            value = int(amplitude * 32767 * math.sin(2 * math.pi * frequency * n / 48000))
            frames += struct.pack("<hh", value, value)
        file.writeframes(bytes(frames))
    return path


def opus_tone(opus, frequency, frames, channels=1, amplitude=0.15):
    """20 ms Opus packets of a tone, from reference libopus."""
    error = ctypes.c_int()
    encoder = opus.lib.opus_encoder_create(48000, channels, 2049, ctypes.byref(error))
    assert error.value == 0
    try:
        assert opus.lib.opus_encoder_ctl(encoder, 4002, 64000) == 0
        packets = []
        for frame in range(frames):
            samples = [amplitude * math.sin(2 * math.pi * frequency * (frame * 960 + n) / 48000)
                       for n in range(960) for _ in range(channels)]
            pcm = (ctypes.c_float * len(samples))(*samples)
            output = ctypes.create_string_buffer(1275)
            count = opus.lib.opus_encode_float(encoder, pcm, 960, output, 1275)
            assert count > 0
            packets.append(list(output.raw[:count]))
        return packets
    finally:
        opus.lib.opus_encoder_destroy(encoder)


def level(samples, frequency):
    """Amplitude of one frequency (Goertzel)."""
    coefficient = 2 * math.cos(2 * math.pi * frequency / 48000)
    previous = before = 0.0
    for sample in samples:
        previous, before = sample + coefficient * previous - before, previous
    power = previous * previous + before * before - coefficient * previous * before
    return 2 * math.sqrt(max(power, 0)) / len(samples)


class CallAudio:
    """A call audio worker speaking its protocol on stdin/stdout."""

    def __init__(self, command, pipewire, log):
        env = dict(pipewire.env) | {"PIPEWIRE_DEBUG": "2"}
        self.codec = WorkerCodec("call_audio", command[0])
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=log.open("w"), env=env)
        self.events = []
        self.lock = threading.Lock()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        while (event := self.codec.read_frame(self.process.stdout, 0, limit=CALL_AUDIO_FRAME_LIMIT)) is not None:
            event["received"] = time.monotonic()
            with self.lock:
                self.events.append(event)

    def send(self, payload):
        with self.lock:
            self.codec.write_frame(self.process.stdin, 0, payload, limit=CALL_AUDIO_FRAME_LIMIT)

    def packets(self, start, end):
        with self.lock:
            stopped = [e for e in self.events if e["kind"] == "stopped"]
            assert not stopped, f"the share stopped: {stopped}"
            return [e for e in self.events if e["kind"] == "packet" and start <= e["received"] < end]

    def stop(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)


class Loop(threading.Thread):
    """Sends `packets` on repeat, one every 20 ms, until stopped."""

    def __init__(self, send):
        super().__init__(daemon=True)
        self.send, self.running = send, True

    def run(self):
        start, index = time.monotonic(), 0
        while self.running:
            time.sleep(max(0, start + index * 0.02 - time.monotonic()))
            self.send(index)
            index += 1


def play(pipewire, path, target, properties=""):
    """pw-play of `path` into `target`, its node named after the file."""
    command = [str(pipewire.pipewire / "bin/pw-play"), "--target", target,
               "-P", f"{{ node.name = tone-{path.stem} {properties} }}", str(path)]
    return subprocess.Popen(command, env=pipewire.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                            start_new_session=True)


def graph(pipewire):
    """Node names by id and links as (output node, input node) names."""
    result = subprocess.run([str(pipewire.pipewire / "bin/pw-dump")], env=pipewire.env, capture_output=True,
                            text=True, timeout=10)
    objects = json.loads(result.stdout or "[]")
    names = {entry["id"]: entry["info"]["props"].get("node.name") for entry in objects
             if entry.get("type") == "PipeWire:Interface:Node"}
    links = {(names.get(entry["info"].get("output-node-id")), names.get(entry["info"].get("input-node-id")))
             for entry in objects if entry.get("type") == "PipeWire:Interface:Link"}
    return set(names.values()), links


def playing(pipewire, name):
    """The process of the Nuschl stream `name`, which must reach the speakers."""
    result = subprocess.run([str(pipewire.pipewire / "bin/pw-dump")], env=pipewire.env, capture_output=True,
                            text=True, timeout=10)
    objects = json.loads(result.stdout or "[]")
    props = {entry["id"]: entry["info"]["props"] for entry in objects if "props" in entry.get("info", {})}
    played = [id for id, node in props.items() if node.get("media.class") == "Stream/Output/Audio"
              and node.get("media.name") == name]
    speakers = [id for id, node in props.items() if node.get("node.name") == "speakers"]
    linked = {(entry["info"]["output-node-id"], entry["info"]["input-node-id"]) for entry in objects
              if entry.get("type") == "PipeWire:Interface:Link"}
    assert any((node, sink) in linked for node in played for sink in speakers), f"{name} does not play"
    return props[int(props[played[0]]["client.id"])].get("application.process.binary")


def heard(opus, worker, seconds, label):
    """The tones in the next `seconds` of shared audio, after a settling second."""
    time.sleep(1)
    start = time.monotonic()
    time.sleep(seconds)
    packets = worker.packets(start, time.monotonic())
    assert len(packets) >= seconds * 40, f"{label}: {len(packets)} packets in {seconds} s"
    timestamps = [p["timestamp"] for p in packets]
    assert timestamps == sorted(timestamps) and len(set(timestamps)) == len(timestamps), f"{label}: clock"
    samples = opus.decode([p["opus"] for p in packets])
    levels = {frequency: level(samples, frequency)
              for frequency in (APPLICATION, SECOND, VOICE, NAMED, RECEIVED, MICROPHONE)}
    print(f"{label}: " + ", ".join(f"{f} Hz {v:.4f}" for f, v in levels.items()), flush=True)
    return levels


def check_share(command, binary, pipewire, opus, output, label):
    files = Path(pipewire.root)
    application = play(pipewire, tone_wav(files / "application.wav", APPLICATION), "speakers")
    named = play(pipewire, tone_wav(files / "named.wav", NAMED), "speakers", "application.name = Nuschl")
    voice = Worker(binary, pipewire)
    voice.ready()
    voice_packets = opus_tone(opus, VOICE, 50)
    voice_loop = Loop(lambda i: voice.send({"kind": "playback", "sender": 9, "sequence": 2 * i,
                                            "opus": bytes(voice_packets[i % 50]), "end": False, "volume": 1.0}))
    voice_loop.start()
    worker = CallAudio(command, pipewire, output / f"call-audio-{label}.log")
    received = opus_tone(opus, RECEIVED, 50, channels=2)
    received_loop = Loop(lambda i: worker.send({"kind": "play", "publisher": 7, "timestamp": 960 * i,
                                                "opus": bytes(received[i % 50])}))
    received_loop.start()
    second = None
    try:
        time.sleep(1.5)
        _, before = graph(pipewire)
        worker.send({"kind": "share", "active": True})
        levels = heard(opus, worker, 3, f"{label}: one application")
        assert levels[APPLICATION] > 0.08, levels
        for excluded in (SECOND, VOICE, NAMED, RECEIVED, MICROPHONE):
            assert levels[excluded] < 0.01, (excluded, levels)
        nodes, links = graph(pipewire)
        assert SHARE_NODE in nodes, nodes
        into = {source for source, sink in links if sink == SHARE_NODE}
        assert into and not into & {"microphone", "test-microphone", "speakers"}, into
        print(f"{label}: Nuschl's voice plays from {playing(pipewire, 'Voice chat')!r}, received shared audio "
              f"from {playing(pipewire, 'Shared audio')!r}.", flush=True)

        second = play(pipewire, tone_wav(files / "second.wav", SECOND), "speakers")
        levels = heard(opus, worker, 2, f"{label}: a second application starts")
        assert levels[APPLICATION] > 0.08 and levels[SECOND] > 0.08, levels
        for excluded in (VOICE, NAMED, RECEIVED, MICROPHONE):
            assert levels[excluded] < 0.01, (excluded, levels)

        os.killpg(application.pid, signal.SIGTERM)
        application.wait(timeout=5)
        levels = heard(opus, worker, 2, f"{label}: the first application quits")
        assert levels[SECOND] > 0.08 and levels[APPLICATION] < 0.01, levels

        def speakers(name):
            """The received audio's level on the speakers over the next second."""
            recorder = pipewire.record(output / f"{label}-{name}.wav")
            time.sleep(1.5)
            samples = [sample / 32768 for sample in recorder.stop()]
            return level(samples[len(samples) // 3:], RECEIVED)

        assert speakers("heard") > 0.03
        worker.send({"kind": "volume", "publisher": 7, "volume": 0.0})
        time.sleep(0.5)
        assert speakers("muted") < 0.003, "muted received audio still plays"
        worker.send({"kind": "volume", "publisher": 7, "volume": 1.0})
        time.sleep(0.5)
        assert speakers("unmuted") > 0.03, "the received stream did not continue through the mute"
        print(f"{label}: muting silenced the received stream locally; it played again on unmute.", flush=True)

        worker.send({"kind": "share", "active": False})
        time.sleep(1)
        nodes, after = graph(pipewire)
        assert SHARE_NODE not in nodes, nodes
        assert all(SHARE_NODE not in link for link in after), after
        # The first application quit; every other link the session had is still there.
        kept = {link for link in before if SHARE_NODE not in link and "tone-application" not in link}
        assert kept <= after, f"links lost: {kept - after}"
        print(f"{label}: the share linked only applications and left the session's links in place.", flush=True)
    finally:
        for loop in (received_loop, voice_loop):
            loop.running = False
            loop.join()
        worker.stop()
        voice.stop()
        for player in (application, named, second):
            if player and player.poll() is None:
                os.killpg(player.pid, signal.SIGTERM)
                player.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--pipewire", type=Path, required=True)
    parser.add_argument("--wireplumber", type=Path, required=True)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--dbus-daemon", default="dbus-daemon")
    parser.add_argument("--output", type=Path, default=Path(tempfile.gettempdir()))
    parser.add_argument("--no-sandbox", action="store_true", help="skip the run in the call's sandbox")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    Worker.logs = args.output
    opus = ReferenceOpus(args.opus_library)
    binary = args.binaries / "nuschl-media-worker"
    runs = [("direct", [str(binary), "--call-audio"])]
    if not args.no_sandbox:
        runs.append(("sandbox", [str(args.binaries / "nuschl"), "media-sandbox", "--call-audio"]))
    with tempfile.TemporaryDirectory(prefix="nuschl-share-", dir=os.environ.get("NUSCHL_SHORT_TMP")) as temporary:
        for label, command in runs:
            pipewire = PipeWire(args.pipewire, args.wireplumber, args.dbus_daemon, Path(temporary) / label)
            try:
                (pipewire.config / "pipewire/pipewire.conf.d/20-test-microphone.conf").write_text(TEST_MICROPHONE)
                pipewire.up(expected=("speakers", "microphone", "test-microphone"))
                assert any("test-microphone" in line for line in defaults(pipewire)), defaults(pipewire)
                check_share(command, binary, pipewire, opus, args.output, label)
            finally:
                pipewire.close()


if __name__ == "__main__":
    main()
