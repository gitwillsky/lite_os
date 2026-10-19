from __future__ import annotations

import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))

import kernel_fixture_view as view  # noqa: E402


class KernelFixtureViewTests(unittest.TestCase):
    def build_tree(self, root: Path) -> None:
        (root / "kernel/src/ipc/pipe").mkdir(parents=True)
        (root / "kernel/src/ipc/pipe.rs").write_text("mod pipe_open;\n")
        (root / "kernel/src/ipc/pipe/pipe_open.rs").write_text("")
        (root / "kernel/src/ipc/eventfd.rs").write_text("")
        (root / "tools/kernel-unit/src").mkdir(parents=True)

    def patched(self, root: Path):
        return patch.multiple(
            view,
            ROOT=root,
            KERNEL=root / "kernel/src",
            VIEW=root / "tools/kernel-view",
            FIXTURE_ROOTS=(root / "tools/kernel-unit",),
        )

    def test_children_bearing_file_must_go_through_the_view(self) -> None:
        with tempfile.TemporaryDirectory() as directory, self.patched(Path(directory)):
            root = Path(directory)
            self.build_tree(root)
            fixture = root / "tools/kernel-unit/src/pipe_fixture.rs"
            fixture.write_text('#[path = "../../../kernel/src/ipc/pipe.rs"]\nmod pipe;\n')
            self.assertTrue(any("ipc/pipe.rs" in error for error in view.check()))

    def test_leaf_files_may_be_included_directly(self) -> None:
        with tempfile.TemporaryDirectory() as directory, self.patched(Path(directory)):
            root = Path(directory)
            self.build_tree(root)
            fixture = root / "tools/kernel-unit/src/pipe_fixture.rs"
            fixture.write_text('#[path = "../../../kernel/src/ipc/eventfd.rs"]\nmod eventfd;\n')
            self.assertEqual(view.check(), [])

    def test_write_flattens_parent_and_children_and_rewrites_references(self) -> None:
        with tempfile.TemporaryDirectory() as directory, self.patched(Path(directory)):
            root = Path(directory)
            self.build_tree(root)
            fixture = root / "tools/kernel-unit/src/pipe_fixture.rs"
            fixture.write_text('#[path = "../../../kernel/src/ipc/pipe.rs"]\nmod pipe;\n')
            view.write()
            self.assertIn("tools/kernel-view/ipc/pipe/pipe/pipe.rs", fixture.read_text())
            linked = root / "tools/kernel-view/ipc/pipe/pipe"
            self.assertEqual(
                sorted(path.name for path in linked.iterdir()), ["pipe.rs", "pipe_open.rs"]
            )
            self.assertEqual((linked / "pipe_open.rs").read_text(), "")
            self.assertEqual(view.check(), [])

    def test_check_detects_a_stale_view(self) -> None:
        with tempfile.TemporaryDirectory() as directory, self.patched(Path(directory)):
            root = Path(directory)
            self.build_tree(root)
            fixture = root / "tools/kernel-unit/src/pipe_fixture.rs"
            fixture.write_text('#[path = "../../../kernel/src/ipc/pipe.rs"]\nmod pipe;\n')
            view.write()
            (root / "kernel/src/ipc/pipe/extra.rs").write_text("")
            self.assertTrue(any("不一致" in error for error in view.check()))

    def test_check_detects_an_orphan_view(self) -> None:
        with tempfile.TemporaryDirectory() as directory, self.patched(Path(directory)):
            root = Path(directory)
            self.build_tree(root)
            fixture = root / "tools/kernel-unit/src/pipe_fixture.rs"
            fixture.write_text('#[path = "../../../kernel/src/ipc/pipe.rs"]\nmod pipe;\n')
            view.write()
            fixture.write_text("")
            self.assertTrue(any("没有夹具引用" in error for error in view.check()))


if __name__ == "__main__":
    unittest.main()
