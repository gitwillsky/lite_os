#!/usr/bin/env python3
"""Wine 运行时的安装清单：`prebuilt/wine/` 的压缩包加固定的 Alpine 运行时库，供开发镜像同步使用。

1. 压缩包按 `prebuilt/wine/SHA256SUMS` 校验后展开到 `.local/inputs/wine/`（内容寻址，已展开则复用）。
2. 运行时库来自固定的 Alpine 闭包（每个 APK 的 SHA-256 写在下面）；只取 `.so`、fontconfig 与 XKB 配置，其余（可执行文件、man、头文件）丢弃。
3. 符号链接一律展开成同内容的普通文件：安装器只写普通文件，musl 按 DT_NEEDED 的 soname 找文件。
详见 docs/development/wine-runtime.md。
"""

from __future__ import annotations

import configparser
import io
import os
import shutil
import stat
import tarfile
from xml.etree import ElementTree as ET
from pathlib import Path

from apk_cache import ALPINE_ARCH, download
from build_cache import cache_lock, sha256, temporary_directory
from local_inputs import INPUTS
from userland import UserlandArtifact

ROOT = Path(__file__).resolve().parent.parent
ARCHIVE = ROOT / "prebuilt/wine/wine-11.16-aarch64-musl.tar.xz"
CHECKSUMS = ROOT / "prebuilt/wine/SHA256SUMS"
CACHE = INPUTS / "wine"
# APK 展开是派生产物；每个 worktree 在自己的 target 内构建，只读共享的固定归档。
LIBRARY_CACHE = ROOT / "target" / "wine-runtime" / ALPINE_ARCH
INSTALL_ROOT = "/opt/wine"
LIBRARY_DIRECTORY = "/usr/lib/liteos"
# 展开后的开发镜像至少需要的容量（MiB）：运行时树约 750 MiB 加库与余量。
REQUIRED_CAPACITY_MIB = 1792

# Alpine v3.22 aarch64。musl 与 busybox 由系统自己提供，所以不在闭包里。
# winewayland 在监听 wl_keyboard 前必须解析默认 XKB ruleset，不能只安装共享库与 keymap。
RUNTIME_PACKAGES: tuple[tuple[str, str], ...] = (
    ("alsa-lib-1.2.14-r0.apk", "da15717b4cc87913363bc53dff9c45d6341c84103b18352b208cd727aa8c3d70"),
    ("brotli-libs-1.1.1-r0.apk", "c4ecb26f02ab93c7e41c019c98bf024bd6d90593be308a003bc516806732500e"),
    ("fontconfig-2.15.0-r3.apk", "a8bb31f32197dc18297aa3a618f630ed7c3efdfb875caa8a7b1fc2648834cbea"),
    ("freetype-2.13.3-r0.apk", "0cb7c95edd545b634912d869ff6c50deeedde9b0d18cdc4f934ff9ef816bb1b3"),
    ("gmp-6.3.0-r3.apk", "0d2eb1079b1b5692e9e6652ff0e269caeb9c812f483e34c88d461c03bcf75460"),
    ("gnutls-3.8.13-r0.apk", "66a9bdc03d6943321847063d29dab193a59aa431c78aa874cfd3c746b0650737"),
    ("libbz2-1.0.8-r6.apk", "1d588037cc45108acafdbb1507e9f683019a8816fb74d9aba9b0a2dc099463c4"),
    ("libexpat-2.9.0-r0.apk", "3a53825b470556ebd0c08b27dd6c23f6b039232f27951ca983f1b1540edf8d1b"),
    ("libffi-3.4.8-r0.apk", "9391f60a14c146655deaf65115563bc8dcd749cf0f93ec567e6443f2ed7d3bfc"),
    ("libidn2-2.3.7-r0.apk", "9ddc248988707da96752077d05bacbda751d46cc1f7aa1460b3a40c4fbf66a6e"),
    ("libpng-1.6.59-r0.apk", "93132da4155e062e12347aca01a761491b8fc40a1ffa955bd588d911550f2bdf"),
    ("libtasn1-4.21.0-r0.apk", "c348eb9a293bbf1ff2d922fe200b525088e0038c0dd5154b68a4b22753e9385f"),
    ("libunistring-1.3-r0.apk", "6b6631284b25fa28bb9e63f9d423145e96d0f7aeb55a592f5cd5d54fb39380f7"),
    ("libxkbcommon-1.8.1-r2.apk", "8a37b225bae2144e7caa0e7f01d7d65c6d9a8493ed640ca2a7a3836af70883dc"),
    ("libxml2-2.13.9-r1.apk", "e358e1c32ba6923386263dd641a6e4bcb56a0345fecadef9ef1103892cf3d81d"),
    ("nettle-3.10.2-r0.apk", "be9d47dd38b2f9649a18cd1a3096d3d131427aadfdd95dd380f7d762e6f4036a"),
    ("p11-kit-0.26.2-r0.apk", "401078e81e024616fc61b7f631baeefe33b43865c638a12be5b8e9b20d1e1f88"),
    ("wayland-libs-client-1.23.1-r3.apk", "9481b11de37951462cf26a8309e410346455f67b17f7f2d48f0d2326c522ebee"),
    ("xkeyboard-config-2.43-r0.apk", "d0ba36ffc8d6b8614ec6d7ef5a2ef9edbb6f9016473ae9c077df3d0d9de38049"),
    ("xz-libs-5.8.4-r0.apk", "8a64b55223f29e24f934ccbc8167d535b913bf923dab5e14de7946a7c36091a7"),
    ("zlib-1.3.2-r1.apk", "2d274cb1b2a36d0e8347d802e6dea8648cab952ea5fae1989ab72e725842a2c3"),
)
# Fontconfig 主配置由 base_userland 拥有；Wine 只追加配置片段，不能重新发布上游 fonts.conf。
# 缺 fontconfig 配置片段会破坏字体回退；缺 XKB 默认 ruleset 会使 Wine 不注册键盘监听。
CONFIG_PREFIXES = ("etc/fonts/conf.d/", "usr/share/fontconfig/", "usr/share/X11/xkb/")


