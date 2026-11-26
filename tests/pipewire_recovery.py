#!/usr/bin/env python3
"""Live media worker against a private PipeWire session (no host devices).

Runs PipeWire, WirePlumber and pipewire-pulse with their own runtime directory
and D-Bus daemon. Only virtual nodes exist: WirePlumber's ALSA, Bluetooth and
camera monitors are disabled, and the test asserts that no hardware node
appeared. Checks:

1. Playback through the native PipeWire host reaches the speakers, with a
   raised quantum range (1024..2048, as in virtual machines).
2. PipeWire restart: the worker reports the failure; a replacement worker
   pinned to PipeWire (as the desktop supervisor does) tries no other audio
   server while PipeWire is down, and plays again once it is back.
3. A pipewire-pulse restart does not disturb a native PipeWire worker; a
   worker on the PulseAudio API recovers after pipewire-pulse restarts.
4. A PipeWire echo-cancel node as the default microphone is recognised:
   Nuschl does not stack its own canceller, routes only its own playback
   into the canceller's sink and leaves the session defaults alone. Device
   lists hold only real sources and sinks (no other applications' streams,
   sink monitors or virtual default entries).
5. An RNNoise filter-chain behind the echo canceller (PipeWire's own
   source-rnnoise.conf layout: mic → echo-cancel → RNNoise → default source)
   is followed upstream: Nuschl turns off both its echo canceller and its
   noise suppressor and feeds the system canceller its playback.
6. Nuschl's own canceller in a room on one clock (the speakers looped into
   the microphone at -37 dB): it records what the speakers play, so both its
   own playback and another application's sound are removed from the
   microphone, every run.
7. Switching the default output while the microphone is open (#298):
   headphones connect and become the default sink, then switch profile (the
   headphones' sink goes away; a headset sink and microphone appear and
   become the defaults). The worker's streams follow the defaults: it keeps
   running without reopening its devices, and its playback reaches each new
   sink.
8. Mute and unmute close and open the microphone inside the running worker
   (#73): playback resumes after each mute and unmute, PipeWire removes the
   capture streams, the worker confirms each mute, and no microphone audio
   is encoded after a mute. Silence of 60 ms or more and a confirmation
   after 100 ms are quarantined steps on a loaded host (#376).
9. The restarts of 2 and 3 with every worker in the sandbox a call gives it
   (`nuschl media-sandbox`, the desktop's own bwrap command), replaced as
   the desktop replaces it. PipeWire's socket is bound as a file, so after a
   restart only a replacement worker reaches the new one; PulseAudio's
   directory is bound, so a pipewire-pulse restart shows up in place.
"""
import argparse
import itertools
import json
import math
import os
from pathlib import Path
import random
import shutil
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
import quarantine  # noqa: E402
from worker_ipc import WorkerCodec  # noqa: E402

VIRTUAL_NODES = """# A raised quantum range, as PipeWire applies in virtual machines (and some users
# configure): Nuschl must fit its buffer request into it.
context.properties = { default.clock.min-quantum = 1024 default.clock.max-quantum = 2048 }
context.objects = [
  { factory = adapter
    args = { factory.name = support.null-audio-sink node.name = "speakers" node.description = "Speakers"
             media.class = "Audio/Sink" audio.position = [ FL FR ] object.linger = true
             monitor.channel-volumes = true } }
  { factory = adapter
    args = { factory.name = support.null-audio-sink node.name = "microphone" node.description = "Microphone"
             media.class = "Audio/Source/Virtual" audio.position = [ MONO ] object.linger = true } }
]
"""
ECHO_CANCEL = """context.modules = [
  { name = libpipewire-module-echo-cancel
    args = {
      library.name = aec/libspa-aec-webrtc
      capture.props = { node.name = "echo-cancel-capture" target.object = "microphone" node.passive = true }
      source.props = { node.name = "echo-cancel-source" node.description = "Echo-Cancel Source" }
      sink.props = { node.name = "echo-cancel-sink" node.description = "Echo-Cancel Sink" }
      playback.props = { node.name = "echo-cancel-playback" target.object = "speakers" node.passive = true }
    }
  }
]
"""
# PipeWire's source-rnnoise.conf, mono, capturing the echo canceller's output.
# Without --rnnoise-ladspa a pass-through plugin stands in for RNNoise (the
# graph and names are what Nuschl recognises).
RNNOISE = """context.modules = [
  { name = libpipewire-module-filter-chain
    args = {
      node.description = "Noise Canceling source"
      media.name = "Noise Canceling source"
      filter.graph = { nodes = [ PLUGIN ] }
      audio.position = [ MONO ]
      capture.props = { node.name = "effect_input.rnnoise" node.passive = true target.object = "echo-cancel-source" }
      playback.props = { node.name = "effect_output.rnnoise" media.class = Audio/Source }
    }
  }
]
"""
# The room: what the speakers play reaches a microphone source, attenuated.
ROOM = """context.modules = [
  { name = libpipewire-module-loopback
    args = {
      node.description = "room"
      capture.props = { node.name = "room.capture" stream.capture.sink = true target.object = "speakers" node.passive = true }
      playback.props = { node.name = "room.mic" node.description = "Room microphone" media.class = Audio/Source audio.position = [ MONO ] }
    }
  }
]
"""
# nuschl_core::audio::SILENT_OPUS: the terminator that ends a transmission.
SILENT_OPUS = bytes([0xF8, 0xFF, 0xFE])
NO_HARDWARE = """wireplumber.profiles = {
  main = {
    monitor.alsa = disabled
    monitor.alsa-midi = disabled
    monitor.bluez = disabled
    monitor.bluez-midi = disabled
    monitor.v4l2 = disabled
    monitor.libcamera = disabled
  }
}
"""
# The session has no X11 display. Without one, module-x11-bell stops its
# freshly started thread at once, and PipeWire 1.6 can deadlock there: the
# stop request runs on the main thread while the new thread already waits in
# epoll, so the join never returns and the daemon stops serving.
NO_X11_BELL = "context.properties = { module.x11.bell = false }\n"


