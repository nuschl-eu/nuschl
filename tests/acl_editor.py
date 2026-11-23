#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Owned stock/nuschld native ACL editor + fresh offline-account lookup proof."""

import argparse
import json
import subprocess
import time
from pathlib import Path

import document_link_regression as fixture
import welcome_html
from accessibility import items
from admin_bootstrap import Operator, encode, scalar
from hash_document_wire import fields
from layout_regression import inside
from local_murmur import Murmur, eventually
from local_nuschld import Nuschld
from mumble_client import Client

ARGS = None
SERVER = None
IDS = {}


class OwnedMurmur(Murmur):
    def __init__(self, *args):
        global SERVER
        super().__init__(*args)
        if ARGS.high_admin_limit:
            self.ini.write_text(
                self.ini.read_text() + "messagelimit=100\nmessageburst=100\n"
            )
        SERVER = self


class OwnedNuschld(Nuschld):
    def __init__(self, _binary, root, port):
        global SERVER
        super().__init__(ARGS.binaries, root, port)
        self.ini = root / "welcome-bridge.ini"
        self.ini.write_text(
            'welcometext="Welcome to the <b>Nuschl</b> screenshot server."'
        )
        self.database = root / "unused-database-path"
        SERVER = self

    def seed(self):
        super().seed()
        self.write_config(message_limit=1, message_burst=5)
        welcome = self.ini.read_text().split('welcometext="', 1)[1].rsplit('"', 1)[0]
        config = self.config.read_text().splitlines()
        self.config.write_text(
            "\n".join(
                "welcome_text = " + json.dumps(welcome)
                if line.startswith("welcome_text =")
                else line
                for line in config
            )
            + "\n"
        )


class Listing(Operator):
    """An operator that also keeps the channel IDs by name from its sync."""

    def receive(self):
        kind, value = super().receive()
        if kind == 7 and 3 in value:
            self.__dict__.setdefault("channels", {})[value[3][0].decode()] = value.get(1, [0])[0]
        return kind, value


def operator(root, pin, kind=Operator):
    certificate, key = root / "operator.pem", root / "operator-key.pem"
    if not certificate.exists():
        subprocess.run(
            [
                "openssl",
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-config",
                "/dev/null",
                "-subj",
                "/CN=Owned ACL fixture",
                "-days",
                "1",
                "-keyout",
                str(key),
                "-out",
                str(certificate),
            ],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=30,
        )
    return kind(SERVER.port, pin, SERVER.SUPERUSER_PASSWORD, certificate, key)


def configure(_shell, root, output):
    bookmark = json.loads((root / "profile/config/servers.json").read_text())[0]
    pin = bookmark["certificate_sha256"]
    reader = Client(
        ARGS.binaries, "127.0.0.1", SERVER.port, pin, "Welcome reader", root / "profile"
    )
    offline = None
    try:
        reader.send(kind="register")
        registered = reader.wait(
            lambda event: (
                event["kind"] == "user"
                and event["user"]["session"] == reader.session
                and event["user"]["user_id"] is not None
            )
        )
        IDS["reader"] = registered["user"]["user_id"]
        offline = Client(
            ARGS.binaries,
            "127.0.0.1",
            SERVER.port,
            pin,
            "Offline member",
            root / "offline",
        )
        offline.send(kind="register")
        registered = offline.wait(
            lambda event: (
                event["kind"] == "user"
                and event["user"]["session"] == offline.session
                and event["user"]["user_id"] is not None
            )
        )
        IDS["offline"] = registered["user"]["user_id"]
        offline.close()
        offline = None
        admin = operator(root, pin)
        try:
            admin.promote(IDS["reader"])
        finally:
            admin.close()
        reader.send(kind="acl_draft", channel=0, request=100)
        reader.wait(
            lambda event: (
                event["kind"] == "acl_reply"
                and event["request"] == 100
                and "acl" in event
            )
        )
        reader.send(
            kind="lookup_account", channel=0, request=101, name="Offline member"
        )
        resolved = reader.wait(
            lambda event: event["kind"] == "account_reply" and event["request"] == 101
        )
        assert resolved["account"]["id"] == IDS["offline"], resolved
        reader.send(
            kind="lookup_account",
            channel=0,
            request=102,
            name="No such private fixture account",
        )
        missing = reader.wait(
            lambda event: event["kind"] == "account_reply" and event["request"] == 102
        )
        assert "error" in missing, missing
        (output / "fresh-wire-lookup.json").write_text(
            json.dumps({"offline": resolved, "unknown": missing}, indent=2) + "\n"
        )
    finally:
        if offline:
            offline.close()
        reader.close()
    return {"RUST_LOG": "nuschl=info,nuschl::layout=debug,nuschl::acl_editor=debug"}


