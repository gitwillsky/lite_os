# Phase 49：VirtIO-net、IPv4 与 UDP 竖切

本阶段固定对照 VirtIO 1.4 CS01、Linux v7.1 的 socket/ioctl UAPI 与 smoltcp 0.13.1，只建立一条 `VirtIO-net → NetworkDevice → NetworkStack → Socket/OFD → Linux syscall` 路径。不存在私有网络 ABI、静态 guest 地址、第二套协议状态或绕过 OFD 的 userspace 入口。

## 唯一 owner 与执行上下文

- `VirtIONetworkDevice` 唯一拥有 legacy MMIO RX/TX virtqueue、DMA buffer、MAC 与 packet/byte counter。hardirq 只确认设备中断并发布 network softirq，不解析 Ethernet frame。
- `NetworkStack` 唯一拥有 interface 地址/prefix/default route、ARP cache、UDP socket set、peer、`IP_PKTINFO` 与 ephemeral port allocation；ioctl、procfs 与 socket syscall 只消费同一 owner。
- 网络 softirq 每轮维护一次协议时钟、最多处理 64 个 RX frame，并执行一次 smoltcp 有界 egress。budget 用尽时重新投递 softirq，防止持续流量饿死 task，也避免 used ring 中无新 IRQ edge 的 frame 滞留；timer softirq 只在 `poll_at` deadline 到期时推进 ARP/UDP egress，保证协议 timer 不依赖新的外部 IRQ。
- 网络锁内只收集 readiness transition；释放后再写 notification Pipe，保持 `NetworkStack → wait source` 的单向锁序。

## Linux ABI 边界

- `AF_INET/SOCK_DGRAM` 支持 bind/connect、read/write、sendto/recvfrom、sendmsg/recvmsg、blocking/nonblocking、ppoll/epoll 与标准 sockaddr_in。
- message ABI 支持 RV64 `msghdr/iovec/cmsghdr`、`MSG_PEEK/DONTWAIT/TRUNC/NOSIGNAL` 与 IPv4 `IP_PKTINFO`；sendto/recvfrom 和 sendmsg/recvmsg 共用同一 endpoint 与等待路径。
- standard socket ioctl 配置唯一 `eth0` 的 address/netmask/up/default route，并查询 name/index/flags/broadcast/MTU/MAC；`/proc/net/dev` 与 `/proc/net/route` 投影同一配置和真实 driver counter。
- 本阶段完成时的 TCP 缺口已由 Phase 50 在同一 NetworkStack/Socket/OFD seam 上关闭；其余缺口仍为 IPv6、raw/ICMP userspace socket、DHCP/DNS、multicast、多 interface/network namespace，以及 reuse/broadcast option 的完整 policy。未开放的协议不以 smoltcp feature 或空成功分支冒充支持。

## 运行验收事实

动态 BusyBox 通过 `ifconfig eth0 10.0.2.15 netmask 255.255.255.0 up` 与 `route add default gw 10.0.2.2` 配置唯一 interface。QEMU user-net hostfwd 向 guest `nc -u -l -p 5555` 发送 datagram 后，guest 收到 host payload，host 同时收到 guest response；该链路经过标准 ioctl、`recvmsg/IP_PKTINFO`、VirtIO RX/TX 和唯一 smoltcp stack。
