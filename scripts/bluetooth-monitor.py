#!/usr/bin/env python3
"""Logitech receivers and devices, plus other battery-powered peripherals, for the
GNOME top bar (stdlib only).

Speaks Logitech's HID++ protocol directly over /dev/hidraw: lists Unifying, Bolt,
Nano and Lightspeed receivers with their paired devices, plus Logitech devices on
Bluetooth or USB cable, and reads battery, DPI, SmartShift, scroll wheel and Fn-key
settings. Reading /dev/hidraw needs the uaccess rule from scripts/allow-access.sh
(the same rule Solaar installs).

Protocol: one JSON command per line on stdin
  {"cmd": "open"} / {"cmd": "closed"} / {"cmd": "refresh"}
  {"cmd": "set", "device": "<id>", "key": "dpi|smartshift|threshold|invert|hires|fnswap", "value": ...}
and one JSON snapshot per line on stdout.

Other peripherals (Bluetooth mice, keyboards, trackpads, headsets, controllers, ...)
come from UPower (`upower --dump`, refreshed on `upower --monitor` events): battery
only, no settings.

Settings changed from the panel are remembered per device (~/.config/
bluetooth-monitor/settings.json) and written again when the device reconnects,
because some devices forget them when they power off.
"""

import errno
import glob
import json
import math
import os
import select
import signal
import stat
import subprocess
import sys
import tempfile
import time

VENDOR = 0x046D
SW_ID = 0x0E  # our software id in HID++ 2.0 requests (the kernel uses 0x01, Solaar 0x0B)
SHORT, LONG = 0x10, 0x11
SHORT_LEN, LONG_LEN = 7, 20

# Receiver product ids -> family. Unifying, Nano and Lightspeed share the Unifying
# registers; Bolt has its own pairing and name sub-registers.
RECEIVERS = {
    0xC52B: "unifying", 0xC532: "unifying",
    0xC548: "bolt",
    0xC52F: "nano", 0xC531: "nano", 0xC534: "nano", 0xC535: "nano", 0xC537: "nano",
    0xC539: "lightspeed", 0xC53A: "lightspeed", 0xC53D: "lightspeed", 0xC53F: "lightspeed",
    0xC541: "lightspeed", 0xC545: "lightspeed", 0xC547: "lightspeed",
}
FAMILY_NAMES = {
    "unifying": "Unifying receiver", "bolt": "Bolt receiver",
    "nano": "Nano receiver", "lightspeed": "Lightspeed receiver",
}
KINDS = {
    1: "keyboard", 2: "mouse", 3: "numpad", 4: "presenter", 7: "remote", 8: "trackball",
    9: "touchpad", 10: "tablet", 11: "gamepad", 12: "joystick", 13: "headset",
}

# HID++ 1.0 registers (receiver)
REG_CONNECTION = 0x02
REG_INFO = 0xB5
# HID++ 2.0 features
F_ROOT = 0x0000
F_NAME = 0x0005
F_BATTERY = 0x1000
F_VOLTAGE = 0x1001
F_UNIFIED_BATTERY = 0x1004
F_HOSTS_INFO = 0x1815
F_DPI = 0x2201
F_SMARTSHIFT = 0x2110
F_SMARTSHIFT_ENH = 0x2111
F_HIRES_WHEEL = 0x2121
F_FN_INVERSION = 0x40A0
F_FN_INVERSION_NEW = 0x40A2
F_FN_INVERSION_HOST = 0x40A3

ERR_RESOURCE = 0x09        # HID++ 1.0: device not reachable (asleep / switched off)
ERR_CONNECT_FAILED = 0x04
ERR_UNKNOWN_DEVICE = 0x08
ERR_INVALID_SUBID = 0x01   # HID++ 1.0 device answering a 2.0 ping

VOLTAGE_CURVE = [(4186, 100), (4067, 90), (3989, 80), (3922, 70), (3859, 60), (3811, 50),
                 (3778, 40), (3751, 30), (3717, 20), (3671, 10), (3646, 5), (3579, 2), (3500, 0)]

SYSFS_HIDRAW = "/sys/class/hidraw"
MAX_EVENTS = 256          # unsolicited reports kept per node between scans
DRAIN_BUDGET = 512        # reports read per drain; a flooding device cannot stall the loop
CLOSED_INTERVAL = 60.0
OPEN_INTERVAL = 10.0
MAX_NAME = 64
MAX_COMMAND = 65536  # longest panel command line; longer ones are dropped whole


class HidppError(Exception):
    def __init__(self, code, v2=False):
        super().__init__(f"HID++ error 0x{code:02x}")
        self.code = code
        self.v2 = v2


class Timeout(Exception):
    pass


def clean(text, limit=MAX_NAME):
    """Printable, markup-free text from device-supplied bytes or strings."""
    if isinstance(text, (bytes, bytearray)):
        text = bytes(text).split(b"\0", 1)[0].decode("utf-8", "replace")
    out = "".join(c for c in str(text) if c.isprintable() and c not in "<>&")
    return out.strip()[:limit]


def config_dir():
    base = os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config")
    return os.path.join(base, "bluetooth-monitor")


# ---------------------------------------------------------------------------- discovery

def read_text(path, limit=4096):
    try:
        with open(path, "rb") as f:
            return f.read(limit).decode("utf-8", "replace")
    except OSError:
        return ""


def parse_uevent(text):
    out = {}
    for line in text.splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            out[k] = v
    return out


def is_hidpp_descriptor(desc):
    """Vendor page 0xFF00 with the short (0x10) or long (0x11) HID++ report."""
    return b"\x06\x00\xff" in desc and (b"\x85\x10" in desc or b"\x85\x11" in desc)


