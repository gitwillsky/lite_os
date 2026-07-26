#!/usr/bin/env python3
"""驱动 Codex 生成图片素材并校验/预览的 harness。

本会话实测跑通两次（XP 文件管理器图标、XP 光标）后固化的流程。分两步：

  gen      —— 用 codex exec 在目标目录生成一组图片（后台运行，返回 task id）。
             prompt 只描述"图片要求"，不指导 Codex 怎么画（Codex 自己会画）。
  check    —— 校验每张图的精确尺寸与颜色模式（RGBA），并拼一张放大预览
             （NEAREST 放大 + 棋盘背景）供肉眼检查透明边缘/形状。

用法：
  # 1. 生成（把每张图的 名字/尺寸 写清楚，风格描述清楚，透明背景/RGBA 要求写清楚）
  python3 .claude/skills/generate-assets-with-codex/driver.py gen \\
      --dir assets/my-icons-src \\
      --spec 'arrow.png=32x32' --spec 'file.png=32x32' \\
      --prompt '生成一组忠实还原 Windows XP (Luna) 风格的图标 PNG。透明背景、RGBA、
                边缘抗锯齿、明快 XP 配色与轻微高光。需要：arrow.png(32x32) 经典左上箭头；
                file.png(32x32) 白色文档带折角。每张在其像素尺寸内清晰居中留 1px 边距。'

  # 2. 生成完成后（收到后台完成通知）校验 + 预览
  python3 .claude/skills/generate-assets-with-codex/driver.py check \\
      --dir assets/my-icons-src \\
      --spec 'arrow.png=32x32' --spec 'file.png=32x32' \\
      --preview /tmp/preview.png

关键约束（本会话踩坑固化）：
  - Codex prompt 只写"图片要求"，绝不写"用 Pillow 怎么画"——它自己知道怎么生成。
  - codex exec 用 --sandbox workspace-write --cd <目标目录>，让它只写该目录。
  - 生成是后台任务：gen 立即返回，收到完成通知后再 check。
  - check 必须肉眼看预览：尺寸/模式对不代表图形对（形状、朝向、透明边缘）。
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path


def parse_specs(specs: list[str]) -> list[tuple[str, int, int]]:
    """把 'name.png=WxH' 解析成 (name, w, h)。"""
    out = []
    for spec in specs:
        name, _, dims = spec.partition("=")
        w, _, h = dims.partition("x")
        out.append((name, int(w), int(h)))
    return out


def cmd_gen(args: argparse.Namespace) -> int:
    target = Path(args.dir).resolve()
    target.mkdir(parents=True, exist_ok=True)
    # codex 自己知道如何生成图片，prompt 只描述要求。--cd 限定写入目录。
    codex = subprocess.Popen(
        [
            "codex",
            "exec",
            "--sandbox",
            "workspace-write",
            "--cd",
            str(target),
            args.prompt,
        ],
    )
    print(f"codex 生成已启动（pid={codex.pid}），目标目录 {target}")
    print("等它结束后再跑 `check`（用本工具的 gen 时通常放后台，收到完成通知后 check）。")
    codex.wait()
    return codex.returncode


def cmd_check(args: argparse.Namespace) -> int:
    try:
        from PIL import Image
    except ModuleNotFoundError:
        print("check 需要 Pillow：pip install Pillow", file=sys.stderr)
        return 2
    target = Path(args.dir).resolve()
    specs = parse_specs(args.spec)
    ok = True
    images = []
    for name, w, h in specs:
        path = target / name
        if not path.is_file():
            print(f"BAD  {name}: 缺失")
            ok = False
            continue
        im = Image.open(path).convert("RGBA") if False else Image.open(path)
        good = im.size == (w, h) and im.mode == "RGBA"
        print(f"{'OK ' if good else 'BAD'}  {name} {im.size} {im.mode} (want {w}x{h} RGBA)")
        ok = ok and good
        images.append((name, Image.open(path).convert("RGBA")))

    if args.preview and images:
        scale = args.scale
        cols = min(args.cols, len(images))
        rows = (len(images) + cols - 1) // cols
        cell_w = max(im.width for _, im in images) * scale + 16
        cell_h = max(im.height for _, im in images) * scale + 28
        sheet = Image.new("RGBA", (cols * cell_w + 8, rows * cell_h + 8), (0, 0, 0, 0))
        # 棋盘背景，透明区域可见。
        for y in range(sheet.height):
            for x in range(sheet.width):
                shade = 205 if ((x // 12 + y // 12) % 2 == 0) else 150
                sheet.putpixel((x, y), (shade, shade, shade, 255))
        from PIL import ImageDraw

        draw = ImageDraw.Draw(sheet)
        for i, (name, im) in enumerate(images):
            big = im.resize((im.width * scale, im.height * scale), Image.NEAREST)
            cx = 8 + (i % cols) * cell_w
            cy = 8 + (i // cols) * cell_h
            sheet.alpha_composite(big, (cx, cy))
            draw.text((cx + 2, cy + im.height * scale + 6), name, fill=(0, 0, 0, 255))
        Path(args.preview).parent.mkdir(parents=True, exist_ok=True)
        sheet.convert("RGB").save(args.preview)
        print(f"预览已写出：{args.preview}（用 Read 工具打开肉眼检查形状/朝向/透明边缘）")

    return 0 if ok else 1


def main() -> int:
    parser = argparse.ArgumentParser(description="Codex 图片素材生成 + 校验/预览")
    sub = parser.add_subparsers(dest="mode", required=True)

    g = sub.add_parser("gen", help="用 codex exec 生成图片")
    g.add_argument("--dir", required=True, help="目标目录（相对仓库根或绝对）")
    g.add_argument("--prompt", required=True, help="图片要求描述（只写要求，不指导实现）")
    g.add_argument("--spec", action="append", default=[], help="name.png=WxH（gen 时仅记录，可省）")
    g.set_defaults(func=cmd_gen)

    c = sub.add_parser("check", help="校验尺寸/模式并拼放大预览")
    c.add_argument("--dir", required=True)
    c.add_argument("--spec", action="append", required=True, help="name.png=WxH，可多次")
    c.add_argument("--preview", help="预览 PNG 输出路径")
    c.add_argument("--scale", type=int, default=6, help="预览放大倍数")
    c.add_argument("--cols", type=int, default=5, help="预览每行图数")
    c.set_defaults(func=cmd_check)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
