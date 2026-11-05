import contextlib
import importlib.util
import io
import json
import os
import tempfile
import threading
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("ld", os.path.join(HERE, "..", "scripts", "bluetooth-monitor.py"))
ld = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ld)


class FakeNode:
    """Answers HID++ requests from a table keyed by (dev, b2, b3, first param)."""

    def __init__(self, table):
        self.table = table
        self.sent = []
        self.events = []

    def request(self, dev, b2, b3, params=b"", long=False, timeout=1.0, echo=None):
        params = bytes(params)
        self.sent.append((dev, b2, b3, params))
        key = (dev, b2, b3, params[0] if params else None)
        answer = self.table.get(key)
        if answer is None:
            raise ld.HidppError(0x02)
        if isinstance(answer, Exception):
            raise answer
        return bytes(answer)


class PureHelpers(unittest.TestCase):
    def test_dpi_list_with_step_range(self):
        raw = bytes([0x00, 0xC8, 0xE0, 0x32, 0x1F, 0x40, 0, 0])  # 200, step 50 up to 8000
        values = ld.parse_dpi_list(raw)
        self.assertEqual(values[0], 200)
        self.assertEqual(values[-1], 8000)
        self.assertEqual(ld.dpi_range(values), {"min": 200, "max": 8000, "step": 50})

    def test_dpi_list_plain_values(self):
        raw = bytes([0x01, 0x90, 0x03, 0x20, 0x06, 0x40, 0, 0])
        values = ld.parse_dpi_list(raw)
        self.assertEqual(values, [400, 800, 1600])
        self.assertEqual(ld.dpi_range(values)["choices"], [400, 800, 1600])

    def test_voltage_curve(self):
        self.assertEqual(ld.voltage_percent(4200), 100)
        self.assertEqual(ld.voltage_percent(3400), 0)
        self.assertEqual(ld.voltage_percent(3811), 50)
        self.assertTrue(50 < ld.voltage_percent(3840) < 60)

    def test_smartshift_state(self):
        self.assertEqual(ld.smartshift_state(2, 10, True)["mode"], "smartshift")
        self.assertEqual(ld.smartshift_state(2, 0xFF, False)["mode"], "ratchet")
        self.assertEqual(ld.smartshift_state(1, 10, False)["mode"], "freespin")

    def test_clean_strips_markup_and_controls(self):
        self.assertEqual(ld.clean(b"MX <b>Keys\x07\0junk"), "MX bKeys")

    def test_battery_summary_picks_lowest_online(self):
        devs = [
            {"id": "a", "name": "A", "online": True, "battery": {"percent": 80}},
            {"id": "b", "name": "B", "online": True, "battery": {"percent": 30, "charging": True}},
            {"id": "c", "name": "C", "online": False, "battery": {"percent": 5}},
        ]
        self.assertEqual(ld.battery_summary(devs)["id" if False else "device"], "b")
        self.assertIsNone(ld.battery_summary([]))


class Discovery(unittest.TestCase):
    def make(self, root, name, hid_id, desc, parent):
        dev_real = os.path.join(root, "devices", parent, hid_id.replace(":", "_") + name)
        os.makedirs(dev_real)
        with open(os.path.join(dev_real, "uevent"), "w") as f:
            f.write(f"HID_ID={hid_id}\nHID_NAME=Logitech USB Receiver\n")
        with open(os.path.join(dev_real, "report_descriptor"), "wb") as f:
            f.write(desc)
        node = os.path.join(root, "hidraw", name)
        os.makedirs(node)
        os.symlink(dev_real, os.path.join(node, "device"))

    def test_receivers_and_children(self):
        hidpp = b"\x06\x00\xff\x09\x01\xa1\x01\x85\x10\x85\x11"
        with tempfile.TemporaryDirectory() as root:
            self.make(root, "hidraw5", "0003:0000046D:0000C52B", hidpp, "3-6:1.2")
            self.make(root, "hidraw2", "0003:0000046D:0000C548", hidpp, "7-1:1.2")
            self.make(root, "hidraw0", "0003:0000046D:0000C548", b"\x05\x01\x09\x06", "7-1:1.0")
            self.make(root, "hidraw7", "0003:0000046D:00004082", hidpp, "0003:046D:C52B.0008")
            self.make(root, "hidraw9", "0005:0000046D:0000B034", hidpp, "hci0:256")
            self.make(root, "hidraw4", "0003:00001462:00007D70", hidpp, "1-7:1.0")
            nodes = {n["node"]: n for n in ld.discover(os.path.join(root, "hidraw"))}
        self.assertEqual(nodes["hidraw5"]["family"], "unifying")
        self.assertEqual(nodes["hidraw2"]["family"], "bolt")
        self.assertNotIn("hidraw0", nodes)  # no HID++ reports
        self.assertNotIn("hidraw7", nodes)  # DJ child behind the receiver
        self.assertNotIn("hidraw4", nodes)  # not Logitech
        self.assertEqual(nodes["hidraw9"]["bus"], "bluetooth")
        self.assertIsNone(nodes["hidraw9"]["family"])