def discover(sysfs=SYSFS_HIDRAW):
    """HID++ capable hidraw nodes: receivers and directly attached devices.

    Devices behind a receiver (the kernel's DJ children, whose parent is another HID
    device) are skipped; they are reached through their receiver's node."""
    nodes = []
    for entry in sorted(glob.glob(os.path.join(sysfs, "hidraw*"))):
        name = os.path.basename(entry)
        dev = os.path.join(entry, "device")
        ev = parse_uevent(read_text(os.path.join(dev, "uevent")))
        hid_id = ev.get("HID_ID", "")
        try:
            bus, vendor, product = (int(x, 16) for x in hid_id.split(":"))
        except ValueError:
            continue
        if vendor != VENDOR:
            continue
        try:
            with open(os.path.join(dev, "report_descriptor"), "rb") as f:
                desc = f.read(4096)
        except OSError:
            continue
        if not is_hidpp_descriptor(desc):
            continue
        parent = os.path.basename(os.path.dirname(os.path.realpath(dev)))
        if parent.count(":") == 2 and "." in parent:  # e.g. 0003:046D:C52B.0008 -> DJ child
            continue
        family = RECEIVERS.get(product) if bus == 0x03 else None
        nodes.append({
            "node": name,
            "path": "/dev/" + name,
            "bus": "bluetooth" if bus == 0x05 else "usb",
            "product": product,
            "family": family,
            "hid_name": clean(ev.get("HID_NAME", "")),
            "phys": ev.get("HID_PHYS", ""),
        })
    return nodes


# ---------------------------------------------------------------------------- transport

class Node:
    """One open hidraw node speaking HID++."""

    def __init__(self, path):
        self.path = path
        self.fd = os.open(path, os.O_RDWR | os.O_NONBLOCK | os.O_CLOEXEC)
        self.events = []  # unsolicited reports seen while waiting for a reply

    def close(self):
        try:
            os.close(self.fd)
        except OSError:
            pass

    def _read(self, timeout):
        r, _, _ = select.select([self.fd], [], [], max(0.0, timeout))
        if not r:
            return None
        try:
            return os.read(self.fd, 64)
        except BlockingIOError:
            return None

    def note_event(self, data):
        if len(self.events) < MAX_EVENTS:
            self.events.append(data)

    def drain(self):
        for _ in range(DRAIN_BUDGET):
            data = self._read(0)
            if not data:
                return
            self.note_event(data)

    def request(self, dev, b2, b3, params=b"", long=False, timeout=1.5, echo=None):
        """Send [report, dev, b2, b3, params] and return the reply's payload (bytes 4..)."""
        params = bytes(params)
        long = long or len(params) > 3
        size = LONG_LEN if long else SHORT_LEN
        msg = bytes([LONG if long else SHORT, dev, b2, b3]) + params
        msg = msg + b"\0" * (size - len(msg))
        self.drain()
        os.write(self.fd, msg)
        deadline = time.monotonic() + timeout
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                raise Timeout()
            r = self._read(left)
            if not r or len(r) < 7 or r[0] not in (SHORT, LONG):
                if r:
                    self.note_event(r)
                continue
            if r[1] != dev and r[1] != dev ^ 0xFF:
                self.note_event(r)
                continue
            if r[2] == 0x8F and r[3] == b2 and r[4] == b3:
                raise HidppError(r[5])
            if r[2] == 0xFF and r[3] == b2 and r[4] == b3:
                raise HidppError(r[5], v2=True)
            if r[2] == b2 and r[3] == b3:
                if echo is not None and r[4] != echo:
                    continue
                # Padded to a long report's payload, so a short or truncated reply reads
                # as zeros instead of raising IndexError in the field parsers.
                return bytes(r[4:]).ljust(LONG_LEN - 4, b"\0")
            self.note_event(r)


class Receiver:
    def __init__(self, node, family):
        self.node = node
        self.family = family

    def read_register(self, reg, params=b"", long=True):
        sub = 0x83 if long else 0x81
        params = bytes(params)
        return self.node.request(0xFF, sub, reg, params + b"\0" * (3 - len(params)),
                                 echo=params[0] if params else None, timeout=1.0)

    def connected_count(self):
        try:
            return self.read_register(REG_CONNECTION, long=False)[1]
        except (HidppError, Timeout):
            return None

    def paired(self):
        """[(slot, wpid, kind, codename)] for every occupied pairing slot."""
        out = []
        for slot in range(1, 7):
            try:
                if self.family == "bolt":
                    info = self.read_register(REG_INFO, [0x50 + slot])
                    kind = info[1] & 0x0F
                    wpid = (info[3] << 8) | info[2]
                    try:
                        nm = self.read_register(REG_INFO, [0x60 + slot, 0x01])
                        codename = clean(nm[3:3 + min(14, nm[2])])
                    except (HidppError, Timeout):
                        codename = ""
                else:
                    info = self.read_register(REG_INFO, [0x20 + slot - 1])
                    wpid = (info[3] << 8) | info[4]
                    kind = info[7] & 0x0F
                    try:
                        nm = self.read_register(REG_INFO, [0x40 + slot - 1])
                        codename = clean(nm[2:2 + min(16, nm[1])])
                    except (HidppError, Timeout):
                        codename = ""
            except (HidppError, Timeout):
                continue
            if wpid in (0, 0xFFFF):
                continue
            out.append((slot, wpid, kind, codename))
        return out


