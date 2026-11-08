pub static CLOSE_WINDOW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub struct WindowControl;
#[zbus::interface(name = "io.github.ubruckhaus.BluetoothMonitor.Window")]
impl WindowControl {
    fn close(&self) {
        CLOSE_WINDOW.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
