mod i18n;
use i18n::{tr, trf};
mod shared;
mod window_control;
mod shot;
mod corners;
mod look;
mod window_size;
use shared::script_path;
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
const WINDOW_APP_ID: &str = "io.github.ubruckhaus.CosmicMaprismaBluetoothMonitor.Window";
static NOTE_ID: LazyLock<cosmic::widget::Id> = LazyLock::new(|| cosmic::widget::Id::new("bluetooth-monitor-shot-note"));
// F12 pictures go to ~/Pictures/<this>/ (the app's name, the same in every language).
const SHOT_FOLDER: &str = "Maprisma Bluetooth Monitor";
static AUTOSIZE_ID: LazyLock<Id> = LazyLock::new(|| Id::new("bluetooth-monitor-autosize"));

const RESTART_DELAY: Duration = Duration::from_secs(5); // before a helper that exited starts again
const PENDING_TIMEOUT: Duration = Duration::from_secs(60); // pair + connect take up to 45 s
const SCAN_SECONDS: u32 = 300; // bluetoothctl's own scan timeout (a backstop)
const BLUEZ_POLL: Duration = Duration::from_secs(2); // the Bluetooth page's refresh
const MAX_AVAILABLE: usize = 40; // devices found by a scan that are listed
const POPUP_WIDTH: f32 = 440.0;
const POPUP_MAX_HEIGHT: f32 = 760.0;
const DIM: f32 = 0.6;

// Battery states in the theme's state colours (as the Omarchy widget): charging/fine green,
// low the warning colour, critical the destructive colour.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Level {
    Ok,
    Low,
    Critical,
}

impl Level {
    fn color(self, theme: &cosmic::Theme) -> Color {
        let c = theme.cosmic();
        match self {
            Level::Ok => c.success_color(),
            Level::Low => c.warning_color(),
            Level::Critical => c.destructive_color(),
        }
        .into()
    }

    fn text(self) -> theme::Text {
        fn ok(t: &cosmic::Theme) -> iced_text::Style { iced_text::Style { color: Some(Level::Ok.color(t)), ..Default::default() } }
        fn low(t: &cosmic::Theme) -> iced_text::Style { iced_text::Style { color: Some(Level::Low.color(t)), ..Default::default() } }
        fn critical(t: &cosmic::Theme) -> iced_text::Style { iced_text::Style { color: Some(Level::Critical.color(t)), ..Default::default() } }
        theme::Text::Custom(match self { Level::Ok => ok, Level::Low => low, Level::Critical => critical })
    }
}

fn main() -> cosmic::iced::Result {
    if std::env::args().any(|arg| arg == "--window") {
        let connection = zbus::blocking::connection::Builder::session().expect("session bus")
            .serve_at("/Window", window_control::WindowControl).expect("window control")
            .build().expect("session bus connection");
        if connection.request_name_with_flags(shared::WINDOW_BUS, zbus::fdo::RequestNameFlags::DoNotQueue.into()).is_err() {
            // A second `--window` start toggles (layout rule 5a): the open window is told to close,
            // as its tray does, so every entry point (tray, Maprisma Shell's glyph) toggles.
            let _ = connection.call_method(Some(shared::WINDOW_BUS), "/Window", Some("io.github.ubruckhaus.BluetoothMonitor.Window"), "Close", &());
            return Ok(());
        }
        let result = cosmic::app::run::<Monitor<true>>(cosmic::app::Settings::default()
            .size({ let (width, height) = window_start_size(); cosmic::iced::Size::new(width, height) })
            .resizable(Some(8.0)).transparent(false).is_daemon(false), true);
        drop(connection);
        result
    } else {
        cosmic::applet::run::<Monitor>(false)
    }
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

fn connection_name(code: &str) -> &'static str {
    match code {
        "unifying" => tr("Unifying dongle"), "bolt" => tr("Bolt dongle"),
        "nano" => tr("Nano dongle"), "lightspeed" => tr("Lightspeed dongle"),
        "bluetooth" => tr("Bluetooth"), "usb" => tr("USB cable"),
        "wireless" => tr("Wireless"), _ => "",
    }
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

// Translate helper messages here; the shared Python helpers remain byte-for-byte unchanged.
fn helper_error(message: &str) -> String {
    match message {
        "not a number" => tr("not a number").into(),
        "not on/off" => tr("not on/off").into(),
        "no DPI setting" => tr("no DPI setting").into(),
        "no DPI range" => tr("no DPI range").into(),
        "DPI out of range" => tr("DPI out of range").into(),
        "no answer" => tr("no answer").into(),
        _ => if let Some(code) = message.strip_prefix("HID++ error ") {
            trf("HID++ error {0}", &[code.into()])
        } else { message.to_string() },
    }
}

fn battery_text(b: &Value) -> String {
    match b.get("percent").and_then(Value::as_i64) {
        Some(p) => format!("{p}%"),
        None => match s(b, "level") {
            "critical" => tr("critical"), "low" => tr("low"), "empty" => tr("empty"),
            "good" => tr("good"), "full" => tr("full"), _ => tr("unknown"),
        }.to_string(),
    }
}

fn device_name(d: &Value) -> String {
    let name = clean(s(d, "name"));
    if name.is_empty() { tr("Logitech device").into() } else { name }
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
    language: String,
    low_at: i64,
    critical_at: i64,
    other_devices: bool,
    hidden: BTreeSet<String>,
    // F12 saves a screenshot (on by default) and Corners ("desktop" by default).
    screenshot_key: bool,
    corners: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self { language: "auto".into(), low_at: 25, critical_at: 10, other_devices: true, hidden: BTreeSet::new(), screenshot_key: true, corners: "desktop".into() }
    }
}

