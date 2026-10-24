#!/usr/bin/env python3
"""从用户的 Windows 2000 简中 ISO 提取本地参照素材；不执行 PE，不修改产品 rootfs。

1. 7zz 读取 ISO，cabextract 展开安装文件，wrestool 重建资源容器。
2. 保留 ICO/CUR/ANI、字体与 WAV 原始字节；另输出有来源记录的 PNG。
3. manifest 最后写入；--check 校验完整目录，不把半成品当成可用素材。
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import struct
import subprocess
import tempfile
import wave
from pathlib import Path

from local_inputs import INPUTS

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_OUTPUT = INPUTS / "windows2000/assets"
PE_FILES = (
    "shell32.dll", "explorer.exe", "msgina.dll", "ntoskrnl.exe", "user32.dll",
    "mmsys.cpl", "sysdm.cpl", "main.cpl", "timedate.cpl", "taskmgr.exe",
    "cmd.exe", "sndvol32.exe", "mydocs.dll", "moricons.dll", "comctl32.dll",
    "desk.cpl", "netplwiz.dll", "mplayer2.exe", "stobject.dll",
)
# 资源号来自这份安装介质的资源目录及逐图核对，不是 shell icon 的零起始索引。
ICONS = {
    "my-computer": ("shell32.dll", 16),
    "my-documents": ("mydocs.dll", 100),
    "recycle-bin-empty": ("shell32.dll", 32),
    "recycle-bin-full": ("shell32.dll", 33),
    "folder-closed": ("shell32.dll", 4),
    "folder-open": ("shell32.dll", 5),
    "file-generic": ("shell32.dll", 1),
    "file-text": ("shell32.dll", 152),
    "file-app": ("shell32.dll", 3),
    "drive-hdd": ("shell32.dll", 9),
    "drive-cd": ("shell32.dll", 12),
    "control-panel": ("shell32.dll", 137),
    "programs": ("shell32.dll", 20),
    "documents": ("shell32.dll", 21),
    "settings": ("shell32.dll", 22),
    "search": ("shell32.dll", 23),
    "help": ("shell32.dll", 24),
    "run": ("shell32.dll", 25),
    "shutdown": ("shell32.dll", 28),
    "logoff": ("shell32.dll", 45),
    "lock": ("shell32.dll", 48),
    "key": ("shell32.dll", 194),
    "command-prompt": ("cmd.exe", "IDI_APPICON"),
    "task-manager": ("taskmgr.exe", 107),
    "media-player": ("mplayer2.exe", 110),
    "show-desktop": ("explorer.exe", 103),
    "display-properties": ("desk.cpl", 40),
    "date-time": ("timedate.cpl", 200),
    "sound-volume": ("sndvol32.exe", 304),
    "users": ("netplwiz.dll", 102),
    "tray-volume": ("stobject.dll", 230),
    "tray-volume-muted": ("stobject.dll", 231),
    "ctrl-alt-del": ("msgina.dll", 109),
    "msg-error": ("user32.dll", 103),
    "msg-warning": ("user32.dll", 101),
    "msg-info": ("user32.dll", 104),
    "msg-question": ("user32.dll", 102),
}
CURSORS = {
    "arrow": 100, "text": 101, "busy": 102, "precision": 103,
    "resize-ns": 104, "resize-nwse": 105, "resize-nesw": 106,
    "resize-ew": 107, "move": 109, "unavailable": 110,
    "working": 111, "help": 112, "link": 114,
}
BITMAPS = {
    "boot-scene": ("ntoskrnl.exe", 1),
    "logon-banner": ("msgina.dll", 101),
    "start-banner": ("explorer.exe", 157),
}
# HIVEDEF.INF 的默认 AppEvents 配置；logoff 是独立提供的注销素材。
SOUNDS = {
    "logon": "logon.wav", "logoff": "logoff.wav",
    "critical": "chord.wav", "exclamation": "chord.wav",
    "asterisk": "chord.wav", "question": "chord.wav",
}


def digest(path: Path) -> str:
    """返回文件的流式 SHA256；读取失败时抛出 OSError。"""
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def run(*args: str | Path) -> str:
    """直接执行工具并返回 stdout；工具失败时保留 stderr 并抛出异常。"""
    return subprocess.run([str(a) for a in args], check=True, text=True,
                          stdout=subprocess.PIPE).stdout


def icon_container(data: bytes) -> bytes:
    """按 ICO/CUR 目录重建文件，只发布被引用的图像字节；截断或越界目录抛出 ValueError。"""
    if len(data) < 6:
        raise ValueError("truncated ICO/CUR header")
    reserved, kind, count = struct.unpack_from("<HHH", data)
    offset = 6 + 16 * count
    if reserved != 0 or kind not in (1, 2) or not count or offset > len(data):
        raise ValueError("invalid ICO/CUR directory")
    entries, images = [], []
    for index in range(count):
        entry = data[6 + 16 * index:22 + 16 * index]
        size, start = struct.unpack_from("<II", entry, 8)
        if not size or start < 6 + 16 * count or start + size > len(data):
            raise ValueError("ICO/CUR image outside file")
        entries.append(entry[:8] + struct.pack("<II", size, offset))
        images.append(data[start:start + size])
        offset += size
    return data[:6] + b"".join(entries + images)


def best_icon_frame(ico, size: int):
    """返回 ICO 中 size×size 目录项里色深最高的一帧。

    系统图标同尺寸常带单色/16 色/256 色多个变体，且单色版排在最前；Pillow 的
    `getimage(size)` 取第一个匹配项，会让消息框等图标静默变成黑白版。
    """
    candidates = [i for i, entry in enumerate(ico.entry) if (entry.width, entry.height) == (size, size)]
    return ico.frame(max(candidates, key=lambda i: ico.entry[i].bpp))


def check(directory: Path) -> dict:
    """校验 manifest 中每个文件及逻辑引用；缺失、越界或哈希不符时失败。"""
    manifest = json.loads((directory / "manifest.json").read_text())
    for name, info in manifest["files"].items():
        path = (directory / name).resolve()
        if not path.is_relative_to(directory.resolve()):
            raise ValueError(f"path escapes asset directory: {name}")
        if path.stat().st_size != info["bytes"] or digest(path) != info["sha256"]:
            raise ValueError(f"asset integrity mismatch: {name}")
    for group in manifest["logical"].values():
        for item in group.values():
            for name in item["paths"]:
                if name not in manifest["files"]:
                    raise ValueError(f"unrecorded logical asset: {name}")
    return manifest


def extract(iso: Path, output: Path) -> dict:
    """提取 ISO 到新的本地目录，返回 manifest；不覆盖已有结果，失败清理临时文件。"""
    from PIL import Image

    if output.exists():
        raise FileExistsError(f"output already exists; use --check or a new --output: {output}")
    for tool in ("7zz", "cabextract", "wrestool"):
        if not shutil.which(tool):
            raise RuntimeError(f"missing {tool}; brew install sevenzip cabextract icoutils")
    output.parent.mkdir(parents=True, exist_ok=True)
    # 临时目录与最终输出同一文件系统；只有完整校验后才发布，失败不会留下可消费的半目录。
    with tempfile.TemporaryDirectory(prefix=".extract-", dir=output.parent) as work:
        base = Path(work)
        media, result = base / "media", base / "result"
        media.mkdir()
        result.mkdir()
        run("7zz", "x", "-y", "-bso0", "-bsp0", f"-o{media}", iso, "I386/*")
        i386 = media / "I386"
        expanded = result / "sources"
        expanded.mkdir()
        wanted = {n.upper()[:-1] + "_" for n in PE_FILES}
        wanted.update({"SIMSUN.TT_", "TAHOMA.TT_", "TAHOMABD.TT_"})
        containers = sorted(p for p in i386.iterdir()
                            if p.name in wanted or p.suffix in {".WA_", ".CU_", ".AN_"})
        for path in containers:
            run("cabextract", "-q", "-L", "-d", expanded, path)
        for name in ("HIVEDEF.INF", "TXTSETUP.SIF"):
            shutil.copyfile(i386 / name, expanded / name.lower())
        missing = [name for name in PE_FILES if not (expanded / name).is_file()]
        if missing:
            raise ValueError(f"required PE files missing: {missing}")
        resources = result / "resources"
        resources.mkdir()
        warnings = []
        for name in PE_FILES:
            dest = resources / name
            dest.mkdir()
            for kind in (2, 12, 14):
                process = subprocess.run(
                    ["wrestool", "-x", f"-t{kind}", "-o", str(dest), str(expanded / name)],
                    check=True, capture_output=True, text=True,
                )
                if process.stderr:
                    warnings.append(process.stderr.strip().replace(str(base), "<work>"))
        # 校验全部导出帧，包括未映射资源。wrestool 的资源长度警告不能替代实际解码检查。
        for path in resources.rglob("*"):
            if path.suffix in {".ico", ".cur", ".bmp"}:
                if path.suffix in {".ico", ".cur"}:
                    # 容器由资源重建，不是 PE 的原始文件；只拷贝目录引用的 payload。
                    # 缺少此边界，wrestool 的未引用尾部可能携带进程内存，使哈希不可复现。
                    path.write_bytes(icon_container(path.read_bytes()))
                with Image.open(path) as image:
                    if path.suffix == ".ico":
                        for size in image.ico.sizes():
                            image.ico.getimage(size).load()
                    else:
                        image.load()

        logical: dict[str, dict] = {kind: {} for kind in ("icons", "cursors", "bitmaps", "sounds", "fonts")}

        def resource(source: str, kind: int, number: int | str, suffix: str) -> Path:
            # 这里只接受简中资源，不把其他语言悄悄当成简中参照。
            path = resources / source / f"{source}_{kind}_{number}_2052.{suffix}"
            if not path.is_file():
                raise ValueError(f"missing CHS resource: {path.name}")
            return path

        def record(kind: str, name: str, paths: list[Path], **metadata) -> None:
            logical[kind][name] = {"paths": [str(p.relative_to(result)) for p in paths], **metadata}

        icon_dir = result / "icons-src"
        icon_dir.mkdir()
        for name, (source, number) in ICONS.items():
            path = resource(source, 14, number, "ico")
            with Image.open(path) as image:
                sizes = image.ico.sizes()
                native, paths = [], [path]
                for size in (16, 32, 64):
                    if (size, size) not in sizes:
                        continue
                    dest = icon_dir / f"{name}-{size}.png"
                    best_icon_frame(image.ico, size).convert("RGBA").save(dest)
                    paths.append(dest)
                    native.append(size)
            record("icons", name, paths, source=source, resource=number,
                   native_sizes=sorted([list(size) for size in sizes]), png_sizes=native)
        for name, number in CURSORS.items():
            path = resource("user32.dll", 12, number, "cur")
            data = path.read_bytes()
            reserved, kind, count = struct.unpack_from("<HHH", data)
            if (reserved, kind, count) != (0, 2, 1):
                raise ValueError(f"unexpected default cursor directory: {path.name}")
            width, height, _, _, x, y = struct.unpack_from("<BBBBHH", data, 6)
            record("cursors", name, [path], hotspot=[x, y], size=[width or 256, height or 256],
                   raster_operation="AND/XOR", source="user32.dll", resource=number)
        bitmap_dir = result / "bitmaps-src"
        bitmap_dir.mkdir()
        for name, (source, number) in BITMAPS.items():
            path = resource(source, 2, number, "bmp")
            dest = bitmap_dir / f"{name}-1x.png"
            with Image.open(path) as image:
                image.convert("RGBA").save(dest)
                size = list(image.size)
            record("bitmaps", name, [path, dest], size=size, source=source, resource=number)
        for name, filename in SOUNDS.items():
            path = expanded / filename
            with wave.open(str(path)) as sound:
                record("sounds", name, [path], rate=sound.getframerate(),
                       channels=sound.getnchannels(), sample_bits=sound.getsampwidth() * 8,
                       frames=sound.getnframes())
        for name in ("simsun.ttc", "tahoma.ttf", "tahomabd.ttf"):
            path = expanded / name
            if path.read_bytes()[:4] not in (b"ttcf", b"\0\1\0\0", b"OTTO"):
                raise ValueError(f"invalid font signature: {name}")
            record("fonts", name, [path])
        files = {}
        for path in sorted(result.rglob("*")):
            if path.is_file():
                files[str(path.relative_to(result))] = {"bytes": path.stat().st_size, "sha256": digest(path)}
        manifest = {
            "version": 1, "iso": {"name": iso.name, "bytes": iso.stat().st_size, "sha256": digest(iso)},
            "language": 2052, "ui_brand": "LiteOS", "files": files, "logical": logical,
            "unmapped": ["liteos-logo (keep original LiteOS branding)"],
            "warnings": warnings,
            "limitations": ["not installed into product rootfs", "native resolutions only; no invented 2x assets",
                            "CUR AND/XOR masks and ANI timing retained in original containers"],
            "tools": {"7zz": run("7zz").splitlines()[1],
                      "cabextract": run("cabextract", "--version").splitlines()[0],
                      "wrestool": run("wrestool", "--version").splitlines()[0]},
        }
        (result / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n")
        check(result)
        result.rename(output)
    return manifest


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--iso", type=Path, help="用户提供的 Windows 2000 简中 ISO")
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT, help="必须位于被 Git 忽略的 .local/inputs/ 下")
    parser.add_argument("--check", action="store_true", help="只校验已有提取结果")
    args = parser.parse_args()
    output = args.output.expanduser().resolve()
    if not output.is_relative_to(INPUTS.resolve()):
        parser.error("original assets must stay inside this repository's ignored .local/inputs/ directory")
    if not args.check and args.iso is None:
        parser.error("--iso is required for extraction")
    try:
        manifest = check(output) if args.check else extract(args.iso.expanduser().resolve(), output)
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"{error}\n")
    print(f"verified {len(manifest['files'])} files: {output / 'manifest.json'}")


if __name__ == "__main__":
    main()
