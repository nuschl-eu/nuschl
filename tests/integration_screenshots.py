#!/usr/bin/env python3
"""The music bot's integration card in the desktop, end to end.

A private loopback `nuschld`, a scripted bot that publishes a
`nuschl/kit/1` surface, and the desktop in a private headless GNOME Shell
(never the user's session). The card should sit above the voice status and
take its height from the channel list; hiding it leaves a row that brings it
back.

    NUSCHL_WL_TMP=/tmp/... python3 tests/integration_screenshots.py <binaries> --output DIR

Each scene is a screenshot to look at. `--dismiss` also opens the integrations
picker and hides and shows a card through it; `--stack` adds a second bot's card
and presses the label-only control it publishes. The page control is pressed
too; no bot answers with a link, so no browser opens on the host.
"""
import argparse
import json
import os
import signal
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gnome_session import GnomeSession  # noqa: E402
from local_nuschld import Nuschld, eventually, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402

CHANNELS = [(1, 0, "Lobby"), (2, 1, "Music")]
LOBBY = 1
MUSIC = 2
# Window-relative points: the card's close button, its transport row (the
# client-owned listen control, then play/pause and next), the picker's trigger
# at the right of the voice card's heading, and the picker's first row (the
# music bot's card).
DISMISS = (322, 436)
LISTEN = (120, 549)
PAUSE = (156, 549)
BURGER = (314, 654)
# The page control at the right of the card's transport row.
WEB = (320, 551)
PICKER_ROW = (410, 615)
# The radio card's label-only control, to the right of its play button.
RADIO_OPEN = (196, 594)


def varint(value):
    out = bytearray()
    while value >= 0x80:
        out.append((value & 0x7f) | 0x80)
        value >>= 7
    out.append(value)
    return bytes(out)


def text_node(style, body):
    body = body.encode()
    return bytes([1, style]) + len(body).to_bytes(2, "big") + body


def progress_node(position_ms, duration_ms, advancing):
    return bytes([2]) + varint(position_ms) + varint(duration_ms) + bytes([int(advancing)])


def volume_node():
    return bytes([4])


def buttons_node(items):
    out = bytearray([3, len(items)])
    for control, icon, enabled, label in items:
        label = label.encode()
        out += bytes([control, icon, int(enabled), len(label)]) + label
    return bytes(out)


def radio_surface(revision=1):
    title = text_node(0, "Nightwave Radio")
    subtitle = text_node(1, "Lo-fi beats, all night")
    status = text_node(2, "Live · 12 listening")
    # A control with a label and no icon, after the two the client draws.
    buttons = buttons_node([(0, 5, True, "Listen"), (1, 1, True, "Play"), (3, 0, True, "Open")])
    nodes = [title, subtitle, status, buttons]
    return bytes([1]) + varint(2) + varint(revision) + bytes([len(nodes)]) + b"".join(nodes)


def paused_surface(revision=2):
    title = text_node(0, "Midnight City")
    subtitle = text_node(1, "M83 · Hurry Up, We're Dreaming")
    status = text_node(2, "Paused · 2 listening")
    progress = progress_node(61_000, 245_000, False)
    buttons = buttons_node([(0, 5, True, "Listen"), (1, 1, True, "Play"), (2, 3, False, "Next"),
                            (3, 6, True, "Web")])
    nodes = [title, subtitle, status, progress, buttons, volume_node()]
    return bytes([1]) + varint(1) + varint(revision) + bytes([len(nodes)]) + b"".join(nodes)


