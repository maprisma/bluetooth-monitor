#!/usr/bin/env bash
# Build and install Bluetooth Monitor for the current user (no root needed):
#   ~/.local/bin/cosmic-bluetooth-monitor                  the applet
#   ~/.local/share/cosmic-bluetooth-monitor/scripts/       the helper and its small scripts
#   ~/.local/share/applications/…desktop                   makes it an applet for the panel
#   ~/.local/share/icons/hicolor/scalable/apps/…svg        its icon
# Then add it in COSMIC Settings › Desktop › Panel › Applets.
set -euo pipefail
cd "$(dirname "$0")"
id=io.github.ubruckhaus.CosmicBluetoothMonitor
data="${XDG_DATA_HOME:-$HOME/.local/share}"
bin="$HOME/.local/bin"

cargo build --release
install -Dm755 target/release/cosmic-bluetooth-monitor "$bin/cosmic-bluetooth-monitor"
install -Dm644 scripts/bluetooth-monitor.py "$data/cosmic-bluetooth-monitor/scripts/bluetooth-monitor.py"
install -Dm644 scripts/audio-default.py "$data/cosmic-bluetooth-monitor/scripts/audio-default.py"
for s in bt-device.sh bt-power.sh allow-access.sh; do
  install -Dm755 "scripts/$s" "$data/cosmic-bluetooth-monitor/scripts/$s"
done
install -Dm644 "data/icons/$id-symbolic.svg" "$data/icons/hicolor/scalable/apps/$id-symbolic.svg"
# The panel starts applets without ~/.local/bin in PATH: the full path.
install -d "$data/applications"
sed "s|^Exec=.*|Exec=$bin/cosmic-bluetooth-monitor|" "data/$id.desktop" > "$data/applications/$id.desktop"
gtk-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
echo "Installed. Add “Bluetooth Monitor” in COSMIC Settings › Desktop › Panel › Applets."
