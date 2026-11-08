// Bluetooth Monitor for COSMIC: batteries and connections of wireless mice,
// keyboards, trackpads, earphones and controllers in the panel. The icon is
// green while every connected battery is fine, orange when one runs low and red
// when one is critical, and it turns into the device that needs charging (mouse
// on a tie); dim while no device is connected. Left click opens the panel: a
// connection map (this PC, dongles and Bluetooth adapters, the devices on them),
// one card per device with Logitech settings, a Bluetooth page that scans,
// pairs, connects, disconnects and forgets devices, and the settings. Right
// click refreshes.
//
// Device data comes from scripts/bluetooth-monitor.py (the same helper as the
// GNOME extension and the Omarchy widget: HID++ over /dev/hidraw, UPower,
// BlueZ), which takes JSON commands on stdin and streams JSON snapshots on
// stdout. The Bluetooth page reads BlueZ's device list with busctl (read only);
// every action that changes something runs one of the scripts/ helpers with a
// fixed argv.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use cosmic::app::{Core, Task};
use cosmic::applet::cosmic_panel_config::PanelAnchor;
use cosmic::applet::padded_control;
use cosmic::iced::futures::SinkExt;
use cosmic::iced::futures::channel::mpsc;
use cosmic::iced::platform_specific::shell::wayland::commands::popup::destroy_popup;
use cosmic::iced::widget::{container as iced_container, svg as iced_svg, text as iced_text};
use cosmic::iced::{Alignment, Background, Border, Color, Length, Rectangle, Subscription, stream, window};
use cosmic::widget::rectangle_tracker::{RectangleTracker, RectangleUpdate, rectangle_tracker_subscription};
use cosmic::widget::{
    Id, autosize, button, column, container, divider, icon, mouse_area, row, scrollable, slider, space, text, toggler,
};
use cosmic::{Element, theme};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

const APP_ID: &str = "io.github.ubruckhaus.CosmicMaprismaBluetoothMonitor";
static AUTOSIZE_ID: LazyLock<Id> = LazyLock::new(|| Id::new("bluetooth-monitor-autosize"));

const RESTART_DELAY: Duration = Duration::from_secs(5); // before a helper that exited starts again
const PENDING_TIMEOUT: Duration = Duration::from_secs(60); // pair + connect take up to 45 s
const SCAN_SECONDS: u32 = 300; // bluetoothctl's own scan timeout (a backstop)
const BLUEZ_POLL: Duration = Duration::from_secs(2); // the Bluetooth page's refresh
const MAX_AVAILABLE: usize = 40; // devices found by a scan that are listed
const POPUP_WIDTH: f32 = 440.0;
const POPUP_MAX_HEIGHT: f32 = 760.0;
const DIM: f32 = 0.6;

// Battery states in the GNOME palette (the Omarchy widget used the theme's colours).
const OK: Color = Color::from_rgb(0x33 as f32 / 255.0, 0xd1 as f32 / 255.0, 0x7a as f32 / 255.0);
const LOW: Color = Color::from_rgb(1.0, 0x78 as f32 / 255.0, 0.0);
const CRITICAL: Color = Color::from_rgb(0xe0 as f32 / 255.0, 0x1b as f32 / 255.0, 0x24 as f32 / 255.0);

fn main() -> cosmic::iced::Result {
    cosmic::applet::run::<Monitor>(())
}

// ------------------------------------------------------------------------- values

fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn int(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64)
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool) == Some(true)
}

fn list<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn is_address(text: &str) -> bool {
    let parts: Vec<&str> = text.split(':').collect();
    parts.len() == 6 && parts.iter().all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

// A name that is only the address (AA-BB-…) or a UUID says nothing.
fn meaningless_name(name: &str) -> bool {
    let hex_pairs: Vec<&str> = name.split(['-', ':']).collect();
    let mac = hex_pairs.len() == 6 && hex_pairs.iter().all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()));
    let uuid = name.len() == 36
        && name.split('-').map(str::len).collect::<Vec<_>>() == [8, 4, 4, 4, 12]
        && name.chars().all(|c| c == '-' || c.is_ascii_hexdigit());
    mac || uuid
}

// Device names come from the devices themselves: control characters go.
fn clean(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).take(80).collect::<String>().trim().to_string()
}

const CONNECTIONS: &[(&str, &str)] = &[
    ("unifying", "Unifying dongle"),
    ("bolt", "Bolt dongle"),
    ("nano", "Nano dongle"),
    ("lightspeed", "Lightspeed dongle"),
    ("bluetooth", "Bluetooth"),
    ("usb", "USB cable"),
    ("wireless", "Wireless"),
];

fn connection_name(code: &str) -> &'static str {
    CONNECTIONS.iter().find(|(c, _)| *c == code).map_or("", |(_, n)| n)
}

fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "mouse" | "trackball" => "input-mouse-symbolic",
        "keyboard" => "input-keyboard-symbolic",
        "numpad" | "remote" => "input-dialpad-symbolic",
        "touchpad" => "input-touchpad-symbolic",
        "headset" => "audio-headphones-symbolic",
        "speaker" => "audio-speakers-symbolic",
        "presenter" => "video-display-symbolic",
        "gamepad" | "joystick" => "input-gaming-symbolic",
        "tablet" | "pen" => "input-tablet-symbolic",
        "phone" => "phone-symbolic",
        "camera" => "camera-web-symbolic",
        _ => "bluetooth-active-symbolic",
    }
}

// BlueZ's device icon names (input-mouse, audio-headset, ...) to a symbolic icon.
fn bluez_icon(name: &str, connected: bool) -> &'static str {
    if name.starts_with("audio-head") {
        "audio-headphones-symbolic"
    } else if name.starts_with("audio") {
        "audio-speakers-symbolic"
    } else if name.starts_with("input-mouse") {
        "input-mouse-symbolic"
    } else if name.starts_with("input-keyboard") {
        "input-keyboard-symbolic"
    } else if name.starts_with("input-gaming") {
        "input-gaming-symbolic"
    } else if name.starts_with("input-tablet") {
        "input-touchpad-symbolic" // BlueZ calls trackpads tablets
    } else if name.starts_with("video") {
        "video-display-symbolic"
    } else if name.starts_with("phone") {
        "phone-symbolic"
    } else if name.starts_with("computer") {
        "computer-symbolic"
    } else if connected {
        "bluetooth-active-symbolic"
    } else {
        "bluetooth-disabled-symbolic"
    }
}

fn battery_icon(b: &Value) -> String {
    let charging = if flag(b, "charging") { "-charging" } else { "" };
    match b.get("percent").and_then(Value::as_f64) {
        Some(p) => format!("battery-level-{}{charging}-symbolic", ((p / 10.0).round() * 10.0).clamp(0.0, 100.0) as i64),
        None => {
            let low = matches!(s(b, "level"), "critical" | "low" | "empty");
            format!("battery-level-{}{charging}-symbolic", if low { 10 } else { 80 })
        }
    }
}

fn battery_text(b: &Value) -> String {
    match b.get("percent").and_then(Value::as_i64) {
        Some(p) => format!("{p}%"),
        None => s(b, "level").to_string(),
    }
}

fn device_name(d: &Value) -> String {
    let name = clean(s(d, "name"));
    if name.is_empty() { "Logitech device".into() } else { name }
}

fn device_key(d: &Value) -> String {
    let key = s(d, "key");
    if key.is_empty() { s(d, "id").to_string() } else { key.to_string() }
}

