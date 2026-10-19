#!/usr/bin/env python3
"""为 host 夹具生成“mod-rs 视图”：`tools/kernel-view/` 下只有符号链接，不含任何源码副本。

内核使用标准布局：`X.rs` 声明的子模块放在 `X/` 下。但 host 夹具用 `#[path]` 或 `include!` 把内核文件
包含进测试 crate 时，rustc 把被包含文件当作 mod-rs 文件，到它**所在目录**去找子模块，于是 `X/` 下的子模块
找不到。视图为每个“被夹具包含、且带子模块”的内核文件 `X.rs` 建一个目录 `kernel-view/<路径>/X/`，里面是
`X.rs` 与 `X/` 内全部条目的符号链接，让它们平铺在一起，夹具改为包含视图里的 `X.rs` 即可。

内核源码布局不为测试让步；视图是夹具侧的适配，由本脚本生成并在 `make verify-unit` 校验。

用法：`kernel_fixture_view.py --write` 重新生成；`--check` 校验视图与夹具引用一致（默认）。
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
KERNEL = ROOT / "kernel" / "src"
VIEW = ROOT / "tools" / "kernel-view"
# 会把内核文件编译进测试 crate 的夹具 crate；architecture-check 只读取源码文本，不在此列。
FIXTURE_ROOTS = (
    ROOT / "tools" / "kernel-unit",
    ROOT / "tools" / "architecture-bench",
    ROOT / "tools" / "scheduler-unit",
)
DIRECT = re.compile(r"kernel/src/([\w/]+)\.rs")
# `<路径>/X/X/X.rs` 里最后三段相同时，`<路径>/X` 就是内核文件 `kernel/src/<路径>/X.rs` 去掉扩展名。
VIA_VIEW = re.compile(r"tools/kernel-view/((?:[\w]+/)*?(\w+))/\2/\2\.rs")


def has_children(relative: str) -> bool:
    """`relative` 是 `kernel/src` 下去掉 `.rs` 的路径；有同名目录才是带子模块的文件。"""
    return (KERNEL / relative).is_dir() and (KERNEL / f"{relative}.rs").is_file()


def fixture_sources() -> list[Path]:
    paths = []
    for base in FIXTURE_ROOTS:
        for path in base.rglob("*.rs"):
            if "target" in path.relative_to(ROOT).parts or VIEW in path.parents:
                continue
            paths.append(path)
    return sorted(paths)


def direct_children_bearing() -> dict[str, list[Path]]:
    """夹具里仍然直接引用的“带子模块的内核文件”，它们必须改为经视图包含。"""
    found: dict[str, list[Path]] = {}
    for path in fixture_sources():
        for match in DIRECT.finditer(path.read_text(encoding="utf-8")):
            relative = match.group(1)
            if has_children(relative):
                found.setdefault(relative, []).append(path)
    return found


def referenced_views() -> set[str]:
    """夹具经视图引用的内核文件（`kernel/src` 下去掉 `.rs` 的路径）。"""
    views: set[str] = set()
    for path in fixture_sources():
        for match in VIA_VIEW.finditer(path.read_text(encoding="utf-8")):
            views.add(match.group(1))
    return views


def expected_links(relative: str) -> dict[str, str]:
    """视图目录里应有的符号链接：名字 -> 相对目标。"""
    stem = relative.rsplit("/", 1)[-1]
    directory = VIEW / relative / stem
    links = {f"{stem}.rs": os.path.relpath(KERNEL / f"{relative}.rs", directory)}
    for entry in sorted((KERNEL / relative).iterdir()):
        links[entry.name] = os.path.relpath(entry, directory)
    return links


def write() -> None:
    wanted = referenced_views() | set(direct_children_bearing())
    if VIEW.exists():
        shutil.rmtree(VIEW)
    for relative in sorted(wanted):
        stem = relative.rsplit("/", 1)[-1]
        directory = VIEW / relative / stem
        directory.mkdir(parents=True)
        for name, target in expected_links(relative).items():
            (directory / name).symlink_to(target)
    rewrite_direct_references(set(wanted))
    print(f"kernel fixture view: {len(wanted)} files linked")


def rewrite_direct_references(wanted: set[str]) -> None:
    """把夹具里对带子模块内核文件的直接引用改成视图引用。"""
    for path in fixture_sources():
        text = path.read_text(encoding="utf-8")

        def view(match: re.Match[str]) -> str:
            relative = match.group(1)
            if relative not in wanted:
                return match.group(0)
            stem = relative.rsplit("/", 1)[-1]
            return f"tools/kernel-view/{relative}/{stem}/{stem}.rs"

        updated = DIRECT.sub(view, text)
        if updated != text:
            path.write_text(updated, encoding="utf-8")


def check() -> list[str]:
    errors: list[str] = []
    for relative, users in sorted(direct_children_bearing().items()):
        names = ", ".join(sorted({str(p.relative_to(ROOT)) for p in users}))
        errors.append(
            f"{relative}.rs 带有子模块，夹具必须经 tools/kernel-view 包含它（--write 重新生成）：{names}"
        )
    for relative in sorted(referenced_views()):
        if not has_children(relative):
            errors.append(f"视图引用的 {relative}.rs 不再带有子模块或已不存在")
            continue
        stem = relative.rsplit("/", 1)[-1]
        directory = VIEW / relative / stem
        expected = expected_links(relative)
        actual = (
            {p.name: os.readlink(p) for p in directory.iterdir() if p.is_symlink()}
            if directory.is_dir()
            else {}
        )
        if actual != expected:
            errors.append(f"tools/kernel-view/{relative}/{stem} 与内核布局不一致（--write 重新生成）")
    if VIEW.is_dir():
        # 孤儿视图：目录存在但没有夹具引用。视图叶子目录名等于其父目录名（`<路径>/X/X.rs`）。
        referenced = referenced_views()
        existing = {
            "/".join(p.relative_to(VIEW).parts[:-2])
            for p in VIEW.rglob("*.rs")
            if p.is_symlink() and p.name == f"{p.parent.name}.rs"
        }
        for orphan in sorted(existing - referenced):
            errors.append(f"tools/kernel-view/{orphan} 没有夹具引用（--write 重新生成）")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--write", action="store_true", help="重新生成视图并改写夹具引用")
    group.add_argument("--check", action="store_true", help="校验（默认）")
    args = parser.parse_args()
    if args.write:
        write()
        return 0
    errors = check()
    for error in errors:
        print(f"kernel fixture view: {error}", file=sys.stderr)
    if not errors:
        print("kernel fixture view ok")
    return 1 if errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