// The settings file as it is now (an empty object when missing or broken).
fn config_value() -> Value {
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|data| serde_json::from_str::<Value>(&data).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

// Written through a temp file in the same folder, then renamed.
fn write_config(data: &Value) {
    let Some(path) = config_path() else { return };
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = dir.join(format!(".settings.json.{}", std::process::id()));
    if std::fs::write(&tmp, format!("{data:#}\n")).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

// The `--window` size: the one the user left it at (layout rule 5a), else the popup's.
fn window_start_size() -> window_size::Size {
    window_size::parse(config_value().get(window_size::KEY)).unwrap_or((POPUP_WIDTH, POPUP_MAX_HEIGHT))
}

// Only the window size changes; the other settings stay as they are in the file.
fn save_window_size(size: window_size::Size) {
    let mut data = config_value();
    data[window_size::KEY] = window_size::to_value(size);
    write_config(&data);
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
        out.language = v["language"].as_str().filter(|c| *c == "auto" || i18n::supported(c)).unwrap_or("auto").to_string();
        if let Some(b) = v.get("screenshot_key").and_then(Value::as_bool) {
            out.screenshot_key = b;
        }
        out.corners = corners::normalize(v.get("corners").and_then(Value::as_str).unwrap_or("")).to_string();
        out.hidden = list(&v, "hidden").iter().filter_map(Value::as_str).take(500).map(str::to_string).collect();
        out
    }

    // The window size another process saved stays.
    fn save(&self) {
        let mut data = json!({
            "language": self.language,
            "low_at": self.low_at,
            "critical_at": self.critical_at,
            "other_devices": self.other_devices,
            "hidden": self.hidden.iter().collect::<Vec<_>>(),
            "screenshot_key": self.screenshot_key,
        });
        // "Like the desktop" is the default: no key in the file.
        if self.corners != "desktop" {
            data["corners"] = json!(self.corners);
        }
        if let Some(size) = config_value().get(window_size::KEY) {
            data[window_size::KEY] = size.clone();
        }
        write_config(&data);
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

fn tinted(name: &str, size: u16, level: Option<Level>, opacity: f32) -> Element<'static, Message> {
    let mut i = icon::from_name(name.to_string()).size(size).symbolic(true).icon().opacity(opacity);
    if let Some(level) = level {
        i = i.class(theme::Svg::custom(move |theme| iced_svg::Style { color: Some(level.color(theme)) }));
    }
    i.into()
}

fn section<'a>(title: &str) -> Element<'a, Message> {
    container(text::caption_heading(title.to_uppercase()).class(theme::Text::Custom(dim_text)))
        .padding([8, 0, 0, 0])
        .into()
}

fn card_style(online: bool) -> theme::Container<'static> {
    let radius = corners::now().radius;
    theme::Container::custom(move |theme: &cosmic::Theme| {
        let mut color = Color::from(theme.cosmic().on_bg_color());
        color.a = if online { 0.07 } else { 0.03 };
        iced_container::Style {
            background: Some(Background::Color(color)),
            // Settings › Corners: cards take the full radius.
            border: Border { radius: radius.into(), ..Default::default() },
            ..Default::default()
        }
    })
}

fn pill_style() -> theme::Container<'static> {
    // Settings › Corners: round, or square in Square.
    let radius = corners::now().pill;
    theme::Container::custom(move |theme: &cosmic::Theme| {
        let mut color = Color::from(theme.cosmic().on_bg_color());
        color.a = 0.12;
        iced_container::Style {
            background: Some(Background::Color(color)),
            border: Border { radius: radius.into(), ..Default::default() },
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
    let mut words = column![text::body(caption.to_string())].spacing(4);
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
struct Monitor<const WINDOW_MODE: bool = false> {
    core: Core,
    window_mode: bool,
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
    // F12: the note's state, whether a capture is on its way.
    shot: shot::Phase,
    capturing: bool,
    resize: window_size::Debounce,
}

/// What Esc does (layout rule 5a): the innermost open thing first, then the window or popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscStep {
    /// Skip the screenshot's note (or close its error).
    Note,
    /// Drop the "Forget?" question.
    Confirm,
    /// Back from Settings / Bluetooth to the main page.
    Back,
    /// Close the window (`--window`) or the popup.
    Close,
    Nothing,
}

/// `shown`: Esc came from the window or the open popup (not the panel button).
fn esc_step(shown: bool, note: bool, confirm: bool, side_page: bool) -> EscStep {
    if !shown {
        EscStep::Nothing
    } else if note {
        EscStep::Note
    } else if confirm {
        EscStep::Confirm
    } else if side_page {
        EscStep::Back
    } else {
        EscStep::Close
    }
}

#[derive(Debug, Clone)]
enum Message {
    LanguageTick,
    WindowTick,
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
    Language(String),
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
    ShotKey(window::Id),
    Escape(window::Id),
    Captured(window::Screenshot),
    ShotSaved(Result<shot::Saved, String>),
    NoteInput(String),
    NoteDone,
    NoteSaved(Result<(), String>),
    SetShotKey(bool),
    Corners(String),
}

// Runs `work` on its own thread (saving must not hold up the popup) and answers with `done`.
fn off_thread<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
    done: fn(Result<T, String>) -> Message,
) -> Task<Message> {
    let (sender, receiver) = cosmic::iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    Task::perform(receiver, move |result| {
        cosmic::Action::App(done(result.unwrap_or_else(|_| Err("the file could not be written".into()))))
    })
}

impl<const WINDOW_MODE: bool> Monitor<WINDOW_MODE> {
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
    fn battery_color(&self, b: Option<&Value>) -> Option<Level> {
        let b = b?;
        if flag(b, "charging") {
            return Some(Level::Ok);
        }
        match self.battery_state(Some(b)) {
            2 => Some(Level::Critical),
            1 => Some(Level::Low),
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
        (self.window_mode || self.popup.is_some()) && self.page == Page::Bluetooth && self.bt.powered
    }

    // A resize that has settled (or, on `closing`, any waiting one) goes to settings.json; not
    // while maximised, fullscreen or tiled (libcosmic's sharp corners).
    fn save_size(&mut self, closing: bool) {
        let sharp = self.core.window.sharp_corners;
        let ignore = || window_size::not_users_size(sharp);
        let size = if closing { self.resize.flush(ignore) } else { self.resize.due(Instant::now(), ignore) };
        if let Some(size) = size {
            save_window_size(size);
        }
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
        let color = (online > 0).then(|| [Level::Ok, Level::Low, Level::Critical][state as usize]);
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
                let mut m = trf("{0} of {1} devices connected", &[online.to_string(), shown.len().to_string()]);
                if receivers > 0 {
                    m += &if receivers == 1 { format!(" · {}", tr("1 dongle")) } else { format!(" · {}", trf("{0} dongles", &[receivers.to_string()])) };
                }
                m
            }
            "permission" => tr("Access needed").into(),
            "none" => tr("Nothing found").into(),
            _ => tr("Looking for devices …").into(),
        };
        let mut head = row![
            tinted(APP_ICON, 32, None, if online > 0 { 1.0 } else { DIM }),
            column![text::title4(tr("Maprisma Bluetooth Monitor")), dim(meta)].spacing(4),
            space::horizontal().width(Length::Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        if self.page == Page::Main {
            if let Some(p) = self.lowest(&shown) {
                head = head.push(container(text::caption(trf("Lowest battery {0}%", &[p.to_string()]))).padding([4, 8]).class(pill_style()));
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
            button::text(tr("Back")).leading_icon(icon::from_name("go-previous-symbolic").size(16)).on_press(Message::Page(Page::Main)),
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
            "none" => Some(tr("No Logitech receiver and no battery-powered Bluetooth device found.").to_string()),
            "error" => Some(trf("Error: {0}", &[helper_error(s(&self.stats, "error"))])),
            _ => match s(&self.stats, "message_code") {
                "errNotConnected" => Some(tr("That device is not connected right now.").into()),
                "errChange" => {
                    let why = list(&self.stats, "message_args").first().and_then(Value::as_str).unwrap_or("");
                    Some(trf("Could not change the setting ({0}).", &[helper_error(why)]))
                }
                _ => None,
            },
        };
        if let Some(m) = message {
            content = content.push(if phase == "none" {
                dim(m)
            } else {
                text::caption(m).class(Level::Low.text()).into()
            });
        }
        let devices: Vec<&Value> = if self.show_hidden { self.devices().iter().collect() } else { self.shown() };
        let links = self.links(&devices);
        if !links.is_empty() {
            content = content.push(section(tr("Connections")));
            content = content.push(self.tree(links));
            content = content.push(dim(tr("Dotted line: radio link. Solid line: plugged in or built in.")));
            content = content.push(divider::horizontal::default());
        }
        if !devices.is_empty() {
            content = content.push(section(tr("Devices")));
            for d in devices {
                content = content.push(self.card(d));
            }
        }
        content.into()
    }

    fn setup_card(&self) -> Element<'_, Message> {
        let allow = button::suggested(if self.allowing { tr("Waiting for password …") } else { tr("Allow access") })
            .leading_icon(icon::from_name("dialog-password-symbolic").size(16))
            .on_press_maybe((!self.allowing).then_some(Message::AllowAccess));
        container(
            column![
                text::heading(tr("Not ready yet · step 1 of 1")),
                text::caption(
                    tr("Linux only lets root talk to the receivers. Allowing access installs one udev rule (the same one Solaar uses) that gives the logged-in user access to Logitech receivers and devices. You will be asked for your password once.")
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
                "" => tr("Dongle").to_string(),
                name => name.to_string(),
            };
            if per_family[family] > 1 {
                label += &format!(" {n}");
            }
            out.push((label, trf("{0} of {1} slots", &[paired.to_string(), int(r, "max_devices").unwrap_or(0).to_string()]), "media-removable-symbolic", mine));
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
                tr("Built-in Bluetooth").to_string()
            } else if adapters.len() > 1 {
                format!("{} {}", tr("Bluetooth dongle"), i + 1)
            } else {
                tr("Bluetooth dongle").to_string()
            };
            let mut sub = if builtin { tr("Inside this PC").to_string() } else { tr("USB stick").to_string() };
            if !s(ad, "vendor").is_empty() {
                sub += &format!(" · {}", s(ad, "vendor"));
            }
            out.push((label, sub, "bluetooth-active-symbolic", mine));
        }
        let mut groups = Vec::new();
        if adapters.is_empty() {
            groups.push(("bluetooth", tr("Bluetooth adapter"), "bluetooth-active-symbolic"));
        }
        groups.push(("usb", tr("USB cable"), "media-removable-symbolic"));
        groups.push(("wireless", tr("Wireless"), "network-wireless-symbolic"));
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
            row![tinted("computer-symbolic", 20, None, 1.0), text::heading(format!("{}  ·  {}", tr("This PC"), clean(host.trim())))]
                .spacing(8)
                .align_y(Alignment::Center)
        ]
        .spacing(8);
        let count = links.len();
        for (i, (label, sub, icon_name, devices)) in links.into_iter().enumerate() {
            let live = devices.iter().any(|d| flag(d, "online"));
            let opacity = if live { 1.0 } else { DIM };
            let trunk = if i + 1 == count { "└─" } else { "├─" };
            let mut text_col = column![text::body(label)].spacing(4);
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
        let mut chip = row![dots, tinted(kind_icon(s(d, "kind")), 16, None, opacity)].spacing(8).align_y(Alignment::Center);
        let name = device_name(d);
        chip = chip.push(if online { text::caption(name).into() } else { dim(name) });
        if !online {
            chip = chip.push(dim(tr("off")));
        } else if let Some(b) = battery(d) {
            let color = self.battery_color(Some(b));
            chip = chip.push(tinted(&battery_icon(b), 16, color, 1.0));
            chip = chip.push(match color {
                Some(c) => text::caption(battery_text(b)).class(c.text()),
                None => text::caption(battery_text(b)),
            });
        }
        chip.into()
    }

    fn status_line(d: &Value) -> String {
        let mut place = connection_name(s(d, "connection")).to_string();
        if let Some(slot) = int(d, "slot") {
            place += &format!(" · {}", trf("slot {0}", &[slot.to_string()]));
        }
        if !flag(d, "online") {
            return format!("{place} · {}", tr("not connected"));
        }
        match battery(d) {
            Some(b) if flag(b, "charging") => format!("{place} · {}", tr("charging")),
            Some(b) if s(b, "status") == "full" && flag(b, "external_power") => format!("{place} · {}", tr("charged")),
            None => format!("{place} · {}", tr("no battery reading")),
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
            status += &format!(" · {}", tr("hidden"));
        }
        let name = device_name(d);
        let mut main = row![
            tinted(kind_icon(s(d, "kind")), 24, None, opacity),
            column![if online { text::heading(name).into() } else { dim(name) }, dim(status)].spacing(4),
            space::horizontal().width(Length::Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        if online && let Some(b) = battery(d) {
            let color = self.battery_color(Some(b));
            main = main.push(tinted(&battery_icon(b), 16, color, 1.0));
            main = main.push(match color {
                Some(c) => text::body(battery_text(b)).class(c.text()),
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
        let mut col = column![].spacing(12);
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
                    if Some(v) == default { format!("{v} · {}", tr("default")) } else { v.to_string() }
                },
            );
            let mut head = row![text::body(tr("Pointer speed (DPI)")), space::horizontal().width(Length::Fill), text::body(shown)]
                .spacing(8)
                .align_y(Alignment::Center);
            if let Some(def) = default.filter(|def| *def != value as i64) {
                head = head.push(button::text(tr("Default")).on_press(Message::Set(id.clone(), "dpi", json!(def))));
            }
            col = col.push(column![head, s_el].spacing(4));
        }
        if let Some(ss) = d.get("smartshift").filter(|v| v.is_object()) {
            let mode = ss.get("mode").cloned().unwrap_or(Value::Null);
            let idc = id.clone();
            col = col.push(text::body(tr("Scroll wheel mode")));
            col = col.push(choices(
                vec![
                    (json!("smartshift"), tr("SmartShift").into()),
                    (json!("ratchet"), tr("Ratchet").into()),
                    (json!("freespin"), tr("Free spin").into()),
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
                col = col.push(row![text::body(tr("Sensitivity")), s_el, text::body(shown)].spacing(8).align_y(Alignment::Center));
                col = col.push(dim(tr("Lower values switch to free spin with a gentler flick.")));
            }
        }
        if let Some(wheel) = d.get("wheel").filter(|v| v.is_object()) {
            let (a, b) = (id.clone(), id.clone());
            col = col.push(setting_row(
                tr("Natural scrolling"),
                tr("Content follows the wheel, like on a touchpad"),
                toggler(flag(wheel, "invert")).on_toggle(move |on| Message::Set(a.clone(), "invert", json!(on))).into(),
            ));
            col = col.push(setting_row(
                tr("High-resolution scrolling"),
                tr("Smoother, finer steps in apps that support it"),
                toggler(flag(wheel, "hires")).on_toggle(move |on| Message::Set(b.clone(), "hires", json!(on))).into(),
            ));
        }
        if let Some(fn_swap) = d.get("fnswap").filter(|v| v.is_object()) {
            let on = flag(fn_swap, "on");
            let a = id.clone();
            col = col.push(setting_row(
                tr("Swap Fn keys"),
                if on {
                    tr("On: top row gives media and special functions, hold Fn for F1–F12")
                } else {
                    tr("Off: top row gives F1–F12, hold Fn for media and special functions")
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
            button::suggested(if self.allowing { tr("Waiting for password …") } else { tr("Allow access") })
                .leading_icon(icon::from_name("dialog-password-symbolic").size(16))
                .on_press_maybe((!self.allowing).then_some(Message::AllowAccess))
                .into()
        } else {
            space::horizontal().width(Length::Shrink).into()
        };
        column![
            self.page_header(tr("Settings"), None),
            text::body(tr("Language")),
            column(i18n::language_choices().into_iter().map(|(code, label)| {
                let chosen = code == st.language;
                let b = if chosen { button::suggested(label) } else { button::standard(label) };
                b.on_press_maybe((!chosen).then_some(Message::Language(code))).into()
            })).spacing(4),
            divider::horizontal::default(),
            column![
                text::body(tr("Low battery warning")),
                dim(tr("At this level or less the icon turns orange")),
                choices([15, 20, 25, 30, 40, 50].into_iter().map(pct).collect(), &json!(st.low_at), |v| {
                    Message::LowAt(v.as_i64().unwrap_or(25))
                }),
            ]
            .spacing(4),
            column![
                text::body(tr("Critical battery warning")),
                dim(tr("At this level or less the icon turns red")),
                choices([5, 10, 15, 20].into_iter().map(pct).collect(), &json!(st.critical_at), |v| {
                    Message::CriticalAt(v.as_i64().unwrap_or(10))
                }),
            ]
            .spacing(4),
            setting_row(
                tr("Other Bluetooth devices"),
                tr("Trackpads, earphones, controllers and more (battery only)"),
                toggler(st.other_devices).on_toggle(Message::OtherDevices).into(),
            ),
            setting_row(tr("Receiver access"), if missing { tr("Not granted") } else { tr("Granted") }, access),
            divider::horizontal::default(),
            setting_row(tr("F12 saves a screenshot"), "", toggler(st.screenshot_key).on_toggle(Message::SetShotKey).into()),
            column![
                text::body(tr("Corners")),
                column([
                    ("desktop", trf("Like the desktop ({0} px)", &[corners::desktop_radius().round().to_string()])),
                    ("slight", trf("Slightly rounded ({0} px)", &["6".into()])),
                    ("rounded", trf("Rounded ({0} px)", &["12".into()])),
                    ("square", tr("Square").to_string()),
                ]
                .into_iter()
                .map(|(choice, label)| {
                    let chosen = st.corners == choice;
                    let b = if chosen { button::suggested(label) } else { button::standard(label) };
                    b.on_press_maybe((!chosen).then(|| Message::Corners(choice.into()))).into()
                }))
                .spacing(4),
            ]
            .spacing(4),
        ]
        .spacing(12)
        .into()
    }

    fn bluetooth_page(&self) -> Element<'_, Message> {
        let bt = &self.bt;
        let scanning = self.scanning();
        let trailing: Option<Element<Message>> = bt.has_adapter.then(|| {
            let state = if scanning {
                text::caption(tr("Scanning")).class(theme::Text::Custom(accent_text)).into()
            } else {
                dim(if bt.powered { tr("On") } else { tr("Off") })
            };
            row![state, toggler(bt.powered).on_toggle(Message::BtPower)].spacing(8).align_y(Alignment::Center).into()
        });
        let mut content = column![self.page_header(tr("Bluetooth"), trailing), divider::horizontal::default()].spacing(8);
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
            content = content.push(section(match title { "Connected" => tr("Connected"), "Paired" => tr("Paired"), _ => tr("Available") }));
            for r in rows {
                content = content.push(self.bt_row(r, title == "Available"));
            }
        }
        if !any || (scanning && available == 0) {
            content = content.push(dim(if !bt.has_adapter {
                tr("No Bluetooth adapter")
            } else if !bt.powered {
                tr("Turn Bluetooth on to scan")
            } else {
                tr("Scanning for devices…")
            }));
        }
        content.into()
    }

    fn bt_row<'a>(&'a self, r: &'a BtRow, available: bool) -> Element<'a, Message> {
        let pending = self.pending.get(&r.address).map(|p| p.0);
        let asking = self.confirm_forget == r.address;
        let mut words = column![text::heading(r.name.clone())].spacing(4);
        let status = match pending {
            Some("forgetting") => tr("Forgetting…").to_string(),
            Some("disconnecting") => tr("Disconnecting…").to_string(),
            Some(_) => tr("Connecting…").to_string(),
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
        .spacing(12)
        .align_y(Alignment::Center);
        let idle = pending.is_none();
        if !available {
            let forget = if asking {
                button::destructive(tr("Really forget?"))
                    .leading_icon(icon::from_name("user-trash-symbolic").size(16))
                    .on_press_maybe(idle.then(|| Message::Forget(r.address.clone())))
                    .into()
            } else {
                icon_button("user-trash-symbolic", idle.then(|| Message::Forget(r.address.clone())))
            };
            line = line.push(forget);
        }
        let main = if r.connected {
            button::standard(tr("Disconnect"))
                .leading_icon(icon::from_name("bluetooth-disabled-symbolic").size(16))
                .on_press_maybe(idle.then(|| Message::BtAction("disconnect", r.address.clone())))
        } else {
            let action = if available { "pair" } else { "connect" };
            button::standard(if available { tr("Pair") } else { tr("Connect") })
                .leading_icon(icon::from_name("bluetooth-active-symbolic").size(16))
                .on_press_maybe(idle.then(|| Message::BtAction(action, r.address.clone())))
        };
        line.push(main).into()
    }

    fn footer(&self) -> Element<'_, Message> {
        row![
            button::standard(tr("Refresh")).leading_icon(icon::from_name("view-refresh-symbolic").size(16)).on_press(Message::Refresh),
            button::standard(tr("Bluetooth"))
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

impl<const WINDOW_MODE: bool> cosmic::Application for Monitor<WINDOW_MODE> {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = bool;
    type Message = Message;
    // Wayland's app id: the panel applet's, or the window's own (`<id>.Window`, the id of the
    // `.Window.desktop` entry install.sh writes, so taskbars and window rules find it).
    const APP_ID: &'static str = if WINDOW_MODE { WINDOW_APP_ID } else { APP_ID };

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, window_mode: bool) -> (Self, Task<Message>) {
        let mut app = Self { core, window_mode, settings: Settings::load(), ..Default::default() };
        if window_mode { app.resize = window_size::Debounce::new(window_start_size()); }
        i18n::set_language(&app.settings.language);
        corners::set_current(&app.settings.corners);
        let task = if window_mode {
            use cosmic::ApplicationExt;
            app.set_header_title(tr("Maprisma Bluetooth Monitor").into());
            // The desktop's theme for the whole window (title bar too), see look.rs.
            Task::batch([
                app.set_window_title(tr("Maprisma Bluetooth Monitor").into(), app.core.main_window_id().unwrap()),
                corners::set_app_theme(&app.settings.corners),
            ])
        } else { Task::none() };
        (app, task)
    }

    /// COSMIC's colours or light/dark mode changed: a window with its own corners (a custom
    /// theme, which libcosmic leaves alone) takes the new colours too.
    fn system_theme_update(&mut self, _keys: &[&'static str], new_theme: &cosmic::cosmic_theme::Theme) -> Task<Message> {
        corners::system_theme_update(&self.settings.corners, new_theme, self.window_mode)
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::CloseRequested(id))
    }

    fn on_window_resize(&mut self, id: window::Id, width: f32, height: f32) {
        if self.window_mode && self.core.main_window_id() == Some(id) {
            self.resize.resized(width, height, Instant::now());
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            cosmic::iced::time::every(Duration::from_secs(1)).map(|_| Message::LanguageTick),
            Subscription::run_with("bluetooth-monitor-helper", |_| {
                stream::channel(4, |mut output| async move {
                    run_helper(&mut output).await;
                })
            }),
        ];
        if self.window_mode {
            subscriptions.push(cosmic::iced::time::every(Duration::from_millis(200)).map(|_| Message::WindowTick));
        } else {
            subscriptions.push(rectangle_tracker_subscription(0).map(|update| Message::Rectangle(update.1)));
        }
        // F12 (no modifiers) in the popup or window; Esc closes the innermost thing (esc_step).
        subscriptions.push(cosmic::iced::event::listen_with(|event, status, id| match event {
            cosmic::iced::Event::Keyboard(cosmic::iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                if shot::is_shot_key(&key, modifiers) {
                    Some(Message::ShotKey(id))
                } else if status == cosmic::iced::event::Status::Ignored
                    && matches!(key, cosmic::iced::keyboard::Key::Named(cosmic::iced::keyboard::key::Named::Escape))
                {
                    // Not one an open drop-down list took to close itself.
                    Some(Message::Escape(id))
                } else {
                    None
                }
            }
            _ => None,
        }));
        // The Bluetooth page: BlueZ's list every two seconds, and a scan while it is on
        // screen. The scan is bluetoothctl's own discovery session: BlueZ ends it the
        // moment the process exits, which happens as soon as this subscription goes
        // (page left, popup closed, applet gone): an endless scan makes audio stutter.
        if ((self.window_mode || self.popup.is_some()) && self.page == Page::Bluetooth) || !self.pending.is_empty() {
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
            Message::LanguageTick => {
                let settings = Settings::load();
                // F12 and Corners changed in another interface (window, applet) take effect here too.
                self.settings.screenshot_key = settings.screenshot_key;
                let mut restyle = self.settings.corners != settings.corners;
                if restyle {
                    corners::set_current(&settings.corners);
                    self.settings.corners = settings.corners.clone();
                }
                // The desktop's colours and corners (Maprisma Theme can change them any time).
                restyle |= look::refresh();
                let restyle = if restyle && self.window_mode { corners::set_app_theme(&self.settings.corners) } else { Task::none() };
                if self.settings.language != settings.language {
                    self.settings = settings;
                    i18n::set_language(&self.settings.language);
                    if self.window_mode {
                        use cosmic::ApplicationExt;
                        self.set_header_title(tr("Maprisma Bluetooth Monitor").into());
                        return Task::batch([restyle, self.set_window_title(tr("Maprisma Bluetooth Monitor").into(), self.core.main_window_id().unwrap())]);
                    }
                }
                return restyle;
            }
            Message::WindowTick => {
                if window_control::CLOSE_WINDOW.load(std::sync::atomic::Ordering::Relaxed) {
                    self.save_size(true);
                    return cosmic::iced::exit();
                }
                if self.resize.waiting() { self.save_size(false); }
            }
            Message::Snapshot(stats) => self.stats = stats,
            Message::HelperStarted(sender) => {
                self.helper = Some(sender);
                // The helper (re)started: tell it the options and whether the panel is open.
                self.send(json!({"cmd": "options", "other_devices": self.settings.other_devices}));
                if self.window_mode || self.popup.is_some() {
                    self.send(json!({"cmd": "open"}));
                }
            }
            Message::Rectangle(update) => match update {
                RectangleUpdate::Rectangle(r) => self.rectangle = r.1,
                RectangleUpdate::Init(tracker) => self.tracker = Some(tracker),
            },
            Message::CloseRequested(id) => {
                if self.window_mode { self.save_size(true); return cosmic::iced::exit(); }
                if self.popup == Some(id) {
                    self.popup = None;
                    self.send(json!({"cmd": "closed"}));
                    return self.popup_closed();
                }
            }
            Message::TogglePopup => {
                if self.window_mode { return Task::none(); }
                if let Some(popup) = self.popup.take() {
                    self.send(json!({"cmd": "closed"}));
                    return Task::batch([self.popup_closed(), destroy_popup(popup)]);
                }
                let focus = if matches!(self.shot, shot::Phase::Note { .. }) {
                    cosmic::widget::text_input::focus(NOTE_ID.clone())
                } else {
                    Task::none()
                };
                self.page = Page::Main;
                self.show_hidden = false;
                self.confirm_forget.clear();
                self.send(json!({"cmd": "open"}));
                return Task::batch([cosmic::surface::surface_task(cosmic::surface::action::app_popup(
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
                )), focus]);
            }
            Message::ShotKey(id) => {
                let shown = if self.window_mode { self.core.main_window_id() == Some(id) } else { self.popup == Some(id) };
                if self.settings.screenshot_key && shown && self.shot == shot::Phase::Idle {
                    self.shot = shot::Phase::Busy;
                    self.capturing = true;
                    return window::screenshot(id).map(|picture| cosmic::Action::App(Message::Captured(picture)));
                }
            }
            Message::Captured(picture) => {
                if !std::mem::take(&mut self.capturing) { return Task::none(); }
                let (width, height) = (picture.size.width, picture.size.height);
                return off_thread(move || shot::save_screenshot(SHOT_FOLDER, &picture.rgba, width, height), Message::ShotSaved);
            }
            Message::ShotSaved(result) => {
                self.shot = match result {
                    Ok(saved) => shot::Phase::Note { path: saved.path, shown: saved.shown, text: String::new() },
                    Err(reason) => shot::Phase::Error(trf("The screenshot could not be saved: {0}", &[reason])),
                };
                if matches!(self.shot, shot::Phase::Note { .. }) {
                    return cosmic::widget::text_input::focus(NOTE_ID.clone());
                }
            }
            Message::NoteInput(value) => {
                if let shot::Phase::Note { text, .. } = &mut self.shot {
                    *text = value.chars().take(shot::NOTE_LIMIT).collect();
                }
            }
            Message::NoteDone => return self.finish_note(true),
            Message::Escape(id) => {
                let shown = if self.window_mode { self.core.main_window_id() == Some(id) } else { self.popup == Some(id) };
                match esc_step(shown, self.shot.open(), !self.confirm_forget.is_empty(), self.page != Page::Main) {
                    EscStep::Note => return self.finish_note(false),
                    EscStep::Confirm => self.confirm_forget.clear(),
                    EscStep::Back => return self.set_page(Page::Main),
                    // Like the window's own close (the tray keeps running); the popup like a click on the icon.
                    EscStep::Close if self.window_mode => return self.update(Message::CloseRequested(id)),
                    EscStep::Close => return self.update(Message::TogglePopup),
                    EscStep::Nothing => {}
                }
            }
            Message::NoteSaved(result) => match result {
                Ok(()) => { if self.shot == shot::Phase::Noting { self.shot = shot::Phase::Idle; } }
                Err(reason) => self.shot = shot::Phase::Error(trf("The note could not be saved: {0}", &[reason])),
            },
            Message::SetShotKey(on) => {
                self.settings.screenshot_key = on;
                self.settings.save();
            }
            Message::Corners(choice) => {
                let choice = corners::normalize(&choice);
                self.settings.corners = choice.to_string();
                self.settings.save();
                corners::set_current(choice);
                if self.window_mode { return corners::set_app_theme(choice); }
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
            Message::Language(code) => {
                self.settings.language = code;
                self.settings.save();
                i18n::set_language(&self.settings.language);
                if self.window_mode {
                    use cosmic::ApplicationExt;
                    self.set_header_title(tr("Maprisma Bluetooth Monitor").into());
                    return self.set_window_title(tr("Maprisma Bluetooth Monitor").into(), self.core.main_window_id().unwrap());
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
        if self.window_mode { return corners::apply_window(&self.settings.corners, self.popup_content()); }
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
        if self.window_mode { return corners::apply_window(&self.settings.corners, self.popup_content()); }
        corners::apply(&self.settings.corners, self.core.applet.popup_container(self.popup_content()))
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        if self.window_mode { None } else { Some(cosmic::applet::style()) }
    }
}

impl<const WINDOW_MODE: bool> Monitor<WINDOW_MODE> {
    // Done, Enter or the popup closing (keep: the note typed so far is saved), Esc (skip).
    fn finish_note(&mut self, keep: bool) -> Task<Message> {
        match std::mem::take(&mut self.shot) {
            shot::Phase::Note { path, text, .. } => {
                if keep && shot::note_text(&text).is_some() {
                    // Busy until the note is written, so a new F12 cannot replace it.
                    self.shot = shot::Phase::Noting;
                    return off_thread(
                        move || shot::save_note(&path, SHOT_FOLDER, &text).map(|_| ()).map_err(|e| e.to_string()),
                        Message::NoteSaved,
                    );
                }
            }
            shot::Phase::Error(_) => {}
            other => self.shot = other,
        }
        Task::none()
    }

    // A capture still on its way is dropped with its popup; a typed note is kept (saved).
    fn popup_closed(&mut self) -> Task<Message> {
        if std::mem::take(&mut self.capturing) && self.shot == shot::Phase::Busy {
            self.shot = shot::Phase::Idle;
        }
        if matches!(self.shot, shot::Phase::Note { .. }) { self.finish_note(true) } else { Task::none() }
    }

    // After F12: "Screenshot saved", where, the optional note and Done (or the error and Done).
    fn shot_overlay(&self) -> Option<Element<'_, Message>> {
        let body: Element<Message> = match &self.shot {
            shot::Phase::Note { shown, text: note, .. } => column![
                text::heading(tr("Screenshot saved").to_string()),
                dim(trf("Saved as {0}", &[shown.clone()])),
                cosmic::widget::text_input(tr("What is wrong? (optional)").to_string(), note.as_str())
                    .id(NOTE_ID.clone())
                    .on_input(Message::NoteInput)
                    .on_submit(|_| Message::NoteDone),
                row![space::horizontal().width(Length::Fill), button::suggested(tr("Done").to_string()).on_press(Message::NoteDone)],
            ]
            .spacing(12)
            .into(),
            shot::Phase::Error(reason) => column![
                text::body(reason.clone()),
                row![space::horizontal().width(Length::Fill), button::suggested(tr("Done").to_string()).on_press(Message::NoteDone)],
            ]
            .spacing(12)
            .into(),
            _ => return None,
        };
        let card = container(body).padding(16).width(Length::Fill).class(theme::Container::Card);
        let card = container(card).padding(12).width(if self.window_mode { Length::Fill } else { Length::Fixed(POPUP_WIDTH) });
        Some(card.into())
    }

    fn popup_content(&self) -> Element<'_, Message> {
        if let Some(overlay) = self.shot_overlay() {
            return overlay;
        }
        let body = match self.page {
            Page::Main => self.main_page(),
            Page::Settings => self.settings_page(),
            Page::Bluetooth => self.bluetooth_page(),
        };
        let inner = column![self.header(), divider::horizontal::default(), body].spacing(12);
        let details = scrollable(container(inner).width(Length::Fill).padding([12, 16])).height(if self.window_mode { Length::Fill } else { Length::Shrink });
        let mut content = column![container(details).max_height(if self.window_mode { f32::INFINITY } else { POPUP_MAX_HEIGHT })].width(if self.window_mode { Length::Fill } else { Length::Fixed(POPUP_WIDTH) });
        if self.page == Page::Main {
            content = content.push(divider::horizontal::default());
            content = content.push(container(self.footer()).padding([8, 16]));
        }
        content.padding([0, 0, 8, 0]).height(if self.window_mode { Length::Fill } else { Length::Shrink }).into()
    }
}

// ------------------------------------------------------------------------- processes

// Next to the binary (installed: ../share/maprisma-bluetooth-monitor/scripts), or in
// the data home; BLUETOOTH_MONITOR_SCRIPTS wins (development).
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
    fn esc_closes_the_innermost_thing_first() {
        assert_eq!(esc_step(true, true, true, true), EscStep::Note);
        assert_eq!(esc_step(true, false, true, true), EscStep::Confirm);
        assert_eq!(esc_step(true, false, false, true), EscStep::Back);
        assert_eq!(esc_step(true, false, false, false), EscStep::Close);
        // Not from the window or the open popup (e.g. the panel button): nothing.
        assert_eq!(esc_step(false, true, true, true), EscStep::Nothing);
        assert_eq!(esc_step(false, false, false, false), EscStep::Nothing);
    }

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
