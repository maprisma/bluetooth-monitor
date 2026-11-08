#!/usr/bin/env bash
# Build and install Maprisma Bluetooth Monitor for the current user (no root needed):
#   ~/.local/bin/maprisma-bluetooth-monitor                  the applet
#   ~/.local/share/maprisma-bluetooth-monitor/scripts/       the helper and its small scripts
#   ~/.local/share/applications/…desktop                   makes it an applet for the panel
#   ~/.local/share/icons/hicolor/scalable/apps/…svg        its icon
# Then add it in COSMIC Settings › Desktop › Panel › Applets.
set -euo pipefail
cd "$(dirname "$0")"
id=io.github.ubruckhaus.CosmicMaprismaBluetoothMonitor
data="${XDG_DATA_HOME:-$HOME/.local/share}"
bin="$HOME/.local/bin"

cargo build --release
install -Dm755 target/release/maprisma-bluetooth-monitor "$bin/maprisma-bluetooth-monitor"
install -Dm644 scripts/bluetooth-monitor.py "$data/maprisma-bluetooth-monitor/scripts/bluetooth-monitor.py"
install -Dm644 scripts/audio-default.py "$data/maprisma-bluetooth-monitor/scripts/audio-default.py"
for s in bt-device.sh bt-power.sh allow-access.sh; do
  install -Dm755 "scripts/$s" "$data/maprisma-bluetooth-monitor/scripts/$s"
done
install -Dm644 "data/icons/$id-symbolic.svg" "$data/icons/hicolor/scalable/apps/$id-symbolic.svg"
# The panel starts applets without ~/.local/bin in PATH: the full path.
install -d "$data/applications"
sed "s|^Exec=.*|Exec=$bin/maprisma-bluetooth-monitor|" "data/$id.desktop" > "$data/applications/$id.desktop"
gtk-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
echo "Installed. Add “Maprisma Bluetooth Monitor” in COSMIC Settings › Desktop › Panel › Applets."
