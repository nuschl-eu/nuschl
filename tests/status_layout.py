#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Status lines keep the layout still and their text whole.

The README's scene (tests/readme_screenshot.py: a private nuschld on
127.0.0.1, scripted people in #Development) with the desktop in a private
headless GNOME Shell on the real GPU, a private accessibility bus and a
private PulseAudio whose microphone is a sine tone (never the host's audio,
profile or session). The voice goes through muted, unmuted (push to talk,
idle), speaking (continuous) and muted again, while Lena types and stops.
In each state, at the README's window size (1078 x 782) and at the
desktop's minimum (860 x 520), checks over AT-SPI that:

- the header, the voice card, the composer, the timeline and the controls
  in them keep their extents;
- no label in the window's chrome (outside the timeline and the channel
  tree) is cut: its pixels show no ellipsis the text does not have, and a
  text ending in an ellipsis of its own ends well inside its box;
- the sidebar shows "Connected · N ms", with the transport in its
  accessible description, and the keyboard hint is described in full.

    NUSCHL_WL_TMP=/tmp/... python3 tests/status_layout.py <binaries> --at-spi <at-spi2-core> \\
        --pulse-bin <pulseaudio>/bin --output DIR

Needs gnome-shell, dbus-run-session, gdbus, busctl and ImageMagick's `magick`.
"""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from accessibility import items, start_registry  # noqa: E402
from font_geometry import ellipsis  # noqa: E402
from font_rendering import gray, mode  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from layout_regression import inside  # noqa: E402
from local_nuschld import eventually, free_port  # noqa: E402
from mumble_client import certificate_pin  # noqa: E402
from readme_screenshot import CHANNELS, DEVELOPMENT, PEOPLE, SERVER, Person, Server, conversation, register  # noqa: E402
from seed_server import seed  # noqa: E402

SIZES = [(1078, 782), (860, 520)]
PLACE = f"{SERVER} #Development"
# Controls whose extents stay put: (key, role or None, name, prefix match).
STILL = [
    ("header", None, "Conversation controls", False),
    ("heading", "label", "Development", False),
    ("timeline", "list", "Messages in #Development", False),
    ("composer details", None, "Message details and controls", False),
    ("composer", "entry", "Message", True),
    ("send", "push button", "Send", False),
    ("voice destinations", None, "Voice destinations", False),
    ("voice heading", "label", "Voice connected", False),
    ("hang up", "push button", f"Disconnect from {SERVER} (voice and chat)", False),
    ("place", "push button", f"Go to {PLACE}", False),
    ("audio status", "status bar", f"Audio status for {SERVER}: ", True),
    ("connection", "label", "Connected", True),
]
WINDOW = ("JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window)"
          ".find(w=>(w.get_title()||'').startsWith('Nuschl'));const b=w.get_buffer_rect();"
          "return [b.x,b.y,b.width,b.height];})())")


def description(node):
    return node.bus("get-property", node.destination, node.path, "org.a11y.atspi.Accessible", "Description")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--at-spi", type=Path, required=True, help="at-spi2-core package (bus launcher, registry)")
    parser.add_argument("--pulse-bin", type=Path, required=True, help="pulseaudio bin directory")
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--output", type=Path, default=Path(tempfile.gettempdir()) / "nuschl-status-layout")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    processes, people = [], []

    def spawn(command, env, log):
        process = subprocess.Popen(command, env=env, stdout=(args.output / log).open("w"),
                                   stderr=subprocess.STDOUT, start_new_session=True)
        processes.append(process)
        return process

    with tempfile.TemporaryDirectory(prefix="nuschl-status-") as temporary:
        root = Path(temporary)
        port = free_port()
        (root / "server").mkdir()
        server = Server(args.binaries, root / "server", port)
        server.write_config(message_limit=100, message_burst=100)
        server.start()
        shell = GnomeSession(args.gnome_shell, 1400, 1000, log=(args.output / "gnome-shell.log").open("w"))
        typing = threading.Event()
        nuschl = None
        try:
            pin = certificate_pin(args.binaries, "127.0.0.1", port, root / "probe")
            seed(args.binaries, "127.0.0.1", port, server.password, CHANNELS, root / "seed")
            profile = root / "profile"
            (profile / "config").mkdir(parents=True)
            register(args.binaries, port, pin, "Andi", profile)
            for name in ("Ada", "Lena"):
                register(args.binaries, port, pin, name, root / name.lower())
            people = [Person(args.binaries, port, pin, root, *person) for person in PEOPLE
                      if person[0] in ("Ada", "Lena")]
            ada, lena = people
            shell.start()
            env = shell.app_environment()
            # A screen reader's session has no NO_AT_BRIDGE (NixOS sets it).
            env.pop("NO_AT_BRIDGE", None)
            registry = start_registry(args.at_spi, env, spawn)
            # A tone played into a null sink: its monitor is the microphone.
            pulse = shell.runtime / "pulse"
            pulse.mkdir(mode=0o700, exist_ok=True)
            script = root / "default.pa"
            script.write_text(f"load-module module-native-protocol-unix socket={pulse / 'native'} auth-anonymous=1\n"
                              "load-module module-null-sink sink_name=speakers\n"
                              "load-module module-null-sink sink_name=microphone\n"
                              "load-module module-sine sink=microphone frequency=220\n"
                              "set-default-sink speakers\nset-default-source microphone.monitor\n")
            spawn([str(args.pulse_bin / "pulseaudio"), "--daemonize=no", "--use-pid-file=no", "--exit-idle-time=-1",
                   "-n", f"--file={script}"],
                  env | {"PULSE_RUNTIME_PATH": str(pulse), "PULSE_STATE_PATH": str(root / "pulse-state")}, "pulse.log")
            eventually((pulse / "native").exists, "the private audio daemon did not start")

            bookmark = {"id": str(uuid.uuid4()), "name": SERVER, "host": "127.0.0.1", "port": port,
                        "username": "Andi", "certificate_sha256": pin, "auto_connect": False}
            (profile / "config/servers.json").write_text(json.dumps([bookmark]))
            (profile / "config/settings.json").write_text(json.dumps(
                {"appearance": {"theme": "dark"}, "audio": {"transmit": "push_to_talk"}}))
            app = spawn([str(args.binaries / "nuschl"), "--profile-dir", str(profile),
                         f"mumble://Andi@127.0.0.1:{port}/Development"],
                        env | {"RUST_LOG": "nuschl=info", "NUSCHL_AUDIO_BACKEND": "pulse"}, "desktop.log")

            def application():
                assert app.poll() is None, (args.output / "desktop.log").read_text()[-3000:]
                return next((node for node in registry.children() if node.name.lower() == "nuschl"), None)
            nuschl = eventually(application, "Nuschl did not register with the accessibility registry")

            def named(role, name, prefix=False, tree=None):
                return next((node for node, r, n, _ in tree or items(nuschl)
                             if (not role or r == role) and (n.startswith(name) if prefix else n == name)), None)

            def find(name, role="push button", prefix=False):
                return eventually(lambda: named(role, name, prefix), f"{name!r} absent")

            def voice(status):
                eventually(lambda: named(None, f"{PLACE}: {status}"), f"the voice card did not say {status}")

            def keep_typing():
                while typing.is_set():
                    lena.client.send(kind="typing", conversation=conversation(lena.client, DEVELOPMENT))
                    time.sleep(2.0)

            def setting(mode, value):
                find("Settings").press()
                find("Settings", "dialog")
                control = find(mode, role="radio button")
                control.call("Component", "GrabFocus")
                control.press()
                eventually(lambda: json.loads((profile / "config/settings.json").read_text())["audio"]["transmit"]
                           == value, f"{mode} was not saved")
                shell.key("Escape")
                eventually(lambda: named("dialog", "Settings") is None, "Settings stayed open")

            def capture(size, step):
                """The still controls' extents and every chrome label's verdict."""
                time.sleep(1.0)
                buffer = json.loads(shell.eval(WINDOW))
                shot = args.output / f"{size[0]}x{size[1]}-{step}.png"
                shell.screenshot(shot.resolve())
                tree = items(nuschl)
                extents = {}
                for key, role, name, prefix in STILL:
                    node = named(role, name, prefix, tree)
                    assert node, f"{step}: {key} ({name!r}) absent"
                    extents[key] = node.extents()
                card = max((node for node, role, name, _ in tree
                            if role in ("grouping", "section", "panel") and name.startswith(f"{PLACE}: ")),
                           key=lambda node: node.extents()[3])
                extents["voice card"] = card.extents()
                status = [node for node, role, name, _ in tree if role == "status bar"
                          and inside(node.extents(), extents["composer details"])]
                assert len(status) == 1, f"{step}: typing status {status}"
                extents["typing status"] = status[0].extents()
                window = [0, 0, buffer[2], buffer[3]]
                skip = [extents["timeline"], named("tree", "Channels", tree=tree).extents()]
                labels = []
                for node, role, name, _ in tree:
                    if role != "label" or not name.strip():
                        continue
                    box = node.extents()
                    if not inside(box, window) or any(inside(box, area) for area in skip):
                        continue
                    x, y, w, h = box
                    pixels = gray(shot, (buffer[0] + x, buffer[1] + y, w, h))
                    background = mode(pixels)
                    inked = [column for column in range(w) if any(row[column] - background > 40 for row in pixels)]
                    if name.endswith(("…", "...")):
                        cut = not inked or w - 1 - inked[-1] < 8
                    else:
                        cut = ellipsis(pixels, background, w)
                    labels.append({"name": name, "box": box, "ink_right": inked[-1] if inked else None, "cut": cut})
                # A loaded host can make playback stutter: the card's audio
                # issue button then shares the bitrate line's width.
                issue = any(role == "push button" and name.endswith("; open audio settings") for _, role, name, _ in tree)
                return {"step": step, "size": list(size), "extents": extents, "labels": labels, "issue": issue,
                        "screenshot": shot.name}

            records = []
            failures = []

            def record(size, step):
                entry = capture(size, step)
                records.append(entry)
                (args.output / "status-layout.json").write_text(json.dumps(records, indent=1, ensure_ascii=False) + "\n")
                first = next(r for r in records if r["size"] == entry["size"])
                for key, box in entry["extents"].items():
                    if box != first["extents"][key]:
                        failures.append(f"{size} {step}: {key} moved from {first['extents'][key]} to {box}")
                for label in entry["labels"]:
                    # The hint shows only while it fits; out of sight, it is not cut.
                    hidden = label["name"].startswith(("New line: ", "Send: ")) \
                        and not inside(label["box"], entry["extents"]["composer details"])
                    narrowed = entry["issue"] and label["name"].endswith(" kb/s · server limit")
                    if label["cut"] and not hidden and not narrowed:
                        failures.append(f"{size} {step}: {label['name']!r} is cut in {entry['screenshot']}")
                print(f"{size[0]}x{size[1]} {step}: {len(entry['labels'])} labels checked", flush=True)

            def placed(width, height):
                shell.place("Nuschl", 40, 40, width, height)
                time.sleep(0.3)
                window = shell.window("Nuschl")
                return window["width"] == width and window["height"] == height

            eventually(lambda: shell.window("Nuschl"), "Nuschl window did not appear")
            ada.client.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Andi"
                            and e["user"]["channel"] == DEVELOPMENT, timeout=60)
            ada.say("Morning! Who has time to test the release candidate tonight?")
            find("Unmute")
            voice("Muted")
            # Park the pointer away from the window, so no hover state or tooltip shows.
            shell.move(1380, 980)
            for size in SIZES:
                eventually(lambda: placed(*size), f"Nuschl window could not be placed at {size}")
                # The round trip needs a few pings before the sidebar shows it.
                connection = eventually(lambda: (lambda node: node if node and re.fullmatch(r"Connected · \d+ ms", node.name)
                                                 else None)(named("label", "Connected", True)),
                                        "the sidebar does not show the round trip")
                assert re.fullmatch(r"Connected · (UDP|TCP) · \d+ ms", description(connection)), description(connection)
                hint = find("New line: Shift+Enter", "label")
                assert description(hint) == "Enter to send · Shift+Enter for a new line", description(hint)
                record(size, "muted")
                find("Unmute").press()
                voice("Unmuted")
                record(size, "unmuted")
                setting("Continuous", "continuous")
                voice("Speaking")
                record(size, "speaking")
                typing.set()
                typer = threading.Thread(target=keep_typing, daemon=True)
                typer.start()
                find("Lena is typing…", "status bar")
                record(size, "speaking-typing")
                find("Mute").press()
                voice("Muted")
                record(size, "muted-typing")
                typing.clear()
                typer.join(timeout=10)
                eventually(lambda: named("label", "Message #Development"), "Lena's typing did not end", timeout=30)
                record(size, "muted-idle")
                setting("Push to talk", "push_to_talk")
            assert not failures, "\n".join(failures)
            print("Status lines kept the layout still and their text whole.", flush=True)
        except BaseException:
            if nuschl:
                (args.output / "failure-tree.json").write_text(json.dumps(
                    [[role, name] for _, role, name, _ in items(nuschl)], indent=0, ensure_ascii=False) + "\n")
                shell.screenshot((args.output / "failure.png").resolve())
            raise
        finally:
            typing.clear()
            for person in people:
                person.close()
            for process in reversed(processes):
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    try:
                        process.wait(timeout=8)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
            shell.stop()
            server.stop()


if __name__ == "__main__":
    main()
