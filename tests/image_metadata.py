#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Strip metadata and image quality on the desktop.

Runs the desktop in a private headless GNOME Shell whose FileChooser portal is
a stand-in that hands out only this test's files, against a loopback Murmur
with a 20,000-byte image limit, a scripted peer and a private magic-wormhole
mailbox and relay. No host clipboard, audio, profile or file chooser is used.
Settings' image quality is Low. Checks that:

- an attached image brings no link preview note to the composer, and the
  composer offers Low, High and Original with Low chosen;
- with Strip metadata ticked (the default), an image over 8 MB and a PNG the
  content worker cannot decode leave a note and nothing in the tray, and the
  peer receives no file offer;
- the checkbox is a check box named "Strip metadata" with its description;
- unticked, the image over 8 MB goes into the tray as its original file;
  ticking the box again takes it back out;
- unticked and sent, it reaches the peer as a file offer;
- the next message has the box ticked again, and a HEIC file is refused;
- a synthetic photo with EXIF (camera model, capture time, GPS position,
  orientation 6) reaches the peer at Low upright and without metadata;
- at Original with the box ticked, the peer downloads the photo without its
  metadata, with the same scan data and an EXIF that holds only the
  orientation; the next message is at Low with the box ticked again;
- at Original with the box unticked, the peer downloads the same bytes;
- at High, a noisy picture far over the server's limit reaches the peer
  within it.

python3 tests/image_metadata.py target/debug --murmur <mumble-server>
    --at-spi <at-spi2-core> --wormhole-python <python with magic-wormhole servers>
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import re
import time
import urllib.parse
import uuid
import zlib

sys.path.insert(0, str(Path(__file__).resolve().parent))
from accessibility import items, start_registry  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402
from transfer import Wormhole  # noqa: E402

CHECK_BOX = "role 7"  # ATSPI_ROLE_CHECK_BOX
CHECKED = 1 << 4  # ATSPI_STATE_CHECKED
SELECTED = 1 << 23  # ATSPI_STATE_SELECTED
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


def png(path, width, height):
    """A synthetic RGB PNG."""
    rows = b"".join(b"\0" + bytes((x * 7 + y * 3) % 256 for x in range(width * 3)) for y in range(height))

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    path.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
                     + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))
    return path


# A synthetic 64×32 JPEG, red on the left and blue on the right.
JPEG = base64.b64decode(
    "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAMCAgMCAgMDAwMEAwMEBQgFBQQEBQoHBwYIDAoMDAsKCwsNDhIQDQ4RDgsLEBYQ"
    "ERMUFRUVDA8XGBYUGBIUFRT/2wBDAQMEBAUEBQkFBQkUDQsNFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQU"
    "FBQUFBQUFBQUFBQUFBT/wAARCAAgAEADAREAAhEBAxEB/8QAFgABAQEAAAAAAAAAAAAAAAAAAAcI/8QAFBABAAAAAAAAAAAA"
    "AAAAAAAAAP/EABYBAQEBAAAAAAAAAAAAAAAAAAAJB//EABQRAQAAAAAAAAAAAAAAAAAAAAD/2gAMAwEAAhEDEQA/AM6MMVTA"
    "AAQJcZHQAABfUOViwAAECXGR0AAAX1DlYsAABAlxkdAAAF9Q5WLAAAQJcZHQAAB//9k=")
LIMIT = 20_000


