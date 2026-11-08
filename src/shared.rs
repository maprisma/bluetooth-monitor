use std::path::PathBuf;
pub const APP_NAME: &str = "Maprisma Bluetooth Monitor";
pub const WINDOW_BUS: &str = "io.github.ubruckhaus.CosmicMaprismaBluetoothMonitor.Window";
pub fn script_path(name: &str) -> Option<PathBuf> {
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
