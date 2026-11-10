"""Run with python3 -m unittest discover -s tests."""
import json
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[1]
LANGUAGES = ('de', 'fr', 'es', 'it', 'pt', 'nl', 'pl')


class Translations(unittest.TestCase):
    def test_literal_keys_and_placeholders(self):
        keys = set()
        for path in (ROOT / 'src').rglob('*.rs'):
            source = path.read_text().split('#[cfg(test)]\nmod tests', 1)[0]
            keys.update(json.loads('"' + key + '"') for key in re.findall(r'\btrf?\(\s*"((?:[^"\\]|\\.)*)"', source))
        self.assertGreater(len(keys), 60)
        for language in LANGUAGES:
            base = json.loads((ROOT / f'translations/{language}.json').read_text())
            extra = json.loads((ROOT / f'translations/extra/{language}.json').read_text())
            table = base | extra
            self.assertFalse(keys - table.keys(), (language, keys - table.keys()))
            for key in keys:
                self.assertTrue(table[key].strip(), (language, key))
                self.assertEqual(set(re.findall(r'\{\d+\}', key)), set(re.findall(r'\{\d+\}', table[key])), (language, key))
        extras = [set(json.loads((ROOT / f'translations/extra/{c}.json').read_text())) for c in LANGUAGES]
        self.assertTrue(all(keys == extras[0] for keys in extras))

    def test_desktop_translations(self):
        desktop = next((ROOT / 'data').glob('*.desktop')).read_text()
        for language in LANGUAGES:
            for field in ('Name', 'Comment'):
                self.assertRegex(desktop, rf'(?m)^{field}\[{language}\]=.+$')
