#!/bin/bash
# Lets the logged-in user talk HID++ to Logitech receivers and devices (run as root via pkexec).
# Same scope as Solaar's 42-logitech-unify-permissions.rules: hidraw nodes of USB devices
# with Logitech's vendor id and Logitech devices on Bluetooth, granted with uaccess (only
# the user at the seat, only while logged in).
set -euo pipefail
rule=/etc/udev/rules.d/42-logitech-hidpp-uaccess.rules
cat > "$rule.tmp" <<'RULES'
# Managed by the Bluetooth Monitor GNOME extension (bluetooth-monitor@ubruckhaus.github.io)
ACTION == "remove", GOTO="logitech_hidpp_end"
SUBSYSTEM != "hidraw", GOTO="logitech_hidpp_end"
ATTRS{idVendor}=="046d", GOTO="logitech_hidpp_apply"
KERNELS == "0005:046D:*", GOTO="logitech_hidpp_apply"
GOTO="logitech_hidpp_end"
LABEL="logitech_hidpp_apply"
TAG+="uaccess"
LABEL="logitech_hidpp_end"
RULES
chmod 0644 "$rule.tmp"
mv -f "$rule.tmp" "$rule"
udevadm control --reload-rules
udevadm trigger --subsystem-match=hidraw --action=add
udevadm settle --timeout=5 || true
