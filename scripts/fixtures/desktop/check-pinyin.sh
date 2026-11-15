#!/bin/sh
# 仅注入桌面门禁私有镜像。用真实文件保存和 PTY 字节裁决输入法上屏。
if [ "$1" = terminal ]; then
    printf 'IME ready> '
    IFS= read -r answer || exit 1
    printf '%s\n' "$answer" > /tmp/ime-pty.txt
    exit
fi
for i in $(/bin/seq 1 90); do
    if [ "$(/bin/cat /var/tmp/ime-notepad.txt 2>/dev/null)" = '你好Abc' ]; then
        echo PINYIN_NOTEPAD_OK
        break
    fi
    /bin/sleep 1
done
for i in $(/bin/seq 1 60); do
    if [ -f /tmp/ime-pty.txt ]; then
        if [ "$(/bin/cat /tmp/ime-pty.txt)" = '中文' ]; then
            echo PINYIN_PTY_OK
        else
            printf 'pinyin-check: unexpected PTY input bytes: '
            /bin/od -An -tx1 /tmp/ime-pty.txt
        fi
        break
    fi
    /bin/sleep 1
done
for i in $(/bin/seq 1 60); do
    if [ -f /var/tmp/ime-folder/测试.txt ] && [ ! -e /var/tmp/ime-folder/old.txt ]; then
        echo PINYIN_RENAME_OK
        exit
    fi
    /bin/sleep 1
done