class Protocol(unittest.TestCase):
    def test_bolt_pairing(self):
        table = {(0xFF, 0x83, 0xB5, 0x50 + s): None for s in range(1, 7)}
        table[(0xFF, 0x83, 0xB5, 0x52)] = [0x52, 0x02, 0x42, 0xB0, 1, 2, 3, 4]
        table[(0xFF, 0x83, 0xB5, 0x62)] = [0x62, 0x01, 0x0B] + list(b"MX Master 4") + [0, 0]
        rec = ld.Receiver(FakeNode({k: v for k, v in table.items() if v}), "bolt")
        self.assertEqual(rec.paired(), [(2, 0xB042, 2, "MX Master 4")])

    def test_unifying_pairing(self):
        table = {
            (0xFF, 0x83, 0xB5, 0x20): [0x20, 0x08, 0x14, 0x40, 0x66, 0, 0, 0x01],
            (0xFF, 0x83, 0xB5, 0x40): [0x40, 0x05] + list(b"Craft"),
        }
        rec = ld.Receiver(FakeNode(table), "unifying")
        self.assertEqual(rec.paired(), [(1, 0x4066, 1, "Craft")])

    def test_offline_device_ping(self):
        node = FakeNode({(1, 0x00, 0x10 | ld.SW_ID, 0): ld.HidppError(ld.ERR_RESOURCE)})
        self.assertIsNone(ld.Device(node, 1).ping())

    def test_unified_battery_and_writes(self):
        sw = ld.SW_ID
        table = {
            (2, 0x00, sw, 0x10): [0x08, 0, 3],          # UNIFIED_BATTERY at index 8
            (2, 0x08, 0x10 | sw, None): [42, 4, 1, 1],  # 42 %, good, charging, external power
            (2, 0x00, sw, 0x22): [0x0C, 0, 0],          # 0x2201 DPI at index 12 (getFeature 0x22,0x01)
            (2, 0x0C, 0x30 | sw, 0): [0, 0x06, 0x40],
        }
        node = FakeNode(table)
        dev = ld.Device(node, 2)
        b = dev.battery()
        self.assertEqual((b["percent"], b["level"], b["charging"]), (42, "good", True))
        dev.set_dpi(1600)
        self.assertEqual(node.sent[-1], (2, 0x0C, 0x30 | sw, bytes([0, 0x06, 0x40])))

    def test_settings_store_roundtrip(self):
        with tempfile.TemporaryDirectory() as d:
            path = os.path.join(d, "x", "settings.json")
            s = ld.SettingsStore(path)
            s.put("b042", "dpi", 1600)
            self.assertEqual(ld.SettingsStore(path).get("b042"), {"dpi": 1600})
            with open(path) as f:
                self.assertEqual(json.load(f), {"b042": {"dpi": 1600}})


