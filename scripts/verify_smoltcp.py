#!/usr/bin/env python3
"""校验唯一 smoltcp 源码来源与不可变 upstream inventory；不生成、不修补源码。"""
from __future__ import annotations
import hashlib
import json
from pathlib import Path
import tomllib

ROOT = Path(__file__).resolve().parent.parent
MANIFEST_SHA256 = "e9f524442024f3f36ed3e3c6cd60a412a4d7b8d7879c2f7a31763940496f18f8"
# 本地 transport owner 修改范围；新变化必须有固定规范与实际回归证明，不允许其他文件漂移。
TRANSPORT_FILES = {"src/socket/udp.rs", "src/iface/interface/udp.rs", "src/socket/tcp.rs",
                   "src/socket/raw.rs", "src/iface/interface/mod.rs", "src/iface/interface/ipv4.rs", "src/iface/mod.rs", "src/phy/mod.rs", "src/iface/interface/tcp.rs",
                   "src/wire/udp.rs"}
LOCAL_FILES = {"UPSTREAM.sha256.json", "LITEOS.md"}


def verify(root: Path = ROOT) -> None:
    vendor = root / "vendor/smoltcp"
    inventory = vendor / "UPSTREAM.sha256.json"
    if hashlib.sha256(inventory.read_bytes()).hexdigest() != MANIFEST_SHA256:
        raise ValueError("smoltcp: pinned upstream inventory changed")
    manifest = json.loads(inventory.read_text())
    if manifest["crate"] != "0.13.1" or manifest["commit"] != "e347a1e2d3ac33c5ce2c0c114e24b85ae23c4897":
        raise ValueError("smoltcp: upstream revision changed")
    for relative, digest in manifest["files"].items():
        path = vendor / relative
        if not path.is_file():
            raise ValueError(f"smoltcp: missing upstream file {relative}")
        if relative not in TRANSPORT_FILES and hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise ValueError(f"smoltcp: immutable upstream file changed: {relative}")
    for path in vendor.rglob("*"):
        if not path.is_file() or "target" in path.relative_to(vendor).parts:
            continue
        relative = path.relative_to(vendor).as_posix()
        if relative not in manifest["files"] and relative not in LOCAL_FILES:
            raise ValueError(f"smoltcp: untracked source outside inventory: {relative}")
    for relative, expected in [("kernel/Cargo.toml", "../vendor/smoltcp"),
                               ("tools/kernel-unit/Cargo.toml", "../../vendor/smoltcp"),
                               ("tools/architecture-bench/Cargo.toml", "../../vendor/smoltcp")]:
        dependency = tomllib.loads((root / relative).read_text())["dependencies"]["smoltcp"]
        if dependency.get("path") != expected or dependency.get("version") != "=0.13.1":
            raise ValueError(f"smoltcp: {relative} must use the single pinned local transport")
        if dependency.get("default-features") is not False or set(dependency["features"]) != {
            "alloc", "medium-ethernet", "medium-ip", "proto-ipv4", "socket-raw", "socket-tcp", "socket-tcp-reno", "socket-udp"
        }:
            raise ValueError(f"smoltcp: unexpected protocol features in {relative}")


if __name__ == "__main__":
    verify()
    print("smoltcp single-source provenance and inventory ok")
