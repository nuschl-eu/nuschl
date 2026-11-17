#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Strip metadata on the desktop, for images that cannot be re-encoded.

Runs the desktop in a private headless GNOME Shell whose FileChooser portal is
a stand-in that hands out only this test's files, against a loopback Murmur, a
scripted peer and a private magic-wormhole mailbox and relay. No host
clipboard, audio, profile or file chooser is used. Checks that:

- with Strip metadata ticked (the default), an image over 8 MB and a PNG the
  content worker cannot decode leave a note and nothing in the tray, and the
  peer receives no file offer;
- the checkbox is a check box named "Strip metadata" with its description;
- unticked, the image over 8 MB goes into the tray as its original file;
  ticking the box again takes it back out;
- unticked and sent, it reaches the peer as a file offer;
- the next message has the box ticked again, and a HEIC file is refused.

python3 tests/image_metadata.py target/debug --murmur <mumble-server>
    --at-spi <at-spi2-core> --wormhole-python <python with magic-wormhole servers>
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from accessibility import items, start_registry  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402
from transfer import Wormhole  # noqa: E402

CHECK_BOX = "role 7"  # ATSPI_ROLE_CHECK_BOX
CHECKED = 1 << 4  # ATSPI_STATE_CHECKED
DESCRIPTION = "Remove image metadata such as GPS coordinates, capture time, device model, etc."
# A stand-in org.freedesktop.portal.FileChooser: OpenFile answers with
# global.nuschlChooser.paths.
PORTAL = '''(() => {
  const Gio = imports.gi.Gio, GLib = imports.gi.GLib;
  global.nuschlChooser = {paths: []};
  const xml = '<node><interface name="org.freedesktop.portal.FileChooser">' +
    '<property name="version" type="u" access="read"/>' +
    '<method name="OpenFile"><arg type="s" direction="in"/><arg type="s" direction="in"/>' +
    '<arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method></interface></node>';
  global.nuschlChooser.object = Gio.DBusExportedObject.wrapJSObject(xml, {
    get version() { return 4; },
    OpenFileAsync(params, invocation) {
      const token = params[2].handle_token.deep_unpack();
      const sender = invocation.get_sender().slice(1).replaceAll('.', '_');
      const path = '/org/freedesktop/portal/desktop/request/' + sender + '/' + token;
      const uris = global.nuschlChooser.paths.slice();
      invocation.return_value(new GLib.Variant('(o)', [path]));
      GLib.timeout_add(GLib.PRIORITY_DEFAULT, 100, () => {
        Gio.DBus.session.emit_signal(null, path, 'org.freedesktop.portal.Request', 'Response',
          new GLib.Variant('(ua{sv})', [0, {uris: new GLib.Variant('as', uris)}]));
        return GLib.SOURCE_REMOVE;
      });
    }
  });
  global.nuschlChooser.object.export(Gio.DBus.session, '/org/freedesktop/portal/desktop');
  Gio.bus_own_name_on_connection(Gio.DBus.session, 'org.freedesktop.portal.Desktop',
    Gio.BusNameOwnerFlags.NONE, null, null);
  return 'ready';
})()'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--at-spi", type=Path, required=True)
    parser.add_argument("--wormhole-python", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--output", type=Path, default=Path(tempfile.gettempdir()) / "nuschl-image-metadata")
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix="nuschl-image-metadata-"))
    processes = []
    shell = server = wormhole = peer = None

    def spawn(command, env, log):
        process = subprocess.Popen(command, env=env, stdout=(root / log).open("w"), stderr=subprocess.STDOUT,
                                   start_new_session=True)
        processes.append(process)
        return process

    try:
        shell = GnomeSession(args.gnome_shell, 1280, 900, log=(root / "shell.log").open("w"))
        shell.start()
        env = shell.app_environment()
        env.pop("NO_AT_BRIDGE", None)
        registry = start_registry(args.at_spi, env, spawn)
        assert shell.eval(PORTAL) == "ready"
        wormhole = Wormhole(args.wormhole_python, root)
        port = free_port()
        (root / "murmur").mkdir()
        server = Murmur(args.murmur, root / "murmur", port)
        server.seed()
        server.start()
        pin = certificate_pin(args.binaries, "127.0.0.1", port, root / "probe")
        peer = Client(args.binaries, "127.0.0.1", port, pin, "Receiver", root / "peer")
        peer.send(kind="join", channel=2)
        profile = root / "profile"
        (profile / "config").mkdir(parents=True)
        (profile / "config/servers.json").write_text(json.dumps([dict(
            id=str(uuid.uuid4()), name="Images", host="127.0.0.1", port=port, username="Sender",
            certificate_sha256=pin)]))
        # No audio device: the private session has no sound server.
        app_env = env | wormhole.environment() | {
            "RUST_LOG": "nuschl=info", "NUSCHL_AUDIO_BACKEND": "pulse",
            "PULSE_SERVER": f"unix:{root / 'no-audio'}", "PIPEWIRE_REMOTE": "nuschl-no-audio"}
        app = spawn([str(args.binaries / "nuschl"), "--profile-dir", str(profile),
                     f"mumble://Sender@127.0.0.1:{port}/Lobby/Development"], app_env, "desktop.log")
        peer.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Sender"
                  and e["user"]["channel"] == 2, timeout=40)

        def application():
            assert app.poll() is None, (root / "desktop.log").read_text()[-3000:]
            return next((node for node in registry.children() if node.name.lower() == "nuschl"), None)
        nuschl = eventually(application, "Nuschl did not register with the accessibility registry", timeout=45)

        def find(predicate, message):
            return eventually(lambda: next((entry for entry in items(nuschl) if predicate(*entry)), None),
                              message, timeout=30)

        def present(predicate):
            return any(predicate(*entry) for entry in items(nuschl))

        def attach(path):
            shell.eval(f"global.nuschlChooser.paths = {json.dumps([path.as_uri()])}; 'ok'")
            find(lambda _, role, name, __: name == "Attach images or files", "no Attach button")[0].press()

        def checkbox():
            return find(lambda _, role, name, __: name == "Strip metadata" and role == CHECK_BOX,
                        "Strip metadata is not a named check box")

        def offers():
            return [e for e in peer.events if e["kind"] == "text" and "receive " in e.get("body", "")]

        photo = root / "IMG_20261103_143012.png"
        with photo.open("wb") as file:
            file.write(b"\x89PNG\r\n\x1a\n")
            file.truncate(8 * 1024 * 1024 + 1)
        broken = root / "broken.png"
        broken.write_bytes(b"\x89PNG\r\n\x1a\nnot an image")

        # On: refused with a note, nothing in the tray, nothing offered.
        attach(photo)
        find(lambda _, __, name, ___: name == f"Could not attach {photo.name}: it is larger than 8 MB.",
             "the oversized image was not refused")
        node, _, _, states = checkbox()
        assert states & CHECKED, "Strip metadata is not checked by default"
        description = node.bus("get-property", node.destination, node.path, "org.a11y.atspi.Accessible",
                               "Description")
        assert description == DESCRIPTION, description
        attach(broken)
        find(lambda _, __, name, ___: name.startswith("Could not attach broken.png: "),
             "the undecodable image was not refused")
        assert not present(lambda _, role, name, __: role == "list item" and name.startswith(("File ", "Image "))), \
            "a refused image stayed in the tray"
        print("Strip metadata on: the oversized and the undecodable image were refused with a note.", flush=True)
        shell.screenshot(root / "refused.png")

        # Unticked: the oversized image goes into the tray as its file.
        node.call("Action", "DoAction", "i", "0")
        eventually(lambda: not checkbox()[3] & CHECKED, "Strip metadata did not untick")
        attach(photo)
        find(lambda _, __, name, ___: name == f"{photo.name} goes as a file: it is larger than 8 MB.",
             "the oversized image was not attached as a file")
        find(lambda _, role, name, __: role == "list item" and name.startswith(f"File {photo.name}"),
             "the original is not in the tray")
        # Ticked again, the original leaves the tray.
        checkbox()[0].call("Action", "DoAction", "i", "0")
        eventually(lambda: checkbox()[3] & CHECKED, "Strip metadata did not tick")
        eventually(lambda: not present(lambda _, role, name, __: name.startswith(f"File {photo.name}")),
                   "the original stayed in the tray after ticking Strip metadata")
        time.sleep(3)
        assert not offers(), offers()
        print("Ticking Strip metadata took the original back out of the tray; nothing was offered.", flush=True)

        checkbox()[0].call("Action", "DoAction", "i", "0")
        eventually(lambda: not checkbox()[3] & CHECKED, "Strip metadata did not untick")
        attach(photo)
        find(lambda _, role, name, __: role == "list item" and name.startswith(f"File {photo.name}"),
             "the original is not in the tray")
        find(lambda _, role, name, __: role == "push button" and name == "Send", "no Send button")[0].press()
        offer = peer.wait(lambda e: e["kind"] == "text" and photo.name in e.get("body", "")
                          and "receive " in e["body"], timeout=60)
        print(f"Strip metadata off: the original went as a file offer: {offer['body'][:120]!r}", flush=True)
        shell.screenshot(root / "offered.png")

        # The next message: ticked again, and a HEIC photo is refused.
        heic = root / "IMG_0042.HEIC"
        heic.write_bytes(b"\0\0\0\x18ftypheic\0\0\0\0mif1heic" + bytes(64))
        attach(heic)
        find(lambda _, __, name, ___: name == f"Could not attach {heic.name}: HEIC cannot be re-encoded.",
             "the HEIC photo was not refused")
        assert checkbox()[3] & CHECKED, "Strip metadata is not ticked for the next message"
        assert not present(lambda _, role, name, __: role == "list item" and heic.name in name)
        time.sleep(3)
        assert not any(heic.name in e.get("body", "") for e in offers()), offers()
        print("The next message had Strip metadata ticked again; the HEIC photo was refused.", flush=True)
    finally:
        if peer:
            peer.close()
        for process in reversed(processes):
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
        if server:
            server.stop()
        if wormhole:
            wormhole.stop()
        if shell:
            shell.stop()
        args.output.mkdir(parents=True, exist_ok=True)
        for name in ("desktop.log", "refused.png", "offered.png"):
            if (root / name).exists():
                shutil.copy(root / name, args.output / name)
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
