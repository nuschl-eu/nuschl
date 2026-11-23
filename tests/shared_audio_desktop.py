#!/usr/bin/env python3
"""Shared audio between two desktops in a real native call (local-only).

Andi and Bea run in a private headless GNOME Shell with the real ScreenCast
portal, and a private PipeWire session serves its runtime directory. A loopback
Murmur carries the invitation; a loopback SFU behind a TLS terminator carries the
call, with a test certificate the desktops trust (built with
`--features nuschl/call-test-roots`). Andi starts a call by sharing the screen;
Bea joins through the invitation. Bea's tile of Andi's screen shows "No shared
audio". Andi turns on Share audio while an application plays a tone into its
own sink. Bea's tile then shows "Mute shared audio", and the tone plays on Bea's
speakers, which carry nothing else. Muting silences it there while the stream
continues: unmuting brings it back. The control stays on Andi's tile in every
view: featured alone, in the tiled view beside Bea's own screen, and in full
screen, also after the call controls fade. Screenshots of Bea's window land in
the output directory.

    python3 tests/shared_audio_desktop.py <binaries> --murmur <mumble-server> \\
        --at-spi <at-spi2-core> --gnome-shell <gnome-shell> --portals <xdg-desktop-portal> \\
        <xdg-desktop-portal-gnome> --pipewire <pipewire> --wireplumber <wireplumber> \\
        --dbus-daemon <dbus-daemon> --openssl <openssl> --opus-library <libopus.so> --output <directory>
"""
import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
import accessibility  # noqa: E402
from accessibility import snapshot, start_registry  # noqa: E402
from calls_desktop import CallTest  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from mumble_client import Client  # noqa: E402
from pipewire_recovery import PipeWire  # noqa: E402
from share_audio import level, tone_wav  # noqa: E402

accessibility.ROLES.update({51: "slider"})
TONE = 550
SPEAKER = ("No shared audio", "Mute shared audio", "Unmute shared audio")
CALL_IP = "127.0.0.2"
# A sink for the application, so that the speakers carry only what Nuschl plays.
APPLICATION_SINK = """context.objects = [
  { factory = adapter
    args = { factory.name = support.null-audio-sink node.name = "application-sink" node.description = "Application"
             media.class = "Audio/Sink" audio.position = [ FL FR ] object.linger = true } }
]
"""


def terminate(listen, upstream, certificate, key):
    """TLS for the SFU's HTTP listener, as its host does in production."""
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(certificate, key)
    loop = asyncio.new_event_loop()

    async def pipe(reader, writer):
        try:
            while data := await reader.read(65536):
                writer.write(data)
                await writer.drain()
        except (ConnectionError, OSError):
            pass
        finally:
            writer.close()

    async def connection(reader, writer):
        try:
            up_reader, up_writer = await asyncio.open_connection(*upstream)
        except OSError:
            writer.close()
            return
        await asyncio.gather(pipe(reader, up_writer), pipe(up_reader, writer))

    server = loop.run_until_complete(asyncio.start_server(connection, sock=listen, ssl=context))
    threading.Thread(target=loop.run_forever, daemon=True).start()
    return server


