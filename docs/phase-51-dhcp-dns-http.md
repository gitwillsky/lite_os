# Phase 51：DHCP、DNS 与 HTTP userspace 竖切

本阶段固定对照 Linux v7.1 packet/socket UAPI、musl v1.2.6 resolver 与 BusyBox 1.37.0 consumer，在唯一 `VirtIO-net → socket → OFD → syscall → musl/BusyBox` 路径上完成自动网络配置和真实应用流量。内核不拥有 lease、DNS cache、HTTP 状态，也没有静态配置 fallback 或第二套 userspace image。

## Owner 与分层

- `PacketRegistry` 唯一拥有 effective-root 创建的 `AF_PACKET/SOCK_DGRAM/ETH_P_IP` endpoint、binding 与 64-packet 有界 RX queue。Ethernet RX 在 smoltcp ingress 前镜像一次，SOCK_DGRAM 对 userspace 去除/补充 Ethernet header，不复制 L3 interface 或 route 状态。
- `NetworkStack` 继续唯一拥有 IPv4/UDP/TCP 与 endpoint option；`SO_REUSEADDR` 参与 bind collision，`SO_BROADCAST` 授权 limited/subnet broadcast，`SO_BINDTODEVICE` 在当前单 interface scope 验证并记录 `eth0` binding。
- RX tap 只在 NetworkStack lock 内排队；Pipe notification 延迟到解锁后，保持 protocol state 与 wait source 的固定锁序。
- `/usr/share/udhcpc/default.script` 是 BusyBox rootfs 的唯一 lease consumer，通过标准 ioctl/route 配置 `eth0`，并生成 musl resolver 唯一读取的 `/etc/resolv.conf`。
- BusyBox init 以 `respawn` 监督前台 network service；console 与 DHCP 并发启动，service 退出由 init 重启，stale pidfile 只由 service entry 清理。正常 boot 不再依赖交互 shell 手工启动 DHCP。
- BusyBox `spawn_and_wait` 通过 musl vfork 执行 lease script；vfork child 使用独立页表/trap frame、共享已驻留用户 frame，process graph 唯一挂起 parent 到 child exec/exit，不把 script 执行特判进网络层。

## Linux ABI 边界

- 支持 20-byte `sockaddr_ll`、bind/sendto/recvfrom/read/MSG_PEEK/MSG_TRUNC/poll/epoll 与 source address 回写；创建 AF_PACKET 要求 effective UID 0，当前单 user namespace 中等价于 Linux `CAP_NET_RAW` policy。
- 当前 packet protocol 精确限定 ETH_P_IP；`PACKET_AUXDATA` 返回 `ENOPROTOOPT`，BusyBox 按其标准 fallback 继续。raw IP/ICMP、ARP EtherType、SOCK_RAW、packet fanout/ring、promiscuous membership 与多 interface 尚未开放。
- DNS 与 HTTP 完全在 userspace：musl 通过 AF_INET UDP/TCP resolver 读取 resolv.conf，BusyBox wget 通过既有 TCP stream 发送 HTTP/1.x。

## 运行验收事实

- QEMU slirp 冷启动后，init 监督的 `udhcpc -f -i eth0` 经 raw DHCP discover/request 取得并持续续租；gate 只等待自动生成的 address/default route/nameserver，不执行手工 DHCP 命令。
- 固定动态 musl probe 以 `getaddrinfo(AF_INET)` 解析 `example.com` 并返回数值地址，证明 DHCP DNS 配置被真实消费；不把 BusyBox 自带 DNS engine 当作 libc resolver 证明。
- host loopback 固定 HTTP origin 由 QEMU `10.0.2.2` 暴露；guest `wget` 下载固定 payload 并从 ext2 读回，证明 TCP/HTTP application path。