class Device:
    """A HID++ 2.0 device: index 1..6 behind a receiver, 0xFF when attached directly."""

    def __init__(self, node, index):
        self.node = node
        self.index = index
        self.features = {}

    def ping(self):
        """Protocol version (e.g. 4.5), or None while the device is unreachable."""
        mark = int(time.monotonic() * 1000) & 0xFF
        try:
            r = self.node.request(self.index, 0x00, 0x10 | SW_ID, [0, 0, mark], timeout=2.0)
        except HidppError as e:
            if e.code == ERR_INVALID_SUBID and not e.v2:
                return 1.0
            return None
        except Timeout:
            return None
        return r[0] + r[1] / 10.0

    def feature(self, fid):
        if fid not in self.features:
            try:
                r = self.node.request(self.index, 0x00, 0x00 | SW_ID, [fid >> 8, fid & 0xFF, 0])
                self.features[fid] = r[0] or None
            except HidppError:
                self.features[fid] = None
        return self.features[fid]

    def call(self, fid, fn, params=b""):
        idx = self.feature(fid)
        if not idx:
            raise HidppError(0x02, v2=True)
        return self.node.request(self.index, idx, (fn << 4) | SW_ID, params, timeout=2.0)

    def has(self, fid):
        try:
            return bool(self.feature(fid))
        except Timeout:
            return False

    # -------------------------------------------------------------- reads

    def name(self):
        if not self.has(F_NAME):
            return ""
        length = self.call(F_NAME, 0)[0]
        name = b""
        while len(name) < min(length, MAX_NAME):
            chunk = self.call(F_NAME, 1, [len(name)])
            name += bytes(chunk[:length - len(name)])
            if not chunk.strip(b"\0"):
                break
        return clean(name)

    def battery(self):
        if self.has(F_UNIFIED_BATTERY):
            r = self.call(F_UNIFIED_BATTERY, 1)
            levels = {8: "full", 4: "good", 2: "low", 1: "critical"}
            status = {0: "discharging", 1: "charging", 2: "charging", 3: "full", 4: "error"}.get(r[2], "unknown")
            return {"percent": r[0] or None, "level": levels.get(r[1], "empty"),
                    "status": status, "charging": r[2] in (1, 2), "external_power": bool(r[3])}
        if self.has(F_BATTERY):
            r = self.call(F_BATTERY, 0)
            status = {0: "discharging", 1: "charging", 2: "charging", 3: "full", 4: "charging",
                      5: "error", 6: "error"}.get(r[2], "unknown")
            return {"percent": r[0] or None, "level": level_for(r[0]), "status": status,
                    "charging": r[2] in (1, 2, 4), "external_power": r[2] in (1, 2, 3, 4)}
        if self.has(F_VOLTAGE):
            r = self.call(F_VOLTAGE, 0)
            mv = (r[0] << 8) | r[1]
            flags = r[2]
            charging = bool(flags & 0x80)
            full = charging and (flags & 0x03) == 0x01
            pct = 100 if full else voltage_percent(mv)
            return {"percent": pct, "level": level_for(pct), "voltage": mv,
                    "status": "full" if full else ("charging" if charging else "discharging"),
                    "charging": charging and not full, "external_power": charging}
        return None

    def dpi(self):
        if not self.has(F_DPI):
            return None
        raw = bytes(self.call(F_DPI, 1, [0])[1:])
        choices = parse_dpi_list(raw)
        r = self.call(F_DPI, 2, [0])
        value = (r[1] << 8) | r[2]
        if not value:
            value = (r[3] << 8) | r[4]
        default = (r[3] << 8) | r[4]
        return dict(dpi_range(choices), value=value, default=default)

    def smartshift(self):
        if self.has(F_SMARTSHIFT_ENH):
            r = self.call(F_SMARTSHIFT_ENH, 1)
            enhanced = True
        elif self.has(F_SMARTSHIFT):
            r = self.call(F_SMARTSHIFT, 0)
            enhanced = False
        else:
            return None
        return smartshift_state(r[0], r[1], enhanced)

    def wheel(self):
        if not self.has(F_HIRES_WHEEL):
            return None
        mode = self.call(F_HIRES_WHEEL, 1)[0]
        return {"invert": bool(mode & 0x04), "hires": bool(mode & 0x02), "raw": mode}

    def fn_feature(self):
        for fid in (F_FN_INVERSION_HOST, F_FN_INVERSION_NEW, F_FN_INVERSION):
            if self.has(fid):
                return fid
        return None

    def host_prefix(self):
        try:
            if self.has(F_HOSTS_INFO):
                return self.call(F_HOSTS_INFO, 0)[3]
        except (HidppError, Timeout):
            pass
        return 0xFF

    def fnswap(self):
        fid = self.fn_feature()
        if not fid:
            return None
        if fid == F_FN_INVERSION_HOST:
            r = self.call(fid, 0, [self.host_prefix()])
            return {"on": bool(r[1] & 0x01)}
        return {"on": bool(self.call(fid, 0)[0] & 0x01)}

    # -------------------------------------------------------------- writes

    def set_dpi(self, value):
        value = int(value)
        self.call(F_DPI, 3, [0, value >> 8, value & 0xFF])

    def set_smartshift(self, mode, threshold=None):
        """mode: 'smartshift' (ratchet that frees itself), 'ratchet' or 'freespin'."""
        wheel_mode = 1 if mode == "freespin" else 2
        if mode == "ratchet":
            auto = 0xFF
        elif mode == "smartshift":
            auto = max(1, min(50, int(threshold or 10)))
        else:
            auto = 0  # keep the stored threshold
        if self.has(F_SMARTSHIFT_ENH):
            self.call(F_SMARTSHIFT_ENH, 2, [wheel_mode, auto, 0])
        else:
            self.call(F_SMARTSHIFT, 1, [wheel_mode, auto, 0])

    def set_wheel_bit(self, bit, on):
        mode = self.call(F_HIRES_WHEEL, 1)[0]
        mode = (mode | bit) if on else (mode & ~bit)
        self.call(F_HIRES_WHEEL, 2, [mode & 0xFF])

    def set_fnswap(self, on):
        fid = self.fn_feature()
        if fid == F_FN_INVERSION_HOST:
            self.call(fid, 1, [self.host_prefix(), 1 if on else 0])
        elif fid:
            self.call(fid, 1, [1 if on else 0])