def actions(args, root, shell, application, bot, murmur):
    shell.key("Shift_L")
    shell.place("Nuschl", 40, 40, 1100, 800)
    pin = bot.process.args[bot.process.args.index("--certificate-sha256") + 1]
    cases = []

    def mark(name):
        cases.append(name)
        (args.output / "cases.json").write_text(json.dumps(cases, indent=2) + "\n")

    def named(name, role=None):
        return next(
            (
                node
                for node, kind, label, _ in items(application)
                if label == name and (role is None or kind == role)
            ),
            None,
        )

    def wait(name, role=None):
        return eventually(lambda: named(name, role), f"missing ACL UI {name}")

    def point(node):
        origin = json.loads(
            shell.eval(
                "JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return {x:b.x,y:b.y};})())"
            )
        )
        # A focus reveal scrolls the dialog; click where the control settles.
        extents = node.extents()
        for _ in range(20):
            time.sleep(0.1)
            settled, extents = extents, node.extents()
            if settled == extents:
                break
        x, y, width, height = extents
        return origin["x"] + x + width // 2, origin["y"] + y + height // 2

    def activate(name):
        node = wait(name, "push button")
        assert node.call("Component", "GrabFocus")[0]
        time.sleep(0.15)
        shell.click(*point(node))
        return node

    def type_field(label, value):
        node = eventually(
            lambda: next(
                (
                    node
                    for node, role, name, _ in items(application)
                    if name == label and role in ("entry", "text")
                ),
                None,
            ),
            f"missing input {label}",
        )
        assert node.call("Component", "GrabFocus")[0]
        time.sleep(0.15)
        shell.click(*point(node))
        shell.chord("Control_L", "a")
        shell.type(value)
        time.sleep(0.2)
        return node

    def text_contains(text):
        return any(text in label for _, _, label, _ in items(application))

    def open_acl(channel="Root", expand=True):
        row = eventually(
            lambda: next(
                (
                    node
                    for node, role, name, _ in items(application)
                    if role == "tree item" and name.startswith(channel)
                ),
                None,
            ),
            f"missing channel {channel}",
        )
        assert row.call("Component", "GrabFocus")[0]
        time.sleep(0.2)
        shell.click(*point(row), button=3)
        wait("Edit permissions…", "menu item").press()
        eventually(
            lambda: text_contains("Edit a local rule or group"),
            "ACL draft did not load",
        )
        # The rule list is collapsed while the permission table shows the channel.
        expand = expand and next(
            (
                node
                for node, role, name, _ in items(application)
                if role == "push button" and name.startswith("Show advanced rules")
            ),
            None,
        )
        if expand:
            assert expand.call("Component", "GrabFocus")[0]
            time.sleep(0.15)
            shell.click(*point(expand))
            eventually(lambda: text_contains("Advanced rules · "), "advanced rules stayed hidden")

    def matching(fragment):
        return eventually(
            lambda: next(
                (
                    node
                    for node, role, name, _ in items(application)
                    if role == "push button" and fragment in name
                ),
                None,
            ),
            f"missing action containing {fragment}",
        )

    def click_matching(fragment):
        node = matching(fragment)
        assert node.call("Component", "GrabFocus")[0]
        time.sleep(0.15)
        shell.click(*point(node))

    def query_channel(admin, channel):
        admin.send(13, scalar(1, channel) + scalar(5, 1))
        return admin.wait(13)

    def local_rules(acl):
        return [fields(raw) for raw in acl.get(4, []) if not fields(raw).get(3, [1])[0]]

    def save():
        activate("Save permissions")
        eventually(
            lambda: text_contains("Permissions saved and confirmed"),
            "ACL save was not verified",
        )

    if args.pending_input:
        open_acl()
        # The first transaction consumes the real default control credit. The
        # second naturally waits in the production pacer, without server delays.
        save()
        field = type_field("New group name", "pending-input-original")
        before = field.call("Accessible", "GetState")
        wait("Save permissions", "push button").press()
        assert field.call("Component", "GrabFocus")[0]
        time.sleep(0.05)
        assert field.focused
        shell.type("X")
        during = field.call("Accessible", "GetState")
        assert not text_contains("Permissions saved and confirmed"), (
            "typing probe missed the pending transaction"
        )
        eventually(
            lambda: text_contains("Permissions saved and confirmed"),
            "pending save never completed",
        )
        activate("Create group")
        save()
        admin = operator(root, pin)
        try:
            groups = [fields(raw)[1][0].decode() for raw in admin.acl()[3]]
            assert "pending-input-original" in groups, groups
            assert "pending-input-originalX" not in groups, groups
        finally:
            admin.close()
        shell.screenshot(args.output / "pending-input.png")
        mark("focused_input_rejects_typing_during_pending_save_and_unlocks_after_reply")
        return {
            "pending_input_readonly": True,
            "before_state": before,
            "during_state": during,
            "groups": groups,
        }

    if args.channel_types:
        # Channel types write standard Mumble rules and read back from them.
        def selected(name):
            node = named(name, "radio button")
            # ATSPI_STATE_SELECTED and ATSPI_STATE_CHECKED, in the first word.
            return bool(node and node.call("Accessible", "GetState")[0][0] & (1 << 23 | 1 << 4))

        def choose(name):
            node = wait(name, "radio button")
            assert node.call("Component", "GrabFocus")[0]
            time.sleep(0.15)
            shell.click(*point(node))
            eventually(lambda: selected(name), f"{name} not selected")

        def rules_of(channel):
            admin = operator(root, pin)
            try:
                return [
                    (rule.get(5), rule.get(6, [0])[0], rule.get(7, [0])[0], rule.get(2, [1])[0])
                    for rule in local_rules(query_channel(admin, channel))
                ]
            finally:
                admin.close()

        def saved(rules, what):
            save()
            eventually(lambda: rules_of(6) == rules, f"{what} saved other rules")
            eventually(lambda: not text_contains("Unsaved changes"), "the save was not confirmed")

        access = 0x2 | 0x4 | 0x800 | 0x8 | 0x100 | 0x200
        locked = 0x4 | 0x8 | 0x100 | 0x200 | 0x80 | 0x2
        open_acl("Music", expand=False)
        assert selected("Open"), "a channel without rules is not Open"
        # A row set outside the types' templates makes the table custom.
        activate("Registered users · Speak: Yes, not set here")
        eventually(
            lambda: any(label == "Custom" for _, _, label, _ in items(application)),
            "a custom table showed a type",
        )
        assert not any(selected(kind) for kind in ("Open", "Members only", "Hidden"))
        choose("Members only")
        eventually(
            lambda: text_contains("Registered users may join, speak and write."),
            "Members only did not apply",
        )
        shell.screenshot(args.output / "channel-type.png")
        saved([([b"all"], 0, access & ~0x2, 1), ([b"auth"], access, 0, 1)], "Members only")
        mark("channel_type_saves_its_rules_and_reads_back")
        # Password takes the typed password and stores Mumble's two rules.
        type_field("New channel password", "door-code")
        choose("Password")
        saved([([b"all"], 0, locked, 0), ([b"#door-code"], locked, 0, 0)], "Password")
        assert text_contains("Channel password: door-code")
        choose("Open")
        saved([], "Open")
        mark("password_type_uses_the_typed_password")
        activate("Close dialog")
        # A type picked when creating a channel is saved once the server shows it.
        row = eventually(
            lambda: next(
                (
                    node
                    for node, role, name, _ in items(application)
                    if role == "tree item" and name.startswith("Music")
                ),
                None,
            ),
            "missing channel Music",
        )
        assert row.call("Component", "GrabFocus")[0]
        time.sleep(0.2)
        shell.click(*point(row), button=3)
        wait("Create sub-channel…", "menu item").press()
        type_field("Create a channel", "Stage")
        choose("Announcements")
        eventually(
            lambda: text_contains("Only @admin may speak."), "the prompt kept its type"
        )
        shell.screenshot(args.output / "create-channel-type.png")
        activate("Create")
        eventually(lambda: text_contains("Permissions · Stage"), "the new channel did not open")
        eventually(
            lambda: text_contains("Permissions saved and confirmed"),
            "the new channel's type was not saved",
        )
        assert selected("Announcements")
        shell.screenshot(args.output / "created-channel.png")
        lister = operator(root, pin, Listing)
        try:
            stage = lister.channels["Stage"]
        finally:
            lister.close()
        announcements = [([b"all"], 0, 0x8 | 0x100, 1), ([b"admin"], 0x8 | 0x100, 0, 1)]
        assert rules_of(stage) == announcements, rules_of(stage)
        activate("Close dialog")
        mark("channel_created_with_a_type")
        return {"channel_types": cases, "stage": stage, "server": args.server}

    if args.geometry:
        admin = operator(root, pin)
        try:
            acl = admin.acl()
            entries = [raw for raw in acl.get(4, []) if not fields(raw).get(3, [1])[0]]
            for index in range(len(entries), 512):
                entries.append(
                    encode(
                        {
                            1: [1],
                            2: [1],
                            3: [0],
                            5: [
                                (
                                    f"geometry-rule-{index:04}-" + "longunbroken" * 12
                                ).encode()
                            ],
                            6: [0],
                            7: [0],
                        }
                    )
                )
            groups = []
            for raw in acl.get(3, []):
                group = fields(raw)
                group.pop(7, None)
                groups.append(encode(group))
            for index in range(len(groups), 512):
                groups.append(
                    encode(
                        {
                            1: [
                                (
                                    f"geometry-group-{index:04}-" + "longunbroken" * 12
                                ).encode()
                            ],
                            2: [0],
                            3: [1],
                            4: [1],
                        }
                    )
                )
            acl[3], acl[4], acl[5] = groups, entries, [0]
            admin.send(13, encode(acl))
            checked = admin.acl()
            assert len(checked[3]) == len(checked[4]) == 512
            first_rule = fields(checked[4][0])[5][0].decode()
            last_rule = fields(checked[4][-1])[5][0].decode()
            first_group = fields(checked[3][0])[1][0].decode()
            last_group = fields(checked[3][-1])[1][0].decode()
        finally:
            admin.close()
        open_acl()
        assert text_contains("Advanced rules · 512") and text_contains("Groups · 512")
        records = []
        for width, height, zoom in [(1280, 800, 100), (900, 600, 100), (900, 800, 200)]:
            shell.chord("Control_L", "0")
            shell.place("Nuschl", 40, 40, width, height)
            for _ in range(5 if zoom == 200 else 0):
                shell.chord("Control_L", "equal")
                time.sleep(0.1)
            time.sleep(0.3)
            dialog = wait("Details", "dialog")
            record = {"width": width, "height": height, "zoom": zoom, "lists": []}
            for label, first, last in [
                ("ACL rules", "@" + first_rule, "@" + last_rule),
                ("ACL groups", "@" + first_group, "@" + last_group),
            ]:
                owner = wait(label, "list box")
                assert owner.call("Component", "GrabFocus")[0]
                eventually(
                    lambda owner=owner, dialog=dialog: inside(
                        owner.extents(), dialog.extents()
                    ),
                    f"{label} focus not revealed",
                )
                shell.key("Home")
                eventually(
                    lambda first=first: named(first, "push button"),
                    f"first {label} row absent",
                )
                shell.key("End")
                final = eventually(
                    lambda last=last: named(last, "push button"),
                    f"last {label} row absent",
                )
                assert owner.focused, f"{label} lost its retained navigation focus"
                assert inside(final.extents(), dialog.extents()), (
                    f"last {label} row outside modal: {final.extents()} / {dialog.extents()}"
                )
                # Both endpoints are real pointer actions, not synthetic state writes.
                shell.click(*point(final))
                owner = wait(label, "list box")
                assert owner.call("Component", "GrabFocus")[0]
                shell.key("Home")
                initial = eventually(
                    lambda first=first: named(first, "push button"),
                    f"first {label} row not restored",
                )
                shell.click(*point(initial))
                assert owner.call("Component", "GrabFocus")[0]
                shell.key("Page_Down")
                shell.key("End")
                eventually(
                    lambda last=last: named(last, "push button"),
                    f"Page/End {label} did not restore final row",
                )
                rows = [
                    node
                    for node, role, name, _ in items(application)
                    if role == "push button" and name.startswith("@geometry-")
                ]
                assert len(rows) < 32, f"512-row models instantiated {len(rows)} rows"
                record["lists"].append(
                    {
                        "name": label,
                        "owner": owner.extents(),
                        "final_row": final.extents(),
                        "visible_geometry_rows": len(rows),
                    }
                )
            # The input retains native editing keys; list/outer scroll handlers
            # must not steal arrows/Home/End from a focused text field.
            field = type_field("New group name", "unsubmitted-geometry-name")
            shell.key("Home")
            shell.key("End")
            assert field.focused
            save_button = wait("Save permissions", "push button")
            assert save_button.call("Component", "GrabFocus")[0]
            eventually(
                lambda save_button=save_button, dialog=dialog: inside(
                    save_button.extents(), dialog.extents()
                ),
                "Save focus not revealed",
            )
            shell.screenshot(args.output / f"geometry-{width}-{height}-{zoom}.png")
            record["dialog"] = dialog.extents()
            records.append(record)
            (args.output / "geometry.json").write_text(
                json.dumps(records, indent=2) + "\n"
            )
        mark("512_rule_and_group_lists_pointer_keyboard_focus_zoom")
        return {"geometry": records, "model_rows": 512, "server": args.server}

    open_acl()
    stale_save = wait("Save permissions", "push button")
    stale_close = wait("Close dialog", "push button")
    activate("Close dialog")
    open_acl()
    try:
        accepted = stale_save.call("Action", "DoAction", "i", "0")[0]
    except RuntimeError:
        accepted = False
    assert accepted is False, "closed/reopened editor reused an old Save action"
    try:
        closed = stale_close.call("Action", "DoAction", "i", "0")[0]
    except RuntimeError:
        closed = False
    assert closed is False, "old close action discarded a replacement editor"
    assert named("Save permissions", "push button") is not None
    mark("close_reopen_stale_save_refused")
    type_field("New group name", "native-team")
    activate("Create group")
    type_field("Registered group member", "Offline member")
    activate("Resolve member")
    eventually(
        lambda: text_contains(f"Resolved Offline member (#{IDS['offline']})"),
        "fresh member lookup was not pinned",
    )
    activate("Add member")
    save()
    admin = operator(root, pin)
    try:
        acl = admin.acl()
        group = next(
            fields(raw) for raw in acl[3] if fields(raw)[1] == [b"native-team"]
        )
        assert group.get(5) == [IDS["offline"]], group
        assert group.get(3, [1]) == [1] and group.get(4, [1]) == [1], group
    finally:
        admin.close()
    mark("native_root_group_and_offline_member_saved")
    shell.screenshot(args.output / "native-root-group.png")
    activate("Close dialog")
    open_acl("Lobby")
    activate("@native-team · inherited")
    type_field("Registered group member", f"#{IDS['offline']}")
    activate("Exclude inherited member")
    click_matching("Inheritable by subchannels")
    activate("Add rule")
    target_field = type_field("ACL rule target", "native-team")
    activate("Speak: Allow")
    activate("Save permissions")
    eventually(
        lambda: text_contains("Apply or discard the edited rule target"),
        "unapplied displayed group was silently saved as @all",
    )
    click_matching("Inherited · @admin · ")
    assert named("Apply target", "push button") is not None, (
        "selection silently left the edited rule"
    )
    assert target_field.call("Component", "GrabFocus")[0]
    time.sleep(0.2)
    shell.screenshot(args.output / "native-unapplied-target.png")
    (args.output / "target-field-interfaces.json").write_text(
        json.dumps(target_field.call("Accessible", "GetInterfaces"), indent=2) + "\n"
    )
    admin = operator(root, pin)
    try:
        assert not local_rules(query_channel(admin, 1)), (
            "blocked Save wrote an @all rule"
        )
    finally:
        admin.close()
    activate("Apply target")
    type_field("ACL rule target", "never-apply-this-target")
    activate("Discard target edit")
    click_matching("Inherited · @admin · ")
    click_matching("@native-team · Allows")
    mark("unapplied_target_save_and_selection_blocked_until_explicit_apply_or_discard")
    click_matching("Apply to subchannels")
    activate("Add rule")
    activate("Registered user")
    type_field("ACL rule target", "Offline member")
    activate("Resolve account")
    eventually(
        lambda: text_contains(f"Resolved Offline member (#{IDS['offline']})"),
        "fresh rule account did not resolve",
    )
    type_field("ACL rule target", "Control sender")
    activate("Apply target")
    eventually(
        lambda: text_contains("Resolve the registered account first"),
        "changed input reused an earlier resolved principal",
    )
    activate("Add rule")
    eventually(
        lambda: text_contains("Apply or discard the edited rule target"),
        "new rule silently discarded an unresolved target",
    )
    type_field("ACL rule target", "Offline member")
    activate("Resolve account")
    eventually(
        lambda: text_contains(f"Resolved Offline member (#{IDS['offline']})"),
        "replacement account lookup did not resolve",
    )
    activate("Apply target")
    activate("Speak: Deny")
    activate("Move rule up")
    save()
    shell.screenshot(args.output / "native-child-rules.png")
    admin = operator(root, pin)
    try:
        child = query_channel(admin, 1)
        group = next(
            fields(raw) for raw in child[3] if fields(raw)[1] == [b"native-team"]
        )
        assert (
            group.get(2) == [1]
            and group.get(3, [1]) == [1]
            and group.get(4, [1]) == [0]
        ), group
        assert group.get(6) == [IDS["offline"]], group
        rules = local_rules(child)
        assert len(rules) == 2, rules
        assert rules[0].get(4) == [IDS["offline"]] and rules[0].get(7) == [8], rules
        assert rules[0].get(1, [1]) == [1] and rules[0].get(2, [1]) == [1], rules
        assert rules[1].get(5) == [b"native-team"] and rules[1].get(6) == [8], rules
        assert rules[1].get(1, [1]) == [1] and rules[1].get(2, [1]) == [0], rules
    finally:
        admin.close()
    mark("inherited_group_override_and_ordered_rules_saved")
    observer = Client(
        args.binaries, "127.0.0.1", SERVER.port, pin, "Offline member", root / "offline"
    )
    try:
        for channel in (1, 2):
            observer.send(kind="permissions", channel=channel)
            permission = observer.wait(
                lambda event, channel=channel: (
                    event["kind"] == "permissions" and event["channel"] == channel
                )
            )
            assert permission["permissions"] & 8 == 0, permission
        bot.send(kind="permissions", channel=1)
        control = bot.wait(
            lambda event: event["kind"] == "permissions" and event["channel"] == 1
        )
        assert control["permissions"] & 8 != 0, control
    finally:
        observer.close()
    mark("deny_and_subchannel_inheritance_with_unrelated_control")
    # A second operator changes the ACL while the native draft is dirty. Root
    # Write must still allow ACL recovery even when the child's Write is denied.
    activate("Add rule")
    admin = operator(root, pin)
    try:
        current = query_channel(admin, 1)
        current[4] = [
            raw for raw in current.get(4, []) if not fields(raw).get(3, [1])[0]
        ]
        current[4].append(
            encode({1: [1], 2: [1], 3: [0], 4: [IDS["reader"]], 6: [0], 7: [1]})
        )
        local_groups = []
        for raw in current.get(3, []):
            group = fields(raw)
            if (
                group.get(2, [1]) == [1]
                and group.get(3, [1]) == [1]
                and group.get(4, [1]) == [1]
                and not group.get(5)
                and not group.get(6)
            ):
                continue
            group.pop(7, None)
            local_groups.append(encode(group))
        current[3] = local_groups
        current[5] = [0]
        admin.send(13, encode(current))
        assert len(local_rules(query_channel(admin, 1))) == 3
    finally:
        admin.close()
    activate("Save permissions")
    eventually(
        lambda: text_contains("Permissions changed on the server"),
        "dirty ACL conflict was silently overwritten",
    )
    assert text_contains("Unsaved changes")
    activate("Reload and discard draft")
    eventually(
        lambda: (
            text_contains("Edit a local rule or group")
            and not text_contains("Unsaved changes")
        ),
        "explicit ACL reload did not discard the draft",
    )
    activate("Close dialog")
    # The menu may not offer ordinary channel editing, but root administrators
    # retain the separate Edit permissions action and can remove the denial.
    open_acl("Lobby")
    click_matching(f"Welcome reader (#{IDS['reader']}) · ")
    activate("Remove rule")
    save()
    admin = operator(root, pin)
    try:
        assert local_rules(query_channel(admin, 1)) == rules
    finally:
        admin.close()
    mark("dirty_conflict_retained_and_root_admin_recovers_denied_child_acl")
    activate("Close dialog")
    open_acl()
    type_field("New group name", "unsaved-draft")
    activate("Create group")
    pending_save = wait("Save permissions", "push button")
    bot.close()
    SERVER.stop()
    eventually(
        lambda: text_contains("Unsaved draft retained"),
        "disconnect silently discarded the draft",
    )
    SERVER.start()
    eventually(
        lambda: any(
            role == "tree item" and name.startswith("Welcome reader")
            for _, role, name, _ in items(application)
        ),
        "native client did not reconnect",
    )
    eventually(
        lambda: named("@unsaved-draft", "push button"),
        "stale inspector lost its unsaved group",
    )
    try:
        accepted = pending_save.call("Action", "DoAction", "i", "0")[0]
    except RuntimeError:
        accepted = False
    assert accepted is False, "old Save became active on replacement connection"
    mark("dirty_draft_retained_and_stale_reconnect_save_refused")
    shell.screenshot(args.output / "native-stale-draft.png")
    admin = operator(root, pin)
    try:
        persisted = query_channel(admin, 1)
        assert local_rules(persisted) == rules, (
            "local rule order/permissions did not persist"
        )
        root_acl = admin.acl()
        assert all(fields(raw)[1] != [b"unsaved-draft"] for raw in root_acl[3]), (
            "unsent draft appeared on server"
        )
    finally:
        admin.close()
    mark("rules_persist_and_unsent_draft_absent_after_restart")
    if args.reconnect_settle:
        time.sleep(args.reconnect_settle)
    activate("Close dialog")
    open_acl()
    save()
    mark("fresh_editor_after_reconnect")
    # A channel password is stored as Mumble's channel dialog stores it: @all is
    # denied Enter, Speak, Whisper, Text message, Link channel and Traverse here,
    # and the #token group is granted them.
    activate("Close dialog")
    open_acl("Lobby")
    type_field("New channel password", "door-code")
    activate("Set password")
    eventually(lambda: text_contains("Channel password: door-code"), "password not shown")
    save()
    locked = 0x4 | 0x8 | 0x100 | 0x200 | 0x80 | 0x2
    admin = operator(root, pin)
    try:
        password_rules = [
            (rule.get(5), rule.get(6, [0])[0], rule.get(7, [0])[0], rule.get(2, [1])[0])
            for rule in local_rules(query_channel(admin, 1))[-2:]
        ]
    finally:
        admin.close()
    assert password_rules == [([b"all"], 0, locked, 0), ([b"#door-code"], locked, 0, 0)], password_rules
    visitor = Client(args.binaries, "127.0.0.1", SERVER.port, pin, "Visitor", root / "visitor")
    try:
        def may_enter():
            visitor.send(kind="permissions", channel=1)
            event = visitor.wait(lambda event: event["kind"] == "permissions" and event["channel"] == 1)
            return event["permissions"] & 0x4 != 0
        assert not may_enter(), "a visitor without the token may enter"
        visitor.send(kind="tokens", tokens=["door-code"])
        eventually(may_enter, "the token holder may not enter")
    finally:
        visitor.close()
    activate("Remove password")
    eventually(lambda: text_contains("No channel password"), "password not removed")
    save()
    admin = operator(root, pin)
    try:
        assert local_rules(query_channel(admin, 1)) == rules, "removing the password left rules behind"
    finally:
        admin.close()
    mark("channel_password_set_and_removed")
    # A picked built-in group becomes the rule's target once applied; the
    # groups are searched.
    activate("Add rule")
    type_field("Group for this rule", "registered")
    shell.key("Return")
    activate("Apply target")
    wait("Registered users · Sets no permissions", "push button")
    activate("Remove rule")
    mark("group_picked_as_rule_target")
    # Check access evaluates the parents' rules with the draft: an unsaved
    # password denies Everyone Traverse in Lobby, so nothing there is allowed;
    # a reload lets them in again.
    type_field("New channel password", "door-code")
    activate("Set password")
    # Whom to check is searched: offline accounts are listed too, and Enter
    # picks the highlighted match.
    type_field("Check access for", "offline")
    eventually(
        lambda: any(
            role == "list item" and name.startswith("Offline member")
            for _, role, name, _ in items(application)
        ),
        "offline accounts missing from Check access",
    )
    type_field("Check access for", "every")
    shell.key("Return")
    eventually(
        lambda: text_contains("Enter: Not allowed · Cannot pass Lobby"),
        "Check access did not explain the draft",
    )
    assert text_contains("Includes unsaved changes")
    activate("Reload and discard draft")
    eventually(
        lambda: text_contains("Enter: Allowed · "), "Check access kept the discarded draft"
    )
    mark("check_access_explains_the_draft")
    # The permission table writes its rules: denying Registered users Speak in Music
    # saves one rule for @auth, applying here and below.
    activate("Close dialog")
    open_acl("Music")
    eventually(lambda: text_contains("Channel permissions"), "access table missing")
    # Registered users inherit Make temporary from Root, so their row is there.
    shell.screenshot(args.output / "native-access-table-inherited.png")
    # Each cell shows the result; Speak is allowed by Mumble's default until
    # it is set here.
    activate("Registered users · Speak: Yes, not set here")
    activate("Registered users · Speak: Yes, set here")
    wait("Registered users · Speak: No, set here", "push button")
    shell.screenshot(args.output / "native-access-table.png")
    save()
    admin = operator(root, pin)
    try:
        music = [
            (rule.get(5), rule.get(6, [0])[0], rule.get(7, [0])[0], rule.get(2, [1])[0])
            for rule in local_rules(query_channel(admin, 6))
        ]
    finally:
        admin.close()
    assert music == [([b"auth"], 0, 0x8 | 0x100, 1)], music
    mark("access_table_saves_rules")
    # Root's @admin rule grants Write, which brings the rest of Manage channel;
    # removing the own row returns Speak to the default, and closing asks first.
    wait("@admin · Manage channel: Yes, not set here", "push button")
    activate("Remove Registered users")
    wait("Registered users · Speak: Yes, not set here", "push button")
    # Rows are added by searching, as Check access picks.
    type_field("Add a row", "native")
    shell.key("Return")
    matching("@native-team · Speak: ")
    activate("Close dialog")
    eventually(lambda: text_contains("Save your changes?"), "closing did not ask")
    activate("Discard")
    eventually(lambda: not text_contains("Channel permissions"), "Discard kept the editor open")
    mark("unsaved_changes_ask_before_closing")
    # An inherited value shows where it is set: the cell's menu opens Root's
    # permissions there, the cell highlighted. The menu appears once the
    # parents' rules are read.
    open_acl("Music")

    def show_in_root():
        cell = wait("@admin · Manage channel: Yes, not set here", "push button")
        shell.click(*point(cell), button=3)
        try:
            return eventually(lambda: named("Show in Root", "menu item"), "", timeout=1)
        except AssertionError:
            return None

    eventually(show_in_root, "the inherited cell offered no Show in Root").press()
    eventually(lambda: text_contains("Permissions · Root"), "Show in Root did not open Root")
    wait("@admin · Manage channel: Yes, set here", "push button")
    time.sleep(0.3)
    shell.screenshot(args.output / "native-access-jump.png")
    activate("Close dialog")
    mark("inherited_value_shows_its_channel")
    return {
        "fresh_offline_lookup": True,
        "unknown_account_refused": True,
        "close_reopen_stale_save_refused": True,
        "native_group_saved": True,
        "inherited_group_override_saved": True,
        "ordered_group_and_registered_user_rules_saved": True,
        "deny_applies_to_user_and_subchannel": True,
        "unrelated_control_can_speak": True,
        "dirty_conflict_retained": True,
        "root_admin_recovers_denied_child_acl": True,
        "dirty_draft_retained_read_only_after_disconnect": True,
        "stale_reconnect_save_refused": True,
        "persistence_after_restart": True,
        "fresh_editor_after_reconnect": True,
        "channel_password_admits_token_holders": True,
        "group_picked_as_rule_target": True,
        "check_access_explains_the_draft": True,
        "access_table_saves_rules": True,
        "unsaved_changes_ask_before_closing": True,
        "inherited_value_shows_its_channel": True,
        "registered_ids": IDS.copy(),
        "server": args.server,
        "diagnostic_rate_override": args.high_admin_limit,
        "diagnostic_reconnect_settle_seconds": args.reconnect_settle,
    }


