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
    transport = {"alloc", "medium-ethernet", "medium-ip", "proto-ipv4", "socket-raw", "socket-tcp", "socket-tcp-reno", "socket-udp"}
    # 产品协议引擎与 host 独立 oracle 共用固定源码；DHCP 编解码已迁至 packet-wire。
    for relative, section, features in [("system/netstack/Cargo.toml", "dependencies", transport),
                                        ("tools/netstack-unit/Cargo.toml", "dependencies", transport),
                                        ("tools/netstack-unit/Cargo.toml", "dev-dependencies", {"proto-dhcpv4"})]:
        dependency = tomllib.loads((root / relative).read_text())[section]["smoltcp"]
        if dependency.get("path") != "../../vendor/smoltcp" or dependency.get("version") != "=0.13.1":
            raise ValueError(f"smoltcp: {relative} must use the single pinned local transport")
        if dependency.get("default-features") is not False or set(dependency["features"]) != features:
            raise ValueError(f"smoltcp: unexpected protocol features in {relative}")

    dhcp = tomllib.loads((root / "system/dhcp-client/Cargo.toml").read_text())["dependencies"]
    if "smoltcp" in dhcp or dhcp.get("packet-wire", {}).get("path") != "../../libraries/packet-wire":
        raise ValueError("dhcp-client: packet formats must use packet-wire without a second codec")


if __name__ == "__main__":
    verify()
    print("smoltcp single-source provenance and inventory ok")
