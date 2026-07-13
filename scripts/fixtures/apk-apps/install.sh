#!/bin/sh
set -eu

# 1. 全部 dependency 必须来自 host 已校验并注入的固定 APK 闭包，禁止运行时网络解析。
APK='/sbin/apk.static --no-network --no-progress'
$APK add /run/apk-apps/*.apk

# 2. 版本与 package database 必须同时证明三个顶层应用安装完成。
$APK info -e 'curl=8.14.1-r2'
$APK info -e 'sqlite=3.49.2-r1'
$APK info -e 'git=2.49.1-r0'

# 3. 删除传输载荷并恢复正常 init policy；最终镜像只保留 package-owned 文件。
rm -rf /run/apk-apps /run/verify-apk-install.sh
cp /run/normal.inittab /etc/inittab
rm -f /run/normal.inittab
sync
echo LITEOS_APK_APPLICATIONS_INSTALLED
while :; do sleep 1; done
