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

    def test_active_icon_is_the_accent_coloured_normal_icon(self):
        root = pathlib.Path(__file__).resolve().parents[1]
        installer = (root / "install.sh").read_text()
        tray = (root / "src" / "tray" / "main.rs").read_text()
        self.assertIn('pub const ICON_ACTIVE: &str = "io.github.ubruckhaus.CosmicMaprismaBluetoothMonitor-active-symbolic"', tray)
        self.assertIn('data/icons/bluetooth-monitor-active-symbolic.svg "$data/icons/hicolor/scalable/apps/$id-active-symbolic.svg"',
                      installer)
        active = (root / "data" / "icons" / "bluetooth-monitor-active-symbolic.svg").read_text()
        self.assertIn(".ColorScheme-Highlight { color:#3daee9; }", active)
        self.assertNotIn("ColorScheme-Text", active)
        normal = (root / "data" / "icons" / "io.github.ubruckhaus.CosmicMaprismaBluetoothMonitor-symbolic.svg").read_text()
        shape = lambda svg: [l for l in svg.splitlines() if " d=" in l or "<rect" in l]
        self.assertEqual([l.replace("Highlight", "Text") for l in shape(active)], shape(normal))


if __name__ == "__main__":
    unittest.main()