fn battery(d: &Value) -> Option<&Value> {
    d.get("battery").filter(|b| b.is_object())
}

fn has_settings(d: &Value) -> bool {
    ["dpi", "smartshift", "wheel", "fnswap"].iter().any(|k| d.get(*k).is_some_and(|v| v.is_object()))
}

// ------------------------------------------------------------------------- settings

// The GNOME port's GSettings keys, as one JSON file.
#[derive(Debug, Clone)]
struct Settings {
    low_at: i64,
    critical_at: i64,
    other_devices: bool,
    hidden: BTreeSet<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { low_at: 25, critical_at: 10, other_devices: true, hidden: BTreeSet::new() }
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("maprisma-bluetooth-monitor/settings.json"))
}

impl Settings {
    fn load() -> Self {
        let mut out = Self::default();
        let Some(data) = config_path().and_then(|p| std::fs::read_to_string(p).ok()) else { return out };
        let Ok(v) = serde_json::from_str::<Value>(&data) else { return out };
        if let Some(n) = int(&v, "low_at").filter(|n| [15, 20, 25, 30, 40, 50].contains(n)) {
            out.low_at = n;
        }
        if let Some(n) = int(&v, "critical_at").filter(|n| [5, 10, 15, 20].contains(n)) {
            out.critical_at = n;
        }
        if let Some(b) = v.get("other_devices").and_then(Value::as_bool) {
            out.other_devices = b;
        }
        out.hidden = list(&v, "hidden").iter().filter_map(Value::as_str).take(500).map(str::to_string).collect();
        out
    }

    // Written through a temp file in the same folder, then renamed.
    fn save(&self) {
        let Some(path) = config_path() else { return };
        let Some(dir) = path.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let data = json!({
            "low_at": self.low_at,
            "critical_at": self.critical_at,
            "other_devices": self.other_devices,
            "hidden": self.hidden.iter().collect::<Vec<_>>(),
        });
        let tmp = dir.join(format!(".settings.json.{}", std::process::id()));
        if std::fs::write(&tmp, format!("{data:#}\n")).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

// ------------------------------------------------------------------------- Bluetooth

#[derive(Debug, Clone, Default)]
struct BtRow {
    address: String,
    name: String,
    icon: String,
    connected: bool,
    known: bool,
    battery: Option<i64>,
}

#[derive(Debug, Clone, Default)]
struct BtState {
    has_adapter: bool,
    powered: bool,
    devices: Vec<BtRow>,
}

impl BtState {
    fn find(&self, address: &str) -> Option<&BtRow> {
        self.devices.iter().find(|d| d.address == address)
    }

    // Connected, paired (or trusted) and, while scanning, the other devices nearby.
    fn sections(&self, scanning: bool) -> [(&'static str, Vec<&BtRow>); 3] {
        let (mut connected, mut paired, mut available) = (Vec::new(), Vec::new(), Vec::new());
        for d in &self.devices {
            if d.name.is_empty() || meaningless_name(&d.name) || !is_address(&d.address) {
                continue;
            }
            if d.connected {
                connected.push(d);
            } else if d.known {
                paired.push(d);
            } else if scanning {
                available.push(d);
            }
        }
        for v in [&mut connected, &mut paired, &mut available] {
            v.sort_by_key(|d| d.name.to_lowercase());
        }
        available.truncate(MAX_AVAILABLE);
        [("Connected", connected), ("Paired", paired), ("Available", available)]
    }
}

// busctl's JSON: {"type": …, "data": [{path: {interface: {property: {"type", "data"}}}}]}.
fn parse_bluez(output: &str) -> BtState {
    let mut state = BtState::default();
    let Ok(v) = serde_json::from_str::<Value>(output) else { return state };
    let Some(objects) = v.get("data").and_then(|d| d.get(0)).and_then(Value::as_object) else { return state };
    let prop = |props: &Value, key: &str| props.get(key).and_then(|p| p.get("data")).cloned().unwrap_or(Value::Null);
    // The default adapter: the first one (hci0), as GnomeBluetooth picks it.
    let mut adapters: Vec<(&String, &Value)> = objects
        .iter()
        .filter_map(|(path, ifs)| ifs.get("org.bluez.Adapter1").map(|a| (path, a)))
        .collect();
    adapters.sort_by(|a, b| a.0.cmp(b.0));
    let Some((adapter_path, adapter)) = adapters.first() else { return state };
    state.has_adapter = true;
    state.powered = prop(adapter, "Powered").as_bool() == Some(true);
    let prefix = format!("{adapter_path}/");
    for (path, ifs) in objects {
        let Some(dev) = ifs.get("org.bluez.Device1") else { continue };
        if !path.starts_with(&prefix) {
            continue;
        }
        let (alias, plain) = (prop(dev, "Alias"), prop(dev, "Name"));
        let name = clean(alias.as_str().or_else(|| plain.as_str()).unwrap_or(""));
        let yes = |key: &str| prop(dev, key).as_bool() == Some(true);
        state.devices.push(BtRow {
            address: prop(dev, "Address").as_str().unwrap_or("").to_string(),
            name,
            icon: prop(dev, "Icon").as_str().unwrap_or("").to_string(),
            connected: yes("Connected"),
            known: yes("Paired") || yes("Trusted") || yes("Bonded"),
            battery: ifs
                .get("org.bluez.Battery1")
                .and_then(|b| prop(b, "Percentage").as_i64())
                .filter(|p| (0..=100).contains(p)),
        });
        if state.devices.len() >= 500 {
            break;
        }
    }
    state
}

async fn read_bluez() -> BtState {
    let out = tokio::process::Command::new("busctl")
        .args(["--system", "--json=short", "call", "org.bluez", "/", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(5), out).await {
        Ok(Ok(out)) if out.stdout.len() < 8 << 20 => parse_bluez(&String::from_utf8_lossy(&out.stdout)),
        _ => BtState::default(),
    }
}

// ------------------------------------------------------------------------- widgets

fn dim_text(theme: &cosmic::Theme) -> iced_text::Style {
    let mut color = Color::from(theme.cosmic().on_bg_color());
    color.a = DIM;
    iced_text::Style { color: Some(color), ..Default::default() }
}

fn accent_text(theme: &cosmic::Theme) -> iced_text::Style {
    iced_text::Style { color: Some(Color::from(theme.cosmic().accent_text_color())), ..Default::default() }
}

fn dim<'a>(content: impl Into<std::borrow::Cow<'a, str>> + 'a) -> Element<'a, Message> {
    text::caption(content).class(theme::Text::Custom(dim_text)).into()
}

fn tinted(name: &str, size: u16, color: Option<Color>, opacity: f32) -> Element<'static, Message> {
    let mut i = icon::from_name(name.to_string()).size(size).symbolic(true).icon().opacity(opacity);
    if let Some(c) = color {
        i = i.class(theme::Svg::custom(move |_| iced_svg::Style { color: Some(c) }));
    }
    i.into()
}

fn section<'a>(title: &str) -> Element<'a, Message> {
    container(text::caption_heading(title.to_uppercase()).class(theme::Text::Custom(dim_text)))
        .padding([8, 0, 0, 0])
        .into()
}

fn card_style(online: bool) -> theme::Container<'static> {
    theme::Container::custom(move |theme: &cosmic::Theme| {
        let mut color = Color::from(theme.cosmic().on_bg_color());
        color.a = if online { 0.07 } else { 0.03 };
        iced_container::Style {
            background: Some(Background::Color(color)),
            border: Border { radius: 10.0.into(), ..Default::default() },
            ..Default::default()
        }
    })
}

fn pill_style() -> theme::Container<'static> {
    theme::Container::custom(|theme: &cosmic::Theme| {
        let mut color = Color::from(theme.cosmic().on_bg_color());
        color.a = 0.12;
        iced_container::Style {
            background: Some(Background::Color(color)),
            border: Border { radius: 10.0.into(), ..Default::default() },
            ..Default::default()
        }
    })
}

fn icon_button<'a>(name: &str, message: Option<Message>) -> Element<'a, Message> {
    button::icon(icon::from_name(name.to_string()).size(16)).on_press_maybe(message).into()
}

// Pills of which one is picked.
fn choices<'a>(options: Vec<(Value, String)>, value: &Value, pick: impl Fn(Value) -> Message) -> Element<'a, Message> {
    let mut line = row![].spacing(4);
    for (v, label) in options {
        let chosen = &v == value;
        let b = if chosen { button::suggested(label) } else { button::standard(label) };
        line = line.push(b.on_press_maybe((!chosen).then(|| pick(v))));
    }
    line.into()
}

// Caption and hint on the left, a control on the right.
fn setting_row<'a>(caption: &str, hint: &str, control: Element<'a, Message>) -> Element<'a, Message> {
    let mut words = column![text::body(caption.to_string())].spacing(2);
    if !hint.is_empty() {
        words = words.push(dim(hint.to_string()));
    }
    row![container(words).width(Length::Fill), control].spacing(12).align_y(Alignment::Center).into()
}

