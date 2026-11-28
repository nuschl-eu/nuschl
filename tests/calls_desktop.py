#!/usr/bin/env python3
"""Native desktop call controls and invitations on a loopback Mumble server.

Andi and Bea run in a private headless GNOME Shell with synthetic PulseAudio.
A Mumble-protocol witness checks the public and private chat links. The call
host listens on loopback and holds TLS admission open, so the test exercises the
call UI and invitation lifecycle without granting camera or screen capture.
Set TMPDIR and NUSCHL_WL_TMP to a short disk-backed scratch path.
Run with --fullscreen to sample the light-theme video surface and controls with ffmpeg.

    python3 tests/calls_desktop.py <binaries> --murmur <mumble-server> \
        --at-spi <at-spi2-core> --pulse-bin <pulseaudio>/bin --opus-library <libopus.so> \
        --gnome-shell <gnome-shell> --output <directory>
"""
import argparse
import base64
import hashlib
import html
import hmac
import json
from pathlib import Path
import re
import shlex
import socket
import ssl
import subprocess
import sys
import tempfile
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from accessibility import items, start_registry  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from mumble_client import Client  # noqa: E402
from voice_servers import VoiceTest  # noqa: E402

AT_SPI_CHECK_MENU_ITEM = "role 8"
AT_SPI_RADIO_MENU_ITEM = "role 45"
AT_SPI_CHECKED = 1 << 4


