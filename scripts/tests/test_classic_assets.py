"""产品资源必须在 clone 后独立可用，缺失或损坏不得静默降级。"""
from __future__ import annotations

import json
import re
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import classic_assets


class ProductAssetTests(unittest.TestCase):
    def test_product_is_complete_without_any_local_inputs(self) -> None:
        manifest = classic_assets.check()
        self.assertEqual(manifest['iso']['sha256'], 'a8e8d656bd147700c8c73e2ef0a47de7832b6c430d442f539f731db180ea8cb0')
        for group in ('boot', 'cursors', 'ui', 'fonts', 'sounds'):
            paths = classic_assets._files(group)
            self.assertTrue(paths)
            self.assertTrue(all(path.is_relative_to(classic_assets.DIRECTORY) for path in paths))
        self.assertEqual({path.name for path in classic_assets.fonts()},
                         {'simsun.ttc', 'tahoma.ttf', 'tahomabd.ttf', 'marlett.ttf', 'fontmap.ini'})
        self.assertIn('network-places-32.bmp', {path.name for path in classic_assets.shell_bitmaps()})
        self.assertIn('recycle.wav', dict(classic_assets.sounds()))

    def test_ui_assets_have_consumers_and_fixed_requests_are_installed(self) -> None:
        root = classic_assets.ROOT
        sources = [path.read_text() for path in (root / 'user').rglob('*.rs')
                   if path.name != 'tests.rs' and 'target' not in path.parts and 'examples' not in path.parts]
        entries = [path.read_text() for path in (root / 'user/base').glob('*.desktop')]
        ui = {Path(name).stem for name, info in classic_assets.check()['files'].items() if info['group'] == 'ui'}
        fixed = set()
        for text in sources:
            fixed.update(re.findall(r'"(?:/usr/share/liteos/ui/)?([a-z][a-z0-9-]*-(?:16|32|64))(?:\.bmp)?"', text))
        self.assertFalse(fixed - ui, f'missing fixed icon requests: {sorted(fixed - ui)}')
        for name in ui:
            stem = re.sub(r'-(?:16|32|64|1x)$', '', name)
            # 工具栏的三种状态由 strip 名在运行时组合，仍属于同一个资源消费者。
            stem = re.sub(r'-(?:default|hot|disabled)$', '', stem)
            pattern = r'"(?:/usr/share/liteos/ui/)?' + re.escape(stem) + r'(?:-(?:16|32|64|1x))?(?:\.bmp)?"'
            used = any(re.search(pattern, text) for text in sources) or any('Icon=' + stem + '\n' in text for text in entries)
            self.assertTrue(used, f'unreferenced UI resource: {name}')

    def test_missing_corrupt_and_unregistered_assets_fail(self) -> None:
        with tempfile.TemporaryDirectory() as work:
            root = Path(work)
            info = classic_assets.check()['files']['ui/my-documents-32.bmp']
            (root / 'manifest.json').write_text(json.dumps({'files': {'ui/my-documents-32.bmp': info}}))
            with mock.patch.object(classic_assets, 'DIRECTORY', root):
                with self.assertRaises(FileNotFoundError):
                    classic_assets.shell_bitmaps()
                (root / 'ui').mkdir()
                path = root / 'ui/my-documents-32.bmp'
                path.write_bytes(b'X' * info['bytes'])
                with self.assertRaisesRegex(ValueError, 'integrity mismatch'):
                    classic_assets.shell_bitmaps()
                path.write_bytes((classic_assets.ROOT / 'assets/classic/ui/my-documents-32.bmp').read_bytes())
                (root / 'theme').mkdir()
                (root / 'theme/classic.theme').write_text('')
                (root / 'old-default.png').write_bytes(b'unused')
                with self.assertRaisesRegex(ValueError, 'unregistered'):
                    classic_assets.check()


if __name__ == '__main__':
    unittest.main()
