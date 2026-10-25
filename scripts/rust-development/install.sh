#!/bin/sh
set -eu
PATH='/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin'
export PATH

# 安装失败仍恢复原 init policy；APK 数据库与文件事务由 apk 自身负责。
restore_init() {
    cp /var/tmp/liteos-rust/normal.inittab /etc/inittab
    sync
}
trap restore_init EXIT HUP INT TERM

/sbin/apk.static --no-network --no-progress add /var/tmp/liteos-rust/apks/*.apk &
install_pid=$!
while kill -0 "$install_pid" 2>/dev/null; do
    echo LITEOS_RUST_PACKAGES_INSTALLING
    sleep 5
done
wait "$install_pid"
mkdir -p /usr/share/liteos
cp /var/tmp/liteos-rust/stamp.json /usr/share/liteos/rust-development.json
restore_init
trap - EXIT HUP INT TERM
rm -rf /var/tmp/liteos-rust
sync
echo LITEOS_RUST_PACKAGES_INSTALLED
while :; do sleep 1; done