class PipeWire:
    def __init__(self, pipewire, wireplumber, dbus_daemon, root, runtime=None):
        """`runtime`: an existing runtime directory to serve in (a private desktop session's)."""
        self.pipewire, self.wireplumber = Path(pipewire), Path(wireplumber)
        self.root = root
        self.runtime = runtime or root / "run"
        self.runtime.mkdir(mode=0o700, parents=True, exist_ok=runtime is not None)
        root.mkdir(parents=True, exist_ok=True)
        self.config = root / "config"
        (self.config / "pipewire/pipewire.conf.d").mkdir(parents=True)
        (self.config / "pipewire/pipewire.conf.d/05-no-x11-bell.conf").write_text(NO_X11_BELL)
        (self.config / "pipewire/pipewire.conf.d/10-nodes.conf").write_text(VIRTUAL_NODES)
        (self.config / "wireplumber/wireplumber.conf.d").mkdir(parents=True)
        (self.config / "wireplumber/wireplumber.conf.d/10-no-hardware.conf").write_text(NO_HARDWARE)
        bus = f"unix:path={self.runtime}/bus"
        self.env = {"PATH": os.environ.get("PATH", ""), "HOME": str(root), "XDG_RUNTIME_DIR": str(self.runtime),
                    "XDG_CONFIG_HOME": str(self.config), "XDG_STATE_HOME": str(root / "state"),
                    "XDG_DATA_DIRS": f"{self.wireplumber}/share:{self.pipewire}/share",
                    "DBUS_SESSION_BUS_ADDRESS": bus, "PIPEWIRE_RUNTIME_DIR": str(self.runtime),
                    "PULSE_RUNTIME_PATH": str(self.runtime / "pulse")}
        # A private bus: WirePlumber must never talk to the desktop's session.
        self.dbus = subprocess.Popen([dbus_daemon, "--session", "--nofork", f"--address={bus}"], env=self.env,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        self.processes = {}

    def enable_echo_cancel(self):
        (self.config / "pipewire/pipewire.conf.d/20-echo-cancel.conf").write_text(ECHO_CANCEL)

    def enable_rnnoise(self, ladspa=None):
        plugin = (f'{{ type = ladspa name = rnnoise plugin = "{ladspa}" label = noise_suppressor_mono }}'
                  if ladspa else "{ type = builtin name = rnnoise label = copy }")
        (self.config / "pipewire/pipewire.conf.d/30-rnnoise.conf").write_text(RNNOISE.replace("PLUGIN", plugin))

    def start(self, name):
        binary = {"pipewire": self.pipewire / "bin/pipewire", "pipewire-pulse": self.pipewire / "bin/pipewire-pulse",
                  "wireplumber": self.wireplumber / "bin/wireplumber"}[name]
        self.processes[name] = subprocess.Popen([str(binary)], env=self.env,
                                                stdout=(self.root / f"{name}.log").open("a"),
                                                stderr=subprocess.STDOUT, start_new_session=True)

    def stop(self, name):
        process = self.processes.pop(name, None)
        if process and process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            process.wait(timeout=10)

    def nodes(self):
        result = subprocess.run([str(self.pipewire / "bin/pw-dump")], env=self.env, capture_output=True, text=True,
                                timeout=10)
        objects = json.loads(result.stdout or "[]")
        return {entry["info"]["props"].get("node.name"): entry["info"]["props"] for entry in objects
                if entry.get("type") == "PipeWire:Interface:Node"}

    def up(self, expected=("speakers", "microphone")):
        # Start in dependency order, each once the previous is serving (as systemd
        # orders the user services): WirePlumber exits if PipeWire is not up yet.
        deadline = time.monotonic() + 30

        def wait(condition):
            while time.monotonic() < deadline:
                try:
                    if condition():
                        return True
                except (json.JSONDecodeError, subprocess.TimeoutExpired):
                    pass
                time.sleep(0.2)
            return False
        self.start("pipewire")
        if wait(lambda: set(expected) <= set(self.nodes())):
            self.start("wireplumber")
            if wait(lambda: any("'default.audio.sink'" in line for line in defaults(self))):
                self.start("pipewire-pulse")
                if wait(lambda: (self.runtime / "pulse/native").exists()):
                    nodes = self.nodes()
                    hardware = [name for name, props in nodes.items() if props.get("device.api")]
                    assert not hardware, f"hardware nodes appeared in the private session: {hardware}"
                    time.sleep(0.5)
                    return
        try:
            nodes = sorted(name for name in self.nodes() if name)
        except (json.JSONDecodeError, subprocess.TimeoutExpired) as error:
            nodes = f"unavailable ({error})"
        try:
            session_defaults = defaults(self)
        except subprocess.TimeoutExpired as error:
            session_defaults = f"unavailable ({error})"
        logs = "\n".join(f"--- {name}.log\n" + (self.root / f"{name}.log").read_text()[-2000:]
                         for name in ("pipewire", "wireplumber", "pipewire-pulse") if (self.root / f"{name}.log").exists())
        raise AssertionError(f"private PipeWire did not come up: nodes {nodes}, expected {sorted(expected)}, "
                             f"pulse socket {(self.runtime / 'pulse/native').exists()}, defaults {session_defaults}\n{logs}")

    def down(self):
        for name in ("pipewire-pulse", "wireplumber", "pipewire"):
            self.stop(name)

    def record(self, path, target="speakers"):
        """Record what reaches a sink (its monitor) as 16-bit mono WAV."""
        return Recorder(self, path, target)

    def close(self):
        self.down()
        if self.dbus.poll() is None:
            os.killpg(self.dbus.pid, signal.SIGTERM)


class Worker:
    count = 0
    logs = Path(tempfile.gettempdir())

    def __init__(self, binary, pipewire, host=None, backend=None, muted=True, settings=None, preview=False):
        """`binary`: the media worker, or a whole command that runs it (the sandbox)."""
        env = dict(pipewire.env) | {"PULSE_SERVER": f"unix:{pipewire.runtime}/pulse/native", "PIPEWIRE_DEBUG": "2"}
        if backend:
            env["NUSCHL_AUDIO_BACKEND"] = backend
        Worker.count += 1
        self.log = Worker.logs / f"worker-{Worker.count}.log"
        command = binary if isinstance(binary, list) else [str(binary), "--live"]
        self.codec = WorkerCodec("live_media", command[0])
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=self.log.open("w"), env=env)
        self.events = []
        threading.Thread(target=self._read, daemon=True).start()
        self.send({"kind": "configure", "bitrate": 40000, "packet_loss_percent": 5, "settings": settings or {},
                   "host": host})
        if preview:
            # The Settings check: the microphone open, nothing encoded, a level meter.
            self.send({"kind": "preview", "capture": True, "loopback": False})
        else:
            self.send({"kind": "set_state", "epoch": 1, "muted": muted, "deafened": False})

    def _read(self):
        while (event := self.codec.read_frame(self.process.stdout, 0)) is not None:
            event["received"] = time.monotonic()
            self.events.append(event)

    def send(self, payload):
        self.codec.write_frame(self.process.stdin, 0, payload)

    def wait(self, predicate, message, timeout=20, since=0):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for event in list(self.events)[since:]:
                if predicate(event):
                    return event
            time.sleep(0.05)
        statuses = [e.get("message") for e in self.events if e["kind"] == "status"][-5:]
        raise AssertionError(f"{message}; recent status: {statuses}")

    def ready(self, since=0, timeout=20):
        return self.wait(lambda e: e["kind"] == "status" and e["ready"], "audio did not become ready",
                         timeout=timeout, since=since)

    def stop(self):
        try:
            self.send({"kind": "shutdown"})
            self.process.wait(timeout=5)
        except (BrokenPipeError, subprocess.TimeoutExpired):
            self.process.kill()
            self.process.wait(timeout=5)


