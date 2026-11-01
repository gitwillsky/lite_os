#!/usr/bin/env python3
"""为包含生产模块的宿主夹具维护符号链接视图，不复制生产源码。

Rust 的 #[path] 把 foo.rs 当作 mod.rs 解析，foo/ 下的子模块因此需要平铺到同一目录。
视图按实际 crate owner 分目录；kernel 与各服务使用同一生成器，迁移不保留旧路径别名。
"""
from __future__ import annotations

import argparse
import os
from pathlib import Path
import re
import shutil
import sys
import tomllib

ROOT = Path(__file__).resolve().parent.parent
VIEW = ROOT / "tools/source-view"
MEMBERS = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["members"]
SOURCES = {Path(member).name: f"{member}/src" for member in MEMBERS if (ROOT / member / "src").is_dir()}
SOURCE_PATTERN = "|".join(re.escape(value) for value in sorted(SOURCES.values(), key=len, reverse=True))
DIRECT = re.compile(rf"({SOURCE_PATTERN})/([\w/]+)\.rs")
VIA_VIEW = re.compile(r"tools/source-view/([\w-]+)/((?:\w+/)*?(\w+))/\3/\3\.rs")
FIXTURES = tuple(ROOT / "tools" / name for name in ("kernel-unit", "scheduler-unit", "architecture-bench", "architecture-check"))


def fixture_sources() -> list[Path]:
    """返回需要编译生产模块的工具源码；生成目录不是扫描输入。"""
    return sorted(path for base in FIXTURES for path in base.rglob("*.rs") if "target" not in path.parts)


def source_path(owner: str, relative: str) -> Path:
    """解析实际 crate owner 内去掉 .rs 的模块路径。"""
    return ROOT / SOURCES[owner] / f"{relative}.rs"


def has_children(owner: str, relative: str) -> bool:
    """只有同名源码文件及子模块目录同时存在时才需要视图。"""
    path = source_path(owner, relative)
    return path.is_file() and path.with_suffix("").is_dir()


def direct_references() -> dict[tuple[str, str], list[Path]]:
    """收集仍需改为视图的直接源码引用。"""
    owners = {source: owner for owner, source in SOURCES.items()}
    found: dict[tuple[str, str], list[Path]] = {}
    for path in fixture_sources():
        for match in DIRECT.finditer(path.read_text()):
            key = (owners[match[1]], match[2])
            if has_children(*key):
                found.setdefault(key, []).append(path)
    return found


def referenced_views() -> set[tuple[str, str]]:
    """收集夹具实际使用的 owner/module 视图。"""
    return {(match[1], match[2]) for path in fixture_sources() for match in VIA_VIEW.finditer(path.read_text())}


def directory(owner: str, relative: str) -> Path:
    """视图叶目录同时包含 foo.rs 和 foo/ 的全部子项。"""
    return VIEW / owner / relative / Path(relative).name


def expected_links(owner: str, relative: str) -> dict[str, str]:
    """返回当前生产源码决定的精确链接清单。"""
    source = source_path(owner, relative)
    destination = directory(owner, relative)
    paths = [source, *sorted(source.with_suffix("").iterdir())]
    return {path.name: os.path.relpath(path, destination) for path in paths}


def remove_view(view: Path) -> None:
    """只删除生成的链接树；发现普通文件就停止，防止覆盖手写源码。"""
    if not view.exists():
        return
    if any(path.is_file() and not path.is_symlink() for path in view.rglob("*")):
        raise RuntimeError(f"{view}: source view contains a non-generated file")
    shutil.rmtree(view)


def write() -> None:
    """从实际引用重建视图，并把直接引用改成按 owner 隔离的链接入口。"""
    wanted = referenced_views() | set(direct_references())
    remove_view(VIEW)
    # 退休单体内核时期的生成物；它们没有生产调用者，也不作为新布局的兼容入口。
    remove_view(ROOT / "tools/kernel-view")
    for owner, relative in sorted(wanted):
        if not has_children(owner, relative):
            raise RuntimeError(f"{owner}/{relative}: referenced production module no longer exists")
        target = directory(owner, relative)
        target.mkdir(parents=True)
        for name, link in expected_links(owner, relative).items():
            (target / name).symlink_to(link)
    owners = {source: owner for owner, source in SOURCES.items()}
    for path in fixture_sources():
        original = path.read_text()
        def replace(match: re.Match[str]) -> str:
            owner, relative = owners[match[1]], match[2]
            if (owner, relative) not in wanted:
                return match[0]
            stem = Path(relative).name
            return f"tools/source-view/{owner}/{relative}/{stem}/{stem}.rs"
        updated = DIRECT.sub(replace, original)
        if updated != original:
            path.write_text(updated)
    print(f"fixture source view: {len(wanted)} production modules linked")


def check() -> list[str]:
    """核对直接引用、缺失/漂移链接和没有夹具使用的孤儿视图。"""
    errors = [f"{owner}/{relative}: children require source-view (--write)" for owner, relative in direct_references()]
    wanted = referenced_views()
    expected: dict[str, str] = {}
    for owner, relative in sorted(wanted):
        if owner not in SOURCES or not has_children(owner, relative):
            errors.append(f"{owner}/{relative}: missing production module")
            continue
        target = directory(owner, relative)
        expected.update({str((target / name).relative_to(VIEW)): link for name, link in expected_links(owner, relative).items()})
    actual = {str(path.relative_to(VIEW)): os.readlink(path) for path in VIEW.rglob("*") if path.is_symlink()} if VIEW.exists() else {}
    if expected != actual:
        errors.append("source view contains missing, stale or orphan links (--write)")
    if (ROOT / "tools/kernel-view").exists():
        errors.append("retired kernel-view must be removed (--write)")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    actions = parser.add_mutually_exclusive_group()
    actions.add_argument("--write", action="store_true", help="重建视图并更新夹具引用")
    actions.add_argument("--check", action="store_true", help="检查视图（默认）")
    args = parser.parse_args()
    if args.write:
        write()
        return 0
    errors = check()
    for error in errors:
        print(f"fixture source view: {error}", file=sys.stderr)
    if not errors:
        print("fixture source view ok")
    return bool(errors)


if __name__ == "__main__":
    raise SystemExit(main())