// ------------------------------------------------------------------------- applet

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Page {
    #[default]
    Main,
    Settings,
    Bluetooth,
}

#[derive(Default)]
struct Monitor {
    core: Core,
    popup: Option<window::Id>,
    rectangle: Rectangle,
    tracker: Option<RectangleTracker<u32>>,
    stats: Value,
    settings: Settings,
    helper: Option<UnboundedSender<String>>,
    page: Page,
    show_hidden: bool,
    expanded: BTreeSet<String>,
    confirm_forget: String,
    allowing: bool,
    bt: BtState,
    // address -> ("connecting" | "disconnecting" | "forgetting", since)
    pending: HashMap<String, (&'static str, Instant)>,
    // Actions whose helper ended, cleared at the next BlueZ reading.
    finished: Vec<String>,
    // A slider being dragged: (device id, key) -> value, written when let go.
    dragging: HashMap<(String, &'static str), f64>,
}

#[derive(Debug, Clone)]
enum Message {
    Snapshot(Value),
    HelperStarted(UnboundedSender<String>),
    TogglePopup,
    CloseRequested(window::Id),
    Rectangle(RectangleUpdate<u32>),
    Refresh,
    Page(Page),
    ToggleShowHidden,
    Expand(String),
    SetHidden(String, bool),
    Set(String, &'static str, Value),
    Drag(String, &'static str, f64),
    Release(String, &'static str),
    LowAt(i64),
    CriticalAt(i64),
    OtherDevices(bool),
    AllowAccess,
    AllowDone,
    BtTick,
    Bluez(BtState),
    BtAction(&'static str, String),
    BtActionDone(String),
    BtPower(bool),
    Forget(String),
}

impl Monitor {
    fn send(&self, obj: Value) {
        if let Some(helper) = &self.helper {
            let _ = helper.send(format!("{obj}\n"));
        }
    }

    fn devices(&self) -> &[Value] {
        list(&self.stats, "devices")
    }

    fn is_hidden(&self, d: &Value) -> bool {
        self.settings.hidden.contains(&device_key(d))
    }

    fn shown(&self) -> Vec<&Value> {
        self.devices().iter().filter(|d| !self.is_hidden(d)).collect()
    }

    // "ok" | "low" | "critical" for a battery (percent if known, else its level).
    fn battery_state(&self, b: Option<&Value>) -> u8 {
        let Some(b) = b else { return 0 };
        if flag(b, "charging") {
            return 0;
        }
        match b.get("percent").and_then(Value::as_i64) {
            Some(p) if p <= self.settings.critical_at => 2,
            Some(p) if p <= self.settings.low_at => 1,
            Some(_) => 0,
            None => match s(b, "level") {
                "critical" | "empty" => 2,
                "low" => 1,
                _ => 0,
            },
        }
    }

    // A colour only for warnings and charging; a fine battery keeps the text colour.
    fn battery_color(&self, b: Option<&Value>) -> Option<Color> {
        let b = b?;
        if flag(b, "charging") {
            return Some(OK);
        }
        match self.battery_state(Some(b)) {
            2 => Some(CRITICAL),
            1 => Some(LOW),
            _ => None,
        }
    }

    // The worst state among connected devices and its kind; on a tie a mouse wins.
    fn alert(&self, devices: &[&Value]) -> (u8, String) {
        let mut best = (0u8, "mouse".to_string());
        for d in devices.iter().filter(|d| flag(d, "online")) {
            let state = self.battery_state(battery(d));
            if state > best.0 || (state == best.0 && state > 0 && s(d, "kind") == "mouse") {
                best = (state, s(d, "kind").to_string());
            }
        }
        best
    }

    fn lowest(&self, devices: &[&Value]) -> Option<i64> {
        devices
            .iter()
            .filter(|d| flag(d, "online"))
            .filter_map(|d| battery(d).and_then(|b| b.get("percent")).and_then(Value::as_i64))
            .min()
    }

    fn scanning(&self) -> bool {
        self.popup.is_some() && self.page == Page::Bluetooth && self.bt.powered
    }

    fn set_page(&mut self, page: Page) -> Task<Message> {
        self.page = page;
        self.confirm_forget.clear();
        if page == Page::Bluetooth {
            return Task::perform(read_bluez(), |state| cosmic::Action::App(Message::Bluez(state)));
        }
        Task::none()
    }

    // Finished actions leave the pending list (when BlueZ shows the result, the helper
    // exited, or the time is up); a headset or speaker that connected becomes the
    // audio output.
    fn check_pending(&mut self, finished: Option<&str>) {
        let now = Instant::now();
        let entries: Vec<(String, &'static str, Instant)> =
            self.pending.iter().map(|(a, (act, t))| (a.clone(), *act, *t)).collect();
        for (address, action, since) in entries {
            let d = self.bt.find(&address);
            let done = match action {
                "connecting" => d.is_some_and(|d| d.connected),
                "disconnecting" => d.is_some_and(|d| !d.connected),
                _ => d.is_none_or(|d| !d.known),
            };
            if !done && finished != Some(address.as_str()) && now.duration_since(since) < PENDING_TIMEOUT {
                continue;
            }
            self.pending.remove(&address);
            if action == "connecting"
                && d.is_some_and(|d| d.connected)
                && let Some(script) = script_path("audio-default.py")
            {
                spawn(vec!["python3".into(), "-I".into(), script.display().to_string(), address]);
            }
        }
    }

    // ------------------------------------------------------------ panel

    fn panel_content(&self) -> Element<'_, Message> {
        let shown = self.shown();
        let online = shown.iter().filter(|d| flag(d, "online")).count();
        let (state, kind) = self.alert(&shown);
        let size = self.core.applet.suggested_size(true).0;
        let color = (online > 0).then(|| [OK, LOW, CRITICAL][state as usize]);
        tinted(kind_icon(&kind), size, color, if online > 0 { 1.0 } else { DIM })
    }

    // ------------------------------------------------------------ popup

    fn header(&self) -> Element<'_, Message> {
        let shown = self.shown();
        let online = shown.iter().filter(|d| flag(d, "online")).count();
        let receivers = list(&self.stats, "receivers").len();
        let phase = s(&self.stats, "phase");
        let meta = match phase {
            "ok" => {
                let mut m = format!("{online} of {} devices connected", shown.len());
                if receivers > 0 {
                    m += &if receivers == 1 { " · 1 dongle".to_string() } else { format!(" · {receivers} dongles") };
                }
                m
            }
            "permission" => "Access needed".into(),
            "none" => "Nothing found".into(),
            _ => "Looking for devices …".into(),
        };
        let mut head = row![
            tinted(APP_ICON, 32, None, if online > 0 { 1.0 } else { DIM }),
            column![text::title4("Maprisma Bluetooth Monitor"), dim(meta)].spacing(2),
            space::horizontal().width(Length::Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        if self.page == Page::Main {
            if let Some(p) = self.lowest(&shown) {
                head = head.push(container(text::caption(format!("Lowest battery {p}%"))).padding([2, 10]).class(pill_style()));
            }
            let hidden = self.devices().len() - shown.len();
            if hidden > 0 || self.show_hidden {
                let label = if self.show_hidden { String::new() } else { hidden.to_string() };
                let eye = if self.show_hidden { "view-conceal-symbolic" } else { "view-reveal-symbolic" };
                let eye = button::text(label).leading_icon(icon::from_name(eye).size(16));
                let eye = if self.show_hidden { eye.class(theme::Button::Suggested) } else { eye };
                head = head.push(eye.on_press(Message::ToggleShowHidden));
            }
            head = head.push(icon_button("emblem-system-symbolic", Some(Message::Page(Page::Settings))));
        }
        head.into()
    }

    fn page_header<'a>(&self, title: &str, trailing: Option<Element<'a, Message>>) -> Element<'a, Message> {
        let mut line = row![
            button::text("Back").leading_icon(icon::from_name("go-previous-symbolic").size(16)).on_press(Message::Page(Page::Main)),
            text::heading(title.to_uppercase()),
            space::horizontal().width(Length::Fill),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if let Some(t) = trailing {
            line = line.push(t);
        }
        line.into()
    }

    fn main_page(&self) -> Element<'_, Message> {
        let phase = s(&self.stats, "phase");
        let mut content = column![].spacing(8);
        if phase == "permission" {
            content = content.push(self.setup_card());
        }
        let message = match phase {
            "none" => Some("No Logitech receiver and no battery-powered Bluetooth device found.".to_string()),
            "error" => Some(format!("Error: {}", s(&self.stats, "error"))),
            _ => match s(&self.stats, "message_code") {
                "errNotConnected" => Some("That device is not connected right now.".into()),
                "errChange" => {
                    let why = list(&self.stats, "message_args").first().and_then(Value::as_str).unwrap_or("");
                    Some(format!("Could not change the setting ({why})."))
                }
                _ => None,
            },
        };
        if let Some(m) = message {
            content = content.push(if phase == "none" {
                dim(m)
            } else {
                text::caption(m).class(theme::Text::Color(LOW)).into()
            });
        }
        let devices: Vec<&Value> = if self.show_hidden { self.devices().iter().collect() } else { self.shown() };
        let links = self.links(&devices);
        if !links.is_empty() {
            content = content.push(section("Connections"));
            content = content.push(self.tree(links));
            content = content.push(dim("Dotted line: radio link. Solid line: plugged in or built in."));
            content = content.push(divider::horizontal::default());
        }
        if !devices.is_empty() {
            content = content.push(section("Devices"));
            for d in devices {
                content = content.push(self.card(d));
            }
        }
        content.into()
    }

    fn setup_card(&self) -> Element<'_, Message> {
        let allow = button::suggested(if self.allowing { "Waiting for password …" } else { "Allow access" })
            .leading_icon(icon::from_name("dialog-password-symbolic").size(16))
            .on_press_maybe((!self.allowing).then_some(Message::AllowAccess));
        container(
            column![
                text::heading("Not ready yet · step 1 of 1"),
                text::caption(
                    "Linux only lets root talk to the receivers. Allowing access installs one udev rule \
                     (the same one Solaar uses) that gives the logged-in user access to Logitech receivers \
                     and devices. You will be asked for your password once."
                ),
                allow,
            ]
            .spacing(8),
        )
        .padding(12)
        .class(card_style(true))
        .into()
    }

    // Every dongle (numbered when there are several of a kind), then each Bluetooth
    // adapter (built in, or a USB stick), then USB cable and other wireless devices.
    fn links<'a>(&self, devices: &[&'a Value]) -> Vec<(String, String, &'static str, Vec<&'a Value>)> {
        let all = self.devices();
        let receivers = list(&self.stats, "receivers");
        let mut out = Vec::new();
        let mut per_family: HashMap<&str, usize> = HashMap::new();
        for r in receivers {
            *per_family.entry(s(r, "family")).or_default() += 1;
        }
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for r in receivers {
            let family = s(r, "family");
            let n = seen.entry(family).or_default();
            *n += 1;
            let mine: Vec<&Value> = devices.iter().copied().filter(|d| s(d, "receiver") == s(r, "id") && !s(r, "id").is_empty()).collect();
            if mine.is_empty() && !self.show_hidden {
                continue;
            }
            let paired = all.iter().filter(|d| s(d, "receiver") == s(r, "id")).count();
            let mut label = match connection_name(family) {
                "" => "Dongle".to_string(),
                name => name.to_string(),
            };
            if per_family[family] > 1 {
                label += &format!(" {n}");
            }
            out.push((label, format!("{paired} of {} slots", int(r, "max_devices").unwrap_or(0)), "media-removable-symbolic", mine));
        }
        let adapters = list(&self.stats, "adapters");
        let bt: Vec<&Value> = devices
            .iter()
            .copied()
            .filter(|d| d.get("receiver").is_none_or(Value::is_null) && s(d, "connection") == "bluetooth")
            .collect();
        for (i, ad) in adapters.iter().enumerate() {
            let mine: Vec<&Value> = bt
                .iter()
                .copied()
                .filter(|d| s(d, "adapter") == s(ad, "id") || (s(d, "adapter").is_empty() && i == 0))
                .collect();
            if mine.is_empty() && !self.show_hidden {
                continue;
            }
            let builtin = flag(ad, "builtin");
            let label = if builtin {
                "Built-in Bluetooth".to_string()
            } else if adapters.len() > 1 {
                format!("Bluetooth dongle {}", i + 1)
            } else {
                "Bluetooth dongle".to_string()
            };
            let mut sub = if builtin { "Inside this PC".to_string() } else { "USB stick".to_string() };
            if !s(ad, "vendor").is_empty() {
                sub += &format!(" · {}", s(ad, "vendor"));
            }
            out.push((label, sub, "bluetooth-active-symbolic", mine));
        }
        let mut groups = Vec::new();
        if adapters.is_empty() {
            groups.push(("bluetooth", "Bluetooth adapter", "bluetooth-active-symbolic"));
        }
        groups.push(("usb", "USB cable", "media-removable-symbolic"));
        groups.push(("wireless", "Wireless", "network-wireless-symbolic"));
        for (code, name, icon_name) in groups {
            let mine: Vec<&Value> = devices
                .iter()
                .copied()
                .filter(|d| d.get("receiver").is_none_or(Value::is_null) && s(d, "connection") == code)
                .collect();
            if !mine.is_empty() {
                out.push((name.to_string(), String::new(), icon_name, mine));
            }
        }
        out
    }

    // This PC, then each link (plugged in), then its devices (radio) with their battery.
    fn tree<'a>(&'a self, links: Vec<(String, String, &'static str, Vec<&'a Value>)>) -> Element<'a, Message> {
        let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
        let mut tree = column![
            row![tinted("computer-symbolic", 20, None, 1.0), text::heading(format!("This PC  ·  {}", clean(host.trim())))]
                .spacing(8)
                .align_y(Alignment::Center)
        ]
        .spacing(6);
        let count = links.len();
        for (i, (label, sub, icon_name, devices)) in links.into_iter().enumerate() {
            let live = devices.iter().any(|d| flag(d, "online"));
            let opacity = if live { 1.0 } else { DIM };
            let trunk = if i + 1 == count { "└─" } else { "├─" };
            let mut text_col = column![text::body(label)].spacing(1);
            if !sub.is_empty() {
                text_col = text_col.push(dim(sub));
            }
            let mut chips = column![].spacing(4);
            for d in devices {
                chips = chips.push(self.chip(d));
            }
            tree = tree.push(
                row![
                    text::body(trunk).class(theme::Text::Custom(dim_text)),
                    tinted(icon_name, 18, None, opacity),
                    container(text_col).width(Length::Fixed(130.0)),
                    chips,
                ]
                .spacing(8)
                .align_y(Alignment::Start),
            );
        }
        tree.into()
    }

