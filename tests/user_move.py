#!/usr/bin/env python3
"""Moving people between channels in the desktop's tree (#317), run by
`screenshots.py --scenes user-move --at-spi`.

The scene's Murmur grants Move to everyone in Lobby's branch and AFK, not in
the root or Music, and Strategy holds one person. Real pointer drags: Linus
to AFK with the preview checked before the drop; Escape and a channel without
Move change nothing; Margaret, in Music's branch, cannot be picked up; Ada
into the full Strategy gets Murmur's denial. The keyboard: Shift+F10 on
Grace's row, "Move to channel…", Up and Down to AFK and Enter.
"""
import json
import subprocess
import time
from accessibility import items
from local_murmur import eventually

# The scene's ACL rows, inserted before Murmur starts: Move (0x20) for @all.
ACL = [(1, 1, 10, None, "all", 1, 1, 0x20, 0), (1, 8, 10, None, "all", 1, 1, 0x20, 0)]
# Murmur's channel_info key for a channel's user limit, and the full channel.
MAX_USERS, STRATEGY = 2, 4


def seed(database):
    database.executemany("INSERT INTO acl (server_id, channel_id, priority, user_id, group_name,"
                         " apply_here, apply_sub, grantpriv, revokepriv) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)", ACL)
    database.execute("INSERT OR REPLACE INTO channel_info (server_id, channel_id, key, value)"
                     " VALUES (1, ?, ?, '1')", (STRATEGY, MAX_USERS))