# ---------------------------------------------------------------------------- pure helpers

def level_for(pct):
    if pct is None:
        return "unknown"
    if pct <= 5:
        return "critical"
    if pct <= 20:
        return "low"
    if pct >= 90:
        return "full"
    return "good"


def voltage_percent(mv):
    if mv >= VOLTAGE_CURVE[0][0]:
        return 100
    if mv <= VOLTAGE_CURVE[-1][0]:
        return 0
    for (hv, hp), (lv, lp) in zip(VOLTAGE_CURVE, VOLTAGE_CURVE[1:]):
        if lv <= mv <= hv:
            return round(lp + (hp - lp) * (mv - lv) / (hv - lv))
    return 0


def parse_dpi_list(raw):
    """DPI values from getSensorDpiList: plain 16-bit values, or 0xE000|step followed
    by the range's last value; 0 ends the list."""
    values = []
    i = 0
    while i + 1 < len(raw):
        val = (raw[i] << 8) | raw[i + 1]
        if val == 0:
            break
        if val >> 13 == 0b111:
            step = val & 0x1FFF
            if i + 3 >= len(raw) or not values or step <= 0:
                break
            last = (raw[i + 2] << 8) | raw[i + 3]
            values.extend(range(values[-1] + step, last + 1, step))
            i += 4
        else:
            values.append(val)
            i += 2
    return values[:2000]


def dpi_range(choices):
    """min/max/step for an evenly spaced list (what the panel slider needs), else the list."""
    if not choices:
        return {"choices": []}
    steps = {b - a for a, b in zip(choices, choices[1:])}
    if len(steps) <= 1:
        return {"min": choices[0], "max": choices[-1], "step": steps.pop() if steps else 1}
    return {"min": choices[0], "max": choices[-1], "step": 0, "choices": choices}


