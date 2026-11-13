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
install -Dm755 target/release/maprisma-bluetooth-monitor-tray "$bin/maprisma-bluetooth-monitor-tray"
install -Dm644 scripts/bluetooth-monitor.py "$data/maprisma-bluetooth-monitor/scripts/bluetooth-monitor.py"
install -Dm644 scripts/audio-default.py "$data/maprisma-bluetooth-monitor/scripts/audio-default.py"
for s in bt-device.sh bt-power.sh allow-access.sh; do
  install -Dm755 "scripts/$s" "$data/maprisma-bluetooth-monitor/scripts/$s"
done
install -Dm644 "data/icons/$id-symbolic.svg" "$data/icons/hicolor/scalable/apps/$id-symbolic.svg"
# The tray shows the accent one while a device is connected, the hollow one otherwise.
install -Dm644 data/icons/bluetooth-monitor-active-symbolic.svg "$data/icons/hicolor/scalable/apps/$id-active-symbolic.svg"
install -Dm644 data/icons/bluetooth-monitor-off-symbolic.svg "$data/icons/hicolor/scalable/apps/$id-off-symbolic.svg"
# The panel starts applets without ~/.local/bin in PATH: the full path.
install -d "$data/applications"
# Desktop Exec has two escaping layers: quoted argv, then desktop value escapes.
desktop_exec() {
  local v="$1"
  v=${v//\\/\\\\}
  v=${v//\"/\\\"}
  v=${v//\`/\\\`}
  v=${v//\$/\\\$}
  v=${v//\%/%%}
  v=${v//\\/\\\\}
  printf '"%s"' "$v"
}
app_exec=$(desktop_exec "$bin/maprisma-bluetooth-monitor")
tray_exec=$(desktop_exec "$bin/maprisma-bluetooth-monitor-tray")
# Preserve all COSMIC metadata, replacing only Exec.
while IFS= read -r line; do
  case "$line" in Exec=*) printf 'Exec=%s\n' "$app_exec" ;; *) printf '%s\n' "$line" ;; esac
done < "data/$id.desktop" > "$data/applications/$id.desktop"
cat > "$data/applications/$id.Window.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Maprisma Bluetooth Monitor
Name[de]=Maprisma Bluetooth-Monitor
Name[fr]=Moniteur Bluetooth Maprisma
Name[es]=Monitor Bluetooth Maprisma
Name[it]=Monitor Bluetooth Maprisma
Name[pt]=Monitor Bluetooth Maprisma
Name[nl]=Maprisma Bluetooth-monitor
Name[pl]=Monitor Bluetooth Maprisma
Comment=Batteries and connections of wireless mice, keyboards, earphones and controllers
Comment[de]=Akkustände und Verbindungen von kabellosen Mäusen, Tastaturen, Kopfhörern und Controllern
Comment[fr]=Batteries et connexions des souris, claviers, écouteurs et manettes sans fil
Comment[es]=Baterías y conexiones de ratones, teclados, auriculares y mandos inalámbricos
Comment[it]=Batterie e connessioni di mouse, tastiere, auricolari e controller wireless
Comment[pt]=Baterias e ligações de ratos, teclados, auriculares e comandos sem fios
Comment[nl]=Batterijen en verbindingen van draadloze muizen, toetsenborden, oordopjes en controllers
Comment[pl]=Baterie i połączenia bezprzewodowych myszy, klawiatur, słuchawek i kontrolerów
Exec=$app_exec --window
Icon=$id-symbolic
Terminal=false
NoDisplay=false
Categories=Utility;Settings;
EOF
autostart="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
install -d "$autostart"
cat > "$autostart/$id.Tray.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Maprisma Bluetooth Monitor
Name[de]=Maprisma Bluetooth-Monitor
Name[fr]=Moniteur Bluetooth Maprisma
Name[es]=Monitor Bluetooth Maprisma
Name[it]=Monitor Bluetooth Maprisma
Name[pt]=Monitor Bluetooth Maprisma
Name[nl]=Maprisma Bluetooth-monitor
Name[pl]=Monitor Bluetooth Maprisma
Comment=Batteries and connections of wireless mice, keyboards, earphones and controllers
Comment[de]=Akkustände und Verbindungen von kabellosen Mäusen, Tastaturen, Kopfhörern und Controllern
Comment[fr]=Batteries et connexions des souris, claviers, écouteurs et manettes sans fil
Comment[es]=Baterías y conexiones de ratones, teclados, auriculares y mandos inalámbricos
Comment[it]=Batterie e connessioni di mouse, tastiere, auricolari e controller wireless
Comment[pt]=Baterias e ligações de ratos, teclados, auriculares e comandos sem fios
Comment[nl]=Batterijen en verbindingen van draadloze muizen, toetsenborden, oordopjes en controllers
Comment[pl]=Baterie i połączenia bezprzewodowych myszy, klawiatur, słuchawek i kontrolerów
Exec=$tray_exec
Icon=$id-symbolic
Terminal=false
OnlyShowIn=KDE;Hyprland;
EOF
gtk-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
echo "Installed. Add “Maprisma Bluetooth Monitor” in COSMIC Settings › Desktop › Panel › Applets."
