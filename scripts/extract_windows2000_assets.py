#!/usr/bin/env python3
"""从 Windows 2000 简中 ISO 导入产品素材；不执行 PE，不修改运行中的 rootfs。

1. 7zz 读取 ISO，cabextract 展开安装文件，wrestool 重建资源容器。
2. 保留 ICO/CUR/ANI、字体与 WAV 原始字节；另输出有来源记录的 PNG。
3. 只发布产品依赖的运行时文件到 assets/classic；manifest 记录 ISO、资源号与逐文件摘要。
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
FONTS = ("simsun.ttc", "tahoma.ttf", "tahomabd.ttf", "marlett.ttf")
PE_FILES = (
    "shell32.dll", "explorer.exe", "msgina.dll", "ntoskrnl.exe", "user32.dll",
    "mmsys.cpl", "sysdm.cpl", "main.cpl", "timedate.cpl", "taskmgr.exe",
    "cmd.exe", "sndvol32.exe", "mydocs.dll", "moricons.dll", "comctl32.dll",
    "desk.cpl", "netplwiz.dll", "mplayer2.exe", "stobject.dll",
    # 资源管理器的工具栏位图、文件夹/搜索/历史图标与菜单位图。
    "browseui.dll", "shdocvw.dll",
    # 设备管理器、网络连接与记事本的专用图标。
    "devmgr.dll", "netshell.dll", "notepad.exe",
    # 文件夹 Web 视图的模板、样式与左侧水印/分隔线（类型 23 资源）。
    "webvw.dll",
)
# 只为取工具栏位图而加入的 DLL：其中个别资源可能是损坏/非标准图标，跳过并记警告，
# 不影响上面逐图核对的核心素材。
OPTIONAL_PE = {"browseui.dll", "shdocvw.dll"}
# 资源号来自这份安装介质的资源目录及逐图核对，不是 shell icon 的零起始索引。
ICONS = {
    "windows-logo": ("user32.dll", 105),
    "my-computer": ("shell32.dll", 16),
    "my-documents": ("mydocs.dll", 100),
    "recycle-bin-empty": ("shell32.dll", 32),
    "folder-closed": ("shell32.dll", 4),
    "file-generic": ("shell32.dll", 1),
    "file-text": ("shell32.dll", 152),
    "file-app": ("shell32.dll", 3),
    "drive-hdd": ("shell32.dll", 9),
    "drive-cd": ("shell32.dll", 12),
    "file-system": ("shell32.dll", 153),
    "control-panel": ("shell32.dll", 137),
    "programs": ("shell32.dll", 20),
    "documents": ("shell32.dll", 21),
    "settings": ("shell32.dll", 22),
    "help": ("shell32.dll", 24),
    "run": ("shell32.dll", 25),
    "shutdown": ("shell32.dll", 28),
    "logoff": ("shell32.dll", 45),
    "lock": ("shell32.dll", 48),
    "command-prompt": ("cmd.exe", "IDI_APPICON"),
    "task-manager": ("taskmgr.exe", 107),
    "media-player": ("mplayer2.exe", 110),
    "show-desktop": ("explorer.exe", 103),
    "display-properties": ("desk.cpl", 40),
    "date-time": ("timedate.cpl", 200),
    "sound-volume": ("sndvol32.exe", 304),
    "users": ("netplwiz.dll", 102),
    "device-manager": ("devmgr.dll", 201),
    "network-connections": ("netshell.dll", 10010),
    "network-local": ("netshell.dll", 122),
    "network-disabled": ("netshell.dll", 121),
    "tray-network": ("netshell.dll", 190),
    "tray-network-disconnected": ("netshell.dll", 195),
    "network-disconnected": ("netshell.dll", 123),
    "network-places": ("shell32.dll", 18),
    "entire-network": ("shell32.dll", 14),
    "desktop": ("shell32.dll", 35),
    "notepad": ("notepad.exe", 1),
    "tray-volume": ("stobject.dll", 230),
    "tray-volume-muted": ("stobject.dll", 231),
    "ctrl-alt-del": ("msgina.dll", 109),
}
# 资源管理器和应用图标同时用于 16/32 像素；这些外壳专用图标只发布调用者需要的尺寸。
ICON_SIZES = {
    "programs": (32,), "documents": (32,), "help": (16,),
    "shutdown": (32,), "logoff": (32,), "lock": (32,),
    "show-desktop": (16,), "ctrl-alt-del": (32,),
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
    # 资源管理器工具栏：默认（灰阶）/ 悬停（彩色）/ 禁用三条 47 帧 20×20 横条，洋红为透明色。
    "toolbar-default": ("browseui.dll", 272),
    "toolbar-hot": ("browseui.dll", 273),
    "toolbar-disabled": ("browseui.dll", 276),
    # IE 按钮条（后退、前进、停止、刷新、主页、搜索、收藏……历史 12），16 帧 20×20，冷/热/禁用。
    "ie-toolbar-default": ("browseui.dll", 261),
    "ie-toolbar-hot": ("browseui.dll", 262),
    "ie-toolbar-disabled": ("browseui.dll", 265),
    # 地址栏“转到”的绿色箭头，冷/热。
    "go-default": ("browseui.dll", 290),
    "go-hot": ("browseui.dll", 291),
    # ReBar 右端的动画 logo：帧竖排，首帧为静止画面；22/26/38 像素随带区高度选择。
    "brand-22": ("browseui.dll", 306),
    "brand-26": ("browseui.dll", 305),
    "brand-38": ("browseui.dll", 304),
    # “日期/时间 属性”时区页的世界地图（356×184，8 位调色板）。
    "worldmap": ("timedate.cpl", 50),
}
# 这些位图用 #FF00FF 表示透明，转 PNG 时换成 alpha 0。
COLOR_KEYED = {
    "toolbar-default", "toolbar-hot", "toolbar-disabled",
    "ie-toolbar-default", "ie-toolbar-hot", "ie-toolbar-disabled", "go-default", "go-hot",
}
# 保留原始调色板下标的位图：时区地图按注册表 Time Zones 的 MapID 在运行时着色。
INDEXED = {"worldmap"}
# webvw.dll 类型 23 资源：Web 视图左侧水印与标题下的分隔线，按 CSS 原样使用。
WEBVIEW = {"webview-left": "WVLEFT.BMP", "webview-line": "WVLINE.GIF"}
# HIVEDEF.INF 的默认 AppEvents 配置；logoff 是独立提供的注销素材。
SOUNDS = {
    "logon": "logon.wav", "logoff": "logoff.wav",
    "chord": "chord.wav", "recycle": "recycle.wav",
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
        wanted.update(name.upper()[:-1] + "_" for name in FONTS)
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
        # HTML 模板与图片是自定义类型 23，wrestool 只能按原始字节导出。
        subprocess.run(
            ["wrestool", "-x", "--raw", "-t23", "-o", str(resources / "webvw.dll"),
             str(expanded / "webvw.dll")],
            check=True, capture_output=True, text=True,
        )
        # 校验全部导出帧，包括未映射资源。wrestool 的资源长度警告不能替代实际解码检查。
        for path in resources.rglob("*"):
            if path.suffix in {".ico", ".cur", ".bmp"}:
                try:
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
                except (ValueError, OSError) as error:
                    if path.parent.name not in OPTIONAL_PE:
                        raise
                    warnings.append(f"{path.name}: skipped ({error})")
                    path.unlink()

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
                for size in ICON_SIZES.get(name, (16, 32)):
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
                if name in INDEXED:
                    if image.mode != "P":
                        raise ValueError(f"indexed bitmap lost its palette: {path}")
                    rgba = image.copy()
                else:
                    rgba = image.convert("RGBA")
                if name in COLOR_KEYED:
                    rgba.putdata([(0, 0, 0, 0) if px[:3] == (255, 0, 255) else px
                                  for px in rgba.get_flattened_data()])
                rgba.save(dest)
                size = list(image.size)
            record("bitmaps", name, [path, dest], size=size, source=source, resource=number)
        for name, resource_name in WEBVIEW.items():
            path = resources / "webvw.dll" / f"webvw.dll_23_{resource_name}_2052"
            if not path.is_file():
                raise ValueError(f"missing CHS resource: {path.name}")
            dest = bitmap_dir / f"{name}-1x.png"
            with Image.open(path) as image:
                image.convert("RGBA").save(dest)
                size = list(image.size)
            record("bitmaps", name, [path, dest], size=size, source="webvw.dll", resource=resource_name)
        for name, filename in SOUNDS.items():
            path = expanded / filename
            with wave.open(str(path)) as sound:
                record("sounds", name, [path], rate=sound.getframerate(),
                       channels=sound.getnchannels(), sample_bits=sound.getsampwidth() * 8,
                       frames=sound.getnframes())
        for name in FONTS:
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


def publish(directory: Path) -> dict:
    """把校验后的提取结果转换为唯一产品资源集。

    Args:
        directory: `.local/inputs/` 内完整提取目录，保留原始 PE 与全部导出资源。

    Returns:
        已发布产品 manifest；每项标明 ISO 内来源和转换方式。

    Raises:
        OSError: 提取产物缺失、不可读或产品目录不可写。
        ValueError: manifest、图像或光标不能无损转换为产品格式。
    """
    source = check(directory)
    if set(source["logical"].get("fonts", ())) != set(FONTS):
        raise ValueError("font extraction recipe changed; re-extract into a new --output directory")
    from PIL import Image
    from pack_boot_scene import pack as pack_boot
    from pack_cursors import CURSOR_FILES, pack_sources
    from pack_shell_assets import pack as pack_shell
    import classic_assets

    icons = source["logical"]["icons"]
    if set(icons) != set(ICONS) or any(
        (icons[name]["source"], icons[name]["resource"]) != origin
        or icons[name]["png_sizes"] != [size for size in ICON_SIZES.get(name, (16, 32))
                                       if [size, size] in icons[name]["native_sizes"]]
        for name, origin in ICONS.items()
    ):
        raise ValueError("extraction recipe changed; re-extract into a new --output directory")
    # 1. 所有转换先在临时目录完成；失败时保留当前受版本管理的资源。
    with tempfile.TemporaryDirectory(prefix=".publish-", dir=directory.parent) as work:
        result = Path(work)
        files = {}

        def record(path: Path, group: str, origin: dict) -> None:
            files[str(path.relative_to(result))] = {
                "group": group, "bytes": path.stat().st_size, "sha256": digest(path), "origin": origin,
            }

        names = {Path(path).stem for group in ("icons", "bitmaps")
                 for name, item in source["logical"][group].items() if name != "boot-scene"
                 for path in item["paths"] if path.endswith(".png")}
        pack_shell(directory, result / "ui", names)
        for group in ("icons", "bitmaps"):
            for name, item in source["logical"][group].items():
                for image in item["paths"]:
                    path = result / "ui" / (Path(image).stem + ".bmp")
                    if image.endswith(".png") and path.is_file():
                        record(path, "ui", {"source": item["source"], "resource": item["resource"],
                                            "language": 2052, "conversion": "BMP V4; native dimensions"})
        # 2. 启动画面、光标、字体和声音由同一份 ISO 提供，不保留原创或替代来源。
        with Image.open(directory / "bitmaps-src/boot-scene-1x.png") as image:
            (result / "boot-scene.lbs").write_bytes(pack_boot(image))
        record(result / "boot-scene.lbs", "boot", {"source": "ntoskrnl.exe", "resource": 1, "conversion": "LBS1 lossless RLE"})
        link = directory / source["logical"]["cursors"]["link"]["paths"][0]
        (result / "cursors.lcs").write_bytes(pack_sources(directory / "sources", link))
        record(result / "cursors.lcs", "cursors", {
            "sources": [stem + ".cur" for stem in CURSOR_FILES.values()] + ["user32.dll:114"],
            "conversion": "LCS1 premultiplied ARGB; 2x nearest-neighbor; CUR hotspots",
        })
        for group in ("fonts", "sounds"):
            (result / group).mkdir()
            for item in source["logical"][group].values():
                original = directory / item["paths"][0]
                path = result / group / original.name
                shutil.copyfile(original, path)
                record(path, group, {"source": original.name, "conversion": "original bytes"})
        mapping = result / "fonts/fontmap.ini"
        mapping.write_text("; 原版字体目录；仅保留逻辑字体别名与 Tahoma 的中文 FontLink。\n"
                           "[FontSubstitutes]\nMS Shell Dlg=Tahoma\nMS Shell Dlg 2=Tahoma\n"
                           "SimSun=宋体\nNSimSun=新宋体\n\n[SystemLink]\nTahoma=宋体\n")
        record(mapping, "fonts", {"source": "LiteOS font mapping configuration"})
        manifest = {"version": 1, "iso": source["iso"], "language": 2052,
                    "tools": source["tools"], "files": dict(sorted(files.items()))}
        # 3. 清除旧源图与旧产品文件，只留下 manifest 声明的运行时文件和主题配置。
        destination = classic_assets.DIRECTORY
        for path in destination.iterdir():
            if path.name == "theme":
                continue
            if path.is_dir():
                shutil.rmtree(path)
            else:
                path.unlink()
        shutil.copytree(result, destination, dirs_exist_ok=True)
        (destination / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n")
    classic_assets.check()
    return manifest


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--iso", type=Path, default=INPUTS / "windows2000/Windows 2000 Professional Service Pack 4 CHS.iso",
                        help=".local/inputs/ 下的原版 ISO；普通构建不需要它")
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT, help=".local/inputs/ 下的原始提取目录")
    parser.add_argument("--check", action="store_true", help="只校验 Git 中的产品资源，不依赖本机 ISO 或提取目录")
    args = parser.parse_args()
    try:
        if args.check:
            from classic_assets import check as check_product
            manifest = check_product()
        else:
            output = args.output.expanduser().resolve()
            iso = args.iso.expanduser().resolve()
            if not output.is_relative_to(INPUTS.resolve()) or not iso.is_relative_to(INPUTS.resolve()):
                parser.error("ISO and intermediate extraction must stay inside .local/inputs/")
            if output.exists():
                extracted = check(output)
                if extracted["iso"]["sha256"] != digest(iso):
                    raise ValueError("ISO does not match the existing extraction")
            else:
                extract(iso, output)
            manifest = publish(output)
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"{error}\n")
    print(f"verified {len(manifest['files'])} product assets: {ROOT / 'assets/classic/manifest.json'}")


if __name__ == "__main__":
    main()