    fn chip<'a>(&'a self, d: &'a Value) -> Element<'a, Message> {
        let online = flag(d, "online");
        let opacity = if online { 1.0 } else { DIM } * if self.is_hidden(d) { 0.6 } else { 1.0 };
        let dots: Element<Message> = if online {
            text::caption("····").class(theme::Text::Custom(accent_text)).into()
        } else {
            dim("····")
        };
        let mut chip = row![dots, tinted(kind_icon(s(d, "kind")), 16, None, opacity)].spacing(6).align_y(Alignment::Center);
        let name = device_name(d);
        chip = chip.push(if online { text::caption(name).into() } else { dim(name) });
        if !online {
            chip = chip.push(dim("off"));
        } else if let Some(b) = battery(d) {
            let color = self.battery_color(Some(b));
            chip = chip.push(tinted(&battery_icon(b), 16, color, 1.0));
            chip = chip.push(match color {
                Some(c) => text::caption(battery_text(b)).class(theme::Text::Color(c)),
                None => text::caption(battery_text(b)),
            });
        }
        chip.into()
    }

    fn status_line(d: &Value) -> String {
        let mut place = connection_name(s(d, "connection")).to_string();
        if let Some(slot) = int(d, "slot") {
            place += &format!(" · slot {slot}");
        }
        if !flag(d, "online") {
            return format!("{place} · not connected");
        }
        match battery(d) {
            Some(b) if flag(b, "charging") => format!("{place} · charging"),
            Some(b) if s(b, "status") == "full" && flag(b, "external_power") => format!("{place} · charged"),
            None => format!("{place} · no battery reading"),
            _ => place,
        }
    }

