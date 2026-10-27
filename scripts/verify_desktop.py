#!/usr/bin/env python3
"""产品登录/桌面门禁：真实 shadow 认证、非 root 凭据、任务栏、锁定、改密与注销。"""
from __future__ import annotations
import argparse
from pathlib import Path
import shutil
import sys
import tempfile
import time
from build_cache import build_environment, fingerprint, manifest_matches, publish_directory, publish_runtime_gate, runtime_gate_hit, runtime_gate_payload, sha256, temporary_directory, write_manifest
from build_target import acceleration_from_environment, target_from_environment
from display_gate import display_session
from ext4_image import run_debugfs
from classic_assets import shell_bitmaps
from verify_musl import cached_musl_paths, find_compiler, run
from verify_wayland import install_files, pointer_to, button, tap, wait_screen, DESKTOP_BACKGROUND
ROOT = Path(__file__).resolve().parents[1]
RECIPE_VERSION = 1
# 默认 LargeIcons 视图：窗口 (40,40)、Web 面板 200px；首个 cell 标签中心 x=286。
# y=190 在标签背景内、字形上方，检查标签本身，不再采样旧 Details 视图的图标区域。
SELECTED = (10, 36, 106)
EXPLORER_FIRST_LABEL = (286, 190)
# 登录框按 413px 横幅加两侧 3px 外框居中；边框采样不能继续使用旧的 450px 宽度。
LOGON_LEFT = (1280 - (413 + 6)) // 2
LOGON_TOP = (800 - 300) // 2
# taskbar_layout 从右侧保留 4px、60px 时钟和 24px 音量单元，取音量单元中心。
TRAY_VOLUME_X = 1280 - 4 - 60 - 12
# 面板 64×150，菜单在指针左上翻转；分别取面板左内边与菜单行右内边。
VOLUME_PANEL_SAMPLE_OPEN = ((TRAY_VOLUME_X - 32 + 4, 700), (212, 208, 200))
TRAY_CONTEXT_ITEM = (TRAY_VOLUME_X - 8, 774)
# 共享 SDK 音量窗口固定 180×390、默认居中；取底部空白，避开文字与滑块。
VOLUME_WINDOW_SAMPLE = (720, 580)
# 开始菜单"运行"行的中心：7 行 36 高、一道 10 高的分隔，贴在 28 高任务栏之上，行起点 3 像素边框。
START_MENU_RUN = 800 - 28 - (7 * 36 + 10 + 6) + 3 + 3 * 36 + 18
HASH = '$6$liteosgate$MPmkcnoQup38yUQqVGwvFD/ys0EdwXn6gehqNbbruY47MWei35CocL0OE23dNvHpj4di0WX/acm6/cp0cgsAp.'


def build_volume_probe(musl) -> Path:
    """使用固定 Linux headers 的独立 C observer，避免 UI 与验证器共享 Rust control codec。"""
    source = ROOT / 'scripts/fixtures/desktop/volume-check.c'
    work = ROOT / 'target/desktop-runtime/probes'
    payload = {'kind':'volume-check', 'source':sha256(source), 'sysroot':musl.sysroot_fingerprint,
               'driver':sha256(ROOT / 'scripts/musl_clang.py')}
    destination = work / fingerprint(payload)
    if manifest_matches(destination, payload, ('volume-check',)):
        return destination / 'volume-check'
    generation = temporary_directory(work, 'build')
    environment = build_environment()
    environment.update({'LITEOS_MUSL_CLANG':str(musl.compiler), 'LITEOS_MUSL_LLD':str(musl.linker),
                        'LITEOS_MUSL_COMPILER_RUNTIME':str(musl.compiler_runtime), 'LITEOS_MUSL_SYSROOT':str(musl.install)})
    try:
        run([sys.executable, str(ROOT/'scripts/musl_clang.py'), str(source), '-std=gnu11', '-Wall', '-Wextra', '-Werror', '-fPIE', '-pie', '-o', str(generation/'volume-check')], ROOT, environment)
        write_manifest(generation, payload)
        publish_directory(generation, destination)
    finally:
        shutil.rmtree(generation, ignore_errors=True)
    return destination/'volume-check'


