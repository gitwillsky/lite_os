#!/bin/sh
# 使用正式 taskmgr 客户端验证 adm 组内的非 root 桌面能读取原生服务，且身份/错误值不被伪造。
set -eu
exec 1>&2
# 端点只授予 adm 组：属主 root、组 adm、0660 的 socket；组外进程由 VFS 写权限检查拒绝。
endpoint=$(/bin/stat -c '%F %U:%G %a' /dev/task-observer)
if [ "$endpoint" != "socket root:adm 660" ]; then
    echo "task observer: unexpected endpoint permissions: $endpoint"
    exit 1
fi
snapshot=$(/bin/mktemp)
trap '/bin/rm -f "$snapshot"' EXIT
/bin/taskmgr --snapshot > "$snapshot"
if ! /bin/awk -F '\t' '
BEGIN {
    expected["audio-core"]=expected["component-manager"]=expected["driver-manager"]=1;
    expected["fshost"]=expected["netstack"]=expected["dhcp-client"]=expected["netcfg"]=expected["linuxd"]=1;
    # 原生数据卷与 Linux 根卷各有一个 ext4 实例。
    expected["ext4"]=2;
}
$1=="系统服务" {services=1; next}
$1=="进程" {services=0; next}
services {
    if (NF!=7 || $2!~/^[1-9][0-9]*$/ || $3!="运行中" || $6=="—" || $7!~/^[1-9][0-9]*$/) bad=1;
    if (identities[$2]++) bad=1;
    if ($1 ~ /^(virtio-(net|block|console|rng|gpu|input|sound)|xHCI)@[0-9a-f]+$/) {
        split($1, host, "@");
        if (nodes[host[2]]++) bad=1;
        drivers[host[1]]++;
    } else if (!($1 in expected)) bad=1;
    names[$1]++;
}
END {
    for (name in expected) if (names[name]!=expected[name]) bad=1;
    if (bad || !drivers["virtio-gpu"] || drivers["virtio-input"] < 2 || drivers["virtio-block"] < 2) exit 1;
}' "$snapshot"; then
    echo "task observer: unexpected service snapshot"
    /bin/cat "$snapshot"
    exit 1
fi
echo TASK_OBSERVER_READONLY_OK
