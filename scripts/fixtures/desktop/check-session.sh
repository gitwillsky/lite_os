#!/bin/sh
# 仅注入私有测试盘：从 procfs 检查真实凭据与注销后的后台进程清理。
while :; do
    desktop=$(/bin/pgrep -f '^/bin/desktop$')
    [ -n "$desktop" ] && break
    /bin/sleep 1
done
/bin/cat /proc/$desktop/status
/bin/awk '/^Uid:/ {if ($2!=1000 || $3!=1000 || $4!=1000 || $5!=1000) exit 1; uid=1} /^Gid:/ {if ($2!=100 || $3!=100 || $4!=100 || $5!=100) exit 1; gid=1} /^Groups:/ {if (NF!=3 || !(($2==44 && $3==100) || ($2==100 && $3==44))) exit 1; groups=1} END {if (!uid || !gid || !groups) exit 1}' /proc/$desktop/status || { echo 'desktop-check: error: credentials'; exit 1; }
[ "$(/bin/stat -c '%u:%g:%a' /run/user/1000)" = '1000:100:700' ] || { echo 'desktop-check: error: runtime directory'; exit 1; }
echo DESKTOP_IDENTITY_OK
while :; do
    sleeper=$(/bin/pgrep -f '^/bin/sleep 9999$')
    [ -n "$sleeper" ] && break
    /bin/sleep 1
done
echo DESKTOP_CHILD_RUNNING
while /bin/kill -0 "$desktop" 2>/dev/null; do /bin/sleep 1; done
i=0
while /bin/kill -0 "$sleeper" 2>/dev/null && [ "$i" -lt 10 ]; do /bin/sleep 1; i=$((i+1)); done
if /bin/kill -0 "$sleeper" 2>/dev/null; then echo DESKTOP_CHILD_LEAKED; exit 1; fi
echo DESKTOP_CLEANUP_OK