def surface(revision=1):
    title = text_node(0, "Midnight City")
    subtitle = text_node(1, "M83 · Hurry Up, We're Dreaming")
    status = text_node(2, "Playing · 2 listening")
    progress = progress_node(61_000, 245_000, True)
    buttons = buttons_node([(0, 5, True, "Listen"), (1, 2, True, "Pause"), (2, 3, True, "Next"),
                            (3, 6, True, "Web")])
    nodes = [title, subtitle, status, progress, buttons, volume_node()]
    return bytes([1]) + varint(1) + varint(revision) + bytes([len(nodes)]) + b"".join(nodes)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--size", default="1280x800")
    parser.add_argument("--theme", choices=["system", "light", "dark"], default="dark")
    parser.add_argument("--dismiss", action="store_true", help="also hide and show through the picker")
    parser.add_argument("--stack", action="store_true", help="also show a second bot's card")
    parser.add_argument("--pulse-bin", type=Path, help="a private PulseAudio bin directory")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    width, height = map(int, args.size.split("x"))
    with tempfile.TemporaryDirectory(prefix="nuschl-integration-screens-") as temporary:
        root = Path(temporary)
        port = free_port()
        (root / "server").mkdir()
        server = Nuschld(args.binaries, root / "server", port)
        # Nuschld.seed uses the module's fixture; write these channels directly.
        server.write_config(message_limit=100, message_burst=100)
        server.start()
        from seed_server import seed
        seed(args.binaries, "127.0.0.1", port, server.password, CHANNELS, root / "seed")
        shell = GnomeSession(args.gnome_shell, width + 160, height + 100,
                             log=(args.output / "gnome-shell.log").open("w"))
        app = None
        bot = None
        radio = None
        pulse = None
        try:
            shell.start()
            if args.pulse_bin:
                socket_dir = shell.runtime / "pulse"
                socket_dir.mkdir(mode=0o700, exist_ok=True)
                script = shell.runtime / "default.pa"
                script.write_text(
                    f"load-module module-native-protocol-unix socket={socket_dir / 'native'} auth-anonymous=1\n"
                    "load-module module-null-sink sink_name=speakers sink_properties=device.description=Speakers\n"
                    "load-module module-null-sink sink_name=microphone sink_properties=device.description=Mic\n"
                    "set-default-sink speakers\nset-default-source microphone.monitor\n")
                pulse = subprocess.Popen(
                    [str(args.pulse_bin / "pulseaudio"), "--daemonize=no", "--use-pid-file=no",
                     "--exit-idle-time=-1", "-n", "--file=" + str(script)],
                    env=shell.environment() | {"PULSE_RUNTIME_PATH": str(socket_dir),
                                               "PULSE_STATE_PATH": str(shell.runtime / "pulse-state")},
                    stdout=(args.output / "pulse.log").open("w"), stderr=subprocess.STDOUT,
                    start_new_session=True)
                eventually(lambda: (socket_dir / "native").exists(), "private PulseAudio did not start")
            pin = certificate_pin(args.binaries, "127.0.0.1", port, root / "probe")
            profile = root / "profile"
            (profile / "config").mkdir(parents=True)
            bookmark = {"id": str(uuid.uuid4()), "name": "Nuschl test server", "host": "127.0.0.1",
                        "port": port, "username": "Andi", "certificate_sha256": pin}
            (profile / "config/servers.json").write_text(json.dumps([bookmark]))
            (profile / "config/settings.json").write_text(json.dumps({"appearance": {"theme": args.theme}}))
            log = (args.output / "desktop.log").open("w")
            app_env = shell.app_environment() | {"RUST_LOG": "nuschl=debug,nuschl_client=trace,nuschl_session=trace,nuschl_protocol=debug"}
            if args.pulse_bin:
                app_env["NUSCHL_AUDIO_BACKEND"] = "pulse"
                app_env["PULSE_SERVER"] = "unix:" + str(shell.runtime / "pulse" / "native")
            app = subprocess.Popen([str(args.binaries / "nuschl"), "--profile-dir", str(profile),
                                    f"mumble://Andi@127.0.0.1:{port}/Lobby/Music"],
                                   env=app_env,
                                   stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            eventually(lambda: app.poll() is None and shell.window("Nuschl"), "Nuschl window did not appear")

            def placed():
                shell.place("Nuschl", 40, 40, width, height)
                time.sleep(0.3)
                window = shell.window("Nuschl")
                return window["width"] == width and window["height"] == height

            eventually(placed, "Nuschl window could not be placed")

            def shot(name):
                window = shell.window("Nuschl")
                path = shell.screenshot(args.output / f"{name}.png",
                                        crop=(window["x"] - 12, window["y"] - 12,
                                              window["width"] + 24, window["height"] + 24))
                print(path, flush=True)

            def at(x, y):
                window = shell.window("Nuschl")
                return window["x"] - 12 + x, window["y"] - 12 + y

            def park():
                # A pointer left on a control keeps its tooltip in the picture.
                shell.move(*at(700, 400))

            time.sleep(25.0)
            shot("player-00-start")

            # The bot: a registered client in the music channel, publishing the card.
            bot = Client(args.binaries, "127.0.0.1", port, pin, "Music", root / "bot")
            # Receiving plugin data is opt-in, as for stock clients.
            bot.send(kind="plugin_accept", accept=True)
            bot.send(kind="join", channel=MUSIC)
            print("users seen by the bot:", {s: u["name"] for s, u in bot.users.items()}, flush=True)
            andi = next(user for user in bot.users.values() if user["name"] == "Andi")
            print("Andi in channel", andi["channel"], flush=True)
            # The server-history banner is not part of the player scene.
            shell.click(*at(1221, 134))
            time.sleep(0.5)
            time.sleep(1.0)
            bot.send(kind="plugin_data", request=1, receivers=[[andi["session"], andi["instance"]]],
                     data_id="nuschl/kit/1", data=list(surface()))
            print("admission:", bot.wait(lambda e: e["kind"] == "plugin_data_admission" and e["request"] == 1, timeout=10), flush=True)
            time.sleep(1.5)

            shot("player-01-card")

            # The listen control belongs to the client: pressing it asks the
            # server to listen to the sender's channel and never reaches the bot.
            shell.click(*at(*LISTEN))
            time.sleep(2.0)
            assert not [event for event in bot.events
                        if event["kind"] == "plugin_data"
                        and event["data_id"] == "nuschl/kit-action/1"], \
                "the listen control sent a press to the bot"
            park()
            time.sleep(0.5)
            shot("player-01a-listen")

            # The bot moves away. The card stays where it was and its listen
            # control turns off: one press asks the server to listen into the
            # bot's channel, and the bot hears nothing of it.
            bot.send(kind="join", channel=LOBBY)
            time.sleep(2.0)
            park()
            shot("player-01c-away")
            shell.click(*at(*LISTEN))
            time.sleep(2.0)
            assert not [event for event in bot.events
                        if event["kind"] == "plugin_data"
                        and event["data_id"] == "nuschl/kit-action/1"], \
                "the listen control sent a press to the bot"
            park()
            time.sleep(1.0)
            shot("player-01d-listening")
            # Pressing it again stops the listen; once the bot is back in the
            # user's channel the control is on without any press.
            shell.click(*at(*LISTEN))
            time.sleep(1.0)
            bot.send(kind="join", channel=MUSIC)
            time.sleep(2.0)

            # A press goes back to the bot that drew the card, naming surface,
            # revision and control.
            shell.click(*at(*PAUSE))
            action = bot.wait(
                lambda e: e["kind"] == "plugin_data"
                and e["data_id"] == "nuschl/kit-action/1",
                timeout=10)
            print("action:", action, flush=True)
            assert bytes.fromhex(action["data"])[:4] == bytes([1]) + varint(1) + varint(1) + bytes([1]), \
                f"unexpected action {action['data']}"
            assert len([event for event in bot.events if event["kind"] == "plugin_data"
                        and event["data_id"] == "nuschl/kit-action/1"]) == 1, \
                "a control reached the bot twice"
            # The page control reaches the bot, because the link it answers with
            # carries the user's own token. This bot answers nothing, so no
            # browser opens on the host.
            shell.click(*at(*WEB))
            page = bot.wait(
                lambda e: e["kind"] == "plugin_data"
                and e["data_id"] == "nuschl/kit-action/1",
                timeout=10)
            assert bytes.fromhex(page["data"])[:4] == bytes([1]) + varint(1) + varint(1) + bytes([3]), \
                f"unexpected action {page['data']}"
            park()
            bot.send(kind="plugin_data", request=2, receivers=[[andi["session"], andi["instance"]]],
                     data_id="nuschl/kit/1", data=list(paused_surface()))
            bot.wait(lambda e: e["kind"] == "plugin_data_admission" and e["request"] == 2, timeout=10)
            park()
            time.sleep(1.0)
            shot("player-01b-paused")
            if args.dismiss:
                # The picker in the voice card's header hides and shows cards,
                # and stays open across both.
                shell.click(*at(*BURGER))
                park()
                time.sleep(1.0)
                shot("player-02-picker")
                shell.click(*at(*PICKER_ROW))
                park()
                time.sleep(1.0)
                shot("player-03-hidden")
                shell.click(*at(*PICKER_ROW))
                park()
                time.sleep(1.0)
                shot("player-04-restored")
                shell.press("Escape")
                time.sleep(0.5)
            if args.stack:
                # A second bot's card stacks above the voice status too.
                radio = Client(args.binaries, "127.0.0.1", port, pin, "Radio", root / "radio")
                # Presses come back to the bot as plugin data, which is opt-in.
                radio.send(kind="plugin_accept", accept=True)
                radio.send(kind="join", channel=MUSIC)
                listener = next(user for user in radio.users.values() if user["name"] == "Andi")
                radio.send(kind="plugin_data", request=1,
                           receivers=[[listener["session"], listener["instance"]]],
                           data_id="nuschl/kit/1", data=list(radio_surface()))
                print("radio admission:",
                      radio.wait(lambda e: e["kind"] == "plugin_data_admission" and e["request"] == 1,
                                 timeout=10), flush=True)
                park()
                time.sleep(1.5)
                shot("player-05-stack")
                # A control with no icon is a real control: it presses, and the
                # press names the second bot's surface.
                shell.click(*at(*RADIO_OPEN))
                open_action = radio.wait(
                    lambda event: event["kind"] == "plugin_data"
                    and event["data_id"] == "nuschl/kit-action/1",
                    timeout=10)
                print("radio action:", open_action, flush=True)
                assert bytes.fromhex(open_action["data"])[:4] == \
                    bytes([1]) + varint(2) + varint(1) + bytes([3]), \
                    f"unexpected action {open_action['data']}"
        finally:
            if pulse and pulse.poll() is None:
                os.killpg(pulse.pid, signal.SIGTERM)
            if radio:
                radio.close()
            if bot:
                bot.close()
            if app and app.poll() is None:
                app.terminate()
                try:
                    app.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    app.kill()
            shell.stop()
            server.stop()
            if server.log.exists():
                import shutil
                shutil.copy(server.log, args.output / "nuschld.log")


if __name__ == "__main__":
    main()
