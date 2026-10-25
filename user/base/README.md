# user/base — 系统文件与配置

这里不是程序或库，而是安装到 rootfs 的**数据与脚本**：BusyBox 配置、身份文件（passwd/group/shadow）、init 策略
（inittab、rcS、display-service、network-service）、桌面条目（`*.desktop`）、终端 terminfo 与动态链接器搜索路径（`ld-musl.path`）。
安装清单见 `scripts/verify_busybox.py` 的 `base_userland`，owner 是 rootfs builder。