    // One card per device: badge, name and connection, battery, the eye to hide it, and
    // (Logitech devices) its settings when expanded.
    fn card<'a>(&'a self, d: &'a Value) -> Element<'a, Message> {
        let online = flag(d, "online");
        let hidden = self.is_hidden(d);
        let id = s(d, "id").to_string();
        let settings = online && has_settings(d);
        let open = settings && self.expanded.contains(&id);
        let opacity = if online { 1.0 } else { DIM };
        let mut status = Self::status_line(d);
        if hidden {
            status += " · hidden";
        }
        let name = device_name(d);
        let mut main = row![
            tinted(kind_icon(s(d, "kind")), 24, None, opacity),
            column![if online { text::heading(name).into() } else { dim(name) }, dim(status)].spacing(2),
            space::horizontal().width(Length::Fill),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        if online && let Some(b) = battery(d) {
            let color = self.battery_color(Some(b));
            main = main.push(tinted(&battery_icon(b), 16, color, 1.0));
            main = main.push(match color {
                Some(c) => text::body(battery_text(b)).class(theme::Text::Color(c)),
                None => text::body(battery_text(b)),
            });
        }
        if settings {
            main = main.push(tinted(if open { "pan-down-symbolic" } else { "pan-end-symbolic" }, 16, None, DIM));
        }
        let main: Element<Message> = if settings {
            button::custom(main).padding(4).class(theme::Button::Text).on_press(Message::Expand(id.clone())).width(Length::Fill).into()
        } else {
            container(main).padding(4).width(Length::Fill).into()
        };
        let eye = icon_button(
            if hidden { "view-reveal-symbolic" } else { "view-conceal-symbolic" },
            Some(Message::SetHidden(device_key(d), !hidden)),
        );
        let mut card = column![row![main, eye].spacing(4).align_y(Alignment::Center)].spacing(8);
        if open {
            card = card.push(divider::horizontal::light());
            card = card.push(self.device_settings(d));
        }
        container(card).padding(8).width(Length::Fill).class(card_style(online)).into()
    }

    fn slider_row<'a>(
        &'a self,
        id: &str,
        key: &'static str,
        min: f64,
        max: f64,
        step: f64,
        value: f64,
        label: impl Fn(f64) -> String,
    ) -> (Element<'a, Message>, String) {
        let current = self.dragging.get(&(id.to_string(), key)).copied().unwrap_or(value);
        let id1 = id.to_string();
        let id2 = id.to_string();
        let s = slider(min..=max.max(min + 1.0), current, move |v| Message::Drag(id1.clone(), key, v))
            .step(step.max(1.0))
            .on_release(Message::Release(id2, key))
            .width(Length::Fill);
        (s.into(), label(current))
    }

