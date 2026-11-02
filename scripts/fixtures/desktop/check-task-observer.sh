#!/bin/sh
# 使用正式 taskmgr 客户端验证 adm 组内的非 root 桌面能读取原生服务，且身份/错误值不被伪造。
set -eu
exec 1>&2
# 端点只授予 adm 组：属主 root、组 adm、0660 的 socket；组外进程由 VFS 写权限检查拒绝。
[ "$(/bin/stat -c '%F %U:%G %a' /dev/task-observer)" = "socket root:adm 660" ]
snapshot=$(/bin/mktemp)
trap '/bin/rm -f "$snapshot"' EXIT
/bin/taskmgr --snapshot > "$snapshot"
/bin/awk -F '\t' '
$1=="系统服务" {services=1; next}
$1=="进程" {services=0; next}
services {
    if (NF!=7 || $2!~/^[1-9][0-9]*$/ || $3!="运行中" || $6=="—" || $7!~/^[1-9][0-9]*$/) bad=1;
    if (names[$1]++ || identities[$2]++) bad=1;
    count++;
}
END {
    if (bad || count!=6 || !names["root-server"] || !names["driver-host"] || !names["ext4d"] || !names["fshost"] || !names["netstack"] || !names["linuxd"]) exit 1;
}' "$snapshot"
echo TASK_OBSERVER_READONLY_OK
