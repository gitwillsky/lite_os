"""Rust SDK 安装文件：源码 crate 与 Cargo 示例，不创建并行项目入口或包管理器。"""
from __future__ import annotations
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def sdk_files() -> tuple[tuple[Path, str, int], ...]:
    """返回 SDK 的源文件及安装路径；独立 manifest 不继承系统 workspace/toolchain。"""
    generated = ROOT / "target" / "ui-sdk"
    generated.mkdir(parents=True, exist_ok=True)
    files: list[tuple[Path, str, int]] = []
    for name, version in (("classic-ui-abi", "1.0.0"), ("liteos-ui", "1.0.0")):
        manifest = generated / f"{name}.toml"
        dependencies = '\n[dependencies]\nclassic-ui-abi = { path = "../classic-ui-abi" }\n' if name == "liteos-ui" else ""
        manifest.write_text(f'[package]\nname = "{name}"\nversion = "{version}"\nedition = "2024"\nrust-version = "1.85"\n{dependencies}')
        destination = f"/usr/lib/liteos/sdk/{name}"
        files.append((manifest, f"{destination}/Cargo.toml", 0o644))
        source = ROOT / "user" / "libraries" / name / "src"
        files.extend((path, f"{destination}/src/{path.relative_to(source).as_posix()}", 0o644) for path in sorted(source.rglob("*.rs")))
    template = ROOT / "user/libraries/liteos-ui/template"
    files.extend((path, f"/usr/share/liteos/sdk/template/{path.relative_to(template).as_posix()}", 0o644) for path in sorted(template.rglob("*")) if path.is_file())
    return tuple(files)