def whole_number(value):
    """A whole number from a panel command: JSON numbers only (no booleans, strings,
    NaN or infinity), so a hostile value fails with ValueError and nothing else."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError("not a number")
    if isinstance(value, float):
        if not math.isfinite(value):
            raise ValueError("not a number")
        value = round(value)
    return value


def switch_value(value):
    if not isinstance(value, bool):
        raise ValueError("not on/off")
    return value


def valid_dpi(dpi, value):
    """A DPI the device offers: one of its listed choices, or a step inside its range."""
    if not isinstance(dpi, dict):
        raise ValueError("no DPI setting")
    value = whole_number(value)
    choices = [c for c in dpi.get("choices") or [] if isinstance(c, int)]
    if choices:
        return min(choices, key=lambda c: abs(c - value))
    low, high, step = int(dpi.get("min", 0)), int(dpi.get("max", 0)), int(dpi.get("step") or 1)
    if high <= 0:
        raise ValueError("no DPI range")
    value = max(low, min(high, value))
    return low + round((value - low) / step) * step


def smartshift_state(wheel_mode, auto, enhanced):
    if wheel_mode == 1:
        mode = "freespin"
    elif auto in (0, 0xFF):
        mode = "ratchet"
    else:
        mode = "smartshift"
    return {"mode": mode, "threshold": auto if 0 < auto < 0xFF else None, "enhanced": enhanced}


def battery_summary(devices):
    """Lowest battery among online devices that report one."""
    lows = [d for d in devices if d.get("online") and d.get("battery") and d["battery"].get("percent") is not None]
    if not lows:
        return None
    low = min(lows, key=lambda d: d["battery"]["percent"])
    return {"device": low["id"], "name": low["name"], "percent": low["battery"]["percent"],
            "charging": low["battery"].get("charging", False)}


# ---------------------------------------------------------------------------- Bluetooth adapters

SYSFS_BLUETOOTH = "/sys/class/bluetooth"
# USB vendors whose Bluetooth chips sit on internal M.2 / mainboard Wi-Fi + Bluetooth cards
# (they hang off an internal USB port that firmware often still reports as removable).
INTERNAL_BT_VENDORS = {
    0x8087: "Intel", 0x0E8D: "MediaTek", 0x13D3: "AzureWave", 0x0489: "Foxconn",
    0x0CF3: "Qualcomm Atheros", 0x04CA: "Lite-On", 0x105B: "Foxconn",
}


def bt_adapters(sysfs=SYSFS_BLUETOOTH):
    """[{id, builtin, bus, vendor}] for every Bluetooth controller (hci0, hci1, ...)."""
    out = []
    for entry in sorted(glob.glob(os.path.join(sysfs, "hci*"))):
        name = os.path.basename(entry)
        if ":" in name:  # connection objects like hci0:256
            continue
        dev = os.path.realpath(os.path.join(entry, "device"))
        usb = os.path.dirname(dev) if os.path.exists(os.path.join(os.path.dirname(dev), "idVendor")) else None
        vendor = ""
        if usb:
            try:
                vid = int(read_text(os.path.join(usb, "idVendor"), 16).strip() or "0", 16)
            except ValueError:
                vid = 0
            maker = clean(read_text(os.path.join(usb, "manufacturer"), 128), 40)
            vendor = INTERNAL_BT_VENDORS.get(vid) or maker
            removable = read_text(os.path.join(usb, "removable"), 32).strip()
            builtin = removable == "fixed" or vid in INTERNAL_BT_VENDORS
            bus = "usb"
        else:
            builtin, bus = True, "pci" if "/pci" in dev else "other"
        for suffix in (" Inc.", " Inc", " Corp.", " Corporation", " Co., Ltd.", ", Inc."):
            if vendor.endswith(suffix):
                vendor = vendor[: -len(suffix)]
        out.append({"id": name, "builtin": builtin, "bus": bus, "vendor": vendor})
    return out


def adapter_of(native_path, adapters):
    """The hci id a UPower device belongs to (/org/bluez/hci0/dev_...), else the first one."""
    if native_path.startswith("/org/bluez/"):
        part = native_path.split("/")[3] if len(native_path.split("/")) > 3 else ""
        if part.startswith("hci"):
            return part
    return adapters[0]["id"] if adapters else None


# ---------------------------------------------------------------------------- UPower

# UPower device types worth showing; laptop batteries, mains and UPS are not peripherals.
UPOWER_KINDS = {
    "mouse": "mouse", "keyboard": "keyboard", "touchpad": "touchpad", "tablet": "tablet",
    "pen": "pen", "gaming-input": "gamepad", "headset": "headset", "headphones": "headset",
    "speakers": "speaker", "other-audio": "speaker", "remote-control": "remote",
    "phone": "phone", "media-player": "media", "wearable": "wearable", "toy": "toy",
    "bluetooth-generic": "device", "camera": "camera", "printer": "device",
    "scanner": "device", "video": "device", "modem": "device", "network": "device",
}
UPOWER_TYPES = set(UPOWER_KINDS) | {"line-power", "battery", "ups", "monitor", "computer", "pda", "unknown"}
UPOWER_LEVELS = {"critical": "critical", "low": "low", "normal": "good", "high": "good", "full": "full"}
MAX_DUMP = 512 * 1024


def run_bounded(argv, timeout=3.0, limit=MAX_DUMP):
    """stdout of argv, read with one overall deadline and a byte cap; the child is
    killed when either runs out, so nothing beyond `limit` is ever buffered."""
    try:
        proc = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                stdin=subprocess.DEVNULL, preexec_fn=die_with_parent)
    except OSError:
        return ""
    out = bytearray()
    deadline = time.monotonic() + timeout
    fd = proc.stdout.fileno()
    try:
        while len(out) < limit:
            left = deadline - time.monotonic()
            if left <= 0:
                break
            r, _, _ = select.select([fd], [], [], left)
            if not r:
                break
            chunk = os.read(fd, min(65536, limit - len(out)))
            if not chunk:
                break
            out += chunk
    finally:
        if proc.poll() is None:
            proc.kill()
        proc.wait()
        proc.stdout.close()
    return bytes(out).decode("utf-8", "replace")


def parse_upower(text):
    """Peripherals with a battery from `upower --dump`."""
    out = []
    for block in text.split("\nDevice: ")[0:]:
        block = block[len("Device: "):] if block.startswith("Device: ") else block
        lines = block.splitlines()
        if not lines or not lines[0].startswith("/org/freedesktop/UPower/devices/"):
            continue
        path = lines[0].strip()
        if path.endswith("/DisplayDevice"):
            continue
        fields, dtype = {}, None
        for line in lines[1:]:
            stripped = line.strip()
            if not stripped:
                continue
            if ":" in stripped:
                k, v = stripped.split(":", 1)
                fields[k.strip()] = v.strip()
            elif dtype is None and stripped in UPOWER_TYPES:
                dtype = stripped
        kind = UPOWER_KINDS.get(dtype or "")
        if not kind:
            continue
        if fields.get("present", "yes") != "yes":
            continue
        native = fields.get("native-path", "")
        if native.startswith("hidpp_battery"):  # Logitech via the kernel: HID++ covers it
            continue
        pct = None
        m = fields.get("percentage", "").rstrip("%").split(".")[0].split()
        if m and m[0].isdigit():
            pct = max(0, min(100, int(m[0])))
        coarse = UPOWER_LEVELS.get(fields.get("battery-level", "none"))
        state = fields.get("state", "unknown")
        if pct is None and not coarse:
            continue
        if pct == 0 and coarse is None and state in ("unknown", ""):
            continue  # no real reading yet
        name = clean(fields.get("model", "")) or clean(fields.get("vendor", "")) or kind.capitalize()
        bluetooth = native.startswith("/org/bluez/") or "bluez" in native or \
            (native.startswith("hid-") and native.count(":") == 5)
        charging = state in ("charging", "pending-charge")
        out.append({
            "id": "up-" + path.rsplit("/", 1)[-1],
            "key": "up:" + clean(fields.get("serial", "") or native or path, 120),
            "kind": kind,
            "name": name,
            "online": True,
            "source": "upower",
            "battery": {
                "percent": pct if coarse is None else (pct or None),
                "level": coarse or level_for(pct),
                "status": "full" if state == "fully-charged" else ("charging" if charging else "discharging"),
                "charging": charging,
                "external_power": charging or state == "fully-charged",
            },
            "receiver": None, "slot": None, "native": native,
            "connection": "bluetooth" if bluetooth else "wireless",
        })
    return out


def merge_upower(hidpp_devices, upower_devices):
    """UPower entries that are not already shown through HID++ (same name)."""
    known = {d["name"].casefold() for d in hidpp_devices if d.get("name")}
    return [d for d in upower_devices if d["name"].casefold() not in known]


# ---------------------------------------------------------------------------- BlueZ

# Connected Bluetooth peripherals that report no battery anywhere (AirPods and other
# Apple headphones keep theirs to Apple's own protocol), by BlueZ's device icon.
BLUEZ_KINDS = {
    "audio-headphones": "headset", "audio-headset": "headset", "audio-card": "speaker",
    "input-keyboard": "keyboard", "input-mouse": "mouse", "input-tablet": "tablet",
    "input-gaming": "gamepad", "phone": "phone", "camera-video": "camera",
    "camera-photo": "camera", "multimedia-player": "media", "": "device",
}
MAX_BLUEZ = 256
BLUEZ_DUMP = ["busctl", "--system", "--json=short", "call", "org.bluez", "/",
              "org.freedesktop.DBus.ObjectManager", "GetManagedObjects"]


def parse_bluez(text):
    """Paired, connected peripherals from BlueZ's GetManagedObjects (busctl JSON)."""
    try:
        objects = json.loads(text)["data"][0]
    except (ValueError, KeyError, IndexError, TypeError):
        return []
    if not isinstance(objects, dict):
        return []
    out = []
    for path in sorted(objects)[:MAX_BLUEZ]:
        ifaces = objects[path]
        dev = ifaces.get("org.bluez.Device1") if isinstance(ifaces, dict) else None
        if not isinstance(dev, dict):
            continue
        def val(key, default=None):
            v = dev.get(key)
            return v.get("data", default) if isinstance(v, dict) else default
        if val("Paired") is not True or val("Connected") is not True:
            continue
        icon = val("Icon", "")
        kind = BLUEZ_KINDS.get(icon if isinstance(icon, str) else "")
        if not kind:
            continue  # computers, network gear and the like are no peripherals
        address = clean(str(val("Address", "") or ""), 17)
        name = clean(str(val("Alias", "") or val("Name", "") or ""))
        if not address or not name:
            continue
        out.append({
            "id": "bt-" + address.replace(":", ""),
            "key": "bt:" + address.upper(),
            "kind": kind, "name": name, "online": True, "source": "bluez",
            "battery": None, "receiver": None, "slot": None,
            "native": clean(str(path), 120), "connection": "bluetooth",
        })
    return out