    fn device_settings<'a>(&'a self, d: &'a Value) -> Element<'a, Message> {
        let id = s(d, "id").to_string();
        let mut col = column![].spacing(10);
        if let Some(dpi) = d.get("dpi").filter(|v| int(v, "max").unwrap_or(0) > 0) {
            let default = int(dpi, "default");
            let value = int(dpi, "value").unwrap_or(0) as f64;
            let (s_el, shown) = self.slider_row(
                &id,
                "dpi",
                int(dpi, "min").unwrap_or(0) as f64,
                int(dpi, "max").unwrap_or(0) as f64,
                int(dpi, "step").unwrap_or(1) as f64,
                value,
                |v| {
                    let v = snap_dpi(dpi, v);
                    if Some(v) == default { format!("{v} · default") } else { v.to_string() }
                },
            );
            let mut head = row![text::body("Pointer speed (DPI)"), space::horizontal().width(Length::Fill), text::body(shown)]
                .spacing(8)
                .align_y(Alignment::Center);
            if let Some(def) = default.filter(|def| *def != value as i64) {
                head = head.push(button::text("Default").on_press(Message::Set(id.clone(), "dpi", json!(def))));
            }
            col = col.push(column![head, s_el].spacing(4));
        }
        if let Some(ss) = d.get("smartshift").filter(|v| v.is_object()) {
            let mode = ss.get("mode").cloned().unwrap_or(Value::Null);
            let idc = id.clone();
            col = col.push(text::body("Scroll wheel mode"));
            col = col.push(choices(
                vec![
                    (json!("smartshift"), "SmartShift".into()),
                    (json!("ratchet"), "Ratchet".into()),
                    (json!("freespin"), "Free spin".into()),
                ],
                &mode,
                move |v| Message::Set(idc.clone(), "smartshift", v),
            ));
            if mode == json!("smartshift") {
                let (s_el, shown) = self.slider_row(
                    &id,
                    "threshold",
                    1.0,
                    50.0,
                    1.0,
                    int(ss, "threshold").filter(|t| *t > 0).unwrap_or(10) as f64,
                    |v| (v.round() as i64).to_string(),
                );
                col = col.push(row![text::body("Sensitivity"), s_el, text::body(shown)].spacing(8).align_y(Alignment::Center));
                col = col.push(dim("Lower values switch to free spin with a gentler flick."));
            }
        }
        if let Some(wheel) = d.get("wheel").filter(|v| v.is_object()) {
            let (a, b) = (id.clone(), id.clone());
            col = col.push(setting_row(
                "Natural scrolling",
                "Content follows the wheel, like on a touchpad",
                toggler(flag(wheel, "invert")).on_toggle(move |on| Message::Set(a.clone(), "invert", json!(on))).into(),
            ));
            col = col.push(setting_row(
                "High-resolution scrolling",
                "Smoother, finer steps in apps that support it",
                toggler(flag(wheel, "hires")).on_toggle(move |on| Message::Set(b.clone(), "hires", json!(on))).into(),
            ));
        }
        if let Some(fn_swap) = d.get("fnswap").filter(|v| v.is_object()) {
            let on = flag(fn_swap, "on");
            let a = id.clone();
            col = col.push(setting_row(
                "Swap Fn keys",
                if on {
                    "On: top row gives media and special functions, hold Fn for F1–F12"
                } else {
                    "Off: top row gives F1–F12, hold Fn for media and special functions"
                },
                toggler(on).on_toggle(move |v| Message::Set(a.clone(), "fnswap", json!(v))).into(),
            ));
        }
        col.into()
    }

    fn settings_page(&self) -> Element<'_, Message> {
        let st = &self.settings;
        let pct = |v: i64| (json!(v), format!("{v}%"));
        let missing = !list(&self.stats, "denied").is_empty();
        let access: Element<Message> = if missing {
            button::suggested(if self.allowing { "Waiting for password …" } else { "Allow access" })
                .leading_icon(icon::from_name("dialog-password-symbolic").size(16))
                .on_press_maybe((!self.allowing).then_some(Message::AllowAccess))
                .into()
        } else {
            space::horizontal().width(Length::Shrink).into()
        };
        column![
            self.page_header("Settings", None),
            divider::horizontal::default(),
            column![
                text::body("Low battery warning"),
                dim("At this level or less the icon turns orange"),
                choices([15, 20, 25, 30, 40, 50].into_iter().map(pct).collect(), &json!(st.low_at), |v| {
                    Message::LowAt(v.as_i64().unwrap_or(25))
                }),
            ]
            .spacing(4),
            column![
                text::body("Critical battery warning"),
                dim("At this level or less the icon turns red"),
                choices([5, 10, 15, 20].into_iter().map(pct).collect(), &json!(st.critical_at), |v| {
                    Message::CriticalAt(v.as_i64().unwrap_or(10))
                }),
            ]
            .spacing(4),
            setting_row(
                "Other Bluetooth devices",
                "Trackpads, earphones, controllers and more (battery only)",
                toggler(st.other_devices).on_toggle(Message::OtherDevices).into(),
            ),
            setting_row("Receiver access", if missing { "Not granted" } else { "Granted" }, access),
        ]
        .spacing(12)
        .into()
    }

    fn bluetooth_page(&self) -> Element<'_, Message> {
        let bt = &self.bt;
        let scanning = self.scanning();
        let trailing: Option<Element<Message>> = bt.has_adapter.then(|| {
            let state = if scanning {
                text::caption("Scanning").class(theme::Text::Custom(accent_text)).into()
            } else {
                dim(if bt.powered { "On" } else { "Off" })
            };
            row![state, toggler(bt.powered).on_toggle(Message::BtPower)].spacing(8).align_y(Alignment::Center).into()
        });
        let mut content = column![self.page_header("Bluetooth", trailing), divider::horizontal::default()].spacing(8);
        let sections = bt.sections(scanning);
        let mut any = false;
        let mut available = 0;
        for (title, rows) in sections {
            if title == "Available" {
                available = rows.len();
            }
            if rows.is_empty() {
                continue;
            }
            if any {
                content = content.push(divider::horizontal::light());
            }
            any = true;
            content = content.push(section(title));
            for r in rows {
                content = content.push(self.bt_row(r, title == "Available"));
            }
        }
        if !any || (scanning && available == 0) {
            content = content.push(dim(if !bt.has_adapter {
                "No Bluetooth adapter"
            } else if !bt.powered {
                "Turn Bluetooth on to scan"
            } else {
                "Scanning for devices…"
            }));
        }
        content.into()
    }

    fn bt_row<'a>(&'a self, r: &'a BtRow, available: bool) -> Element<'a, Message> {
        let pending = self.pending.get(&r.address).map(|p| p.0);
        let asking = self.confirm_forget == r.address;
        let mut words = column![text::heading(r.name.clone())].spacing(2);
        let status = match pending {
            Some("forgetting") => "Forgetting…".to_string(),
            Some("disconnecting") => "Disconnecting…".to_string(),
            Some(_) => "Connecting…".to_string(),
            None => match r.battery {
                Some(p) if r.connected => format!("{p} %"),
                _ => String::new(),
            },
        };
        if !status.is_empty() {
            words = words.push(if pending.is_some() {
                text::caption(status).class(theme::Text::Custom(accent_text)).into()
            } else {
                dim(status)
            });
        }
        let mut line = row![
            tinted(bluez_icon(&r.icon, r.connected), 20, None, if r.connected { 1.0 } else { DIM }),
            container(words).width(Length::Fill),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        let idle = pending.is_none();
        if !available {
            let forget = if asking {
                button::destructive("Really forget?")
                    .leading_icon(icon::from_name("user-trash-symbolic").size(16))
                    .on_press_maybe(idle.then(|| Message::Forget(r.address.clone())))
                    .into()
            } else {
                icon_button("user-trash-symbolic", idle.then(|| Message::Forget(r.address.clone())))
            };
            line = line.push(forget);
        }
        let main = if r.connected {
            button::standard("Disconnect")
                .leading_icon(icon::from_name("bluetooth-disabled-symbolic").size(16))
                .on_press_maybe(idle.then(|| Message::BtAction("disconnect", r.address.clone())))
        } else {
            let action = if available { "pair" } else { "connect" };
            button::standard(if available { "Pair" } else { "Connect" })
                .leading_icon(icon::from_name("bluetooth-active-symbolic").size(16))
                .on_press_maybe(idle.then(|| Message::BtAction(action, r.address.clone())))
        };
        line.push(main).into()
    }

    fn footer(&self) -> Element<'_, Message> {
        row![
            button::standard("Refresh").leading_icon(icon::from_name("view-refresh-symbolic").size(16)).on_press(Message::Refresh),
            button::standard("Bluetooth")
                .leading_icon(icon::from_name("bluetooth-active-symbolic").size(16))
                .on_press(Message::Page(Page::Bluetooth)),
        ]
        .spacing(8)
        .into()
    }
}