UPOWER_DUMP = """Device: /org/freedesktop/UPower/devices/battery_BAT0
  native-path:          BAT0
  power supply:         yes
  battery
    present:             yes
    state:               discharging
    percentage:          80%

Device: /org/freedesktop/UPower/devices/touchpad_dev_AA_BB_CC_DD_EE_FF
  native-path:          /org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF
  model:                Magic Trackpad
  power supply:         no
  touchpad
    present:             yes
    state:               discharging
    warning-level:       none
    percentage:          22%

Device: /org/freedesktop/UPower/devices/headphones_dev_11_22_33_44_55_66
  native-path:          /org/bluez/hci0/dev_11_22_33_44_55_66
  model:                <b>Galaxy Buds</b>
  power supply:         no
  headphones
    present:             yes
    state:               charging
    percentage:          60%

Device: /org/freedesktop/UPower/devices/mouse_hidpp_battery_0
  native-path:          hidpp_battery_0
  model:                MX Master 4
  mouse
    present:             yes
    percentage:          100%

Device: /org/freedesktop/UPower/devices/mouse_hid_aa_bb_cc_dd_ee_01_battery
  native-path:          hid-aa:bb:cc:dd:ee:01-battery
  model:                MX Master 4
  mouse
    present:             yes
    state:               discharging
    percentage:          99%

Device: /org/freedesktop/UPower/devices/gaming_input_dev_3C
  native-path:          /org/bluez/hci0/dev_3C
  model:                Xbox Wireless Controller
  gaming-input
    present:             yes
    state:               discharging
    battery-level:       low
    percentage:          10%

Device: /org/freedesktop/UPower/devices/DisplayDevice
  power supply:         no
  unknown
    percentage:          0%

Daemon:
  daemon-version:  1.91.3
"""


class UPower(unittest.TestCase):
    def test_parse_peripherals(self):
        devs = {d["name"]: d for d in ld.parse_upower(UPOWER_DUMP)}
        self.assertNotIn("BAT0", str(devs))            # laptop battery skipped
        pad = devs["Magic Trackpad"]
        self.assertEqual((pad["kind"], pad["battery"]["percent"], pad["connection"]), ("touchpad", 22, "bluetooth"))
        buds = devs["bGalaxy Buds/b"]
        self.assertEqual(buds["kind"], "headset")
        self.assertTrue(buds["battery"]["charging"])
        pad_level = devs["Xbox Wireless Controller"]["battery"]
        self.assertEqual((pad_level["level"], pad_level["percent"]), ("low", 10))
        # hidpp_battery entries are skipped; the hid-<mac>-battery one is Bluetooth
        self.assertEqual(devs["MX Master 4"]["connection"], "bluetooth")
        self.assertEqual(sum(1 for d in ld.parse_upower(UPOWER_DUMP) if d["name"] == "MX Master 4"), 1)

    def test_merge_skips_devices_hidpp_already_shows(self):
        hidpp = [{"name": "MX Master 4"}]
        merged = ld.merge_upower(hidpp, ld.parse_upower(UPOWER_DUMP))
        self.assertNotIn("MX Master 4", [d["name"] for d in merged])
        self.assertIn("Magic Trackpad", [d["name"] for d in merged])

    def test_empty_dump(self):
        self.assertEqual(ld.parse_upower(""), [])


class Adapters(unittest.TestCase):
    def make(self, root, hci, parent, ids=None, removable=None, maker=""):
        dev = os.path.join(root, "devices", parent, hci + "-if")
        os.makedirs(dev)
        usb = os.path.dirname(dev)
        if ids:
            with open(os.path.join(usb, "idVendor"), "w") as f:
                f.write(ids)
            with open(os.path.join(usb, "removable"), "w") as f:
                f.write(removable)
            with open(os.path.join(usb, "manufacturer"), "w") as f:
                f.write(maker)
        os.makedirs(os.path.join(root, "bt", hci))
        os.symlink(dev, os.path.join(root, "bt", hci, "device"))

    def test_builtin_and_dongle(self):
        with tempfile.TemporaryDirectory() as root:
            # MediaTek combo card on an internal USB port that claims to be removable
            self.make(root, "hci0", "usb3/3-8", "0e8d", "removable", "MediaTek Inc.")
            # CSR USB Bluetooth stick in a front port
            self.make(root, "hci1", "usb1/1-2", "0a12", "removable", "Cambridge Silicon Radio")
            # Realtek chip on a port the firmware marks as fixed (soldered on the board)
            self.make(root, "hci2", "usb1/1-9", "0bda", "fixed", "Realtek")
            # Controller on PCI / UART (no USB parent)
            self.make(root, "hci3", "pci0000:00/0000:00:14.3")
            os.makedirs(os.path.join(root, "bt", "hci0:256"))
            got = {a["id"]: a for a in ld.bt_adapters(os.path.join(root, "bt"))}
        self.assertEqual(sorted(got), ["hci0", "hci1", "hci2", "hci3"])
        self.assertEqual((got["hci0"]["builtin"], got["hci0"]["vendor"]), (True, "MediaTek"))
        self.assertFalse(got["hci1"]["builtin"])
        self.assertTrue(got["hci2"]["builtin"])
        self.assertEqual((got["hci3"]["builtin"], got["hci3"]["bus"]), (True, "pci"))

    def test_adapter_of(self):
        adapters = [{"id": "hci0"}, {"id": "hci1"}]
        self.assertEqual(ld.adapter_of("/org/bluez/hci1/dev_AA_BB", adapters), "hci1")
        self.assertEqual(ld.adapter_of("hid-aa:bb:cc:dd:ee:ff-battery", adapters), "hci0")
        self.assertIsNone(ld.adapter_of("x", []))


