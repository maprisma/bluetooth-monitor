#!/bin/bash
# Turn Bluetooth on or off through the rfkill soft block, like GNOME's own Bluetooth
# switch (gnome-settings-daemon follows the block, and it survives a reboot).
# Usage: bt-power.sh on|off
set -u
# The default controller: the one the panel shows (GnomeBluetooth's default adapter).
# With two, another powered controller must not make "on" stop early.
powered() {
  [[ $(timeout 2s bluetoothctl show 2>/dev/null) == *"Powered: yes"* ]]
}
wait_powered() {
  local deadline=$((SECONDS + 2))
  while :; do
    powered && return 0
    ((SECONDS < deadline)) || return 1
    sleep 0.2
  done
}
case "${1:-}" in
  on)
    rfkill unblock bluetooth
    wait_powered && exit 0
    timeout 5s bluetoothctl power on >/dev/null 2>&1
    wait_powered
    ;;
  off)
    rfkill block bluetooth
    ;;
  *)
    echo "usage: $0 on|off" >&2
    exit 2
    ;;
esac