def exif(orientation):
    """EXIF as a phone writes it (TIFF, big-endian): camera model, capture
    time, GPS position and `orientation`."""
    def ifd(entries):
        return struct.pack(">H", len(entries)) + b"".join(struct.pack(">HHII", *e) for e in entries) + bytes(4)
    model, time_ = b"NUSCHL-MODEL Pixel 9\0", b"2026:11:03 14:30:12\0"
    model_at = 8 + 2 + 5 * 12 + 4
    time_at = model_at + len(model)
    exif_at = time_at + len(time_)
    original_at = exif_at + 2 + 12 + 4
    gps_at = original_at + len(time_)
    latitude_at = gps_at + 2 + 4 * 12 + 4
    return (b"MM\0*\0\0\0\x08"
            + ifd([(0x0110, 2, len(model), model_at), (0x0112, 3, 1, orientation << 16),
                   (0x0132, 2, len(time_), time_at), (0x8769, 4, 1, exif_at), (0x8825, 4, 1, gps_at)])
            + model + time_ + ifd([(0x9003, 2, len(time_), original_at)]) + time_
            + ifd([(1, 2, 2, ord("N") << 24), (2, 5, 3, latitude_at), (3, 2, 2, ord("E") << 24),
                   (4, 5, 3, latitude_at + 24)])
            + struct.pack(">6I", 48, 1, 12, 1, 2952, 100) + struct.pack(">6I", 16, 1, 22, 1, 2568, 100))


def turned_photo(path):
    """JPEG with the EXIF of a phone held upright: turned, orientation 6."""
    app1 = b"Exif\0\0" + exif(6)
    path.write_bytes(JPEG[:2] + b"\xff\xe1" + struct.pack(">H", len(app1) + 2) + app1 + JPEG[2:])
    return path


def dimensions(image):
    """Width and height of a PNG or JPEG."""
    if image.startswith(b"\x89PNG"):
        return struct.unpack(">II", image[16:24])
    at = 2
    while True:
        code, length = image[at + 1], struct.unpack(">H", image[at + 2:at + 4])[0]
        if 0xc0 <= code <= 0xcf and code not in (0xc4, 0xc8, 0xcc):
            height, width = struct.unpack(">HH", image[at + 5:at + 9])
            return width, height
        at += 2 + length


def inline_image(body):
    """The image in an `<img src="data:…;base64,…">` message."""
    match = re.fullmatch(r'<img src="data:image/(png|jpeg);base64,([^"]*)" />', body, re.S)
    assert match, body[:80]
    return base64.b64decode(urllib.parse.unquote(match[2]).replace("\n", ""))