class Recorder:
    """pw-record of a sink's monitor. It returns once audio is being written,
    so nothing played after it is missed, however slow the host."""

    # A WAV header; the file grows past it once samples arrive.
    HEADER = 44

    def __init__(self, pipewire, path, target):
        self.path, self.log = path, path.with_suffix(".record.log")
        self.process = subprocess.Popen(
            [str(pipewire.pipewire / "bin/pw-record"), "--target", target, "-P", "{ stream.capture.sink = true }",
             "--format", "s16", "--rate", "48000", "--channels", "1", str(path)],
            env=pipewire.env, stdout=subprocess.DEVNULL, stderr=self.log.open("w"), start_new_session=True)
        deadline = time.monotonic() + 30
        first = None
        while True:
            size = path.stat().st_size if path.exists() else 0
            if first is not None and size > first:
                return
            if size > self.HEADER:
                first = size
            if self.process.poll() is not None:
                raise AssertionError(f"pw-record exited ({self.process.returncode}) before recording {path}: "
                                     f"{self.log.read_text()[-2000:]}")
            if time.monotonic() > deadline:
                self.process.kill()
                self.process.wait(timeout=5)
                raise AssertionError(f"pw-record wrote no audio to {path} within 30 s: {self.log.read_text()[-2000:]}")
            time.sleep(0.05)

    def size(self):
        return self.path.stat().st_size

    def sounds_again(self, since, what):
        """Wait until the speakers play again, 100 ms of it recorded after the
        file was `since` bytes long; within a 30 s hang guard."""
        tail = 2 * 4800
        deadline = time.monotonic() + 30
        while True:
            size = self.size()
            if size >= since + tail:
                with self.path.open("rb") as recording:
                    recording.seek(size - tail)
                    samples = struct.unpack(f"<{tail // 2}h", recording.read(tail))
                if math.sqrt(sum(v * v for v in samples) / len(samples)) / 32768 > 0.02:
                    return
            assert time.monotonic() < deadline, f"playback did not resume {what} within 30 s"
            time.sleep(0.01)

    def stop(self):
        """End the recording and return its samples, once the file is complete."""
        os.killpg(self.process.pid, signal.SIGINT)
        try:
            self.process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait(timeout=5)
            raise AssertionError(f"pw-record did not finish {self.path} within 30 s: {self.log.read_text()[-2000:]}")
        with wave.open(str(self.path)) as recording:
            frames = recording.readframes(recording.getnframes())
        return struct.unpack("<%dh" % (len(frames) // 2), frames)


def play_and_measure(worker, pipewire, packets, path, target="speakers"):
    recorder = pipewire.record(path, target)
    start = time.monotonic()
    for index, opus in enumerate(packets):
        time.sleep(max(0, start + index * 0.02 - time.monotonic()))
        worker.send({"kind": "playback", "sender": 9, "sequence": index * 2, "opus": bytes(opus),
                     "end": index == len(packets) - 1, "volume": 1.0})
    time.sleep(0.6)
    samples = recorder.stop()
    return math.sqrt(sum(s * s for s in samples) / max(len(samples), 1)) / 32768


def check_restarts(binary, pipewire, packets, output):
    worker = Worker(binary, pipewire)
    worker.ready()
    devices = worker.wait(lambda e: e["kind"] == "devices", "no device report")
    assert devices["output"].startswith("pipewire:"), \
        f"native PipeWire was not used: {devices}\nworker log:\n{worker.log.read_text()[-3000:]}"
    level = play_and_measure(worker, pipewire, packets, output / "pipewire-before.wav")
    assert level > 0.02, f"playback did not reach the speakers (RMS {level:.4f})"
    print(f"Native PipeWire playback reached the speakers (RMS {level:.3f}).", flush=True)

    mark = len(worker.events)
    pipewire.down()
    worker.wait(lambda e: e["kind"] == "status" and not e["ready"], "the PipeWire failure was not reported",
                since=mark)
    worker.stop()
    # The desktop supervisor replaces the worker and pins it to the server that worked.
    worker = Worker(binary, pipewire, host="pipewire")
    time.sleep(3)
    attempts = [e["message"] for e in worker.events if e["kind"] == "status" and not e["ready"]]
    assert attempts, "the pinned worker did not report PipeWire as unavailable"
    assert not any("Alsa" in m or "PulseAudio" in m for m in attempts), \
        f"a pinned worker tried another audio server while PipeWire was down: {attempts}"
    mark = len(worker.events)
    pipewire.up()
    worker.ready(since=mark, timeout=30)
    level = play_and_measure(worker, pipewire, packets, output / "pipewire-after.wav")
    assert level > 0.02, f"playback did not resume after the PipeWire restart (RMS {level:.4f})"
    print(f"PipeWire restart: pinned worker waited for PipeWire only, then played again (RMS {level:.3f}).",
          flush=True)

    mark = len(worker.events)
    pipewire.stop("pipewire-pulse")
    time.sleep(1)
    pipewire.start("pipewire-pulse")
    time.sleep(2)
    failures = [e for e in worker.events[mark:] if e["kind"] == "status" and not e["ready"]]
    assert not failures, f"a pipewire-pulse restart disturbed the native PipeWire worker: {failures}"
    worker.stop()

    worker = Worker(binary, pipewire, backend="pulse")
    worker.ready()
    mark = len(worker.events)
    pipewire.stop("pipewire-pulse")
    worker.wait(lambda e: e["kind"] == "status" and not e["ready"], "the pipewire-pulse failure was not reported",
                since=mark)
    worker.stop()
    pipewire.start("pipewire-pulse")
    worker = Worker(binary, pipewire, backend="pulse", host="pulseaudio")
    worker.ready(timeout=30)
    level = play_and_measure(worker, pipewire, packets, output / "pulse-after.wav")
    assert level > 0.02, f"playback through pipewire-pulse did not resume (RMS {level:.4f})"
    worker.stop()
    print("pipewire-pulse restart: native worker undisturbed; PulseAudio-API worker recovered.", flush=True)


def check_sandboxed_restarts(sandboxed, pipewire, packets, output):
    """check_restarts through the real sandbox, supervised as the desktop does:
    a worker reporting Status{ready: false} is stopped and, after 2 s, replaced
    by a fresh sandbox pinned to the audio server that last worked."""
    worker = Worker(sandboxed, pipewire)
    worker.ready()
    devices = worker.wait(lambda e: e["kind"] == "devices", "no device report")
    assert devices["output"].startswith("pipewire:"), \
        f"native PipeWire was not used: {devices}\nworker log:\n{worker.log.read_text()[-3000:]}"
    level = play_and_measure(worker, pipewire, packets, output / "sandbox-before.wav")
    assert level > 0.02, f"sandboxed playback did not reach the speakers (RMS {level:.4f})"

    mark = len(worker.events)
    pipewire.down()
    worker.wait(lambda e: e["kind"] == "status" and not e["ready"], "the PipeWire failure was not reported",
                since=mark)
    worker.stop()
    failures = []

    def replacement():
        worker = Worker(sandboxed, pipewire, host="pipewire")
        status = worker.wait(lambda e: e["kind"] == "status", "the replacement reported nothing")
        if not status["ready"]:
            failures.append(status["message"])
            worker.stop()
            time.sleep(2)
        return worker if status["ready"] else None
    for _ in range(2):
        assert replacement() is None, "a replacement found audio while PipeWire was down"
    pipewire.up()
    deadline = time.monotonic() + 30
    while (worker := replacement()) is None:
        assert time.monotonic() < deadline, f"no sandboxed replacement reached the new PipeWire: {failures[-3:]}"
    assert not any("Alsa" in m or "PulseAudio" in m for m in failures), \
        f"a pinned worker tried another audio server while PipeWire was down: {failures}"
    level = play_and_measure(worker, pipewire, packets, output / "sandbox-after.wav")
    assert level > 0.02, f"sandboxed playback did not resume after the PipeWire restart (RMS {level:.4f})"
    print(f"Sandbox, PipeWire restart: {len(failures)} replacements waited for PipeWire only, then a fresh "
          f"sandbox played again (RMS {level:.3f}).", flush=True)

    mark = len(worker.events)
    pipewire.stop("pipewire-pulse")
    time.sleep(1)
    pipewire.start("pipewire-pulse")
    time.sleep(2)
    disturbed = [e for e in worker.events[mark:] if e["kind"] == "status" and not e["ready"]]
    assert not disturbed, f"a pipewire-pulse restart disturbed the sandboxed PipeWire worker: {disturbed}"
    worker.stop()

    worker = Worker(sandboxed, pipewire, backend="pulse")
    worker.ready()
    mark = len(worker.events)
    pipewire.stop("pipewire-pulse")
    worker.wait(lambda e: e["kind"] == "status" and not e["ready"], "the pipewire-pulse failure was not reported",
                since=mark)
    # Not replaced: the bound directory shows the restarted server's socket.
    mark = len(worker.events)
    pipewire.start("pipewire-pulse")
    worker.ready(since=mark, timeout=30)
    level = play_and_measure(worker, pipewire, packets, output / "sandbox-pulse-after.wav")
    assert level > 0.02, f"sandboxed playback through pipewire-pulse did not resume (RMS {level:.4f})"
    worker.stop()
    print("Sandbox, pipewire-pulse restart: the native worker was undisturbed; a PulseAudio-API worker "
          f"reconnected in place (RMS {level:.3f}).", flush=True)


def defaults(pipewire):
    """The session's default sink and source (PipeWire 'default' metadata).

    WirePlumber owns that metadata, and on a loaded host its properties can
    reach pw-metadata after pw-metadata's round trip ends: a reading without
    them is repeated."""
    for _ in range(10):
        result = subprocess.run([str(pipewire.pipewire / "bin/pw-metadata"), "-n", "default"], env=pipewire.env,
                                capture_output=True, text=True, timeout=10)
        found = sorted(line.split("key:", 1)[1] for line in result.stdout.splitlines() if "key:'default.audio." in line)
        if found:
            return found
        time.sleep(0.1)
    return found


def connect_device(pipewire, name, media_class, position):
    """A virtual device that appears as hardware does (Bluetooth headphones) and,
    with the highest priority, becomes the session's default."""
    subprocess.run([str(pipewire.pipewire / "bin/pw-cli"), "create-node", "adapter",
                    f"{{ factory.name = support.null-audio-sink node.name = {name} node.description = {name} "
                    f"media.class = {media_class} audio.position = [ {position} ] priority.session = 3000 "
                    "object.linger = true }"],
                   env=pipewire.env, check=True, capture_output=True, timeout=10)
    key = "'default.audio.sink'" if media_class == "Audio/Sink" else "'default.audio.source'"
    wait_default(pipewire, lambda line: key in line and f'"{name}"' in line, f"{name} did not become the default")


def wait_default(pipewire, condition, message):
    deadline = time.monotonic() + 10
    while not any(condition(line) for line in defaults(pipewire)):
        assert time.monotonic() < deadline, f"{message}: {defaults(pipewire)}, nodes {sorted(pipewire.nodes())}"
        time.sleep(0.2)


def check_output_switch(binary, pipewire, packets, output):
    worker = Worker(binary, pipewire, muted=False)
    worker.ready()
    mark = len(worker.events)
    connect_device(pipewire, "headphones", "Audio/Sink", "FL FR")
    time.sleep(1)
    level = play_and_measure(worker, pipewire, packets, output / "switch-headphones.wav", target="headphones")
    assert level > 0.02, f"playback did not follow the new default sink (RMS {level:.4f})"
    # A profile switch: the headphones' sink goes; a headset sink and microphone come.
    node = pipewire.nodes()["headphones"]["object.id"]
    subprocess.run([str(pipewire.pipewire / "bin/pw-cli"), "destroy", str(node)], env=pipewire.env, check=True,
                   capture_output=True, timeout=10)
    wait_default(pipewire, lambda line: "'default.audio.sink'" in line and '"speakers"' in line,
                 "the session kept the removed sink")
    connect_device(pipewire, "headset", "Audio/Sink", "FL FR")
    connect_device(pipewire, "headset-mic", "Audio/Source/Virtual", "MONO")
    time.sleep(1)
    level = play_and_measure(worker, pipewire, packets, output / "switch-headset.wav", target="headset")
    assert level > 0.02, f"playback did not follow the headset profile (RMS {level:.4f})"
    assert ("headset-mic", "Nuschl") in links(pipewire), f"the microphone did not follow: {sorted(links(pipewire))}"
    reopened = [e for e in worker.events[mark:] if e["kind"] in ("status", "devices")]
    worker.stop()
    assert not reopened, f"switching the output device reopened the microphone's devices: {reopened}"
    print(f"Output device switch: the open microphone and playback followed the defaults without a restart "
          f"(RMS {level:.3f}).", flush=True)


class NodeMonitor:
    """PipeWire's own account of nodes coming and going (pw-mon), timed on arrival."""

    def __init__(self, pipewire):
        self.process = subprocess.Popen([str(pipewire.pipewire / "bin/pw-mon"), "-N", "--hide-params"], env=pipewire.env,
                                        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
                                        start_new_session=True)
        self.nodes = {}
        self.removed = {}
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        kind = node = None
        for line in self.process.stdout:
            if line.rstrip() in ("added:", "changed:", "removed:"):
                kind, node = line.strip().rstrip(":"), None
            elif line.strip().startswith("id: "):
                node = int(line.split(":", 1)[1])
                if kind == "removed":
                    self.removed[node] = time.monotonic()
                    self.nodes.pop(node, None)
                elif kind == "added":
                    self.nodes[node] = {}
            elif node in self.nodes and " = " in line:
                key, value = line.strip().lstrip("*").strip().split(" = ", 1)
                self.nodes[node][key] = value.strip('"')

    def captures(self):
        """Nuschl's capture streams (the only application in the session)."""
        return {node for node, props in list(self.nodes.items())
                if props.get("media.class") == "Stream/Input/Audio" and props.get("node.name") == "Nuschl"}

    def close(self):
        os.killpg(self.process.pid, signal.SIGTERM)
        self.process.wait(timeout=5)


def percentiles(values):
    ordered = sorted(values)
    pick = lambda share: ordered[min(len(ordered) - 1, int(share * len(ordered)))]  # noqa: E731
    return {"p50": pick(.5), "p95": pick(.95), "p99": pick(.99), "max": ordered[-1]}


def check_mute(binary, pipewire, packets, output, cycles):
    """Mute and unmute `cycles` times while voice plays (#73)."""
    monitor = NodeMonitor(pipewire)
    worker = Worker(binary, pipewire, muted=False, settings={"transmit": "continuous"})
    worker.ready()
    deadline = time.monotonic() + 10
    while len(monitor.captures()) < 2:
        assert time.monotonic() < deadline, f"no microphone and reference streams: {monitor.nodes}"
        time.sleep(0.01)
    stop = threading.Event()

    def play():
        start, index = time.monotonic(), 0
        while not stop.is_set():
            time.sleep(max(0, start + index * 0.02 - time.monotonic()))
            worker.send({"kind": "playback", "stream": 1, "sender": 9, "sequence": index * 2,
                         "opus": bytes(packets[index % len(packets)]), "end": False, "volume": 1.0})
            index += 1
    path = output / "mute-playback.wav"
    recorder = pipewire.record(path)
    threading.Thread(target=play, daemon=True).start()
    time.sleep(1)
    gone, confirmed, late = [], [], []
    epoch = 1
    started = quarantine.cpu_counters()
    for _ in range(cycles):
        streams = monitor.captures()
        epoch += 1
        mark = len(worker.events)
        muted, recorded = time.monotonic(), recorder.size()
        worker.send({"kind": "set_state", "epoch": epoch, "muted": True, "deafened": False})
        closed = worker.wait(lambda e: e["kind"] == "microphone_closed" and e["epoch"] >= epoch,
                             "the worker did not confirm the mute", since=mark)
        confirmed.append(closed["received"] - muted)
        deadline = time.monotonic() + 10
        while not all(node in monitor.removed for node in streams):
            assert time.monotonic() < deadline, f"PipeWire kept the capture streams {streams}"
            time.sleep(0.001)
        gone.append(max(monitor.removed[node] for node in streams) - muted)
        recorder.sounds_again(recorded, "after a mute")
        time.sleep(0.25)
        # Microphone audio encoded before the worker applied the mute carries
        # the old epoch (the supervisor drops it); after it, only the
        # terminator, which is silence.
        packets_after = [e for e in worker.events[mark:] if e["kind"] == "packet"]
        assert all(e["epoch"] < epoch or (e["end"] and bytes(e["opus"]) == SILENT_OPUS)
                   for e in packets_after), f"microphone audio was encoded after the mute: {packets_after}"
        late += [e["received"] - muted for e in packets_after if e["epoch"] < epoch]
        epoch += 1
        mark, recorded = len(worker.events), recorder.size()
        worker.send({"kind": "set_state", "epoch": epoch, "muted": False, "deafened": False})
        worker.wait(lambda e: e["kind"] == "packet" and e["epoch"] == epoch, "the Unmute sent no voice", since=mark)
        recorder.sounds_again(recorded, "after an unmute")
        time.sleep(0.25)
    stop.set()
    time.sleep(0.3)
    samples = recorder.stop()
    statuses = [e for e in worker.events if e["kind"] == "status" and not e["ready"]]
    worker.stop()
    monitor.close()
    assert not statuses, f"the worker restarted its devices: {statuses}"
    window = 480
    levels = [math.sqrt(sum(v * v for v in samples[i:i + window]) / window) / 32768
              for i in range(0, len(samples) - window, window)]
    playing = [i for i, level in enumerate(levels) if level > 0.02]
    silent = [i for i in range(playing[0], playing[-1]) if levels[i] < 0.01]
    runs = [len(list(group)) for _, group in itertools.groupby(
        enumerate(silent), key=lambda pair: pair[1] - pair[0])]
    report = {"cycles": cycles, "stream_gone_ms": {k: round(v * 1000, 1) for k, v in percentiles(gone).items()},
              "confirmed_ms": {k: round(v * 1000, 1) for k, v in percentiles(confirmed).items()},
              "old_epoch_packets_after_mute": len(late),
              "silent_10ms_windows": len(silent), "longest_silence_ms": 10 * max(runs, default=0)}
    (output / "mute-timing.json").write_text(json.dumps(report, indent=2))
    print(f"Mute in place: {report}", flush=True)
    measured = (f"longest silence {report['longest_silence_ms']} ms, mute confirmed within "
                f"{report['confirmed_ms']['max']} ms; {quarantine.stolen(started)}")
    # A loaded host drops periods and delays the close; a restarted worker
    # leaves 150 ms and more of silence.
    with quarantine.step("vm-smoke/pipewire-mute-silence", measured=measured):
        assert max(runs, default=0) < 6, f"playback stopped across mute/unmute: {report}"
    with quarantine.step("vm-smoke/pipewire-mute-close-time", measured=measured):
        assert max(confirmed) < 0.1, f"a mute was confirmed after the 100 ms bound: {report}"


def check_echo_cancel(binary, pipewire):
    # The user chose the canceller as their microphone (as in GNOME Settings).
    subprocess.run([str(pipewire.pipewire / "bin/pw-metadata"), "-n", "default", "0",
                    "default.configured.audio.source", '{ "name": "echo-cancel-source" }', "Spa:String:JSON"],
                   env=pipewire.env, check=True, capture_output=True, timeout=10)
    deadline = time.monotonic() + 10
    while not any("echo-cancel-source" in line and "'default.audio.source'" in line for line in defaults(pipewire)):
        assert time.monotonic() < deadline, f"the echo-cancel source did not become the default: {defaults(pipewire)}"
        time.sleep(0.2)
    before = defaults(pipewire)
    worker = Worker(binary, pipewire, muted=False)
    devices = worker.wait(lambda e: e["kind"] == "devices", "no device report")
    worker.ready()
    assert devices["echo"]["kind"] == "upstream", devices["echo"]
    assert devices["output"].endswith(":echo-cancel-sink"), devices["output"]
    inputs = [entry["id"] for entry in devices["inputs"]]
    outputs = [entry["id"] for entry in devices["outputs"]]
    # cpal's PipeWire host omits Audio/Source/Virtual nodes (the test microphone), see TODO.md.
    assert inputs == ["pipewire:echo-cancel-source"], f"the microphone list should hold only real sources: {inputs}"
    assert sorted(outputs) == ["pipewire:echo-cancel-sink", "pipewire:speakers"], \
        f"the speaker list should hold only real sinks: {outputs}"
    time.sleep(1)
    after = defaults(pipewire)
    worker.stop()
    assert before == after, f"the session defaults changed: {before} -> {after}"
    print("PipeWire echo-cancel recognised: no second canceller; only Nuschl's playback goes to its sink; "
          "session defaults untouched.", flush=True)


def links(pipewire):
    """(output node name, input node name) for every link in the graph."""
    result = subprocess.run([str(pipewire.pipewire / "bin/pw-dump")], env=pipewire.env, capture_output=True,
                            text=True, timeout=10)
    objects = json.loads(result.stdout or "[]")
    names = {entry["id"]: entry["info"]["props"].get("node.name") for entry in objects
             if entry.get("type") == "PipeWire:Interface:Node"}
    return {(names.get(entry["info"].get("output-node-id")), names.get(entry["info"].get("input-node-id")))
            for entry in objects if entry.get("type") == "PipeWire:Interface:Link"}


def check_rnnoise_chain(binary, pipewire):
    subprocess.run([str(pipewire.pipewire / "bin/pw-metadata"), "-n", "default", "0",
                    "default.configured.audio.source", '{ "name": "effect_output.rnnoise" }', "Spa:String:JSON"],
                   env=pipewire.env, check=True, capture_output=True, timeout=10)
    deadline = time.monotonic() + 10
    while not any("effect_output.rnnoise" in line and "'default.audio.source'" in line for line in defaults(pipewire)):
        assert time.monotonic() < deadline, f"the RNNoise source did not become the default: {defaults(pipewire)}"
        time.sleep(0.2)
    # The worker inspects the graph once at start; on a busy host PipeWire may
    # still be linking the canceller into the chain.
    deadline = time.monotonic() + 30
    while ("echo-cancel-source", "effect_input.rnnoise") not in links(pipewire):
        assert time.monotonic() < deadline, f"the RNNoise chain was not linked: {sorted(links(pipewire))}"
        time.sleep(0.2)
    before = defaults(pipewire)
    worker = Worker(binary, pipewire, muted=False)
    # The graph about when the worker inspects it, for a failure report (#321).
    started = links(pipewire)
    devices = worker.wait(lambda e: e["kind"] == "devices", "no device report")
    status = worker.ready()
    graph = links(pipewire)
    assert devices["input"] == "pipewire:effect_output.rnnoise", devices["input"]
    assert devices["echo"] == {"kind": "upstream", "device": "Echo-Cancel Source"}, \
        f"{devices['echo']}; links at the worker's start {sorted(started, key=str)}, " \
        f"once ready {sorted(graph, key=str)}"
    assert devices["noise"] == "Noise Canceling source", devices["noise"]
    assert devices["output"].endswith(":echo-cancel-sink"), \
        f"Nuschl's playback must reach the canceller's reference: {devices['output']}"
    assert "system echo cancellation" in status["message"], status
    # The chain Nuschl recognised is the one PipeWire actually built.
    assert ("echo-cancel-source", "effect_input.rnnoise") in graph, sorted(graph)
    time.sleep(1)
    after = defaults(pipewire)
    worker.stop()
    assert before == after, f"the session defaults changed: {before} -> {after}"
    print("RNNoise behind echo-cancel recognised: Nuschl's canceller and noise suppressor are off, "
          "its playback feeds the system canceller, session defaults untouched.", flush=True)


def check_room_echo(binary, pipewire, output):
    """Nuschl's canceller with its reference recorded from the speakers."""
    env = pipewire.env
    subprocess.run([str(pipewire.pipewire / "bin/pw-metadata"), "-n", "default", "0",
                    "default.configured.audio.source", '{ "name": "room.mic" }', "Spa:String:JSON"],
                   env=env, check=True, capture_output=True, timeout=10)
    dump = json.loads(subprocess.run([str(pipewire.pipewire / "bin/pw-dump")], env=env, capture_output=True,
                                     text=True, timeout=10).stdout)
    room = next(entry["id"] for entry in dump if entry.get("type") == "PipeWire:Interface:Node"
                and entry["info"]["props"].get("node.name") == "room.capture")
    subprocess.run([str(pipewire.pipewire / "bin/pw-cli"), "s", str(room), "Props", "{ volume: 0.03 }"],
                   env=env, check=True, capture_output=True, timeout=10)
    time.sleep(1)
    removed = []
    for run in range(3):
        worker = Worker(binary, pipewire, muted=False)
        devices = worker.wait(lambda e: e["kind"] == "devices", "no device report")
        assert devices["input"] == "pipewire:room.mic" and devices["echo"]["kind"] == "nuschl", devices
        worker.ready()
        worker.send({"kind": "echo_test"})
        report = worker.wait(lambda e: e["kind"] == "echo_test", "no echo test result", timeout=20)
        worker.stop()
        with quarantine.step("vm-smoke/pipewire-own-playback-echo"):
            assert report["echo_db"] is not None and report["reduction_db"] >= 30 and report["transmitted"] <= 0.02, \
                f"run {run}: the canceller did not remove its own playback: {report}"
        removed.append("none" if report["reduction_db"] is None else f"{report['reduction_db']:.0f}")
    # Another application plays into the speakers; Nuschl plays nothing. Its music
    # is a melody per channel, the channels taking turns: notes of random pitch
    # with two harmonics. A pure tone fits the echo at many delays.
    music = output / "other-app.wav"
    pitches = random.Random(275)
    melodies = [[110 * 2 ** (pitches.randrange(24) / 12) for _ in range(100)] for _ in range(2)]
    with wave.open(str(music), "wb") as file:
        file.setnchannels(2)
        file.setsampwidth(2)
        file.setframerate(48000)
        frames = bytearray()
        for n in range(48000 * 12):
            t = n / 48000
            left, right = (0.17 * sum(math.sin(2 * math.pi * k * melody[int(t / 0.12)] * t) / k for k in (1, 2, 3))
                           for melody in melodies)
            left *= max(0.0, math.sin(2 * math.pi * 2.3 * t))
            right *= max(0.0, math.sin(2 * math.pi * 1.7 * t + 1))
            frames += struct.pack("<hh", int(left * 32767), int(right * 32767))
        file.writeframes(bytes(frames))
    levels = {}
    for mode in ("on", "off"):
        player = subprocess.Popen([str(pipewire.pipewire / "bin/pw-play"), "--target", "speakers", str(music)],
                                  env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                  start_new_session=True)
        worker = Worker(binary, pipewire, preview=True, settings={"echo_cancellation": mode})
        worker.ready()
        time.sleep(6)
        mark = len(worker.events)
        time.sleep(3)
        window = [e for e in worker.events[mark:] if e["kind"] == "level"]
        worker.stop()
        os.killpg(player.pid, signal.SIGTERM)
        player.wait(timeout=5)
        loud = sorted(e["level_db"] for e in window)[len(window) // 2:]
        levels[mode] = (sum(loud) / max(1, len(loud)), sum(e["transmitting"] for e in window) / max(1, len(window)))
    print(f"Room on one clock: Nuschl's canceller removed its own playback by {', '.join(removed)} dB.", flush=True)
    assert levels["off"][0] - levels["on"][0] >= 20, f"another application's sound was not removed: {levels}"
    print(f"Room on one clock: the canceller removed another application's stereo sound "
          f"({levels['off'][0] - levels['on'][0]:.0f} dB).", flush=True)
    with quarantine.step("vm-smoke/pipewire-other-app-gate"):
        assert levels["on"][1] <= 0.05, f"the gate opened for another application's sound: {levels}"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--pipewire", type=Path, required=True)
    parser.add_argument("--wireplumber", type=Path, required=True)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--dbus-daemon", default=shutil.which("dbus-daemon") or "dbus-daemon")
    parser.add_argument("--output", type=Path, default=Path(tempfile.gettempdir()))
    parser.add_argument("--rnnoise-ladspa", type=Path, help="librnnoise_ladspa.so (else a pass-through)")
    parser.add_argument("--mute-cycles", type=int, default=8, help="mute/unmute cycles of the mute check")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    Worker.logs = args.output
    packets = ReferenceOpus(args.opus_library).encode_tone(100)
    binary = args.binaries / "nuschl-media-worker"
    sandboxed = [str(args.binaries / "nuschl"), "media-sandbox"]
    with tempfile.TemporaryDirectory(prefix="nuschl-pw-", dir=os.environ.get("NUSCHL_SHORT_TMP")) as temporary:
        only = os.environ.get("NUSCHL_PIPEWIRE_ONLY")
        if only in (None, "restarts"):
            pipewire = PipeWire(args.pipewire, args.wireplumber, args.dbus_daemon, Path(temporary) / "plain")
            try:
                pipewire.up()
                check_restarts(binary, pipewire, packets, args.output)
            finally:
                pipewire.close()
        if only in (None, "switch"):
            pipewire = PipeWire(args.pipewire, args.wireplumber, args.dbus_daemon, Path(temporary) / "switch")
            try:
                pipewire.up()
                check_output_switch(binary, pipewire, packets, args.output)
            finally:
                pipewire.close()
        if only in (None, "mute"):
            pipewire = PipeWire(args.pipewire, args.wireplumber, args.dbus_daemon, Path(temporary) / "mute")
            try:
                pipewire.up()
                check_mute(binary, pipewire, packets, args.output, args.mute_cycles)
            finally:
                pipewire.close()
        if only in (None, "sandbox"):
            pipewire = PipeWire(args.pipewire, args.wireplumber, args.dbus_daemon, Path(temporary) / "sandbox")
            try:
                pipewire.up()
                check_sandboxed_restarts(sandboxed, pipewire, packets, args.output)
            finally:
                pipewire.close()
        if only in (None, "echo"):
            pipewire = PipeWire(args.pipewire, args.wireplumber, args.dbus_daemon, Path(temporary) / "echo")
            try:
                pipewire.enable_echo_cancel()
                pipewire.up(expected=("speakers", "microphone", "echo-cancel-source", "echo-cancel-sink"))
                check_echo_cancel(binary, pipewire)
            finally:
                pipewire.close()
        if only in (None, "room"):
            pipewire = PipeWire(args.pipewire, args.wireplumber, args.dbus_daemon, Path(temporary) / "room")
            try:
                (pipewire.config / "pipewire/pipewire.conf.d/40-room.conf").write_text(ROOM)
                pipewire.up(expected=("speakers", "microphone", "room.capture", "room.mic"))
                check_room_echo(binary, pipewire, args.output)
            finally:
                pipewire.close()
        if only in (None, "rnnoise"):
            pipewire = PipeWire(args.pipewire, args.wireplumber, args.dbus_daemon, Path(temporary) / "rnnoise")
            try:
                pipewire.enable_echo_cancel()
                pipewire.enable_rnnoise(args.rnnoise_ladspa)
                pipewire.up(expected=("speakers", "microphone", "echo-cancel-source", "echo-cancel-sink",
                                      "effect_output.rnnoise"))
                check_rnnoise_chain(binary, pipewire)
            finally:
                pipewire.close()


if __name__ == "__main__":
    main()
