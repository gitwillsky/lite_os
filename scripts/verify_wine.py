#!/usr/bin/env python3
"""在私有 AArch64/HVF 镜像验证 Wine 安装、标准桌面入口、i386 编辑保存与冷启动。"""
from __future__ import annotations

import argparse
import shutil
import struct
import tempfile
import time
from pathlib import Path

from build_target import acceleration_from_environment, target_from_environment
from display_gate import TIMEOUT_SECONDS, display_session
from ext4_image import ensure_ext4_capacity
from sync_userland import synchronize
from verify_busybox import base_userland
from verify_desktop import click
from verify_musl import cached_musl_paths, find_compiler
from verify_wayland import install_files, key_events, tap, tap_steps
from wine_runtime import REQUIRED_CAPACITY_MIB, unpack_runtime, wine_userland

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / 'scripts/fixtures/wine'
OUTPUT = ROOT / 'target/wine-runtime/acceptance'


def press(session, *keys: str) -> None:
    """按真实时间先按下再释放；Wine 通过异步消息队列处理输入，零时长洪泛不能替代用户敲键。"""
    for key, down in tap_steps(*keys):
        session.qmp.execute('input-send-event', {'events': key_events((key, down))})
        time.sleep(0.06)


def wait_document(session) -> None:
    """等待固定私有桌面里 Notepad 的文档首帧；空白初始 buffer 和插入光标不能算文件加载完成。"""
    deadline = time.monotonic() + TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        width, _, pixels = session.screendump()
        # 固定初始窗口 (40,40)，首行 seed 有 112 个深色像素；空白客户区/单光标低于 40。
        ink = sum(max(pixels[y * width + x]) < 80 for y in range(96, 112) for x in range(48, 180))
        if ink > 40:
            click(session, 120, 105)
            return
        time.sleep(0.25)
    raise RuntimeError('Notepad did not present the loaded document')


def exercise(image: Path, cold: bool, saved: Path) -> None:
    """启动一个独立 VM，通过真实桌面快捷方式操作 i386 Notepad，关机后保留磁盘供冷启动。

    Args:
        image: 已离线安装产品与 Wine 的源镜像。
        cold: 第二次启动只打开已保存文件，不重新安装或修改内容。
        saved: guest 正常 poweroff 后的镜像保存位置。

    Raises:
        RuntimeError: 目标 phase、窗口、文件或退出条件未满足。
    """
    with display_session(image) as session:
        try:
            # Wine 11.16 run_wineboot 自身允许首次初始化等待 5 分钟；普通窗口/输入 phase 仍用显示门禁预算。
            phase = session.console.wait_for_match(r'WIN32_SHORTCUT_READY|WIN32_CHECK_FAILED', timeout_seconds=300)
            if phase.group() == 'WIN32_CHECK_FAILED':
                raise RuntimeError('Windows installation fixture failed; see console.log')
            session.console.wait_for(('logon: desktop ready', 'WIN32_ICON_READY'))
            print('Wine phase: generated desktop entry and PNG ready', flush=True)
            if cold:
                session.console.wait_for(('WIN32_COLD_BOOT_OK',))
            # 固定私有空白桌面的第五项；前四项为系统图标，快捷方式由真实安装器生成。
            time.sleep(1)
            click(session, 43, 332)
            click(session, 43, 332)
            session.console.wait_for(('WIN32_PROCESS_STARTED',))
            print('Wine phase: desktop launched installed i386 PE', flush=True)
            # compositor 在映射时记录 app_id；Notepad 稍后打开参数文件并更新标题，不能等待旧日志里的最终文件名。
            session.console.wait_for_match(r'compositor: window "[^"\n]*" app_id="notepad.exe"')
            wait_document(session)
            if not cold:
                press(session, 'ctrl', 'a')
                for key in 'win32 saved 42':
                    press(session, 'spc' if key == ' ' else key)
                press(session, 'ctrl', 's')
                phase = session.console.wait_for_match(r'WIN32_SAVE_VISIBLE|WIN32_CHECK_FAILED')
                if phase.group() == 'WIN32_CHECK_FAILED':
                    raise RuntimeError('Notepad exited before saving the expected text')
            session.qmp.execute('screendump', {'filename': str(OUTPUT / ('cold.ppm' if cold else 'notepad.ppm'))})
            tap(session, 'alt', 'f4')
            phase = session.console.wait_for_match(r'WIN32_EDIT_SAVE_OK|WIN32_CHECK_FAILED')
            if phase.group() == 'WIN32_CHECK_FAILED':
                raise RuntimeError('Notepad saved contents mismatch; see console.log')
            session.console.process.wait(timeout=20)
            if session.console.process.returncode != 0:
                raise RuntimeError(f'guest poweroff returned {session.console.process.returncode}')
            shutil.copyfile(session.directory / image.name, saved)
        except Exception:
            if session.console.process.poll() is None:
                session.qmp.execute('screendump', {'filename': str(OUTPUT / 'failure.ppm')})
            session.console.process.terminate()
            session.console.process.wait(timeout=10)
            shutil.copyfile(session.directory / image.name, OUTPUT / 'failure.img')
            raise
        finally:
            (OUTPUT / ('cold-console.log' if cold else 'console.log')).write_text(session.console.text)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', type=Path, default=ROOT / 'target/rootfs/aarch64.img')
    args = parser.parse_args()
    if target_from_environment().arch != 'aarch64' or acceleration_from_environment() != 'hvf':
        parser.error('Wine/FEX acceptance requires AArch64/HVF')
    # 不以 ARM64 原生 Wine 程序代替 FEX；检查安装样本的真实 PE Machine。
    program = (unpack_runtime() / 'lib/wine/i386-windows/notepad.exe').read_bytes()
    pe = struct.unpack_from('<I', program, 0x3c)[0]
    if program[pe:pe + 4] != b'PE\0\0' or struct.unpack_from('<H', program, pe + 4)[0] != 0x14c:
        raise RuntimeError('Notepad fixture is not an i386 PE')
    OUTPUT.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='liteos-wine-', dir=OUTPUT) as work:
        directory = Path(work)
        image = directory / 'rootfs.img'
        shutil.copyfile(args.image, image)
        ensure_ext4_capacity(image, REQUIRED_CAPACITY_MIB)
        synchronize(image, base_userland(cached_musl_paths(find_compiler())) + wine_userland())
        inittab = directory / 'inittab'
        inittab.write_text((ROOT / 'user/base/inittab').read_text() + '::once:/bin/sh /var/tmp/install-check.sh\n')
        install_files(image, [(FIXTURES / 'install-notepad.vbs', '/var/tmp/install-notepad.vbs'),
                              (FIXTURES / 'install-check.sh', '/var/tmp/install-check.sh'),
                              (inittab, '/etc/inittab')], directory)
        saved = directory / 'saved.img'
        exercise(image, False, saved)
        exercise(saved, True, directory / 'cold.img')
    print('Wine gate: i386 install, generated desktop icon/entry, launch, edit/save, exit and cold boot passed')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
