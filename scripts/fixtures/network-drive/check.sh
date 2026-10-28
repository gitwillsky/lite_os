#!/bin/sh
# 只在一次性验收镜像中以普通用户执行；连接和凭据均经过产品入口。
set -eu
[ "$(id -u)" != 0 ]
source=$1
policy=$2
credentials=$3
trap '/bin/smbfs disconnect Z >/dev/null 2>&1 || true' EXIT
/bin/smbfs map Z "$source" "$policy" save reconnect < "$credentials"
[ "$(stat -c %a "$HOME/.config/liteos/network-drives")" = 700 ]
[ "$(stat -c %a "$HOME/.config/liteos/network-drives/Z.credentials")" = 600 ]
/var/tmp/network-drive-probe "$HOME/Network/Z"
# 独立 helper 卸载保留记录，然后用保存的凭据恢复；不靠内存里的旧会话伪装登录重连。
/bin/fusermount3 -u "$HOME/Network/Z"
/bin/smbfs reconnect
[ -d "$HOME/Network/Z" ]
/bin/smbfs disconnect Z
[ ! -e "$HOME/.config/liteos/network-drives/Z.credentials" ]
[ ! -e "$HOME/.config/liteos/network-drives/Z.conf" ]
/bin/smbfs map Z "$source" "$policy" session once < "$credentials"
[ ! -e "$HOME/.config/liteos/network-drives/Z.credentials" ]
/bin/smbfs disconnect Z
trap - EXIT
echo LITEOS_SMB_42