def merge_bluez(devices, bluez_devices):
    """BlueZ entries not already shown through HID++ or UPower (same name or same BlueZ path)."""
    names = {d["name"].casefold() for d in devices if d.get("name")}
    paths = {d.get("native") for d in devices if d.get("native")}
    return [d for d in bluez_devices if d["name"].casefold() not in names and d["native"] not in paths]


# ---------------------------------------------------------------------------- settings store

class SettingsStore:
    """Per-device settings the user changed in the panel, keyed by device key."""

    def __init__(self, path=None):
        self.path = path or os.path.join(config_dir(), "settings.json")
        self.data = {}
        try:
            raw = json.loads(self.read_own_file(self.path, 256 * 1024).decode("utf-8"))
            if isinstance(raw, dict):
                self.data = {str(k): v for k, v in raw.items() if isinstance(v, dict)}
        except (OSError, ValueError, RecursionError):
            pass

    @staticmethod
    def read_own_file(path, limit):
        """A regular file of ours, not a symlink, FIFO or device (those would hang or
        mislead the start-up)."""
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
        try:
            st = os.fstat(fd)
            if not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid():
                raise OSError("not a settings file")
            return os.read(fd, limit)
        finally:
            os.close(fd)

    def get(self, key):
        return dict(self.data.get(key, {}))

    def put(self, key, field, value):
        entry = self.data.setdefault(key, {})
        entry[field] = value
        self.save()

    def save(self):
        """Best effort: an unwritable settings folder only means the change is not kept."""
        d = os.path.dirname(self.path)
        try:
            os.makedirs(d, mode=0o700, exist_ok=True)
            fd, tmp = tempfile.mkstemp(prefix=".settings-", dir=d)
        except OSError:
            return
        try:
            with os.fdopen(fd, "w") as f:
                json.dump(self.data, f, indent=2, sort_keys=True)
            os.replace(tmp, self.path)
        except OSError:
            try:
                os.unlink(tmp)
            except OSError:
                pass


# ---------------------------------------------------------------------------- monitor

def die_with_parent():
    """Child setup: get SIGTERM when this helper dies, even if it is killed outright."""
    try:
        import ctypes
        ctypes.CDLL(None, use_errno=True).prctl(1, signal.SIGTERM)  # PR_SET_PDEATHSIG
    except (OSError, AttributeError):
        pass