def icon_sample(name: str, origin: tuple[int, int], background: tuple[int, int, int]) -> tuple[tuple[int, int], tuple[int, int, int]]:
    # 直接读取标准 BMP 的一个不透明像素，采样受版本管理的原版图标，并独立验证桌面没有被登录背景遮住。
    path = next(path for path in shell_bitmaps() if path.name == name)
    data = path.read_bytes()
    offset = int.from_bytes(data[10:14], 'little')
    for index in range(32 * 32):
        blue, green, red, alpha = data[offset + index * 4:offset + index * 4 + 4]
        if alpha == 255 and (red, green, blue) != background:
            return ((origin[0] + index % 32, origin[1] + index // 32), (red, green, blue))
    raise RuntimeError(f'{name} has no opaque sample distinct from its background')


def click(session, x: int, y: int) -> None:
    pointer_to(session, x, y)
    button(session, True)
    button(session, False)


def right_click(session, x: int, y: int) -> None:
    pointer_to(session, x, y)
    button(session, True, 'right')
    button(session, False, 'right')


def type_text(session, text: str) -> None:
    for char in text:
        if char.islower() or char.isdigit(): tap(session, char)
        elif char == ' ': tap(session, 'spc')
        elif char == '/': tap(session, 'slash')
        else: raise RuntimeError(f'unsupported gate character: {char!r}')


def install(image: Path, directory: Path, volume_probe: Path) -> None:
    sources = []
    files = {
        '/etc/passwd': (ROOT / 'user/base/passwd').read_text() + 'alice:x:1000:100:Alice:/home/alice:/bin/sh\nbob:x:1001:100:Bob:/home/bob:/bin/sh\n',
        '/etc/group': (ROOT / 'user/base/group').read_text() + 'users:x:100:\nvideo:x:44:alice\n',
        # 产品默认的 root 没有口令，开机即自动登录；本门禁要走完整的密码登录，所以给 root 设同一口令。
        '/etc/shadow': (ROOT / 'user/base/shadow').read_text().replace('root::', f'root:{HASH}:') + f'alice:{HASH}:1:0:99999:7:::\nbob:{HASH}:1:0:99999:7:::\n',
        '/etc/inittab': (ROOT / 'user/base/inittab').read_text() + '\n::once:/bin/sh /var/tmp/check-desktop\n::once:/bin/volume-check\n',
        '/home/alice/readme.txt': 'Desktop session fixture\n',
    }
    for index, (destination, text) in enumerate(files.items()):
        source = directory / f'file-{index}'
        source.write_text(text)
        source.chmod(0o600 if destination == '/etc/shadow' else 0o644)
        sources.append((source, destination))
    sources.append((ROOT / 'scripts/fixtures/desktop/check-session.sh', '/var/tmp/check-desktop'))
    sources.append((volume_probe, '/bin/volume-check'))
    install_files(image, sources, directory)
    # debugfs 新建目录/文件默认归 root；私有 fixture 的 home 必须符合实际 alice 账户的所有权。
    # 否则非 root 的删除/回收站测试只会得到 EACCES，根本没有验证文件操作。
    for path in ("/home/alice", "/home/alice/readme.txt"):
        run_debugfs(image, f"set_inode_field {path} uid 1000", writable=True)
        run_debugfs(image, f"set_inode_field {path} gid 100", writable=True)


def login(session) -> None:
    console = session.console
    print('desktop gate: welcome and authentication', flush=True)
    console.wait_for(('logon: welcome',))
    wait_screen(session, [((0, 0), DESKTOP_BACKGROUND), ((LOGON_LEFT + 1, LOGON_TOP + 1), (255, 255, 255))], 'welcome')
    tap(session, 'ctrl', 'alt', 'delete')
    console.wait_for(('logon: secure attention Credentials',))
    click(session, 600, 395)
    tap(session, 'ctrl', 'a')
    type_text(session, 'alice')
    tap(session, 'tab')
    type_text(session, 'wrong')
    mark = console.mark()
    tap(session, 'ret')
    console.wait_for(('logon: authentication rejected',), after=mark)
    tap(session, 'ret')
    time.sleep(1.1)
    type_text(session, 'desktop42')
    tap(session, 'ret')
    console.wait_for(('logon: authenticated uid=1000', 'logon: desktop ready', 'DESKTOP_IDENTITY_OK'))
    wait_screen(session, [icon_sample("my-documents-32.bmp", (6 + (75 - 32) // 2, 12 + 4), DESKTOP_BACKGROUND), ((1200, 400), DESKTOP_BACKGROUND), ((1000, 780), (212, 208, 200))], 'desktop first frame')


def wait_security(session) -> None:
    # SAS 的进程日志早于 layer commit/scanout；屏障期间输入会被正确吞掉，必须等真正的安全页再点击。
    wait_screen(session, [((200, 200), DESKTOP_BACKGROUND), ((LOGON_LEFT + 16, LOGON_TOP + 173), (255, 255, 255))], 'secure page presented')


def check_session(session) -> None:
    console = session.console
    print('desktop gate: identity checked; testing applications and taskbar', flush=True)
    # 图标启动实际资源管理器（主目录显示为"我的文档"）；任务按钮最小化、激活同一窗口。
    click(session, 42, 43)
    click(session, 42, 43)
    console.wait_for(('compositor: window "我的文档"',))
    wait_screen(session, [((200, 200), (255, 255, 255)), (EXPLORER_FIRST_LABEL, (255, 255, 255))], 'file explorer before keyboard selection')
    # 首个图标的标签选中底色只出现在键盘事件送达之后。
    tap(session, 'down')
    wait_screen(session, [(EXPLORER_FIRST_LABEL, SELECTED)], 'new window keyboard focus')
    # 删除先确认；确认后条目离开列表（真实移入回收站），首行底色消失。
    tap(session, 'delete')
    wait_screen(session, [((360, 300), (212, 208, 200))], 'delete confirmation')
    # 共享 MessageBox 的首项是 DEFAULT；用 Enter 验证默认按钮激活，而非旧私有对话框的裸 Y 绑定。
    tap(session, 'ret')
    wait_screen(session, [(EXPLORER_FIRST_LABEL, (255, 255, 255))], 'deleted item leaves the list')
    click(session, 190, 786)
    wait_screen(session, [((200, 200), DESKTOP_BACKGROUND)], 'taskbar minimize')
    click(session, 190, 786)
    wait_screen(session, [((200, 200), (255, 255, 255))], 'taskbar restore')
    # 原目录删除后已空；返回上级并选中首项，只有导航和选择都成功才会出现标签高亮。
    tap(session, 'backspace')
    # 我的文档的 shell parent 是桌面；等回收站图标真正 commit，不能把 Down 发送给尚未替换的空列表。
    wait_screen(session, [icon_sample("recycle-bin-empty-32.bmp", (270, 155), (255, 255, 255))], 'shell parent navigation committed')
    tap(session, 'down')
    wait_screen(session, [(EXPLORER_FIRST_LABEL, SELECTED)], 'taskbar activation navigation and keyboard focus')
    # 非 root 没有原始音频设备权限：单击托盘喇叭弹出面板（不是窗口），Esc 关闭；右键菜单才打开完整音量窗口。
    click(session, TRAY_VOLUME_X, 786)
    wait_screen(session, [VOLUME_PANEL_SAMPLE_OPEN], 'restricted volume popup')
    tap(session, 'esc')
    wait_screen(session, [(VOLUME_PANEL_SAMPLE_OPEN[0], DESKTOP_BACKGROUND)], 'volume popup closes')
    right_click(session, TRAY_VOLUME_X, 786)
    wait_screen(session, [(TRAY_CONTEXT_ITEM, (212, 208, 200))], 'tray context menu')
    click(session, *TRAY_CONTEXT_ITEM)
    console.wait_for(('compositor: window "音量控制"',))
    wait_screen(session, [(VOLUME_WINDOW_SAMPLE, (212,208,200))], 'restricted volume window')
    tap(session, 'esc')
    wait_screen(session, [(VOLUME_WINDOW_SAMPLE, DESKTOP_BACKGROUND)], 'restricted volume close')
    # 实际 Run 对话框启动后台进程；随后注销必须终止它。
    click(session, 25, 786)
    wait_screen(session, [((180, 620), (212, 208, 200))], 'start menu mapped')
    # 开始菜单自上而下：程序、文档、设置、运行；运行行 36 高，菜单贴在任务栏上方。
    click(session, 110, START_MENU_RUN)
    console.wait_for(('compositor: layer "org.liteos.run" Top',))
    wait_screen(session, [((100, 650), (255, 255, 255)), ((200, 700), (212, 208, 200))], 'run dialog presented')
    type_text(session, '/bin/sleep 9999')
    tap(session, 'ret')
    console.wait_for(('DESKTOP_CHILD_RUNNING',))
    mark = console.mark()
    tap(session, 'ctrl', 'alt', 'delete')
    console.wait_for(('logon: secure attention Security',), after=mark)
    wait_security(session)
    # 安全界面遮住应用，锁定后只有 SAS 能进入密码页。
    wait_screen(session, [((200, 200), DESKTOP_BACKGROUND)], 'secure desktop')
    click(session, 480, 434)
    console.wait_for(('logon: locked',))
    tap(session, 'ctrl', 'alt', 'delete')
    console.wait_for(('logon: secure attention Unlock',))
    type_text(session, 'desktop42')
    tap(session, 'ret')
    console.wait_for(('logon: unlocked',))
    wait_screen(session, [((200, 200), (255, 255, 255))], 'unlock preserves windows')
    print('desktop gate: lock/unlock checked; testing password change', flush=True)
    mark = console.mark()
    tap(session, 'ctrl', 'alt', 'delete')
    console.wait_for(('logon: secure attention Security',), after=mark)
    wait_security(session)
    click(session, 480, 468)
    type_text(session, 'desktop42')
    tap(session, 'tab')
    type_text(session, 'desktop43')
    tap(session, 'tab')
    type_text(session, 'desktop43')
    mark = console.mark()
    tap(session, 'ret')
    console.wait_for(('logon: password changed', 'logon: page Message'), after=mark)
    mark = console.mark()
    tap(session, 'ret')
    console.wait_for(('logon: page Security',), after=mark)
    wait_security(session)
    mark = console.mark()
    click(session, 480, 434)
    console.wait_for(('logon: locked',), after=mark)
    tap(session, 'ctrl', 'alt', 'delete')
    console.wait_for(('logon: secure attention Unlock',), after=mark)
    type_text(session, 'desktop42')
    tap(session, 'ret')
    console.wait_for(('logon: authentication rejected',), after=mark)
    tap(session, 'ret')
    time.sleep(1.1)
    type_text(session, 'desktop43')
    tap(session, 'ret')
    console.wait_for(('logon: unlocked',), after=mark)
    # 另一个普通账户不能解锁；UID 0 管理员解锁保持原 alice 桌面及进程身份。
    mark = console.mark()
    tap(session, 'ctrl', 'alt', 'delete')
    console.wait_for(('logon: secure attention Security',), after=mark)
    wait_security(session)
    click(session, 480, 434)
    console.wait_for(('logon: locked',), after=mark)
    tap(session, 'ctrl', 'alt', 'delete')
    console.wait_for(('logon: secure attention Unlock',), after=mark)
    click(session, 600, 395)
    tap(session, 'ctrl', 'a')
    type_text(session, 'bob')
    tap(session, 'tab')
    type_text(session, 'desktop42')
    tap(session, 'ret')
    console.wait_for(('logon: authentication rejected',), after=mark)
    tap(session, 'ret')
    time.sleep(1.1)
    click(session, 600, 395)
    tap(session, 'ctrl', 'a')
    type_text(session, 'root')
    tap(session, 'tab')
    type_text(session, 'desktop42')
    tap(session, 'ret')
    console.wait_for(('logon: unlocked',), after=mark)
    wait_screen(session, [((200, 200), (255,255,255))], 'administrator unlock preserves alice session')
    mark = console.mark()
    tap(session, 'ctrl', 'alt', 'delete')
    console.wait_for(('logon: secure attention Security',), after=mark)
    wait_security(session)
    click(session, 625, 434)
    console.wait_for(('logon: logged out', 'DESKTOP_CLEANUP_OK'), after=mark)
    wait_screen(session, [((200, 200), DESKTOP_BACKGROUND), ((1279, 790), DESKTOP_BACKGROUND)], 'logout clears desktop')


def check_volume_popup_drag(session) -> None:
    """真实托盘滑块：边缘抓取不跳位，连续 motion 与移出控件保持隐式抓取。"""
    click(session, TRAY_VOLUME_X, 786)
    wait_screen(session, [VOLUME_PANEL_SAMPLE_OPEN], 'root volume popup')
    width, _, pixels = session.screendump()
    xs = [x for x in range(1100, 1279) if pixels[700 * width + x] != DESKTOP_BACKGROUND]
    left, right = min(xs), max(xs)
    top = min(y for y in range(500, 772) if pixels[y * width + left] != DESKTOP_BACKGROUND)
    bar_x = left + (right - left + 1 - 40) // 2

    def thumb_top() -> int:
        width, _, pixels = session.screendump()
        return min(y for y in range(top + 22, top + 118)
                   if pixels[y * width + bar_x] == (255, 255, 255))

    def wait_thumb(expected: int) -> None:
        deadline = time.monotonic() + 10
        while thumb_top() != expected:
            if time.monotonic() >= deadline:
                raise RuntimeError(f'volume thumb did not reach {expected}')
            time.sleep(0.02)

    before = thumb_top()
    pointer_to(session, bar_x + 10, before + 9)
    button(session, True)
    for delta in range(1, 21):
        pointer_to(session, bar_x + 10, before + 9 + delta)
    wait_thumb(before + 20)
    # 按住移出滑块矩形，再回到原来的轴坐标；grab offset 必须保持，不能转成绝对定位。
    pointer_to(session, bar_x - 30, before + 9)
    wait_thumb(before)
    button(session, False)
    tap(session, 'esc')
    wait_screen(session, [(VOLUME_PANEL_SAMPLE_OPEN[0], DESKTOP_BACKGROUND)], 'root popup closes')


def check_volume(session) -> None:
    console = session.console
    print('desktop gate: root audio control UI and ALSA readback', flush=True)
    mark = console.mark()
    tap(session, 'ctrl', 'alt', 'delete')
    console.wait_for(('logon: secure attention Credentials',), after=mark)
    click(session, 600, 395)
    tap(session, 'ctrl', 'a')
    type_text(session, 'root')
    tap(session, 'tab')
    type_text(session, 'wrong')
    tap(session, 'ret')
    console.wait_for(('logon: authentication rejected',), after=mark)
    tap(session, 'ret')
    time.sleep(1.1)
    type_text(session, 'desktop42')
    tap(session, 'ret')
    console.wait_for(('logon: authenticated uid=0', 'logon: desktop ready'), after=mark)
    console.wait_for(('VOLUME_CONTROL_OK',))
    wait_screen(session, [((1000, 780), (212, 208, 200)), ((1200, 400), DESKTOP_BACKGROUND)], 'root desktop first frame')
    check_volume_popup_drag(session)
    right_click(session, TRAY_VOLUME_X, 786)
    wait_screen(session, [(TRAY_CONTEXT_ITEM, (212, 208, 200))], 'root tray context menu')
    click(session, *TRAY_CONTEXT_ITEM)
    console.wait_for(('compositor: window "音量控制"',), after=mark)
    wait_screen(session, [(VOLUME_WINDOW_SAMPLE, (212, 208, 200))], 'volume window')
    mark = console.mark()
    width, _, pixels = session.screendump()
    visible = [(index % width,index // width) for index,pixel in enumerate(pixels)
               if pixel != DESKTOP_BACKGROUND and 90 <= index % width < 1200 and index // width < 740]
    left = min(point[0] for point in visible)
    top = min(point[1] for point in visible)
    right = max(point[0] for point in visible)
    bottom = max(point[1] for point in visible)
    if (right-left+1,bottom-top+1) != (180,390):
        raise RuntimeError(f'volume frame is not 180x390: {(left,top,right,bottom)}')
    # 原始旧窗口的 drag 像素已不属于 SDK 控件；聚焦实际纵向 slider，Home 后十个 5% 步进精确到 50%。
    click(session, left+90, top+200)
    tap(session, 'home')
    for _ in range(10):
        tap(session, 'down')
    console.wait_for(('VOLUME_UI_50',), after=mark)
    tap(session, 'tab')
    tap(session, 'spc')
    console.wait_for(('VOLUME_UI_MUTED',), after=mark)
    mark = console.mark()
    tap(session, 'ctrl', 'alt', 'delete')
    console.wait_for(('logon: secure attention Security',), after=mark)
    wait_security(session)
    click(session, 625, 434)
    console.wait_for(('logon: logged out',), after=mark)


def check_passwordless(image: Path) -> None:
    """产品默认 root 没有口令：开机直接进桌面，锁定后 Ctrl+Alt+Del 直接解锁，全程不出现登录对话框。"""
    print('desktop gate: passwordless account skips the logon dialog', flush=True)
    with display_session(image, sound=True) as session:
        console = session.console
        console.wait_for(('logon: welcome', 'logon: account has no password, skipping the logon dialog',
                          'logon: authenticated uid=0', 'logon: desktop ready'))
        if 'logon: secure attention Credentials' in console.text:
            raise RuntimeError('logon dialog was shown for an account without a password')
        wait_screen(session, [((1000, 780), (212, 208, 200)), ((1200, 400), DESKTOP_BACKGROUND)], 'passwordless desktop')
        mark = console.mark()
        tap(session, 'ctrl', 'alt', 'delete')
        console.wait_for(('logon: secure attention Security',), after=mark)
        wait_security(session)
        click(session, 480, 434)
        console.wait_for(('logon: locked',), after=mark)
        mark = console.mark()
        tap(session, 'ctrl', 'alt', 'delete')
        console.wait_for(('logon: secure attention Unlock', 'logon: account has no password, skipping the logon dialog',
                          'logon: unlocked'), after=mark)
        wait_screen(session, [((1000, 780), (212, 208, 200)), ((1200, 400), DESKTOP_BACKGROUND)], 'passwordless unlock')


def main() -> int:
    target = target_from_environment()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', type=Path, default=ROOT / 'target/rootfs' / f'{target.arch}.img')
    args = parser.parse_args()
    if target.arch != 'aarch64' or acceleration_from_environment() != 'hvf':
        print(f'desktop gate skipped: {target.arch} has no display path in the gate')
        return 0
    image = args.image.resolve()
    stamp = ROOT / 'target/verify-gates/desktop-aarch64.json'
    volume_probe = build_volume_probe(cached_musl_paths(find_compiler()))
    payload = runtime_gate_payload('desktop', RECIPE_VERSION, (image, volume_probe, Path(__file__).resolve(), ROOT / 'scripts/fixtures/desktop/check-session.sh', ROOT / 'scripts/display_gate.py', ROOT / 'scripts/verify_wayland.py'))
    try:
        if runtime_gate_hit(stamp, payload, (image,)):
            print('desktop verification cache hit')
            return 0
        with tempfile.TemporaryDirectory(prefix='liteos-desktop-') as temp:
            directory = Path(temp)
            private = directory / 'rootfs.img'
            shutil.copyfile(image, private)
            install(private, directory, volume_probe)
            with display_session(private, sound=True) as session:
                try:
                    login(session)
                    snapshot = ROOT / 'target/desktop-runtime'
                    snapshot.mkdir(parents=True, exist_ok=True)
                    session.qmp.execute('screendump', {'filename': str(snapshot / 'desktop.ppm')})
                    check_session(session)
                    check_volume(session)
                except (RuntimeError, OSError):
                    snapshot = ROOT / 'target/desktop-runtime'
                    snapshot.mkdir(parents=True, exist_ok=True)
                    session.qmp.execute('screendump', {'filename': str(snapshot / 'failure.ppm')})
                    (snapshot / 'console.log').write_text(session.console.text)
                    raise
        with tempfile.TemporaryDirectory(prefix='liteos-desktop-passwordless-') as temp:
            private = Path(temp) / 'rootfs.img'
            shutil.copyfile(image, private)
            check_passwordless(private)
        publish_runtime_gate(stamp, payload)
        print('desktop gate: authentication, identity, taskbar, run, SAS, lock/unlock, password change, logout, ALSA volume/mute and passwordless login passed')
    except (RuntimeError, OSError) as error:
        print(f'desktop verification failed: {error}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