def checked_actions(*args):
    try:
        return actions(*args)
    except BaseException:
        options, _root, shell, application, _bot, _server = args
        for log in ("murmur.log", "nuschld.log"):
            source = _root / log
            if source.exists():
                (options.output / log).write_text(source.read_text())
        try:
            shell.screenshot(options.output / "failed-editor.png")
            (options.output / "failed-accessible.json").write_text(
                json.dumps(
                    [(role, name) for _, role, name, _ in items(application)],
                    indent=2,
                )
                + "\n"
            )
        except (RuntimeError, subprocess.SubprocessError, OSError) as error:
            print(f"Could not capture failing editor: {error}")
        raise


def main():
    global ARGS
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", type=Path, required=True)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--at-spi", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--geometry", action="store_true")
    parser.add_argument("--pending-input", action="store_true")
    parser.add_argument("--channel-types", action="store_true")
    parser.add_argument("--server", choices=("stock", "nuschld"), default="stock")
    parser.add_argument(
        "--high-admin-limit",
        action="store_true",
        help="Diagnostic only: raise the owned Murmur control bucket",
    )
    parser.add_argument(
        "--reconnect-settle",
        type=float,
        default=0,
        help="Diagnostic only: wait before the final fresh editor",
    )
    ARGS = parser.parse_args()
    ARGS.document_links, ARGS.expect_markers = True, False
    fixture.WELCOME_EXTRA = ""
    fixture.seed_description = lambda _database: None
    fixture.configure_handler, fixture.run_actions = configure, checked_actions
    welcome_html.Murmur = OwnedMurmur if ARGS.server == "stock" else OwnedNuschld
    welcome_html.run(ARGS)


if __name__ == "__main__":
    main()
