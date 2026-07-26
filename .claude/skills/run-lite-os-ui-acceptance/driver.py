#!/usr/bin/env python3
"""通过代码对 LiteOS 桌面 GUI 做 UI 验收：冷启动合成器栈，用 QMP 注入真实
virtio-input（指针移动/点击/双击/按键），并 screendump 抓帧存为 PNG 供肉眼核对。

无头环境下这是"看到"渲染的唯一途径：`make run-gui` 需要真实显示器（cocoa），
而本工具走 QMP `screendump`，在容器里也能产出截图。

复用 scripts/qemu_gate.py 的启动、QMP 客户端与 boot-marker 等待逻辑，不引入新的
QEMU 命令行或协议。坐标以 1504x846 逻辑视口的比例表达（QMP abs 映射到 0..0x7FFF），
与 scripts/qemu_gate.py 的 frame-timing gate 一致。

用法：
    python3 .claude/skills/run-lite-os-ui-acceptance/driver.py [--out DIR] [--open ICON]

    --out   截图输出目录（默认 /tmp/liteos-ui）。每一步产出 <name>.png。
    --open  桌面第一屏要双击打开的应用（默认 file-manager）。可选：
            my-computer | file-manager | terminal。

    默认脚本演示 File Manager 的验收流：Icons 视图 → 单击选中 → 切 Details 视图；
    my-computer 的验收流：打开我的电脑 → 单击 本地磁盘 (C:) 看选中态+详细信息联动。
    要验收别的界面，照抄 main() 里的 shot()/click()/double_click() 序列改坐标即可。

前置：已 `make build`（内核+bootloader）并 `make sync-userland ARCH=aarch64`
（把最新 ui/dist 同步进 fs-aarch64.img——注意这不是 target/rootfs 只读基线）。
"""

from __future__ import annotations

import argparse
import os
import select
import subprocess
import sys
import threading
import time
from pathlib import Path

# scripts/ 在 sys.path 上，以复用 qemu_gate 的公共设施。
ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "scripts"))

from qemu_gate import ANSI, QmpClient, _qemu_command, terminate  # noqa: E402

# make sync-userland 写入新 ui/dist 的开发实例镜像。target/rootfs/<arch>.img 是
# 未同步的只读基线——用它会看到旧版应用，这是最常见的"改了没生效"陷阱。
IMAGE = ROOT / "fs-aarch64.img"

# 桌面栈完全就绪的确定信号（与 scripts/qemu_gate.py 的 frame-timing gate 同源）。
BOOT_MARKERS = (
    "compositor: mode",
    "compositor: desktop connected",
    "compositor: desktop first scene presented",
)

# 桌面第一屏图标为竖排：My Computer(my-computer) 在最上，Terminal 次之，
# My Documents(file-manager) 第四。坐标为 1504x846 逻辑视口比例；
# 同 frame-timing gate 用的双击点。
DESKTOP_ICONS = {
    "my-computer": (47 / 1504, 34 / 846),
    "terminal": (47 / 1504, 92 / 846),
    "file-manager": (47 / 1504, 208 / 846),
}
# 应用窗口就绪 marker。
APP_READY = {
    "my-computer": "lite-ui: app my-computer ready",
    "file-manager": "lite-ui: app file-manager ready",
    "terminal": "lite-ui: app terminal ready",
}