def without_metadata(image):
    return not any(marker in image for marker in (b"NUSCHL", b"2026:11:03"))


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
        server.ini.write_text(server.ini.read_text() + f"imagemessagelength={LIMIT}\n")
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
        (profile / "config/settings.json").write_text(json.dumps({"image_quality": "low"}))
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

        def choose(quality):
            find(lambda _, role, name, __: role == "radio button" and name == quality,
                 f"no {quality} option")[0].press()

        def chosen():
            return [name for _, role, name, states in items(nuschl)
                    if role == "radio button" and states & SELECTED]

        def send():
            find(lambda _, role, name, __: role == "push button" and name == "Send", "no Send button")[0].press()

        def download(name):
            """The file the peer was last offered as `name`, received with the reference CLI."""
            offer = peer.wait(lambda e: e["kind"] == "text" and name in e.get("body", "")
                              and "receive " in e["body"], timeout=60)
            code = re.search(r"receive (\S+)</code>", offer["body"])[1]
            target = root / "received" / f"{len(offers())}-{name}"
            target.parent.mkdir(exist_ok=True)
            subprocess.run(wormhole.cli("receive", "--accept-file", "--hide-progress", "-o", str(target), code),
                           check=True, timeout=120, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            return target.read_bytes()

        photo = root / "IMG_20261103_143012.png"
        with photo.open("wb") as file:
            file.write(b"\x89PNG\r\n\x1a\n")
            file.truncate(8 * 1024 * 1024 + 1)
        broken = root / "broken.png"
        broken.write_bytes(b"\x89PNG\r\n\x1a\nnot an image")

        # An image in the tray, with text typed after it, brings no link
        # preview note.
        shot = png(root / "shot.png", 64, 48)
        attach(shot)
        find(lambda _, role, name, __: role == "list item" and name.startswith(f"Image {shot.name}, ")
             and not name.endswith("being prepared"), "the image was not prepared")
        assert chosen() == ["Low"], chosen()
        find(lambda _, role, name, __: role in ("text", "entry") and name == "Message",
             "no composer")[0].call("Component", "GrabFocus")
        time.sleep(0.5)
        shell.type("Look")
        time.sleep(1.0)
        notes = [name for _, _, name, _ in items(nuschl)
                 if name == "Link preview" or name.startswith(("Text only", "No link preview"))]
        assert not notes, notes
        shell.chord("Control_L", "a")
        shell.key("BackSpace")
        find(lambda _, role, name, __: name == f"Remove {shot.name}", "no Remove button")[0].press()
        eventually(lambda: not present(lambda _, role, name, __: name.startswith(f"Image {shot.name}")),
                   "the image stayed in the tray")
        print("An attached image brought no link preview note.", flush=True)

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
        send()
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

        # Low: re-encoded upright, without metadata.
        turned = turned_photo(root / "IMG_0042.jpg")
        assert chosen() == ["Low"], chosen()
        attach(turned)
        find(lambda _, role, name, __: name.startswith(f"Image {turned.name}, ")
             and not name.endswith("being prepared"), "the photo was not prepared")
        send()
        sent = inline_image(peer.wait(lambda e: e["kind"] == "text" and e.get("body", "").startswith("<img"),
                                      timeout=60)["body"])
        assert without_metadata(sent) and b"Exif" not in sent
        assert dimensions(sent) == (32, 64), dimensions(sent)
        print("Low: the photo arrived upright, 32×64, without metadata.", flush=True)

        # Original, ticked: the file without its metadata, upright as before.
        attach(turned)
        choose("Original")
        find(lambda _, role, name, __: name.startswith(f"File {turned.name}, "),
             "the photo is not in the tray as a file")
        shell.screenshot(root / "original.png")
        send()
        stripped = download(turned.name)
        scan = JPEG[JPEG.index(b"\xff\xda"):]
        assert without_metadata(stripped) and stripped.endswith(scan), stripped[:64]
        orientation = b"MM\0*\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06" + bytes(6)
        assert b"Exif\0\0" + orientation in stripped
        assert stripped.count(b"Exif") == 1
        print(f"Original: the peer downloaded {len(stripped)} bytes without metadata, "
              f"with the scan data and only the orientation.", flush=True)

        # The next message: Low and ticked again. Original, unticked: the same
        # bytes (another name: the first offer is posted again after its pickup).
        turned = turned_photo(root / "IMG_0043.jpg")
        attach(turned)
        find(lambda _, role, name, __: name.startswith(f"Image {turned.name}, "), "the photo is not in the tray")
        assert chosen() == ["Low"], chosen()
        assert checkbox()[3] & CHECKED, "Strip metadata is not ticked for the next message"
        choose("Original")
        checkbox()[0].call("Action", "DoAction", "i", "0")
        eventually(lambda: not checkbox()[3] & CHECKED, "Strip metadata did not untick")
        find(lambda _, role, name, __: name.startswith(f"File {turned.name}, "), "the original is not in the tray")
        send()
        original = download(turned.name)
        assert hashlib.sha256(original).digest() == hashlib.sha256(turned.read_bytes()).digest()
        print("Original, unticked: the peer downloaded the same bytes.", flush=True)

        # High, far over the server's image limit: smaller, within it.
        noisy = root / "noise.png"
        with noisy.open("wb") as file:
            rows = b"".join(b"\0" + os.urandom(400 * 3) for _ in range(300))
            def chunk(kind, data):
                return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
            file.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 400, 300, 8, 2, 0, 0, 0))
                       + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))
        attach(noisy)
        choose("High")
        find(lambda _, role, name, __: name.startswith(f"Image {noisy.name}, ")
             and not name.endswith("being prepared"), "the noisy picture was not prepared")
        send()
        body = peer.wait(lambda e: e["kind"] == "text" and e.get("body", "").startswith("<img"), timeout=60)["body"]
        assert len(body) <= LIMIT, len(body)
        print(f"High: a {noisy.stat().st_size}-byte picture arrived as a {len(body)}-byte image "
              f"within the {LIMIT}-byte limit.", flush=True)
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
        for name in ("desktop.log", "refused.png", "offered.png", "original.png"):
            if (root / name).exists():
                shutil.copy(root / name, args.output / name)
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