def run(args, root, shell, registry, bots, profile, port, pin, shot, app):
    def application():
        return next((node for node in registry.children() if node.name.lower() == "nuschl"), None)

    def named(role, name, prefix=False):
        current = application()
        return next((node for node, r, text, _ in items(current)
                     if (not role or role == r) and (text.startswith(name) if prefix else text == name)), None)

    def origin():
        return json.loads(shell.eval(
            "JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window)"
            ".find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return {x:b.x,y:b.y};})())"))

    def point(node):
        o = origin()
        x, y = node.centre()
        return o["x"] + x, o["y"] + y

    def row(name):
        return eventually(lambda: named("tree item", name, prefix=True), f"no row {name!r}")

    def button(down):
        shell.eval("global._nuschlPointer.notify_button(imports.gi.GLib.get_monotonic_time(), 1,"
                   f" imports.gi.Clutter.ButtonState.{'PRESSED' if down else 'RELEASED'}); 'ok'")

    def channel_of(name):
        return next((user["channel"] for user in bots[0].users.values() if user["name"] == name), None)

    def pick(person, onto, frames=None):
        """Press on `person`'s row and glide onto channel `onto`, the button held."""
        start = point(row(person))
        shell.move(*start)
        time.sleep(0.2)
        button(True)
        shell.move(start[0] + 6, start[1] + 4)
        time.sleep(0.1)
        end = point(row(onto))
        for step in range(1, 13):
            shell.move(start[0] + (end[0] - start[0]) * step / 12, start[1] + (end[1] - start[1]) * step / 12)
            time.sleep(0.03)
            if frames is not None:
                frames.append(shot(f"user-move-frame-{len(frames):02}"))
        time.sleep(0.4)

    def below(person, channel):
        """Whether `person`'s row shows under `channel` (and above the next channel)."""
        top = row(channel).extents()[1]
        return row(person).extents()[1] > top

    def unchanged(person, channel, message):
        time.sleep(1.5)
        assert channel_of(person) == channel, f"{message}: {person} is in {channel_of(person)}"

    report, records = args.output / "user-move.json", []
    for person, channel in [("Linus", 5), ("Ada Lovelace", 2), ("Grace", 2), ("Margaret", 7)]:
        eventually(lambda: channel_of(person) == channel, f"{person} not in channel {channel}")
    # Permissions where people are arrive on their own; give them a moment.
    time.sleep(2)

    # Linus from Shooters onto AFK: the tree previews it, nothing is sent yet.
    frames = []
    pick("Linus", "AFK", frames)
    assert below("Linus", "AFK"), "the drag did not preview Linus under AFK"
    assert channel_of("Linus") == 5, "the preview moved Linus before the drop"
    frames.append(shot("user-move-preview"))
    button(False)
    eventually(lambda: channel_of("Linus") == 8, "the drop did not move Linus to AFK")
    for _ in range(6):
        time.sleep(0.05)
        frames.append(shot(f"user-move-frame-{len(frames):02}"))
    shot("user-move-dropped")
    records.append({"drag_drop": "Linus to AFK"})

    # Escape during a drag, and a drop on Music (no Move there), change nothing.
    pick("Ada Lovelace", "Gaming")
    assert below("Ada Lovelace", "Gaming"), "the drag did not preview Ada under Gaming"
    shell.key("Escape")
    button(False)
    unchanged("Ada Lovelace", 2, "Escape")
    eventually(lambda: not below("Ada Lovelace", "Gaming"), "Escape left the preview")
    pick("Ada Lovelace", "Music")
    assert not below("Ada Lovelace", "Music"), "Music took a drop without Move"
    shot("user-move-no-permission")
    button(False)
    unchanged("Ada Lovelace", 2, "Music")
    records.append({"cancel": ["Escape", "no Move in Music"]})

    # Margaret is in Music's branch, where we may not move people.
    pick("Margaret", "AFK")
    button(False)
    unchanged("Margaret", 7, "Margaret")
    records.append({"not_offered": "Margaret"})

    # The keyboard: the row's context menu, "Move to channel…", Down, Enter.
    grace = row("Grace")
    grace.call("Component", "GrabFocus")
    eventually(lambda: row("Grace").focused, "Grace's row took no focus")
    shell.press("Shift_L", "F10")
    eventually(lambda: named("menu item", "Move to channel…"), "no Move to channel… in the menu").press()
    eventually(lambda: named("tree item", "Move Grace", prefix=True), "the row did not announce the move")
    # AFK sorts above Lobby: Up reaches it; Down first runs to the last choice.
    for key in ["Down"] * 8 + ["Up"] * 12:
        if named("tree item", "Move Grace to AFK", prefix=True):
            break
        shell.key(key)
        time.sleep(0.2)
    assert named("tree item", "Move Grace to AFK: Enter moves, Escape cancels"), "Down did not reach AFK"
    shot("user-move-keyboard")
    shell.key("Return")
    eventually(lambda: channel_of("Grace") == 8, "Enter did not move Grace to AFK")
    time.sleep(0.5)
    assert named(None, "@ Grace") is None, "Enter also opened a conversation with Grace"
    records.append({"keyboard": "Grace to AFK"})

    # Strategy holds one person; the next is refused by Murmur and goes back.
    pick("Linus", "Strategy and very long")
    button(False)
    eventually(lambda: channel_of("Linus") == STRATEGY, "Linus did not reach Strategy")
    pick("Ada Lovelace", "Strategy and very long")
    button(False)
    eventually(lambda: named(None, "That channel is full.", prefix=True), "no denial shown")
    unchanged("Ada Lovelace", 2, "the full channel")
    eventually(lambda: not below("Ada Lovelace", "Strategy and very long"), "the refused move kept its preview")
    shot("user-move-refused")
    records.append({"refused": "Ada into the full Strategy"})

    gif = args.output / "user-move.gif"
    subprocess.run(["ffmpeg", "-loglevel", "error", "-y", "-framerate", "12", "-pattern_type", "glob",
                    "-i", str(args.output / "user-move-frame-*.png"), "-vf",
                    "crop=420:520:0:60,split[a][b];[a]palettegen[p];[b][p]paletteuse", str(gif)], check=True)
    records.append({"gif": str(gif)})
    report.write_text(json.dumps(records, indent=2))
    print("User move: drag, preview, cancel, permissions, keyboard and denial passed.", flush=True)