class Limits(unittest.TestCase):
    def test_flooding_node_is_drained_within_budget(self):
        r, w = os.pipe()
        try:
            os.set_blocking(r, False)
            node = ld.Node.__new__(ld.Node)
            node.fd, node.events, node.path = r, [], "fake"
            report = bytes([0x10, 1, 0x41, 0, 0, 0, 0])
            for _ in range(ld.DRAIN_BUDGET + 100):  # more than one drain may read
                os.write(w, report)
            node._read = lambda timeout: os.read(r, 7) if True else None
            node.drain()
            self.assertEqual(len(node.events), ld.MAX_EVENTS)  # kept list is capped
            self.assertTrue(os.read(r, 7))                      # budget left the rest unread
        finally:
            os.close(r)
            os.close(w)

    def test_run_bounded_caps_endless_output(self):
        out = ld.run_bounded(["yes"], timeout=2, limit=5000)
        self.assertEqual(len(out), 5000)

    def test_run_bounded_gives_up_on_silent_child(self):
        import time
        start = time.monotonic()
        self.assertEqual(ld.run_bounded(["sleep", "20"], timeout=0.5), "")
        self.assertLess(time.monotonic() - start, 3)

    def test_untrusted_names_are_plain_text(self):
        dump = UPOWER_DUMP.replace("Magic Trackpad", '<img src="http://x/y.png">Pad\r\nX' + "A" * 200)
        names = [d["name"] for d in ld.parse_upower(dump)]
        for n in names:
            self.assertFalse(set(n) & set("<>&\r\n"))
            self.assertLessEqual(len(n), ld.MAX_NAME)


def bluez_dump(objects):
    def dev(**props):
        types = {bool: "b", str: "s"}
        return {"org.bluez.Device1": {k: {"type": types[type(v)], "data": v} for k, v in props.items()}}
    return json.dumps({"type": "a{oa{sa{sv}}}", "data": [{p: dev(**props) for p, props in objects.items()}]})


