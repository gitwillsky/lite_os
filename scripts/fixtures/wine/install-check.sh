#!/bin/sh
# 仅用于私有运行时盘：产品安装、Wine 生成的标准入口与保存内容均从真实文件回读。
set -eu
trap '[ "$?" = 0 ] || echo WIN32_CHECK_FAILED' EXIT
export HOME=/root
export XDG_RUNTIME_DIR=/run/user/0
while [ ! -S /run/liteos/wayland-0 ]; do sleep 1; done
if [ -f /var/tmp/win32-installed ]; then
    [ "$(cat /var/tmp/win32-note.txt)" = 'win32 saved 42' ]
    echo WIN32_COLD_BOOT_OK
else
    printf 'seed\r\n' > /var/tmp/win32-note.txt
    /bin/wine cscript //nologo Z:\\var\\tmp\\install-notepad.vbs
fi
# ShellLink 的异步导出以两个实际入口文件为完成边界；Wine 常驻服务退出不是安装完成条件。
while [ ! -f "$HOME/Desktop/Win32 Notepad.desktop" ] || [ ! -f "$HOME/.local/share/applications/wine/Programs/Win32 Notepad.desktop" ]; do sleep 1; done
find "$HOME/.local/share/applications" -name '*.desktop'
cat "$HOME/Desktop/Win32 Notepad.desktop"
icon=$(sed -n 's/^Icon=//p' "$HOME/Desktop/Win32 Notepad.desktop")
test -n "$icon"
test -s "$HOME/.local/share/icons/hicolor/32x32/apps/$icon.png"
echo WIN32_ICON_READY
echo WIN32_SHORTCUT_READY
while ! pgrep -f '[n]otepad.exe' >/dev/null; do sleep 1; done
echo WIN32_PROCESS_STARTED
# 先观察磁盘出现实际保存结果，再由 host 关闭窗口；不能让 WM_CLOSE 抢在异步键盘输入前结束应用。
while pgrep -f '[n]otepad.exe' >/dev/null; do
    if [ "$(cat /var/tmp/win32-note.txt)" = 'win32 saved 42' ]; then
        echo WIN32_SAVE_VISIBLE
        break
    fi
    sleep 1
done
while pgrep -f '[n]otepad.exe' >/dev/null; do sleep 1; done
cat /var/tmp/win32-note.txt
if [ "$(cat /var/tmp/win32-note.txt)" = 'win32 saved 42' ]; then
    echo WIN32_EDIT_SAVE_OK
else
    echo 'win32-check: error: saved contents mismatch'
    exit 1
fi
touch /var/tmp/win32-installed
sync
poweroff