def main() -> int:
    parser = argparse.ArgumentParser(description="LiteOS GUI screenshot/acceptance driver")
    parser.add_argument("--out", default="/tmp/liteos-ui", help="截图输出目录")
    parser.add_argument("--open", default="file-manager", choices=sorted(DESKTOP_ICONS))
    args = parser.parse_args()

    out_dir = Path(args.out)
    out_dir.mkdir(parents=True, exist_ok=True)
    if not IMAGE.is_file():
        print(f"missing {IMAGE}; run `make sync-userland ARCH=aarch64` first", file=sys.stderr)
        return 2

    import tempfile

    tmp = tempfile.TemporaryDirectory(prefix="liteos-ui-")
    qmp_socket = Path(tmp.name) / "qmp.sock"
    # 引导开发实例镜像。`-snapshot` 让所有写入落临时 overlay：原镜像以只读打开，
    # 既不申请写锁（与并行 `make run-gui` / 开发实例共存），也省去 8GB 整拷。
    command = _qemu_command(IMAGE, 1, interactive_devices=True, qmp_socket=qmp_socket)
    command.append("-snapshot")
    process = subprocess.Popen(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    assert process.stdout is not None
    output = bytearray()
    lock = threading.Lock()
    stop = threading.Event()

    def reader() -> None:
        while not stop.is_set():
            ready, _, _ = select.select([process.stdout], [], [], 0.1)
            if not ready:
                if process.poll() is not None:
                    return
                continue
            chunk = os.read(process.stdout.fileno(), 16 * 1024)
            if not chunk:
                return
            with lock:
                output.extend(chunk)

    def text() -> str:
        with lock:
            return ANSI.sub("", bytes(output).decode(errors="replace"))

    def wait(markers: tuple[str, ...], phase: str, budget_s: float) -> None:
        deadline = time.monotonic() + budget_s
        while time.monotonic() < deadline:
            current = text()
            if "panicked at" in current or "[ERROR]" in current:
                tail = "\n".join(current.splitlines()[-30:])
                raise RuntimeError(f"guest fatal during {phase}\n--- tail ---\n{tail}")
            if all(m in current for m in markers):
                return
            if process.poll() is not None:
                raise RuntimeError(f"QEMU exited during {phase}")
            time.sleep(0.1)
        missing = [m for m in markers if m not in text()]
        tail = "\n".join(text().splitlines()[-30:])
        raise RuntimeError(f"timed out during {phase}; missing={missing!r}\n--- tail ---\n{tail}")

    thread = threading.Thread(target=reader, daemon=True)
    thread.start()
    qmp: QmpClient | None = None
    try:
        wait(BOOT_MARKERS, "desktop boot", 120.0)
        qmp = QmpClient(qmp_socket)

        def shot(name: str) -> None:
            ppm = Path(tmp.name) / f"{name}.ppm"
            ppm.unlink(missing_ok=True)
            qmp._execute("screendump", {"filename": str(ppm)})
            for _ in range(50):
                if ppm.exists() and ppm.stat().st_size > 0:
                    break
                time.sleep(0.1)
            _ppm_to_png(ppm, out_dir / f"{name}.png")
            print(f"shot {name}: {out_dir / f'{name}.png'}")

        def move(xf: float, yf: float) -> None:
            qmp.move_abs(xf, yf)

        def click(xf: float, yf: float) -> None:
            move(xf, yf)
            qmp.button("left", True)
            qmp.button("left", False)
            time.sleep(0.5)

        def double_click(xf: float, yf: float) -> None:
            move(xf, yf)
            for _ in range(2):
                qmp.button("left", True)
                qmp.button("left", False)
                time.sleep(0.08)

        # 1. 双击桌面图标打开目标应用，等其窗口就绪并稳定几帧。
        double_click(*DESKTOP_ICONS[args.open])
        wait((APP_READY[args.open],), f"{args.open} launch", 20.0)
        time.sleep(2.0)
        shot("opened")

        # 2. ——验收脚本——（下面是各应用的示例流；改这里即可验收别的界面）
        if args.open == "my-computer":
            # 单击主区域的 本地磁盘 (C:) 图标：任务窗格右侧、硬盘分组标题下方。
            click(430 / 1504, 280 / 846)
            shot("selected")
            # 双击 C: 在同一窗口进入 "/"（XP 默认同窗口打开）；地址栏变为 /。
            double_click(430 / 1504, 280 / 846)
            time.sleep(1.0)
            shot("entered")
            # 进入文件夹后图标区无分组标题，首列图标上移：单击第一项选中。
            click(403 / 1504, 251 / 846)
            shot("folder-selected")
        if args.open == "file-manager":
            # 单击第一个图标 (bin)：图标区在 180px 任务面板右侧，首列约 x=405 y=224。
            click(405 / 1504, 224 / 846)
            shot("selected")
            # 点工具栏 Views 按钮 (约 x=495 y=152) 切换 Icons↔Details。
            click(495 / 1504, 152 / 846)
            shot("details")

        return 0
    finally:
        stop.set()
        thread.join(timeout=2)
        if qmp is not None:
            qmp.close()
        terminate(process)
        tmp.cleanup()


def _ppm_to_png(ppm: Path, png: Path) -> None:
    """把 QMP screendump 的 PPM 转成 PNG。优先 Pillow，无则退回 ImageMagick/pnmtopng。"""
    try:
        from PIL import Image

        Image.open(ppm).convert("RGB").save(png)
        return
    except Exception:
        pass
    for tool in (["magick", str(ppm), str(png)], ["convert", str(ppm), str(png)]):
        try:
            subprocess.run(tool, check=True, capture_output=True)
            return
        except Exception:
            continue
    # 兜底：保留 PPM，让调用者自行转换。
    fallback = png.with_suffix(".ppm")
    if ppm.exists():
        fallback.write_bytes(ppm.read_bytes())
    print(f"no PNG converter; wrote {fallback}", file=sys.stderr)


if __name__ == "__main__":
    raise SystemExit(main())
