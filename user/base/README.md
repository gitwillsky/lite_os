# user/base — 系统文件与配置

这里不是程序或库，而是安装到 rootfs 的**数据与脚本**：BusyBox 配置、身份文件（passwd/group/shadow）、init 策略
（inittab、rcS、display-service）、桌面条目（`*.desktop`）、终端 terminfo 与动态链接器搜索路径（`ld-musl.path`）。
系统文件安装清单见 [`scripts/userland/artifacts.py`](../../scripts/userland/artifacts.py)，命令清单见 [`scripts/userland/busybox.py`](../../scripts/userland/busybox.py)。

## 命令行网络诊断

- `ping -c 3 192.0.2.1` 检查目标能否应答，并显示丢包率和往返时间。
- `ping -6 -c 3 2001:db8::1` 或 `ping6 -c 3 2001:db8::1` 强制使用 IPv6 探测。
- `traceroute -4 -n 192.0.2.1` 或 `traceroute -6 -n 2001:db8::1` 用 UDP 逐跳探测，显示返回应答的路由器和往返时间。`-n` 跳过名称解析。`*` 表示该次探测在等待时间内没有收到应答。

将示例地址换成实际目的地。两条命令需要 `CAP_NET_RAW`，可在管理员终端运行。转发端的错误应答遵循 [ICMPv4](https://www.rfc-editor.org/rfc/rfc792.html) 和 [ICMPv6](https://www.rfc-editor.org/rfc/rfc4443.html#section-3.3)。

### 命令行抓包

- `tcpdump -p -ni eth0` 实时查看接口收发的报文。将 `eth0` 换成要观察的接口，可在命令末尾加 `udp port 53` 等筛选条件。
- `tcpdump -p -ni eth0 -w capture.pcap` 保存到文件。按 Ctrl+C 停止并查看统计，用 `tcpdump -nr capture.pcap` 或 Wireshark 打开文件。

实时抓包需要 `CAP_NET_RAW`。`-p` 让程序只观察系统实际收发的帧。筛选由 libpcap 在读取时执行，抓包统计涵盖接口送进缓冲的匹配报文，缓冲丢弃数独立于网卡丢包。
