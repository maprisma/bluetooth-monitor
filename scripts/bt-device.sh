#!/bin/bash
# Pair, connect, disconnect or forget one Bluetooth device with bluetoothctl.
# Usage: bt-device.sh pair|connect|disconnect|forget <address>
# (Same steps as Omarchy's omarchy-bluetooth-device: power on first, trust before
# connecting, every bluetoothctl call with a time limit.)
set -u
action=${1:-}
address=${2:-}
case "$action" in pair|connect|disconnect|forget) ;; *) echo "usage: $0 pair|connect|disconnect|forget <address>" >&2; exit 2 ;; esac
[[ $address =~ ^([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}$ ]] || { echo "not a Bluetooth address" >&2; exit 2; }

here=$(dirname "$(readlink -f "$0")")
power_on() {
  [[ $(timeout 2s bluetoothctl show 2>/dev/null) == *"Powered: yes"* ]] && return
  "$here/bt-power.sh" on || true
}
trust() { timeout 5s bluetoothctl trust "$address" >/dev/null 2>&1 || true; }

case "$action" in
  pair)
    power_on
    timeout 20s bluetoothctl pair "$address" >/dev/null 2>&1 || true
    trust
    timeout 20s bluetoothctl connect "$address" >/dev/null 2>&1 || true
    ;;
  connect)
    power_on
    trust
    timeout 20s bluetoothctl connect "$address" >/dev/null 2>&1 || true
    ;;
  disconnect)
    timeout 10s bluetoothctl disconnect "$address" >/dev/null 2>&1 || true
    ;;
  forget)
    power_on
    timeout 10s bluetoothctl disconnect "$address" >/dev/null 2>&1 || true
    timeout 10s bluetoothctl remove "$address" >/dev/null 2>&1 || true
    ;;
esac