class CallTest(VoiceTest):
    def desktop(self, name, registry, known, *, channel=1, path="Lobby"):
        profile = self.root / name
        (profile / "config").mkdir(parents=True)
        (profile / "config/servers.json").write_text(json.dumps([{
            "id": str(uuid.uuid4()), "name": "Calls", "host": "127.0.0.1", "port": self.murmur.port,
            "username": name, "certificate_sha256": self.pin,
        }]))
        (profile / "config/settings.json").write_text(json.dumps({
            "server_icons": False, "call_host": self.call_host,
            "appearance": {"theme": "light" if self.args.fullscreen else "system"},
        }))
        opened = self.root / f"{name}-opened.txt"
        bin_dir = self.root / f"{name}-bin"
        bin_dir.mkdir()
        launcher = bin_dir / "xdg-open"
        launcher.write_text(f"#!/bin/sh\nprintf '%s\\n' \"$1\" >> {shlex.quote(str(opened))}\n")
        launcher.chmod(0o755)
        env = self.shell.app_environment()
        env.pop("NO_AT_BRIDGE", None)
        env.pop("BROWSER", None)
        env["PATH"] = f"{bin_dir}:{env['PATH']}"
        env.update(NO_PROXY="*", no_proxy="*")
        process = self.spawn([str(self.args.binaries / "nuschl"), "--profile-dir", str(profile),
                              f"mumble://{name}@127.0.0.1:{self.murmur.port}/{path}"],
                             env | {"NUSCHL_AUDIO_BACKEND": "pulse"}, f"desktop-{name}.log")
        app = eventually(lambda: next((node for node in registry.children() if node.name.lower() == "nuschl"
                                       and node.destination not in known), None),
                         f"no accessibility tree for {name}", timeout=45)
        self.witness.wait(lambda event: event["kind"] == "user" and event["user"]["name"] == name
                          and event["user"]["channel"] == channel, timeout=45)
        return app, opened, process.pid

    @staticmethod
    def find(app, role, name, prefix=False, *, subtree=False):
        nodes = items(app, subtree=subtree)
        return next((node for node, node_role, node_name, _ in nodes if role in (None, node_role)
                     and (node_name.startswith(name) if prefix else node_name == name)), None)

    def direct(self, app, name):
        eventually(lambda: self.find(app, "tree item", name, prefix=True), f"no user {name!r}").press()
        eventually(lambda: any(role == "list" and label.startswith("Messages with") and label.endswith(name)
                               for _, role, label, _ in items(app)), f"no conversation with {name}")

    def control(self, app, name, shown=True, *, subtree=False):
        def matches():
            node = self.find(app, "push button", name, subtree=subtree)
            return node if shown else node is None

        return eventually(matches, f"expected {name!r} to be {'shown' if shown else 'hidden'}", timeout=15)

    def press(self, app, name, *, subtree=False):
        self.control(app, name, subtree=subtree).press()

    def click(self, control):
        x, y, width, height = control.extents()
        left, top = json.loads(self.shell.eval(
            "const bounds = global.display.focus_window.get_buffer_rect(); JSON.stringify([bounds.x, bounds.y])"))
        assert width > 0 and height > 0
        self.shell.click(left + x + width // 2, top + y + height // 2)

    def resize_desktop(self, pid, height):
        lookup = ("const window = global.get_window_actors().map(actor => actor.meta_window)"
                  f".find(window => window.get_pid() === {pid});")
        previous = int(self.shell.eval(
            lookup + "const rect = window.get_frame_rect();"
            f"window.move_resize_frame(true, rect.x, rect.y, rect.width, {height});"
            "window.activate(global.get_current_time()); String(rect.height)"))
        eventually(lambda: abs(int(self.shell.eval(lookup + "String(window.get_frame_rect().height)")) - height) <= 24,
                   "the fixture window did not resize", timeout=15)
        return previous

    def select_quality(self, app, current, choices, *, result):
        trigger = self.control(app, f"Quality · {current}")
        chat = self.timeline(app)
        chat_bounds = chat.extents()
        _, trigger_y, _, trigger_height = trigger.extents()
        trigger.press()
        menu = eventually(lambda: self.find(app, "menu", "Video quality"),
                           "video quality dropdown did not open")
        _, menu_y, _, menu_height = menu.extents()
        geometry = f"trigger at {trigger_y} with height {trigger_height}, menu at {menu_y} with height {menu_height}"
        # The dropdown prefers the space above the trigger when its measured
        # rows fit with the four-pixel gap and eight-pixel viewport margin.
        if menu_height + 12 <= trigger_y:
            assert menu_y + menu_height <= trigger_y, f"quality menu should open above: {geometry}"
        else:
            assert menu_y >= trigger_y + trigger_height, f"quality menu should fit below: {geometry}"
        assert chat.extents() == chat_bounds, "opening the quality menu moved the chat"
        for choice in choices:
            role = AT_SPI_CHECK_MENU_ITEM if choice == "Low latency" else AT_SPI_RADIO_MENU_ITEM
            item = eventually(lambda: self.find(app, role, choice),
                              f"quality menu item missing: {choice}")
            item.press()
            eventually(lambda: any(item_role == role and label == choice and state & AT_SPI_CHECKED
                                   for _, item_role, label, state in items(app)),
                       f"quality menu item did not become checked: {choice}")
            assert self.find(app, "menu", "Video quality"), f"selecting {choice} closed the menu"
        self.control(app, f"Quality · {result}")
        self.shell.key("Escape")
        eventually(lambda: not self.find(app, "menu", "Video quality"),
                   "Escape did not close the video quality dropdown")
        assert chat.extents() == chat_bounds, "selecting a quality moved the chat"

    def toggle_quality(self, app):
        def closed():
            names = {(role, label) for _, role, label, _ in items(app)}
            return ("push button", "Quality · Auto") in names and ("menu", "Video quality") not in names

        trigger = self.control(app, "Quality · Auto")
        chat = self.timeline(app)
        chat_bounds = chat.extents()
        self.click(trigger)
        eventually(lambda: self.find(app, "menu", "Video quality"), "video quality dropdown did not open", timeout=5)
        assert chat.extents() == chat_bounds, "opening the quality menu moved the chat with stats hidden"
        self.click(trigger)
        eventually(closed, "video quality dropdown did not close on a second click", timeout=5)
        assert chat.extents() == chat_bounds, "closing the quality menu moved the chat"

    def video_stats(self, app, show):
        self.press(app, "Video options")
        label = "Video stats" if show else "Hide video stats"
        eventually(lambda: self.find(app, "menu item", label), f"missing {label!r} menu item").press()
        eventually(lambda: not self.find(app, "menu", "Context menu"), "video options did not close")
        eventually(lambda: bool(self.find(app, None, "Video stats")) == show,
                   f"expected video stats to be {'shown' if show else 'hidden'}")
        if show:
            for field in ("Resolution", "Video payload bitrate", "Connection"):
                eventually(lambda: self.find(app, None, field), f"video stats lack {field!r}")

    @staticmethod
    def timeline(app):
        return next((node for node, role, label, _ in items(app)
                     if role == "list" and label.startswith(("Messages in #", "Messages with "))), None)

    def invitation(self, app, sender="Andi", *, ended=False, inline=True):
        status = "Call ended" if ended else "Video call"
        label = f"{status} · {sender} · {self.call_host}"

        def card():
            timeline = self.timeline(app)
            if timeline:
                candidate = self.find(timeline, "panel", label, subtree=True)
                if inline:
                    if candidate:
                        cards = [node.path for node, role, name, _ in items(app)
                                 if role == "panel" and name == label]
                        if cards == [candidate.path]:
                            return candidate
                else:
                    assert candidate is None, "a late invitation was added to chat history"
                    return self.find(app, "panel", label)
            return None

        try:
            return eventually(card, f"no {status.lower()} card for {sender}", timeout=15)
        except AssertionError:
            state = [(role, name) for _, role, name, _ in items(app) if role in {"panel", "list item"}]
            raise AssertionError(f"no {status.lower()} card for {sender}: {state}") from None

    def witness_link(self):
        marker = f"https://{self.call_host}/call#"
        event = self.witness.wait(lambda event: event["kind"] == "text" and not event["outgoing"]
                                  and marker in event["body"])
        body = html.unescape(event["body"])
        secret = r"[A-Za-z0-9_-]{43}"
        link = re.search(rf"{re.escape(marker)}v=1&k={secret}&h={secret}(?![A-Za-z0-9_-])", body)
        assert link, body
        assert "&p=" not in body, body
        return event["target"], link.group()

    def witness_end(self, request):
        receivers = [[user["session"], user["instance"]] for user in self.witness.users.values()
                     if user["name"] in {"Andi", "Bea"}]
        assert len(receivers) == 2
        self.witness.send(kind="plugin_data", request=request, receivers=receivers,
                          data_id="nuschl/call", data=[1, 2])
        event = self.witness.wait(lambda event: event["kind"] == "plugin_data_admission"
                                  and event["request"] == request)
        assert event["admitted"], event

    def historical_invitation(self, app, pid):
        def encode(value):
            return base64.urlsafe_b64encode(value).decode().rstrip("=")

        label = b"nuschl/stream/v2"
        publisher = bytes(32)
        extracted = hmac.digest(label, publisher, "sha256")
        host_key = hmac.digest(extracted, label + b" host\x01", "sha256")
        check = hashlib.sha256(host_key).digest()
        old = f"https://{self.call_host}/call#v=1&k={encode(bytes([1]) * 32)}&h={encode(check)}"
        self.witness.send(kind="text", channel=1, body=f"Call: {old}")
        card = self.invitation(app, "Witness")
        self.control(card, "Join video call", shown=False, subtree=True)
        self.copy_link(card, pid, old)

        creator = f"{old}&p={encode(publisher)}"
        self.witness.send(kind="text", channel=1, body=f"Call: {creator}")
        eventually(lambda: (timeline := self.timeline(app)) and self.find(timeline, None, f"Call: {creator}", subtree=True),
                   "a creator URL did not remain ordinary chat")

    def copy_link(self, card, pid, link):
        # The compositor, its clipboard selection and both clients are private.
        self.shell.eval("global.get_window_actors().map(actor => actor.meta_window)"
                        f".find(window => window.get_pid() === {pid}).activate(global.get_current_time()); 'ok'")
        eventually(lambda: self.shell.eval(f"global.display.focus_window?.get_pid() === {pid}") == "true",
                   "the call window did not get focus")
        clipboard = "imports.gi.St.Clipboard.get_default()"
        selection = "imports.gi.St.ClipboardType.CLIPBOARD"
        self.shell.eval(f"{clipboard}.set_text({selection}, 'call fixture sentinel'); 'ok'")
        nodes = items(card, subtree=True)
        title = next(node for node, _, label, _ in nodes if node.path != card.path and label == card.name)
        button = next(node for node, role, label, _ in nodes if role == "push button" and label == "Copy link")
        _, y, _, height = button.extents()
        _, title_y, _, title_height = title.extents()
        assert title_y <= y + height // 2 <= title_y + title_height, (
            f"Copy link moved off the title row: button {(y, height)}, title {(title_y, title_height)}")
        self.click(button)

        def copied():
            self.shell.eval(f"global._callLink = undefined; {clipboard}.get_text({selection},"
                            " (_clipboard, text) => { global._callLink = text; }); 'ok'")
            return self.shell.eval(f"global._callLink === {json.dumps(link)}") == "true"

        eventually(copied, "the private clipboard did not receive the exact guest link", timeout=10)

    def inline(self, app):
        self.control(app, "Full screen")

        def regions_are_disjoint():
            full_screen = self.find(app, "push button", "Full screen")
            messages = self.find(app, "list", "Messages in #Lobby")
            if not full_screen or not messages:
                return False
            _, call_top, _, call_height = full_screen.extents()
            _, chat_top, _, chat_height = messages.extents()
            return call_height > 0 and chat_height > 0 and call_top + call_height <= chat_top

        eventually(regions_are_disjoint, "inline call and chat have overlapping or empty regions", timeout=15)

    def fullscreen(self, app, interaction):
        screenshots = 0

        def pixels(name, width, height, x, y):
            nonlocal screenshots
            screenshots += 1
            screen = self.shell.screenshot(self.args.output / f"{name}-{screenshots:02d}.png")
            return subprocess.check_output([
                "ffmpeg", "-v", "error", "-i", str(screen), "-vf", f"format=rgb24,crop={width}:{height}:{x}:{y}",
                "-f", "rawvideo", "-frames:v", "1", "pipe:1",
            ], timeout=10)

        def window_is_fullscreen():
            return self.shell.eval("global.display.focus_window.is_fullscreen()") == "true"

        black = bytes(3)
        if interaction == "pointer":
            def inline_readable():
                status = self.find(app, None, "Joining video call")
                if not status:
                    return False
                x, y, width, height = status.extents()
                if width == 0 or height == 0:
                    return False
                left, top = json.loads(self.shell.eval(
                    "const bounds = global.display.focus_window.get_buffer_rect(); JSON.stringify([bounds.x, bounds.y])"))
                inline = pixels("inline-status", width, height, left + x, top + y)
                colors = [inline[i:i + 3] for i in range(0, len(inline), 3)]
                return black in colors and any(min(color) >= 192 for color in colors)

            eventually(inline_readable, "light-theme status did not paint light text on a black backdrop", timeout=3)

        self.press(app, "Full screen")
        eventually(window_is_fullscreen, "the compositor did not enter fullscreen")
        exit_button = self.control(app, "Exit full screen")
        eventually(lambda: exit_button.extents()[1] > 800,
                   "fullscreen controls did not reach the display's bottom edge")
        x, y, _, _ = exit_button.extents()

        def toolbar_pixels():
            sample = pixels(interaction, 1, 21, x + 6, y - 14)
            assert len(sample) == 63
            return sample[:3], sample[-3:]

        def toolbar_visible():
            backdrop, toolbar = toolbar_pixels()
            return backdrop == black and toolbar != black

        self.shell.move(40, 40)
        eventually(toolbar_visible, "fullscreen controls did not paint over a black background", timeout=3)
        if interaction != "menu":
            eventually(lambda: toolbar_pixels() == (black, black), "fullscreen controls did not fade", timeout=6)
            assert exit_button.name == "Exit full screen"
        if interaction == "pointer":
            self.shell.move(80, 40)
            eventually(toolbar_visible, "pointer motion did not reveal fullscreen controls", timeout=3)
        elif interaction == "keyboard":
            self.shell.key("Tab")
            eventually(lambda: exit_button.focused, "Tab did not reach the fullscreen controls", timeout=3)
            time.sleep(3.3)
            assert toolbar_visible(), "the focused controls faded"
        else:
            self.press(app, "Video options")
            item = eventually(lambda: self.find(app, "menu item", "Video stats"), "fullscreen video menu did not open")
            self.shell.move(*item.centre())
            time.sleep(3.3)
            assert toolbar_visible(), "the controls faded while the menu was open"
            self.shell.key("Escape")
            eventually(lambda: not self.find(app, "menu", "Context menu"), "Escape did not close the fullscreen menu")
            assert window_is_fullscreen()
        self.shell.key("Escape")
        eventually(lambda: not window_is_fullscreen(), "Escape did not exit fullscreen")
        self.inline(app)

    def run(self):
        self.shell = GnomeSession(self.args.gnome_shell, 1440, 950,
                                  log=(self.args.output / "shell.log").open("w"))
        self.shell.start()
        env = self.shell.app_environment()
        env.pop("NO_AT_BRIDGE", None)
        registry = start_registry(self.args.at_spi, env, self.spawn)
        self.start_pulse()

        # TCP connects to this owned listener. TLS waits with no application
        # reading it, so the desktop never reaches the portal request.
        with socket.create_server(("127.0.0.2", 0)) as listener:
            self.call_host = f"127.0.0.2:{listener.getsockname()[1]}"
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
            andi, andi_opened, andi_pid = self.desktop("Andi", registry, set())
            if self.args.fullscreen:
                for interaction in ("pointer", "keyboard", "menu"):
                    self.press(andi, "Start meeting")
                    self.fullscreen(andi, interaction)
                    self.press(andi, "End call")
                    print(f"Fullscreen {interaction} passed", flush=True)
                return
            bea, bea_opened, bea_pid = self.desktop("Bea", registry, {andi.destination})
            cara, _, _ = self.desktop("Cara", registry, {andi.destination, bea.destination},
                                      channel=2, path="Lobby/Development")
            self.control(andi, "Share screen")
            self.control(andi, "Start call", shown=False)
            self.control(andi, "Start call in browser", shown=False)

            self.witness_end(1)
            self.historical_invitation(bea, bea_pid)
            self.press(andi, "Start meeting")
            self.inline(andi)
            own = self.invitation(andi, "You")
            target, link = self.witness_link()
            assert target == {"kind": "channel", "id": 1}
            self.copy_link(own, andi_pid, link)
            self.copy_link(self.invitation(bea), bea_pid, link)
            assert not any(link in label for _, _, label, _ in items(self.timeline(bea), subtree=True)), "raw call text is visible"

            self.witness_end(2)
            self.witness.send(kind="text", channel=1, body="Call ended")
            eventually(lambda: (timeline := self.timeline(bea)) and self.find(timeline, None, "Call ended", subtree=True),
                       "ordinary text was hidden")
            self.control(self.invitation(bea), "Join video call", subtree=True)

            eventually(lambda: self.find(cara, "tree item", "Lobby"), "Cara cannot see Lobby").press()
            self.press(cara, "Join voice")
            self.control(self.invitation(cara, inline=False), "Join video call", subtree=True)
            self.press(cara, "Join video call")
            self.inline(cara)
            # The stage replaces the card in the same frame.
            assert not self.find(cara, "panel", f"Video call · Andi · {self.call_host}"), \
                "the call stage shows the invitation card of the open call"
            self.press(cara, "Leave video")
            self.control(self.invitation(cara, inline=False), "Join video call", subtree=True)
            self.press(andi, "Minimize call")
            own = self.invitation(andi, "You")
            self.copy_link(own, andi_pid, link)
            self.press(own, "Return to call", subtree=True)
            self.inline(andi)
            self.press(andi, "End call")
            self.control(andi, "Start meeting")
            self.control(bea, "Join video call", shown=False)
            self.control(cara, "Join video call", shown=False)
            self.copy_link(self.invitation(andi, "You", ended=True), andi_pid, link)
            self.invitation(bea, ended=True)

            self.resize_desktop(andi_pid, 880)
            self.press(andi, "Start camera")
            self.inline(andi)
            self.control(andi, "Open in browser", shown=False)
            self.toggle_quality(andi)
            self.video_stats(andi, show=True)
            self.select_quality(andi, "Auto", ("Sharp", "Low latency", "Max 180p", "Max 30 fps"),
                                result="Max 180p · 30 fps")
            assert self.witness_link()[0] == {"kind": "channel", "id": 1}
            self.press(andi, "End call")

            height = self.resize_desktop(andi_pid, 520)
            settings = self.root / "Andi" / "config/settings.json"
            for _ in range(2):
                self.shell.chord("Control_L", "equal")
            eventually(lambda: json.loads(settings.read_text()).get("appearance", {}).get("zoom") == 1.25,
                       "the short fixture window did not reach 125% zoom", timeout=15)
            self.press(andi, "Start camera")
            self.inline(andi)
            self.select_quality(andi, "Auto", ("Max 180p", "Auto frame rate"), result="Max 180p")
            self.shell.chord("Control_L", "0")
            self.resize_desktop(andi_pid, height)
            self.inline(andi)
            self.control(andi, "Video stats", shown=False)
            self.video_stats(andi, show=True)
            self.video_stats(andi, show=False)
            assert self.witness_link()[0] == {"kind": "channel", "id": 1}
            self.invitation(bea)
            self.control(bea, "Open in browser", shown=False)

            self.press(bea, "Join video call")
            self.inline(bea)
            self.press(bea, "Full screen")
            self.control(bea, "Exit full screen")
            self.video_stats(bea, show=True)
            self.video_stats(bea, show=False)
            eventually(lambda: not self.find(bea, "list", "Messages in #Lobby"),
                       "full screen kept the inline chat tree", timeout=15)
            self.press(bea, "Exit full screen")
            self.inline(bea)
            self.press(bea, "Minimize call")
            invited = self.invitation(bea)
            self.control(bea, "Full screen", shown=False)
            self.press(invited, "Return to call", subtree=True)
            self.inline(bea)

            self.press(andi, "End call")
            self.control(andi, "Start camera")
            self.control(bea, "Leave video", shown=False)
            self.control(bea, "Join video call", shown=False)

            # A direct screen call makes the card available to Bea in the
            # direct conversation. The Mumble witness sees only channel links.
            self.direct(andi, "Bea")
            self.direct(bea, "Andi")
            self.press(andi, "Share screen")
            self.invitation(andi, "You")
            self.invitation(bea)
            self.press(bea, "Join video call")
            self.press(bea, "Leave video")
            self.control(bea, "Join video call")
            self.press(andi, "End call")
            self.control(bea, "Join video call", shown=False)

            # The same action carries the link as a private Mumble message
            # when the call recipient uses the Mumble protocol only.
            self.direct(andi, "Witness")
            self.press(andi, "Share screen")
            andi_session = next(user["session"] for user in self.witness.users.values() if user["name"] == "Andi")
            assert self.witness_link()[0] == {"kind": "direct", "id": andi_session}
            self.press(andi, "End call")

            assert not andi_opened.exists() and not bea_opened.exists(), "a call launched the browser"
            assert not self.microphone_open()
            print("Calls passed for native controls, channel and direct invitations, and cleanup.", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--fullscreen", action="store_true")
    for name in ("murmur", "at-spi", "pulse-bin", "opus-library", "gnome-shell", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nuschl-calls-") as temporary:
        test = CallTest(args, Path(temporary))
        try:
            test.run()
        finally:
            test.close()


if __name__ == "__main__":
    main()