class SharedAudioTest(CallTest):
    def start_sfu(self):
        directory = self.root / "sfu"
        directory.mkdir()
        key, certificate = directory / "key.pem", directory / "cert.pem"
        subprocess.run([str(self.args.openssl / "bin/openssl"), "req", "-x509", "-newkey", "ec", "-pkeyopt",
                        "ec_paramgen_curve:P-256", "-nodes", "-days", "1", "-subj", "/CN=call.test",
                        "-addext", f"subjectAltName=IP:{CALL_IP}", "-addext", "basicConstraints=critical,CA:FALSE",
                        "-keyout", str(key), "-out", str(certificate)], check=True, capture_output=True)
        config = directory / "sfu.toml"
        config.write_text(f'listen = "{CALL_IP}:0"\nmedia_listen = "{CALL_IP}:3479"\n'
                          f'media_addresses = ["{CALL_IP}"]\nmax_peers_per_room = 6\n')
        process = subprocess.Popen([str(self.args.binaries / "nuschl-stream-sfu"), "--config", str(config)],
                                   stdout=subprocess.PIPE, stderr=(self.args.output / "sfu.log").open("w"),
                                   start_new_session=True)
        self.processes.append(process)
        ready = process.stdout.readline().decode().split()
        host, port = ready[2].rsplit(":", 1)
        listen = socket.create_server((CALL_IP, 0))
        self.tls = terminate(listen, (host, int(port)), certificate, key)
        self.call_host = f"{CALL_IP}:{listen.getsockname()[1]}"
        os.environ["NUSCHL_CALL_TEST_CA"] = str(certificate)

    def find_any(self, app, role, name):
        return next((node for _, node, node_role, node_name in snapshot(app)
                     if node_role == role and node_name == name), None)

    def grant_screen(self, registry):
        """Choose the virtual monitor in GNOME's screen share dialog."""
        def dialog():
            for app in registry.children():
                if "portal" in app.name.lower():
                    entries = snapshot(app)
                    if any(role == "push button" and name == "Share" for _, _, role, name in entries):
                        return app, entries
            return None

        try:
            app, entries = eventually(dialog, "GNOME did not ask which screen to share", timeout=60)
        except AssertionError:
            (self.args.output / "portal-tree.txt").write_text("\n".join(
                f"{app.name}: {'  ' * depth}{role}: {name}"
                for app in registry.children() if "nuschl" not in app.name.lower()
                for depth, _, role, name in snapshot(app)))
            raise
        (self.args.output / "portal-tree.txt").write_text(
            "\n".join(f"{'  ' * depth}{role}: {name}" for depth, _, role, name in entries))
        monitor = next((node for _, node, role, name in entries
                        if role in ("list item", "toggle button", "radio button", "check box")
                        and name), None)
        assert monitor, "the screen share dialog lists no monitor"
        share = next(node for _, node, role, name in entries if role == "push button" and name == "Share")
        left, top = json.loads(self.shell.eval(
            "const w = global.get_window_actors().map(a => a.meta_window)"
            ".find(w => w.get_title() === 'Share Screen'); const r = w.get_buffer_rect();"
            " JSON.stringify([r.x, r.y])"))
        x, y, width, height = monitor.extents()
        self.shell.click(left + x + width // 2, top + y + height // 2)
        time.sleep(0.5)
        self.shell.screenshot(self.args.output / "portal.png")
        share.press()

    def tile_control(self, app, name):
        return eventually(lambda: self.find_any(app, "push button", name),
                          f"Bea's tile of Andi's screen did not show {name!r}", timeout=60)

    def tile_with_control(self, app, view):
        """Andi's screen tile and its speaker, which lies inside the tile."""
        def found():
            entries = snapshot(app)
            tiles = [node for _, node, role, name in entries if role == "image" and name.startswith("Screen · ")]
            speakers = [node for _, node, role, name in entries if role == "push button" and name in SPEAKER]
            return len(tiles) == 1 and len(speakers) == 1 and (tiles[0], speakers[0])

        tile, speaker = eventually(found, f"{view}: Andi's screen tile has no shared-audio control", timeout=60)
        tx, ty, tw, th = tile.extents()
        x, y, width, height = speaker.extents()
        assert width > 0 and height > 0 and tx <= x and ty <= y and x + width <= tx + tw and y + height <= ty + th, \
            f"{view}: the control {(x, y, width, height)} lies outside the tile {(tx, ty, tw, th)}"
        return speaker

    def window_shot(self, pid, name):
        window = json.loads(self.shell.eval(
            "const w = global.get_window_actors().map(a => a.meta_window)"
            f".find(w => w.get_pid() === {pid}); const r = w.get_frame_rect();"
            " JSON.stringify([r.x, r.y, r.width, r.height])"))
        self.shell.screenshot(self.args.output / name, crop=window)

    def speakers(self, pipewire, name):
        """The tone's level on Bea's speakers over the next second."""
        recorder = pipewire.record(self.args.output / f"{name}.wav")
        time.sleep(1.5)
        samples = [sample / 32768 for sample in recorder.stop()]
        return level(samples[len(samples) // 3:], TONE)

    def run(self):
        # The portal's dialog (GTK) must reach the accessibility bus too.
        for name in ("NO_AT_BRIDGE", "GTK_A11Y"):
            os.environ.pop(name, None)
        self.shell = GnomeSession(self.args.gnome_shell, 1760, 960, log=(self.args.output / "shell.log").open("w"),
                                  portals=self.args.portals)
        self.shell.start()
        pipewire = PipeWire(self.args.pipewire, self.args.wireplumber, self.args.dbus_daemon, self.root / "pipewire",
                            runtime=self.shell.runtime)
        self.pipewires = [pipewire]
        (pipewire.config / "pipewire/pipewire.conf.d/20-application-sink.conf").write_text(APPLICATION_SINK)
        pipewire.up(expected=("speakers", "microphone", "application-sink"))
        subprocess.run([str(pipewire.pipewire / "bin/pw-metadata"), "-n", "default", "0",
                        "default.configured.audio.sink", '{ "name": "speakers" }', "Spa:String:JSON"],
                       env=pipewire.env, check=True, capture_output=True, timeout=10)
        env = self.shell.app_environment()
        env.pop("NO_AT_BRIDGE", None)
        registry = start_registry(self.args.at_spi, env, self.spawn)
        self.start_sfu()
        directory = self.root / "murmur"
        directory.mkdir()
        self.murmur = Murmur(self.args.murmur, directory, free_port())
        self.murmurs.append(self.murmur)
        self.murmur.seed()
        self.murmur.start()
        der = ssl.PEM_cert_to_DER_cert(ssl.get_server_certificate(("127.0.0.1", self.murmur.port)))
        self.pin = hashlib.sha256(der).hexdigest()
        self.witness = self.peers["Witness"] = Client(self.args.binaries, "127.0.0.1", self.murmur.port,
                                                      self.pin, "Witness", self.root / "witness")
        self.witness.send(kind="join", channel=1)
        andi, _, andi_pid = self.desktop("Andi", registry, set())
        bea, _, bea_pid = self.desktop("Bea", registry, {andi.destination})
        for pid, x in ((andi_pid, 0), (bea_pid, 880)):
            self.shell.eval("const w = global.get_window_actors().map(a => a.meta_window)"
                            f".find(w => w.get_pid() === {pid}); if (w.is_maximized()) w.unmaximize();"
                            f" w.move_resize_frame(true, {x}, 32, 880, 928); 'ok'")

        self.press(andi, "Share screen")
        self.grant_screen(registry)
        self.control(andi, "Share audio")
        self.press(self.invitation(bea), "Join video call", subtree=True)
        self.tile_control(bea, "No shared audio")
        # The picture, for the screenshots (a debug decoder can take a while).
        try:
            eventually(lambda: not any(name == "Waiting for video" for _, _, _, name in snapshot(bea)),
                       "no picture yet", timeout=60)
        except AssertionError:
            print("Bea's tile shows no picture yet", flush=True)
        self.window_shot(bea_pid, "bea-screen-without-audio.png")
        assert self.speakers(pipewire, "before-audio") < 0.003

        application = subprocess.Popen(
            [str(pipewire.pipewire / "bin/pw-play"), "--target", "application-sink",
             str(tone_wav(self.root / "application.wav", TONE))],
            env=pipewire.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        self.processes.append(application)
        self.press(andi, "Share audio")
        self.control(andi, "Stop sharing audio")
        mute = self.tile_control(bea, "Mute shared audio")
        heard = eventually(lambda: (value := self.speakers(pipewire, "shared")) > 0.03 and value,
                           "Andi's shared audio did not play on Bea's speakers", timeout=60)
        print(f"Bea hears Andi's shared audio at {heard:.3f}.", flush=True)
        x, y, width, height = mute.extents()
        left, top = json.loads(self.shell.eval(
            "const w = global.get_window_actors().map(a => a.meta_window)"
            f".find(w => w.get_pid() === {bea_pid}); const r = w.get_buffer_rect(); JSON.stringify([r.x, r.y])"))
        self.shell.move(left + x + width // 2, top + y + height // 2)
        eventually(lambda: self.find_any(bea, "slider", "Shared audio volume"),
                   "hovering the speaker did not show the volume", timeout=10)
        time.sleep(1)
        self.window_shot(bea_pid, "bea-screen-with-audio.png")

        mute.press()
        self.tile_control(bea, "Unmute shared audio")
        time.sleep(0.5)
        muted = self.speakers(pipewire, "muted")
        assert muted < 0.003, f"muted shared audio still plays ({muted:.4f})"
        self.control(andi, "Stop sharing audio")
        self.tile_control(bea, "Unmute shared audio").press()
        self.tile_control(bea, "Mute shared audio")
        eventually(lambda: self.speakers(pipewire, "unmuted") > 0.03,
                   "the stream did not continue through the mute", timeout=20)
        print("Bea's control: No shared audio before, Mute after; muting silenced it locally while "
              "Andi's stream went on.", flush=True)

        # The same control in every view: featured alone above, then tiled
        # beside Bea's own screen, then full screen.
        self.tile_with_control(bea, "featured")
        self.press(bea, "Share screen")
        self.grant_screen(registry)
        self.control(bea, "Stop sharing screen")
        self.tile_with_control(bea, "featured beside Bea's own share")
        self.press(bea, "Video sources")
        eventually(lambda: self.find_any(bea, "menu item", "Tiled view") or self.find(bea, None, "Tiled view"),
                   "the video sources menu did not open").press()
        eventually(lambda: self.find_any(bea, "image", "Your screen"), "Bea's own screen did not join the tiles",
                   timeout=60)
        self.tile_with_control(bea, "tiled")
        time.sleep(1)
        self.window_shot(bea_pid, "bea-grid.png")
        self.press(bea, "Stop sharing screen")
        eventually(lambda: not self.find_any(bea, "image", "Your screen"), "Bea's screen stayed in the grid",
                   timeout=30)
        self.press(bea, "Full screen")
        eventually(lambda: json.loads(self.shell.eval(
            "JSON.stringify(global.get_window_actors().map(a => a.meta_window)"
            f".find(w => w.get_pid() === {bea_pid}).is_fullscreen())")), "Bea's call did not fill the screen",
                   timeout=15)
        self.tile_with_control(bea, "full screen")
        time.sleep(1)
        self.shell.screenshot(self.args.output / "bea-fullscreen.png")
        # The call controls fade after 3 s without input; the speaker stays.
        time.sleep(4)
        self.tile_with_control(bea, "full screen, idle")
        self.shell.screenshot(self.args.output / "bea-fullscreen-idle.png")
        self.press(bea, "Exit full screen")
        print("Bea's control shows on Andi's tile on the stage, in the grid and in full screen.", flush=True)

    def close(self):
        for pipewire in getattr(self, "pipewires", []):
            pipewire.close()
        super().close()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binaries", type=Path)
    for name in ("murmur", "at-spi", "gnome-shell", "pipewire", "wireplumber", "openssl", "opus-library", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--portals", type=Path, nargs="+", required=True)
    parser.add_argument("--dbus-daemon", default=shutil.which("dbus-daemon") or "dbus-daemon")
    parser.set_defaults(fullscreen=False)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nuschl-shared-", dir=os.environ.get("NUSCHL_SHORT_TMP")) as temporary:
        test = SharedAudioTest(args, Path(temporary))
        try:
            test.run()
        except Exception:
            if test.shell:
                test.shell.screenshot(args.output / "failure.png")
            raise
        finally:
            test.close()


if __name__ == "__main__":
    main()