const APP_ICON: &str = "io.github.ubruckhaus.CosmicMaprismaBluetoothMonitor-symbolic";

// A device with a list of DPI values (not evenly spaced) only takes those; else
// min..max in steps.
fn snap_dpi(dpi: &Value, raw: f64) -> i64 {
    let choices: Vec<i64> = list(dpi, "choices").iter().filter_map(Value::as_i64).collect();
    if let Some(best) = choices.iter().min_by(|a, b| ((**a as f64) - raw).abs().total_cmp(&((**b as f64) - raw).abs())) {
        return *best;
    }
    let min = int(dpi, "min").unwrap_or(0);
    let max = int(dpi, "max").unwrap_or(min);
    let step = int(dpi, "step").filter(|s| *s > 0).unwrap_or(1);
    (min + (((raw - min as f64) / step as f64).round() as i64) * step).clamp(min, max)
}

impl cosmic::Application for Monitor {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: ()) -> (Self, Task<Message>) {
        (Self { core, settings: Settings::load(), ..Default::default() }, Task::none())
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::CloseRequested(id))
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            rectangle_tracker_subscription(0).map(|update| Message::Rectangle(update.1)),
            Subscription::run_with("bluetooth-monitor-helper", |_| {
                stream::channel(4, |mut output| async move {
                    run_helper(&mut output).await;
                })
            }),
        ];
        // The Bluetooth page: BlueZ's list every two seconds, and a scan while it is on
        // screen. The scan is bluetoothctl's own discovery session: BlueZ ends it the
        // moment the process exits, which happens as soon as this subscription goes
        // (page left, popup closed, applet gone): an endless scan makes audio stutter.
        if (self.popup.is_some() && self.page == Page::Bluetooth) || !self.pending.is_empty() {
            subscriptions.push(cosmic::iced::time::every(BLUEZ_POLL).map(|_| Message::BtTick));
        }
        if self.scanning() {
            subscriptions.push(Subscription::run_with("bluetooth-monitor-scan", |_| {
                stream::channel(1, |_output: mpsc::Sender<Message>| async move {
                    run_scan().await;
                })
            }));
        }
        Subscription::batch(subscriptions)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Snapshot(stats) => self.stats = stats,
            Message::HelperStarted(sender) => {
                self.helper = Some(sender);
                // The helper (re)started: tell it the options and whether the panel is open.
                self.send(json!({"cmd": "options", "other_devices": self.settings.other_devices}));
                if self.popup.is_some() {
                    self.send(json!({"cmd": "open"}));
                }
            }
            Message::Rectangle(update) => match update {
                RectangleUpdate::Rectangle(r) => self.rectangle = r.1,
                RectangleUpdate::Init(tracker) => self.tracker = Some(tracker),
            },
            Message::CloseRequested(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                    self.send(json!({"cmd": "closed"}));
                }
            }
            Message::TogglePopup => {
                if let Some(popup) = self.popup.take() {
                    self.send(json!({"cmd": "closed"}));
                    return destroy_popup(popup);
                }
                self.page = Page::Main;
                self.show_hidden = false;
                self.confirm_forget.clear();
                self.send(json!({"cmd": "open"}));
                return cosmic::surface::surface_task(cosmic::surface::action::app_popup(
                    |_| Default::default(),
                    |app: &mut Self| {
                        let id = window::Id::unique();
                        app.popup = Some(id);
                        let mut settings =
                            app.core.applet.get_popup_settings(app.core.main_window_id().unwrap(), id, None, None, None);
                        let Rectangle { x, y, width, height } = app.rectangle;
                        settings.positioner.anchor_rect = Rectangle::<i32> {
                            x: x.max(1.0) as i32,
                            y: y.max(1.0) as i32,
                            width: width.max(1.0) as i32,
                            height: height.max(1.0) as i32,
                        };
                        settings.positioner.size = None;
                        settings
                    },
                    None,
                ));
            }
            Message::Refresh => self.send(json!({"cmd": "refresh"})),
            Message::Page(page) => return self.set_page(page),
            Message::ToggleShowHidden => self.show_hidden = !self.show_hidden,
            Message::Expand(id) => {
                if !self.expanded.remove(&id) {
                    self.expanded.insert(id);
                }
            }
            Message::SetHidden(key, on) => {
                if !key.is_empty() {
                    if on {
                        self.settings.hidden.insert(key);
                    } else {
                        self.settings.hidden.remove(&key);
                    }
                    self.settings.save();
                }
            }
            Message::Set(device, key, value) => {
                self.send(json!({"cmd": "set", "device": device, "key": key, "value": value}));
            }
            Message::Drag(device, key, value) => {
                self.dragging.insert((device, key), value);
            }
            Message::Release(device, key) => {
                if let Some(value) = self.dragging.remove(&(device.clone(), key)) {
                    let value = if key == "dpi" {
                        let dpi = self.devices().iter().find(|d| s(d, "id") == device).and_then(|d| d.get("dpi")).cloned();
                        json!(dpi.map_or(value.round() as i64, |dpi| snap_dpi(&dpi, value)))
                    } else {
                        json!(value.round() as i64)
                    };
                    self.send(json!({"cmd": "set", "device": device, "key": key, "value": value}));
                }
            }
            Message::LowAt(v) => {
                self.settings.low_at = v;
                self.settings.save();
            }
            Message::CriticalAt(v) => {
                self.settings.critical_at = v;
                self.settings.save();
            }
            Message::OtherDevices(on) => {
                self.settings.other_devices = on;
                self.settings.save();
                self.send(json!({"cmd": "options", "other_devices": on}));
            }
            Message::AllowAccess => {
                // One polkit prompt installs the uaccess udev rule (scripts/allow-access.sh).
                let Some(script) = script_path("allow-access.sh") else { return Task::none() };
                if self.allowing {
                    return Task::none();
                }
                self.allowing = true;
                return Task::perform(
                    async move {
                        let _ = tokio::process::Command::new("pkexec")
                            .arg("/bin/bash")
                            .arg(script)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status()
                            .await;
                    },
                    |_| cosmic::Action::App(Message::AllowDone),
                );
            }
            Message::AllowDone => {
                self.allowing = false;
                self.send(json!({"cmd": "refresh"}));
            }
            Message::BtTick => {
                return Task::perform(read_bluez(), |state| cosmic::Action::App(Message::Bluez(state)));
            }
            Message::Bluez(state) => {
                self.bt = state;
                self.check_pending(None);
                for address in std::mem::take(&mut self.finished) {
                    self.check_pending(Some(&address));
                }
            }
            Message::BtAction(action, address) => {
                if !is_address(&address) || self.pending.contains_key(&address) {
                    return Task::none();
                }
                let Some(script) = script_path("bt-device.sh") else { return Task::none() };
                let label = match action {
                    "disconnect" => "disconnecting",
                    "forget" => "forgetting",
                    _ => "connecting",
                };
                self.pending.insert(address.clone(), (label, Instant::now()));
                let done = address.clone();
                return Task::perform(
                    async move {
                        let _ = tokio::process::Command::new("bash")
                            .arg(script)
                            .arg(action)
                            .arg(address)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status()
                            .await;
                    },
                    move |_| cosmic::Action::App(Message::BtActionDone(done.clone())),
                );
            }
            Message::BtActionDone(address) => {
                // The helper ended: the action leaves the pending list once BlueZ was read.
                self.finished.push(address);
                return Task::perform(read_bluez(), |state| cosmic::Action::App(Message::Bluez(state)));
            }
            Message::Forget(address) => {
                if self.confirm_forget != address {
                    // Forget asks twice.
                    self.confirm_forget = address;
                } else {
                    self.confirm_forget.clear();
                    return self.update(Message::BtAction("forget", address));
                }
            }
            Message::BtPower(on) => {
                if let Some(script) = script_path("bt-power.sh") {
                    spawn(vec!["bash".into(), script.display().to_string(), if on { "on" } else { "off" }.into()]);
                }
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let horizontal = matches!(self.core.applet.anchor, PanelAnchor::Top | PanelAnchor::Bottom);
        let padding = self.core.applet.suggested_padding(true).0;
        let button = button::custom(self.panel_content())
            .padding(if horizontal { [0, padding] } else { [padding, 0] })
            .on_press_down(Message::TogglePopup)
            .class(theme::Button::AppletIcon);
        // Right click: refresh.
        let button = mouse_area(button).on_right_press(Message::Refresh);
        let content: Element<Message> = match self.tracker.as_ref() {
            Some(tracker) => tracker.container(0, button).ignore_bounds(true).into(),
            None => button.into(),
        };
        autosize::autosize(content, AUTOSIZE_ID.clone()).into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Message> {
        self.popup_view()
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

impl Monitor {
    fn popup_view(&self) -> Element<'_, Message> {
        let body = match self.page {
            Page::Main => self.main_page(),
            Page::Settings => self.settings_page(),
            Page::Bluetooth => self.bluetooth_page(),
        };
        let inner = column![self.header(), divider::horizontal::default(), body].spacing(10);
        let details = scrollable(padded_control(inner).padding([12, 16])).height(Length::Shrink);
        let mut content = column![container(details).max_height(POPUP_MAX_HEIGHT)].width(Length::Fixed(POPUP_WIDTH));
        if self.page == Page::Main {
            content = content.push(divider::horizontal::default());
            content = content.push(container(self.footer()).padding([8, 16]));
        }
        self.core.applet.popup_container(content.padding([0, 0, 8, 0])).into()
    }
}

// ------------------------------------------------------------------------- processes

// Next to the binary (installed: ../share/maprisma-bluetooth-monitor/scripts), or in
// the data home; BLUETOOTH_MONITOR_SCRIPTS wins (development).
fn script_path(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("BLUETOOTH_MONITOR_SCRIPTS") {
        dirs.push(dir.into());
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(bin) = exe.parent()
    {
        dirs.push(bin.join("../share/maprisma-bluetooth-monitor/scripts"));
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    if let Some(data) = data_home {
        dirs.push(data.join("maprisma-bluetooth-monitor/scripts"));
    }
    dirs.push("/usr/share/maprisma-bluetooth-monitor/scripts".into());
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

// Runs the helper, streams its snapshots and writes the panel's commands to its stdin;
// starts it again RESTART_DELAY after it exits.
async fn run_helper(output: &mut mpsc::Sender<Message>) {
    loop {
        if let Some(script) = script_path("bluetooth-monitor.py") {
            let child = tokio::process::Command::new("python3")
                .arg("-I")
                .arg(&script)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn();
            if let Ok(mut child) = child
                && let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take())
            {
                let (tx, mut rx) = unbounded_channel::<String>();
                if output.send(Message::HelperStarted(tx)).await.is_err() {
                    return;
                }
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    tokio::select! {
                        line = lines.next_line() => match line {
                            Ok(Some(line)) => {
                                // A partial line while the helper exits is skipped.
                                if let Ok(stats) = serde_json::from_str::<Value>(&line)
                                    && stats.is_object()
                                    && output.send(Message::Snapshot(stats)).await.is_err()
                                {
                                    return;
                                }
                            }
                            _ => break,
                        },
                        command = rx.recv() => match command {
                            Some(command) => {
                                if stdin.write_all(command.as_bytes()).await.is_err() {
                                    break;
                                }
                                let _ = stdin.flush().await;
                            }
                            None => break,
                        },
                    }
                }
                let _ = child.kill().await;
            }
        }
        tokio::time::sleep(RESTART_DELAY).await;
    }
}

// bluetoothctl's discovery session, started again (not in a tight loop) after its
// timeout; killed when the future is dropped.
async fn run_scan() {
    loop {
        let child = tokio::process::Command::new("bluetoothctl")
            .args(["--timeout", &SCAN_SECONDS.to_string(), "scan", "on"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        if let Ok(mut child) = child {
            let _ = child.wait().await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// Runs argv without waiting for it; a thread reaps it.
fn spawn(argv: Vec<String>) {
    if let Ok(mut child) = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bluez_listing() {
        let out = r#"{"type":"a{oa{sa{sv}}}","data":[{
            "/org/bluez/hci0":{"org.bluez.Adapter1":{"Powered":{"type":"b","data":true}}},
            "/org/bluez/hci0/dev_AA":{"org.bluez.Device1":{"Address":{"type":"s","data":"AA:BB:CC:DD:EE:FF"},
              "Alias":{"type":"s","data":"Headset"},"Icon":{"type":"s","data":"audio-headset"},
              "Connected":{"type":"b","data":true},"Paired":{"type":"b","data":true}},
              "org.bluez.Battery1":{"Percentage":{"type":"y","data":64}}},
            "/org/bluez/hci0/dev_BB":{"org.bluez.Device1":{"Address":{"type":"s","data":"11:22:33:44:55:66"},
              "Alias":{"type":"s","data":"11-22-33-44-55-66"},"Connected":{"type":"b","data":false}}}
        }]}"#;
        let state = parse_bluez(out);
        assert!(state.has_adapter && state.powered);
        assert_eq!(state.devices.len(), 2);
        let [(_, connected), (_, paired), (_, available)] = state.sections(true);
        assert_eq!(connected.len(), 1);
        assert_eq!(connected[0].battery, Some(64));
        assert!(paired.is_empty());
        assert!(available.is_empty(), "an address as name is left out");
        assert_eq!(bluez_icon("audio-headset", true), "audio-headphones-symbolic");
    }

    #[test]
    fn dpi_snaps_to_choices_or_steps() {
        assert_eq!(snap_dpi(&json!({"choices": [400, 800, 1600]}), 1100.0), 800);
        assert_eq!(snap_dpi(&json!({"min": 200, "max": 8000, "step": 50}), 1234.0), 1250);
        assert_eq!(snap_dpi(&json!({"min": 200, "max": 8000, "step": 50}), 99999.0), 8000);
    }

    // Reads this PC's BlueZ list (read only): cargo test -- --ignored
    #[test]
    #[ignore]
    fn live_bluez() {
        let state = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(read_bluez());
        println!("adapter {} powered {} devices {}", state.has_adapter, state.powered, state.devices.len());
        for (title, rows) in state.sections(true) {
            println!("{title}: {:?}", rows.iter().map(|r| (&r.name, r.connected, r.battery)).collect::<Vec<_>>());
        }
        assert!(state.has_adapter);
    }

    #[test]
    fn names_and_addresses() {
        assert!(is_address("AA:bb:01:02:03:04"));
        assert!(!is_address("AA:bb:01:02:03"));
        assert!(meaningless_name("F2-B2-05-C6-44-41"));
        assert!(meaningless_name("0000110b-0000-1000-8000-00805f9b34fb"));
        assert!(!meaningless_name("MX Master 4"));
        assert_eq!(clean("Mouse\u{7}\n"), "Mouse");
    }
}
