import pathlib
import unittest


class TrayIconTests(unittest.TestCase):
    def test_off_icon_is_installed_under_the_trays_name(self):
        root = pathlib.Path(__file__).resolve().parents[1]
        installer = (root / "install.sh").read_text()
        tray = (root / "src" / "tray" / "main.rs").read_text()
        self.assertIn('pub const ICON_OFF: &str = "io.github.ubruckhaus.CosmicMaprismaBluetoothMonitor-off-symbolic"', tray)
        self.assertIn('data/icons/bluetooth-monitor-off-symbolic.svg "$data/icons/hicolor/scalable/apps/$id-off-symbolic.svg"',
                      installer)
        self.assertTrue((root / "data" / "icons" / "bluetooth-monitor-off-symbolic.svg").is_file())


if __name__ == "__main__":
    unittest.main()