class Monitor:
    def __init__(self, out=sys.stdout):
        self.out = out
        self.nodes = {}        # path -> Node
        self.devices = {}      # device id -> Device (feature cache)
        self.online = {}       # device id -> last known online state
        self.store = SettingsStore()
        self.opened = False
        self.other_devices = True  # UPower peripherals; the panel's settings switch this
        self.snapshot = {"phase": "starting", "receivers": [], "devices": []}
        self.message = ""
        self.message_args = []

    def node(self, path):
        n = self.nodes.get(path)
        if n is None:
            n = Node(path)
            self.nodes[path] = n
        return n

    def drop(self, path):
        n = self.nodes.pop(path, None)
        if n:
            n.close()
        for did in [k for k, d in self.devices.items() if d.node is n]:
            self.devices.pop(did, None)
            self.online.pop(did, None)  # plugged in again: restore its settings

    def device_for(self, did, node, index):
        d = self.devices.get(did)
        if d is None or d.node is not node:
            d = Device(node, index)
            self.devices[did] = d
        return d

    def scan(self):
        found = discover()
        receivers, devices = [], []
        denied = []
        present = set()
        for info in found:
            path = info["path"]
            present.add(path)
            try:
                node = self.node(path)
            except PermissionError:
                denied.append(info["node"])
                continue
            except OSError:
                continue
            try:
                if info["family"]:
                    rec_id = "r-" + info["node"]
                    rec = Receiver(node, info["family"])
                    slots = rec.paired()
                    rec_entry = {
                        "id": rec_id, "family": info["family"],
                        "name": FAMILY_NAMES[info["family"]],
                        "node": info["node"], "max_devices": 6,
                        "connected": rec.connected_count(), "devices": [],
                    }
                    for slot, wpid, kind, codename in slots:
                        did = f"{info['node']}-{slot}"
                        dev = self.device_for(did, node, slot)
                        entry = self.read_device(dev, did, wpid, KINDS.get(kind, "device"), codename)
                        entry.update({"receiver": rec_id, "slot": slot, "connection": info["family"]})
                        rec_entry["devices"].append(did)
                        devices.append(entry)
                    receivers.append(rec_entry)
                else:
                    did = f"{info['node']}-direct"
                    dev = self.device_for(did, node, 0xFF)
                    kind = "device"
                    entry = self.read_device(dev, did, info["product"], kind, info["hid_name"])
                    entry.update({"receiver": None, "slot": None,
                                  "connection": "bluetooth" if info["bus"] == "bluetooth" else "usb"})
                    devices.append(entry)
            except OSError as e:
                if e.errno in (errno.ENODEV, errno.EIO, errno.EBADF):
                    self.drop(path)
                else:
                    raise
        for path in list(self.nodes):
            if path not in present:
                self.drop(path)
        for n in self.nodes.values():
            n.events.clear()  # replies to our own requests are not news
        if self.other_devices:
            devices.extend(merge_upower(devices, parse_upower(run_bounded(["upower", "--dump"]))))
            devices.extend(merge_bluez(devices, parse_bluez(run_bounded(BLUEZ_DUMP))))
        adapters = bt_adapters()
        for d in devices:
            if d.get("connection") == "bluetooth":
                d["adapter"] = adapter_of(d.pop("native", ""), adapters)
            d.pop("native", None)
        if denied and not receivers and not devices:
            phase = "permission"
        elif not found and not devices:
            phase = "none"
        else:
            phase = "ok"
        self.snapshot = {
            "phase": phase,
            "denied": denied,
            "receivers": receivers,
            "adapters": adapters,
            "devices": devices,
            "lowest": battery_summary(devices),
            "updated": int(time.time()),
        }

    def read_device(self, dev, did, wpid, kind, codename):
        key = f"{wpid:04x}"
        entry = {"id": did, "key": key, "wpid": key, "kind": kind,
                 "name": codename, "online": False}
        proto = dev.ping()
        was_online = self.online.get(did)
        self.online[did] = proto is not None
        if proto is None:
            return entry
        entry["online"] = True
        entry["protocol"] = proto
        if proto < 2.0:
            return entry
        # The first time a device is seen (helper start, receiver plugged in) and every time
        # it comes back: after a reboot it is already on when the helper starts.
        if was_online is not True:
            self.reapply(dev, key)
        for field, reader in (("name", dev.name), ("battery", dev.battery), ("dpi", dev.dpi),
                              ("smartshift", dev.smartshift), ("wheel", dev.wheel), ("fnswap", dev.fnswap)):
            try:
                value = reader()
            except (HidppError, Timeout):
                continue
            if field == "name":
                if value:
                    entry["name"] = value
            elif value is not None:
                entry[field] = value
        return entry

    def reapply(self, dev, key):
        """Write the remembered settings again after the device reconnects.

        The wheel mode and its sensitivity go out as one write: applied one after the
        other, a saved threshold would switch a saved ratchet or free-spin mode back to
        SmartShift (and a SmartShift mode alone would lose the threshold)."""
        saved = self.store.get(key)
        mode, threshold = saved.pop("smartshift", None), saved.pop("threshold", None)
        try:
            if mode in ("smartshift", "ratchet", "freespin"):
                dev.set_smartshift(mode, threshold if mode == "smartshift" else None)
            elif threshold is not None:
                dev.set_smartshift("smartshift", whole_number(threshold))
        except (HidppError, Timeout, ValueError, TypeError, OverflowError):
            pass
        for field, value in saved.items():
            try:
                self.apply(dev, field, value, remember=None)
            except (HidppError, Timeout, ValueError, TypeError, OverflowError):
                continue

    def apply(self, dev, key, value, remember):
        """Write one setting; every value is checked (and normalised, which is also what
        gets saved) before anything goes to the device."""
        if key == "dpi":
            value = whole_number(value)
            if not 0 < value <= 0xFFFF:
                raise ValueError("DPI out of range")
            dev.set_dpi(value)
        elif key == "smartshift":
            if not isinstance(value, str) or value not in ("smartshift", "ratchet", "freespin"):
                raise ValueError(key)
            threshold = self.store.get(remember).get("threshold") if remember else None
            try:
                threshold = max(1, min(50, whole_number(threshold))) if threshold is not None else None
            except ValueError:
                threshold = None
            dev.set_smartshift(value, threshold)
        elif key == "threshold":
            value = max(1, min(50, whole_number(value)))
            dev.set_smartshift("smartshift", value)
        elif key == "invert":
            dev.set_wheel_bit(0x04, switch_value(value))
        elif key == "hires":
            dev.set_wheel_bit(0x02, switch_value(value))
        elif key == "fnswap":
            dev.set_fnswap(switch_value(value))
        else:
            raise ValueError(key)
        if remember:
            self.store.put(remember, key, value)
            if key == "threshold":
                self.store.put(remember, "smartshift", "smartshift")

    def set(self, did, key, value):
        dev = self.devices.get(str(did))
        entry = next((d for d in self.snapshot.get("devices", []) if d["id"] == did), None)
        if not dev or not entry or not entry.get("online"):
            self.note("errNotConnected")
            return
        try:
            if key == "dpi":
                value = valid_dpi(entry.get("dpi"), value)
            self.apply(dev, key, value, remember=entry["key"])
            self.note("")
        except (HidppError, Timeout, ValueError, TypeError, OverflowError) as e:
            self.note("errChange", clean(str(e) or "no answer", 80))

    def note(self, code, *args):
        """A message for the panel as a translation key plus arguments ("" clears it)."""
        self.message = code
        self.message_args = list(args)

    def emit(self):
        snap = dict(self.snapshot)
        snap["message_code"] = self.message
        snap["message_args"] = self.message_args
        self.out.write(json.dumps(snap) + "\n")
        self.out.flush()

    def wants_refresh(self):
        """Connection notifications (HID++ 1.0 sub id 0x41) seen on any node."""
        hit = False
        for n in list(self.nodes.values()):
            try:
                n.drain()
            except OSError:
                continue
            for r in n.events:
                if len(r) >= 3 and r[0] in (SHORT, LONG) and r[2] == 0x41:
                    hit = True
            n.events.clear()
        return hit

    def start_watch(self):
        """`upower --monitor` prints a line whenever a battery changes or a device comes
        or goes; that only schedules a scan."""
        try:
            return subprocess.Popen(["upower", "--monitor"], stdout=subprocess.PIPE,
                                    stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL,
                                    preexec_fn=die_with_parent)
        except OSError:
            return None

    def run(self, stdin=sys.stdin):
        self.watch_proc = self.start_watch()
        try:
            self.loop(stdin)
        finally:
            if self.watch_proc:
                self.watch_proc.terminate()
                try:
                    self.watch_proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    self.watch_proc.kill()

    def loop(self, stdin):
        next_scan = 0.0
        pending = b""
        skipping = False  # inside an oversized line: dropped up to its newline
        watch_proc = self.watch_proc
        fds = [stdin]
        if watch_proc:
            fds.append(watch_proc.stdout)
        while True:
            now = time.monotonic()
            if now >= next_scan:
                try:
                    self.scan()
                except Exception as e:  # keep the bar alive on unexpected device replies
                    self.snapshot = dict(self.snapshot, phase="error", error=clean(str(e), 200))
                self.emit()
                next_scan = time.monotonic() + (OPEN_INTERVAL if self.opened else CLOSED_INTERVAL)
            watch = fds + [n.fd for n in self.nodes.values()]
            hid_from = len(fds)
            try:
                r, _, _ = select.select(watch, [], [], max(0.1, next_scan - time.monotonic()))
            except (OSError, ValueError):
                r = []
                for path in list(self.nodes):
                    self.drop(path)
            if stdin in r:
                # Raw reads with our own line buffer: a buffered readline() after select()
                # would leave a second command that arrived in the same chunk unread
                # until more input comes (e.g. "options" and "open" sent together).
                chunk = os.read(stdin.fileno(), 65536)
                if not chunk:
                    return
                pending += chunk
                *lines, pending = pending.split(b"\n")
                if lines and skipping:
                    lines.pop(0)  # the rest of the oversized line
                    skipping = False
                if len(pending) > MAX_COMMAND:
                    pending, skipping = b"", True
                for line in lines:
                    if len(line) > MAX_COMMAND:
                        continue
                    try:
                        self.handle(line.decode("utf-8", "replace"))
                    except Exception as e:  # one bad command must never end the helper
                        try:
                            print(f"bluetooth-monitor: command ignored: {clean(repr(e), 200)}",
                                  file=sys.stderr, flush=True)
                        except Exception:
                            pass
                next_scan = min(next_scan, time.monotonic() + 0.05)
            if watch_proc and watch_proc.stdout in r:
                chunk = os.read(watch_proc.stdout.fileno(), 4096)
                if chunk:
                    next_scan = min(next_scan, time.monotonic() + 1.0)
                else:  # upower went away: fall back to the timer
                    fds.remove(watch_proc.stdout)
                    watch_proc = None
            if any(fd in r for fd in watch[hid_from:]) and self.wants_refresh():
                next_scan = min(next_scan, time.monotonic() + 1.0)

    def handle(self, line):
        try:
            msg = json.loads(line)
        except (ValueError, RecursionError):  # bad or absurdly nested JSON: ignore it
            return
        if not isinstance(msg, dict):
            return
        cmd = msg.get("cmd")
        if cmd == "open":
            self.opened = True
        elif cmd == "closed":
            self.opened = False
        elif cmd == "options":
            if "other_devices" in msg:
                self.other_devices = bool(msg["other_devices"])
        elif cmd == "set":
            self.set(str(msg.get("device", "")), str(msg.get("key", "")), msg.get("value"))
        # "refresh" and everything else just trigger a scan


def main():
    # SIGTERM from the shell runs the cleanup in Monitor.run (finally) like a normal exit.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    if len(sys.argv) > 1 and sys.argv[1] == "--once":
        m = Monitor()
        m.scan()
        m.emit()
        return
    Monitor().run()


if __name__ == "__main__":
    main()