def expected_archive_digest() -> str:
    """读取 `SHA256SUMS` 里压缩包的期望摘要；记录缺失或格式错误抛出 `RuntimeError`。"""
    for line in CHECKSUMS.read_text().splitlines():
        digest, _, name = line.partition("  ")
        if name == ARCHIVE.name:
            return digest
    raise RuntimeError(f"{CHECKSUMS} has no entry for {ARCHIVE.name}")


def unpack_runtime() -> Path:
    """校验并展开 Wine 运行时压缩包，返回展开后的 `wine-rt` 目录（已展开则直接复用）。"""
    if not ARCHIVE.is_file():
        raise RuntimeError(f"missing {ARCHIVE}; run `git lfs pull`")
    digest = expected_archive_digest()
    if sha256(ARCHIVE) != digest:
        raise RuntimeError(f"{ARCHIVE} does not match prebuilt/wine/SHA256SUMS (is it an LFS pointer?)")
    final = CACHE / digest[:16]
    if (final / "wine-rt/bin/wine").is_file():
        return final / "wine-rt"
    CACHE.mkdir(parents=True, exist_ok=True)
    with cache_lock(CACHE / ".lock"):
        if (final / "wine-rt/bin/wine").is_file():
            return final / "wine-rt"
        temporary = temporary_directory(CACHE, "unpack")
        with tarfile.open(ARCHIVE, "r:xz") as tree:
            tree.extractall(temporary, filter="data")
        shutil.rmtree(final, ignore_errors=True)
        os.replace(temporary, final)
    return final / "wine-rt"


def extract_libraries() -> Path:
    """展开固定 APK 闭包里的 `.so`、fontconfig 与 XKB 配置，符号链接展开为普通文件；返回展开根目录。"""
    key = _library_cache_key()
    final = LIBRARY_CACHE / f"libraries-{key}"
    if final.is_dir():
        return final
    LIBRARY_CACHE.mkdir(parents=True, exist_ok=True)
    with cache_lock(LIBRARY_CACHE / ".lock"):
        if final.is_dir():
            return final
        temporary = temporary_directory(LIBRARY_CACHE, "libraries")
        for name, digest in RUNTIME_PACKAGES:
            archive = download(name, digest)
            with tarfile.open(archive, "r:gz") as tree:
                members = {member.name: member for member in tree.getmembers()}
                for path, member in members.items():
                    if path.startswith(".") or not (
                        (path.startswith("usr/lib/") and ".so" in path)
                        or path.startswith(CONFIG_PREFIXES)
                    ):
                        continue
                    target = resolve_member(members, member)
                    if target is None or not target.isfile():
                        continue
                    destination = temporary / path
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    source = tree.extractfile(target)
                    assert source is not None
                    destination.write_bytes(source.read())
                    destination.chmod(0o755 if ".so" in path else 0o644)
        os.replace(temporary, final)
    return final


