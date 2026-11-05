#!/usr/bin/env python3
"""Make a Bluetooth headset or speaker that just connected the default audio output.

Usage: audio-default.py <bluetooth address>

Waits up to 4 s for PipeWire to create its sink (found by api.bluez5.address in
`pactl -f json list sinks`), sets it as the default and moves the playing
application streams to it. EasyEffects' own stream and DSP filter chains (no
application.name) stay where they are. Only pactl with a fixed argv is run; the
sink name comes from PipeWire, the address is checked first.
"""
import json
import re
import subprocess
import sys
import time

ADDRESS = re.compile(r"^([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}$")
SINK = re.compile(r"^[A-Za-z0-9_][A-Za-z0-9_.:-]{0,199}$")


def pactl_json(*args):
    try:
        out = subprocess.run(["pactl", "-f", "json", *args], capture_output=True, text=True, timeout=3).stdout
        data = json.loads(out)
        return data if isinstance(data, list) else []
    except (OSError, ValueError, subprocess.SubprocessError):
        return []


def sink_for(address):
    for sink in pactl_json("list", "sinks"):
        props = sink.get("properties") if isinstance(sink, dict) else None
        if isinstance(props, dict) and str(props.get("api.bluez5.address", "")).upper() == address:
            name = str(sink.get("name", ""))
            if SINK.match(name):
                return name
    return None


def main():
    if len(sys.argv) != 2 or not ADDRESS.match(sys.argv[1]):
        print("usage: audio-default.py <bluetooth address>", file=sys.stderr)
        return 2
    address = sys.argv[1].upper()
    for _ in range(8):
        name = sink_for(address)
        if name:
            subprocess.run(["pactl", "set-default-sink", name], timeout=3, check=False)
            for stream in pactl_json("list", "sink-inputs"):
                props = stream.get("properties") if isinstance(stream, dict) else None
                app = props.get("application.name") if isinstance(props, dict) else None
                index = stream.get("index") if isinstance(stream, dict) else None
                if app and app != "EasyEffects" and isinstance(index, int):
                    subprocess.run(["pactl", "move-sink-input", str(index), name], timeout=3, check=False)
            return 0
        time.sleep(0.5)
    return 1


if __name__ == "__main__":
    sys.exit(main())
