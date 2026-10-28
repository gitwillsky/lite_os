#!/usr/bin/env python3
"""导入补充文件类型图标：裁剪透明边界并按像素风格导出 XDG hicolor PNG，普通构建只安装产物。"""
from __future__ import annotations
import argparse
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DIRECTORY = ROOT / "assets/file-icons"


def userland():
    """返回受 manifest 管理的补充 MIME 图标；摘要不符或路径越界时拒绝发布。"""
    from verify_busybox import UserlandArtifact
    manifest = json.loads((DIRECTORY / "manifest.json").read_text())
    result = []
    for relative, metadata in manifest["files"].items():
        source = (DIRECTORY / relative).resolve()
        if not source.is_relative_to(DIRECTORY.resolve()) or hashlib.sha256(source.read_bytes()).hexdigest() != metadata["sha256"]:
            raise RuntimeError(f"invalid file type icon: {relative}")
        result.append(UserlandArtifact(source, f"/usr/share/icons/hicolor/{relative}"))
    return tuple(result)


def main() -> None:
    """把一次已授权生成的源图转换成产品图标；源图与 prompt 留在 .local/inputs。"""
    from PIL import Image, ImageOps
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--name", required=True)
    parser.add_argument("--prompt", type=Path, required=True)
    parser.add_argument("--crop", type=int, nargs=4, metavar=("LEFT", "TOP", "RIGHT", "BOTTOM"))
    args = parser.parse_args()
    if not args.name or any(not (character.isascii() and (character.isalnum() or character in "-_")) for character in args.name):
        parser.error("--name must be an icon theme name")
    source = args.input.resolve()
    if not source.is_relative_to((ROOT / ".local/inputs").resolve()):
        parser.error("generated source images must stay in .local/inputs")
    with Image.open(source) as raw:
        image = raw.convert("RGBA")
    if args.crop:
        image = image.crop(tuple(args.crop))
    bounds = image.getchannel("A").point(lambda alpha: 255 if alpha >= 128 else 0).getbbox()
    if bounds is None:
        parser.error("source icon is fully transparent")
    image = image.crop(bounds)
    manifest_path = DIRECTORY / "manifest.json"
    manifest = json.loads(manifest_path.read_text()) if manifest_path.exists() else {"version": 1, "icons": {}, "files": {}}
    origin = {"generator": "OpenAI image_gen", "source_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
              "prompt": args.prompt.read_text(), "crop": args.crop, "conversion": "alpha bounds >=128; centered nearest-neighbor; binary alpha"}
    manifest.setdefault("icons", {})[args.name] = origin
    for size in (16, 32, 64):
        icon = ImageOps.contain(image, (size - 2, size - 2), Image.Resampling.NEAREST)
        icon.putalpha(icon.getchannel("A").point(lambda alpha: 255 if alpha >= 128 else 0))
        canvas = Image.new("RGBA", (size, size))
        canvas.paste(icon, ((size - icon.width) // 2, (size - icon.height) // 2))
        relative = f"{size}x{size}/mimetypes/{args.name}.png"
        destination = DIRECTORY / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        canvas.save(destination)
        manifest["files"][relative] = {"sha256": hashlib.sha256(destination.read_bytes()).hexdigest(), "icon": args.name}
    DIRECTORY.mkdir(parents=True, exist_ok=True)
    manifest_path.write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
