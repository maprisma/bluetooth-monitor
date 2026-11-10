# Maprisma Bluetooth Monitor for COSMIC

Every wireless device at a glance: battery and connection, in the COSMIC panel. A native libcosmic
panel applet and one of the [Maprisma](https://github.com/maprisma) apps: the COSMIC port of the
Omarchy widget, which stays as it is, with the same Python helper.

- **Panel icon:** green while every battery is fine, orange when one runs low, red when one is critical. It
  turns into the device that needs charging (mouse on a tie) and is dim while nothing is connected. Left
  click opens the panel, right click refreshes.
- **Connection map:** this PC, every dongle (Unifying, Bolt, Nano, Lightspeed) and Bluetooth adapter, and
  the devices on each with their battery.
- **Device cards:** battery, connection, an eye to hide a device (a hidden device also leaves the panel
  icon). Logitech devices expand to their settings over HID++: pointer speed (DPI), SmartShift / ratchet /
  free spin and sensitivity, natural and high-resolution scrolling, Fn-key swap.
- **Bluetooth page:** power on/off (rfkill), scan while the page is open (`bluetoothctl scan on`, which
  BlueZ stops when the page closes), pair, connect, disconnect, forget (asks twice). A headset or speaker
  that connects becomes the audio output.
- **Settings:** low and critical battery levels, whether other Bluetooth devices are listed, receiver
  access ("Allow access" installs the uaccess udev rule through `pkexec`).

The interface supports English, Deutsch, Français, Español, Italiano, Português, Nederlands and Polski.
Pick **Language** on the Settings page (applet popup or window) or in the KDE tray menu.
**System (<name>)** follows the first non-empty `LC_ALL`, `LC_MESSAGES`, or `LANG`; unsupported locales use English.
The `language` setting (`auto` or a language code) is stored in the app’s `settings.json`.
Changes apply immediately; other running instances follow within one second.
Catalogs in `translations/` and `translations/extra/` are compiled into both binaries.
To refresh the original catalogs, run `python3 tools/import-translations /path/to/omarchy-bluetooth-monitor/I18n.js`;
the source is only read. Extra port strings are maintained separately.
 COSMIC's own Bluetooth applet stays the Bluetooth switch; this one is the battery monitor.

## Install

Needs Rust (cargo), `python3`, `bluetoothctl`, `busctl`, `upower`, `rfkill` and, for the audio switch, `pactl`.

```sh
./install.sh
```

It installs into `~/.local` (binary, `share/maprisma-bluetooth-monitor/scripts`, the desktop entry with an
absolute Exec path, the icon). Then add **Bluetooth Monitor** in COSMIC Settings › Desktop › Panel › Applets.

## How it works

- `scripts/bluetooth-monitor.py` runs while the applet runs: JSON commands
  on stdin (open, closed, refresh, options, set), JSON snapshots on stdout.
- The Bluetooth page reads BlueZ's objects with `busctl --system --json=short … GetManagedObjects` (read
  only) every two seconds while it is open. Changes go through `scripts/bt-device.sh`, `scripts/bt-power.sh`
  and `scripts/audio-default.py` with a fixed argv and a checked address.
- Settings (language, levels, other devices, hidden devices) are in `~/.config/maprisma-bluetooth-monitor/settings.json`;
  the helper keeps the Logitech settings you changed in `~/.config/omarchy/bluetooth-monitor/settings.json`.

## Development

```sh
env -u DISPLAY -u WAYLAND_DISPLAY python3 -m unittest discover -s tests   # the helper
cargo test                                                                  # parsing, DPI steps
cargo test -- --ignored --nocapture                                         # reads this PC's BlueZ list (read only)
```

Licensed under [MIT](LICENSE).

## KDE Plasma

On KDE Plasma 6.8 (Wayland), run `./install.sh`, then launch **Maprisma Bluetooth
Monitor** from the application launcher. `maprisma-bluetooth-monitor --window`
opens the same libcosmic UI as the COSMIC popup, in a resizable window. Only one
window runs per session; launching it again leaves the existing window open.
The window uses the same settings, helper and controls, including Bluetooth scans
while its Bluetooth page is visible.

The installer also installs `maprisma-bluetooth-monitor-tray` and a KDE-only
entry in `${XDG_CONFIG_HOME:-~/.config}/autostart`. Log in again or start that
binary manually. Left click toggles the window; its menu offers **Open Maprisma
Bluetooth Monitor** and **Quit tray** (which leaves an open window running).
The tooltip reports connected devices and their lowest battery, respecting hidden
devices. The tray runs the shared helper in its cheaper closed mode and reconnects
when Plasma's tray watcher restarts. Both binaries require a session D-Bus.
COSMIC continues to use the original panel applet entry and popup.
