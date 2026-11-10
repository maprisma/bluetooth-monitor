#[path = "../i18n.rs"]
mod i18n;
use i18n::tr;
mod model;
#[path = "../shared.rs"]
mod shared;
use model::Action;
use shared::*;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Mutex, mpsc};
use zbus::{
    Connection,
    object_server::SignalEmitter,
    zvariant::{OwnedObjectPath, OwnedValue, Value as Variant},
};

pub const ICON: &str = "io.github.ubruckhaus.CosmicMaprismaBluetoothMonitor-symbolic";

type Props = HashMap<String, OwnedValue>;
type Layout = (i32, Props, Vec<OwnedValue>);
type Pixmaps = Vec<(i32, i32, Vec<u8>)>;
struct State {
    tooltip: String,
    menu: model::Item,
    revision: u32,
}
impl State {
    fn status(&self) -> &'static str {
        "Active"
    }
    fn tooltip(&self) -> String {
        self.tooltip.clone()
    }
}
type Shared = Arc<Mutex<State>>;
#[derive(Clone)]
struct Menu {
    state: Shared,
    tx: mpsc::UnboundedSender<Action>,
}
fn owned<T: Into<Variant<'static>>>(value: T) -> OwnedValue {
    OwnedValue::try_from(value.into()).unwrap()
}
fn properties(item: &model::Item, names: &[String]) -> Props {
    let mut props = Props::new();
    props.insert("label".into(), owned(item.label.clone()));
    props.insert("enabled".into(), owned(item.enabled));
    props.insert("visible".into(), owned(true));
    if !item.children.is_empty() {
        props.insert("children-display".into(), owned("submenu"));
    }
    if item.separator {
        props.insert("type".into(), owned("separator"));
    }
    if !names.is_empty() {
        props.retain(|key, _| names.contains(key));
    }
    props
}
fn layout(item: &model::Item, depth: i32, names: &[String]) -> Layout {
    (
        item.id,
        properties(item, names),
        if depth == 0 {
            vec![]
        } else {
            item.children
                .iter()
                .map(|c| {
                    OwnedValue::try_from(Variant::new(layout(
                        c,
                        if depth < 0 { -1 } else { depth - 1 },
                        names,
                    )))
                    .unwrap()
                })
                .collect()
        },
    )
}
impl Menu {
    async fn dispatch(&self, id: i32, event: &str) -> bool {
        let s = self.state.lock().await;
        let menu = s.menu.clone();
        let Some(item) = menu.find(id) else {
            return false;
        };
        if event == "clicked" {
            if let Some(action) = menu.activate(item.id) {
                let _ = self.tx.send(action);
            }
        }
        true
    }
}
#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }
    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }
    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }
    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        vec![]
    }
    async fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, Layout)> {
        let s = self.state.lock().await;
        let menu = s.menu.clone();
        let item = menu
            .find(parent_id)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("Unknown menu ID".into()))?;
        Ok((s.revision, layout(item, recursion_depth, &property_names)))
    }
    async fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> Vec<(i32, Props)> {
        let s = self.state.lock().await;
        let menu = s.menu.clone();
        ids.iter()
            .filter_map(|id| {
                menu.find(*id)
                    .map(|i| (*id, properties(i, &property_names)))
            })
            .collect()
    }
    async fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        let s = self.state.lock().await;
        let menu = s.menu.clone();
        menu.find(id)
            .and_then(|i| properties(i, &[]).remove(&name))
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("Unknown menu property".into()))
    }
    async fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        self.dispatch(id, &event_id).await;
    }
    async fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        let mut errors = vec![];
        for (id, event, _, _) in events {
            if !self.dispatch(id, &event).await {
                errors.push(id);
            }
        }
        errors
    }
    async fn about_to_show(&self, _id: i32) -> bool {
        false
    }
    async fn about_to_show_group(&self, ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        let s = self.state.lock().await;
        let menu = s.menu.clone();
        (
            vec![],
            ids.into_iter()
                .filter(|id| menu.find(*id).is_none())
                .collect(),
        )
    }
    #[zbus(signal)]
    async fn layout_updated(
        emitter: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;
}
struct Tray {
    state: Shared,
    tx: mpsc::UnboundedSender<Action>,
}
#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Tray {
    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }
    #[zbus(property)]
    fn id(&self) -> &str {
        "maprisma-bluetooth-monitor"
    }
    #[zbus(property)]
    fn title(&self) -> &str {
        tr("Maprisma Bluetooth Monitor")
    }
    #[zbus(property)]
    async fn status(&self) -> &str {
        self.state.lock().await.status()
    }
    #[zbus(property)]
    fn window_id(&self) -> u32 {
        0
    }
    #[zbus(property)]
    fn icon_name(&self) -> &str {
        ICON
    }
    #[zbus(property)]
    fn icon_pixmap(&self) -> Pixmaps {
        vec![]
    }
    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn overlay_icon_pixmap(&self) -> Pixmaps {
        vec![]
    }
    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        ICON
    }
    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> Pixmaps {
        vec![]
    }
    #[zbus(property)]
    fn attention_movie_name(&self) -> &str {
        ""
    }
    #[zbus(property)]
    async fn tool_tip(&self) -> (String, Pixmaps, String, String) {
        (
            ICON.into(),
            vec![],
            tr("Maprisma Bluetooth Monitor").into(),
            self.state.lock().await.tooltip(),
        )
    }
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from("/MenuBar").unwrap()
    }
    fn activate(&self, _x: i32, _y: i32) {
        let _ = self.tx.send(Action::Activate);
    }
    fn secondary_activate(&self, _x: i32, _y: i32) {
        let _ = self.tx.send(Action::Open);
    }
    fn context_menu(&self, _x: i32, _y: i32) {} // The host displays the exported Menu.
    fn scroll(&self, _delta: i32, _orientation: String) {}
    #[zbus(signal)]
    async fn new_status(emitter: &SignalEmitter<'_>, status: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}
async fn open_window(connection: &Connection, toggle: bool) {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(connection).await else { return };
    // The window's own connection (its unique name), so a close never reaches
    // a window that took the well-known name meanwhile.
    if let Ok(owner) = dbus.get_name_owner(WINDOW_BUS.try_into().unwrap()).await {
        if toggle {
            if let Ok(proxy) = zbus::Proxy::new(
                connection,
                owner.as_str(),
                "/Window",
                "io.github.ubruckhaus.BluetoothMonitor.Window",
            )
            .await
            {
                let _ = proxy.call::<_, _, ()>("Close", &()).await;
            }
        }
        return;
    }
    let exe = std::env::current_exe()
        .unwrap()
        .with_file_name("maprisma-bluetooth-monitor");
    if let Ok(mut child) = std::process::Command::new(exe).arg("--window").spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
async fn snapshots(tx: mpsc::UnboundedSender<serde_json::Value>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    loop {
        if let Some(script) = script_path("bluetooth-monitor.py") {
            if let Ok(mut child) = tokio::process::Command::new("python3")
                .arg("-I")
                .arg(script)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
            {
                let mut input = child.stdin.take().unwrap();
                let settings = model::settings();
                let _ = input.write_all(format!("{}\n", serde_json::json!({"cmd":"options", "other_devices":settings.get("other_devices").and_then(serde_json::Value::as_bool).unwrap_or(true)})).as_bytes()).await;
                let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(snapshot) = serde_json::from_str(&line) {
                        if tx
                            .send(snapshot)
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    i18n::set_language(model::settings()["language"].as_str().unwrap_or("auto"));
    let state = Arc::new(Mutex::new(State {
        tooltip: tr("Looking for devices …").into(),
        menu: model::menu(),
        revision: 1,
    }));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let connection = zbus::connection::Builder::session()?
        .serve_at(
            "/StatusNotifierItem",
            Tray {
                state: state.clone(),
                tx: tx.clone(),
            },
        )?
        .serve_at(
            "/MenuBar",
            Menu {
                state: state.clone(),
                tx,
            },
        )?
        .build()
        .await?;
    if connection
        .request_name_with_flags(
            "io.github.ubruckhaus.CosmicMaprismaBluetoothMonitor.Tray",
            zbus::fdo::RequestNameFlags::DoNotQueue.into(),
        )
        .await
        .is_err()
    {
        return Ok(());
    }
    // Checking the unique owner also handles Plasma starting after us and watcher restarts.
    let watcher_connection = connection.clone();
    tokio::spawn(async move {
        let mut registered = String::new();
        loop {
            let dbus = zbus::fdo::DBusProxy::new(&watcher_connection).await;
            let owner = match dbus {
                Ok(p) => p
                    .get_name_owner("org.kde.StatusNotifierWatcher".try_into().unwrap())
                    .await
                    .ok()
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                Err(_) => String::new(),
            };
            if owner.is_empty() {
                registered.clear();
            } else if owner != registered {
                if let Ok(proxy) = zbus::Proxy::new(
                    &watcher_connection,
                    owner.as_str(),
                    "/StatusNotifierWatcher",
                    "org.kde.StatusNotifierWatcher",
                )
                .await
                {
                    if proxy
                        .call::<_, _, ()>(
                            "RegisterStatusNotifierItem",
                            &(watcher_connection.unique_name().unwrap().as_str(),),
                        )
                        .await
                        .is_ok()
                    {
                        registered = owner;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
    let (status_tx, mut status_rx) = mpsc::unbounded_channel();
    let helper = tokio::spawn(snapshots(status_tx));
    let mut latest = serde_json::Value::Null;
    let mut language_tick = tokio::time::interval(Duration::from_secs(1));
    let mut language = model::settings()["language"].as_str().unwrap_or("auto").to_string();
    loop {
        tokio::select! {
            _ = language_tick.tick() => {
                let current = model::settings()["language"].as_str().unwrap_or("auto").to_string();
                if current != language {
                    language = current; i18n::set_language(&language);
                    let mut s = state.lock().await; s.menu = model::menu(); s.tooltip = model::tooltip(&latest, &model::settings()); s.revision += 1;
                    let revision = s.revision; drop(s);
                    Menu::layout_updated(&SignalEmitter::new(&connection, "/MenuBar")?, revision, 0).await?;
                    Tray::new_tool_tip(&SignalEmitter::new(&connection, "/StatusNotifierItem")?).await?;
                }
            }
            Some(snapshot) = status_rx.recv() => {
                latest = snapshot;
                let text = model::tooltip(&latest, &model::settings());
                let mut s = state.lock().await;
                if s.tooltip != text {
                    s.tooltip = text;
                    drop(s);
                    Tray::new_tool_tip(&SignalEmitter::new(&connection, "/StatusNotifierItem")?).await?;
                }
            }
            Some(action) = rx.recv() => match action {
                Action::Language(index) => {
                    let code = i18n::language_choices()[index].0.clone();
                    model::save_language(&code);
                    i18n::set_language(&code);
                    let mut s = state.lock().await;
                    s.menu = model::menu(); s.tooltip = model::tooltip(&latest, &model::settings()); s.revision += 1;
                    let revision = s.revision; drop(s);
                    Menu::layout_updated(&SignalEmitter::new(&connection, "/MenuBar")?, revision, 0).await?;
                    Tray::new_tool_tip(&SignalEmitter::new(&connection, "/StatusNotifierItem")?).await?;
                }
                Action::Quit => { helper.abort(); let _ = helper.await; return Ok(()); }
                Action::Open => open_window(&connection, false).await,
                Action::Activate => open_window(&connection, true).await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn menu_wire_layout() {
        let menu = model::menu();
        assert_eq!(Variant::new(layout(&menu, -1, &[])).value_signature().to_string(), "(ia{sv}av)");
        assert!(layout(&menu, 0, &[]).2.is_empty());
        assert_eq!(layout(&menu, 1, &[]).2.len(), 3);
        assert_eq!(properties(menu.find(1).unwrap(), &["label".into()]).len(), 1);
    }
    #[tokio::test]
    async fn menu_dispatch() {
        let state = Arc::new(Mutex::new(State { tooltip: String::new(), menu: model::menu(), revision: 1 }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let menu = Menu { state, tx };
        assert!(!menu.dispatch(99, "clicked").await);
        assert!(menu.dispatch(1, "hovered").await);
        assert!(rx.try_recv().is_err());
        menu.dispatch(1, "clicked").await;
        assert_eq!(rx.try_recv().unwrap(), Action::Open);
        menu.dispatch(2, "clicked").await;
        assert_eq!(rx.try_recv().unwrap(), Action::Quit);
        menu.dispatch(10, "clicked").await;
        assert_eq!(rx.try_recv().unwrap(), Action::Language(0));
    }
}