class BluezTest(unittest.TestCase):
    DUMP = bluez_dump({
        "/org/bluez/hci0/dev_F8_73_DF_22_16_63": dict(Name="AirPods Max", Alias="AirPods Max", Icon="audio-headphones",
                                                      Connected=True, Paired=True, Address="F8:73:DF:22:16:63"),
        "/org/bluez/hci0/dev_DB_96_17_76_34_5D": dict(Name="MX KEYS S", Alias="MX KEYS S", Icon="input-keyboard",
                                                      Connected=True, Paired=True, Address="DB:96:17:76:34:5D"),
        "/org/bluez/hci0/dev_3C_FA_06_24_D0_C7": dict(Name="Xbox Wireless Controller", Icon="input-gaming",
                                                      Connected=False, Paired=True, Address="3C:FA:06:24:D0:C7"),
        "/org/bluez/hci0/dev_D4_8D_26_7D_9D_85": dict(Name="TV", Connected=True, Paired=False, Address="D4:8D:26:7D:9D:85"),
        "/org/bluez/hci0/dev_11_22_33_44_55_66": dict(Name="Laptop", Icon="computer", Connected=True, Paired=True,
                                                      Address="11:22:33:44:55:66"),
    })

    def test_only_paired_connected_peripherals(self):
        devs = {d["name"]: d for d in ld.parse_bluez(self.DUMP)}
        self.assertEqual(sorted(devs), ["AirPods Max", "MX KEYS S"])
        air = devs["AirPods Max"]
        self.assertEqual((air["kind"], air["key"], air["battery"], air["connection"]),
                         ("headset", "bt:F8:73:DF:22:16:63", None, "bluetooth"))

    def test_merge_skips_devices_shown_elsewhere(self):
        hidpp = [{"name": "MX Keys S"}]
        upower = [{"name": "Other", "native": "/org/bluez/hci0/dev_F8_73_DF_22_16_63"}]
        self.assertEqual(ld.merge_bluez(hidpp + upower, ld.parse_bluez(self.DUMP)), [])

    def test_garbage_and_hostile_input(self):
        for text in ("", "not json", "[]", '{"data": []}', '{"data": [42]}', '{"data": [{"/x": 1}]}'):
            self.assertEqual(ld.parse_bluez(text), [])
        dump = bluez_dump({"/org/bluez/hci0/dev_1": dict(Alias="<img src=x>\x1b[31m" + "A" * 500, Icon="audio-headset",
                                                         Connected=True, Paired=True, Address="AA:BB:CC:DD:EE:FF")})
        name = ld.parse_bluez(dump)[0]["name"]
        self.assertNotIn("<", name)
        self.assertLessEqual(len(name), ld.MAX_NAME)
        many = bluez_dump({"/org/bluez/hci0/dev_%04d" % i: dict(Name="D%d" % i, Icon="input-mouse", Connected=True,
                                                                Paired=True, Address="00:00:00:00:%02X:%02X" % (i // 256, i % 256))
                           for i in range(1000)})
        self.assertLessEqual(len(ld.parse_bluez(many)), ld.MAX_BLUEZ)


class GnomePortFixes(unittest.TestCase):
    """Fixes from the GNOME port's review: restoring the wheel mode, first-seen restore, DPI choices."""

    class FakeDev:
        def __init__(self):
            self.calls = []

        def set_smartshift(self, mode, threshold=None):
            self.calls.append(("smartshift", mode, threshold))

        def set_dpi(self, value):
            self.calls.append(("dpi", value))

    def monitor(self, saved):
        m = ld.Monitor.__new__(ld.Monitor)
        m.store = ld.SettingsStore(path=os.path.join(tempfile.mkdtemp(), "s.json"))
        for k, v in saved.items():
            m.store.data.setdefault("k", {})[k] = v
        return m

    def test_saved_ratchet_survives_a_saved_threshold(self):
        dev = self.FakeDev()
        self.monitor({"smartshift": "ratchet", "threshold": 12}).reapply(dev, "k")
        self.assertEqual(dev.calls, [("smartshift", "ratchet", None)])

    def test_saved_smartshift_keeps_its_threshold(self):
        dev = self.FakeDev()
        self.monitor({"smartshift": "smartshift", "threshold": 12, "dpi": 1600}).reapply(dev, "k")
        self.assertEqual(dev.calls, [("smartshift", "smartshift", 12), ("dpi", 1600)])

    def test_two_commands_in_one_chunk_are_both_handled(self):
        # The panel keeps stdin open: both commands must be handled without more input.
        m = ld.Monitor.__new__(ld.Monitor)
        m.watch_proc, m.nodes, m.opened, m.snapshot, handled = None, {}, False, {}, []
        m.scan, m.emit = (lambda: None), (lambda: None)
        m.handle = lambda line: handled.append(line.strip())
        read_fd, write_fd = os.pipe()
        stdin = os.fdopen(read_fd, "r")  # text mode, like sys.stdin
        worker = threading.Thread(target=m.loop, args=(stdin,), daemon=True)
        worker.start()
        os.write(write_fd, b'{"cmd": "options"}\n{"cmd": "open"}\n')
        deadline = time.monotonic() + 2
        while len(handled) < 2 and time.monotonic() < deadline:
            time.sleep(0.02)
        got = list(handled)
        os.close(write_fd)
        worker.join(2)
        stdin.close()
        self.assertEqual(got, ['{"cmd": "options"}', '{"cmd": "open"}'])

    def test_dpi_snaps_to_choices_and_steps(self):
        self.assertEqual(ld.valid_dpi({"min": 400, "max": 1600, "step": 0, "choices": [400, 800, 1600]}, 1100), 800)
        self.assertEqual(ld.valid_dpi({"min": 200, "max": 8000, "step": 50}, 1234), 1250)
        self.assertEqual(ld.valid_dpi({"min": 200, "max": 8000, "step": 50}, 99999), 8000)
        with self.assertRaises(ValueError):
            ld.valid_dpi(None, 800)


class HostileCommands(unittest.TestCase):
    """Panel commands with bad values or bad JSON never end the helper or reach the device."""

    class RecordingDev:
        def __init__(self):
            self.calls = []

        def __getattr__(self, name):
            if name.startswith("set_"):
                return lambda *a: self.calls.append((name, a))
            raise AttributeError(name)

    def monitor(self, dpi=None, store_dir=None):
        m = ld.Monitor.__new__(ld.Monitor)
        m.store = ld.SettingsStore(path=os.path.join(store_dir or tempfile.mkdtemp(), "s.json"))
        m.dev = self.RecordingDev()
        m.devices = {"d1": m.dev}
        entry = {"id": "d1", "key": "k", "online": True}
        if dpi is not None:
            entry["dpi"] = dpi
        m.snapshot = {"devices": [entry]}
        m.opened, m.other_devices, m.message, m.message_args = False, True, "", []
        return m

    def send(self, m, key, raw_value):
        m.handle('{"cmd": "set", "device": "d1", "key": "%s", "value": %s}' % (key, raw_value))

    def test_bad_dpi_values_are_rejected(self):
        dpi = {"min": 200, "max": 8000, "step": 50}
        for raw in ("null", '"abc"', "1e400", "-1e400", "NaN", "Infinity", "[1]", "{}", "true"):
            m = self.monitor(dpi)
            self.send(m, "dpi", raw)
            self.assertEqual(m.dev.calls, [], raw)
            self.assertEqual(m.message, "errChange", raw)
            self.assertEqual(m.store.get("k"), {}, raw)

    def test_dpi_on_a_device_without_dpi(self):
        for dpi in (None, {}, {"choices": "x"}, {"min": "a", "max": "b"}):
            m = self.monitor(dpi)
            self.send(m, "dpi", "800")
            self.assertEqual(m.dev.calls, [], dpi)
            self.assertEqual(m.message, "errChange", dpi)

    def test_huge_integer_dpi_is_clamped(self):
        m = self.monitor({"min": 200, "max": 8000, "step": 50})
        self.send(m, "dpi", "9" * 400)
        self.assertEqual(m.dev.calls, [("set_dpi", (8000,))])

    def test_bad_threshold_values_are_rejected(self):
        for raw in ("null", '"abc"', "1e400", "NaN", "Infinity", "[1]", "true"):
            m = self.monitor()
            self.send(m, "threshold", raw)
            self.assertEqual(m.dev.calls, [], raw)
            self.assertEqual(m.message, "errChange", raw)

    def test_switches_take_only_booleans(self):
        for key in ("invert", "hires", "fnswap"):
            m = self.monitor()
            self.send(m, key, '"%s"' % ("x" * 1000))
            self.assertEqual(m.dev.calls, [], key)
            self.assertEqual(m.message, "errChange", key)
            self.assertEqual(m.store.get("k"), {}, key)
        m = self.monitor()
        self.send(m, "invert", "true")
        self.assertEqual(m.store.get("k"), {"invert": True})

    def test_bad_wheel_mode_and_saved_threshold(self):
        m = self.monitor()
        self.send(m, "smartshift", '["ratchet"]')
        self.assertEqual(m.dev.calls, [])
        m.store.data["k"] = {"threshold": "zz"}
        self.send(m, "smartshift", '"smartshift"')
        self.assertEqual(m.dev.calls, [("set_smartshift", ("smartshift", None))])

    def test_threshold_is_clamped_before_it_is_saved(self):
        m = self.monitor()
        self.send(m, "threshold", "9" * 400)
        self.assertEqual(m.dev.calls, [("set_smartshift", ("smartshift", 50))])
        self.assertEqual(m.store.get("k")["threshold"], 50)

    def test_unwritable_settings_folder_keeps_the_helper_running(self):
        base = tempfile.mkdtemp()
        blocker = os.path.join(base, "cfg")
        open(blocker, "w").close()  # a file where the settings folder should be
        m = self.monitor(store_dir=blocker)
        self.send(m, "invert", "true")
        self.assertEqual(m.dev.calls, [("set_wheel_bit", (0x04, True))])

    def test_deeply_nested_json_is_ignored(self):
        m = self.monitor()
        m.handle("[" * 200000 + "]" * 200000)
        m.handle('{"cmd": "open"}')
        self.assertTrue(m.opened)

    def test_unknown_types_in_commands(self):
        m = self.monitor()
        for line in ('{"cmd": ["set"]}', '{"cmd": "set", "device": {"a": 1}, "key": [1]}',
                     '{"cmd": "options", "other_devices": [[]]}', '"set"', "1e400", "\ud800"):
            m.handle(line)
        self.assertEqual(m.dev.calls, [])

    def test_odd_settings_files_are_ignored(self):
        d = tempfile.mkdtemp()
        fifo = os.path.join(d, "fifo.json")
        os.mkfifo(fifo)
        nested = os.path.join(d, "nested.json")
        with open(nested, "w") as f:
            f.write("[" * 100000 + "]" * 100000)
        real = os.path.join(d, "real.json")
        with open(real, "w") as f:
            json.dump({"k": {"invert": True}}, f)
        link = os.path.join(d, "link.json")
        os.symlink(real, link)
        for path in (fifo, nested, link):
            self.assertEqual(ld.SettingsStore(path=path).data, {}, path)
        self.assertEqual(ld.SettingsStore(path=real).data, {"k": {"invert": True}})

    def run_loop(self, writes, want):
        m = ld.Monitor.__new__(ld.Monitor)
        m.watch_proc, m.nodes, m.opened, m.snapshot, handled = None, {}, False, {}, []
        m.scan, m.emit = (lambda: None), (lambda: None)
        m.handle = lambda line: handled.append(line)
        read_fd, write_fd = os.pipe()
        stdin = os.fdopen(read_fd, "r")
        worker = threading.Thread(target=m.loop, args=(stdin,), daemon=True)
        worker.start()
        for data in writes:
            os.write(write_fd, data)
            time.sleep(0.05)
        deadline = time.monotonic() + 2
        while len(handled) < want and time.monotonic() < deadline:
            time.sleep(0.02)
        os.close(write_fd)
        worker.join(2)
        stdin.close()
        return handled

    def test_oversized_line_is_dropped_whole(self):
        # Its valid-looking tail must not run as a command once the newline comes.
        junk = b"x" * (ld.MAX_COMMAND + 10)
        got = self.run_loop([junk, b'{"cmd": "set"}', b'\n{"cmd": "open"}\n'], 1)
        self.assertEqual(got, ['{"cmd": "open"}'])
        got = self.run_loop([junk + b'{"cmd": "set"}\n{"cmd": "open"}\n'], 1)
        self.assertEqual(got, ['{"cmd": "open"}'])

    def test_short_device_replies_read_as_zeros(self):
        class Node(ld.Node):
            def __init__(self, reply):
                self.reply, self.fd, self.events = reply, -1, []

            def drain(self):
                pass

            def _read(self, timeout):
                return self.reply

        orig = ld.os.write
        ld.os.write = lambda fd, data: len(data)
        try:
            payload = Node(bytes([ld.SHORT, 1, 5, 0x10 | ld.SW_ID, 7, 8, 9])).request(1, 5, 0x10 | ld.SW_ID)
        finally:
            ld.os.write = orig
        self.assertEqual(len(payload), ld.LONG_LEN - 4)
        self.assertEqual(payload[:3], bytes([7, 8, 9]))
        self.assertEqual(payload[7], 0)

    def test_loop_survives_a_failing_command(self):
        m = ld.Monitor.__new__(ld.Monitor)
        m.watch_proc, m.nodes, m.opened, m.snapshot, handled = None, {}, False, {}, []
        m.scan, m.emit = (lambda: None), (lambda: None)

        def handle(line):
            if "boom" in line:
                raise RuntimeError("boom")
            handled.append(line)
        m.handle = handle
        read_fd, write_fd = os.pipe()
        stdin = os.fdopen(read_fd, "r")
        worker = threading.Thread(target=m.loop, args=(stdin,), daemon=True)
        with contextlib.redirect_stderr(io.StringIO()) as err:
            worker.start()
            os.write(write_fd, b'boom\n{"cmd": "open"}\n')
            deadline = time.monotonic() + 2
            while not handled and time.monotonic() < deadline:
                time.sleep(0.02)
            alive = worker.is_alive()
            os.close(write_fd)
            worker.join(2)
        self.assertIn("command ignored", err.getvalue())
        stdin.close()
        self.assertTrue(alive)
        self.assertEqual(handled, ['{"cmd": "open"}'])


if __name__ == "__main__":
    unittest.main()