def _library_cache_key() -> str:
    """固定 APK 与提取范围共同标识缓存，避免范围收缩后继续发布旧缓存中的文件。"""
    import hashlib

    return hashlib.sha256(repr((RUNTIME_PACKAGES, CONFIG_PREFIXES)).encode()).hexdigest()[:16]


def resolve_member(members: dict[str, tarfile.TarInfo], member: tarfile.TarInfo) -> tarfile.TarInfo | None:
    """跟随（包内）符号链接到普通文件；链到包外或成环返回 `None`。"""
    for _ in range(8):
        if not member.issym():
            return member
        base = Path(member.name).parent
        target = os.path.normpath(str(base / member.linkname))
        if target not in members:
            return None
        member = members[target]
    return None


def _fontconfig() -> Path:
    """由原生字体映射生成标准 Fontconfig 配置；两种运行时共享目录和家族回退顺序。

    产物位于 worktree 的 target 中。别名追加候选，不覆盖用户实际安装的同名字体；
    未登记系统字体目录时，Wine 的 Fontconfig 扫描不会发现 LiteOS 的中文字体。
    """
    mapping = configparser.ConfigParser(interpolation=None)
    mapping.optionxform = str
    with (ROOT / "assets/classic/fonts/fontmap.ini").open(encoding="utf-8") as stream:
        mapping.read_file(stream)
    root = ET.Element("fontconfig")
    ET.SubElement(root, "dir").text = "/usr/share/liteos/fonts"
    for section in ("FontSubstitutes", "SystemLink"):
        for family, linked in mapping.items(section):
            alias = ET.SubElement(root, "alias", binding="same")
            ET.SubElement(alias, "family").text = family
            accept = ET.SubElement(alias, "accept")
            for name in linked.split(","):
                ET.SubElement(accept, "family").text = name.strip()
    ET.indent(root)
    data = ('<?xml version="1.0" encoding="UTF-8"?>\n'
            '<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">\n'
            + ET.tostring(root, encoding="unicode") + "\n")
    path = LIBRARY_CACHE / "99-liteos-ui.conf"
    path.parent.mkdir(parents=True, exist_ok=True)
    if not path.is_file() or path.read_text(encoding="utf-8") != data:
        path.write_text(data, encoding="utf-8")
    return path


def wine_userland() -> tuple[UserlandArtifact, ...]:
    """Wine 安装清单：`/opt/wine`、`/usr/lib/liteos` 依赖库、fontconfig 与 XKB 数据。

    Raises:
        RuntimeError: 目标架构没有运行时（只有 aarch64 构建了 Wine），或压缩包缺失/校验失败。
    """
    if ALPINE_ARCH != "aarch64":
        raise RuntimeError("the Wine runtime is only built for aarch64")
    artifacts: list[UserlandArtifact] = []
    tree = unpack_runtime()
    for path in sorted(tree.rglob("*")):
        if path.is_file():
            mode = 0o755 if stat.S_IMODE(path.stat().st_mode) & 0o111 else 0o644
            artifacts.append(UserlandArtifact(path, f"{INSTALL_ROOT}/{path.relative_to(tree).as_posix()}", mode))
    libraries = extract_libraries()
    for path in sorted(libraries.rglob("*")):
        if not path.is_file():
            continue
        relative = path.relative_to(libraries).as_posix()
        if relative.startswith("usr/lib/"):
            artifacts.append(UserlandArtifact(path, f"{LIBRARY_DIRECTORY}/{path.name}", 0o755))
        else:
            artifacts.append(UserlandArtifact(path, f"/{relative}", 0o644))
    # 应用安装与生成的快捷方式共用 /bin/wine，按登录用户选择唯一前缀。
    artifacts.append(UserlandArtifact(ROOT / "user/base/wine/wine", "/bin/wine", 0o755))
    artifacts.extend(UserlandArtifact(path, f"/usr/share/applications/{path.name}")
                     for path in sorted((ROOT / "user/base/wine").glob("*.desktop")))
    artifacts.append(UserlandArtifact(_fontconfig(), "/etc/fonts/conf.d/99-liteos-ui.conf", 0o644))
    names = [artifact.destination for artifact in artifacts]
    if len(names) != len(set(names)):
        raise RuntimeError("duplicate Wine runtime destinations")
    return tuple(artifacts)


if __name__ == "__main__":
    install = wine_userland()
    total = sum(artifact.source.stat().st_size for artifact in install)
    print(f"wine runtime: {len(install)} files, {total // 1024 // 1024} MiB")
