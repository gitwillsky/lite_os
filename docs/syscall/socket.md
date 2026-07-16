# LiteOS Socket ABI contract

> 权威入口：[syscall-support.md](../syscall-support.md)
>
> 本文承载 syscall 198–212、242 的完整 domain/protocol scope；编号、总体状态与实现位置仍由入口矩阵索引。

## 1. 通用 message 与 readiness

- `sendmsg/recvmsg` 与 `readv/writev` 共用唯一 page-batched raw iovec importer。scalar/vector transfer 按 Linux `MAX_RW_COUNT` 截断有效 prefix；stream vector total 可超过 65,535 bytes，以固定 64 KiB reusable staging 返回真实 short/partial count，不按 request total 分配。首次 copy fault 不提交 staged prefix；`EPIPE` 只在零 progress 且未设置 `MSG_NOSIGNAL` 时投递 `SIGPIPE`。
- 短 `recvmsg/recvfrom` 只复制 buffer capacity 并消费整条 datagram，输出 `MSG_TRUNC` 与输入 `MSG_TRUNC` 的原始长度返回语义精确保留。超大 receive capacity 只按 protocol 最大有用值分配 staging，不作为 oversized message 拒绝。
- 每个 socket/listener 的内部 notification Pipe 只承载 edge，最终 readiness 始终复查 backend level state；blocking wait、nonblocking `EAGAIN`、pselect/ppoll/epoll 共用同一 generation/wait seam。

## 2. AF_UNIX

- abstract stream/datagram 保持当前范围；stream transport 使用 64 KiB data Pipe，write 在任意发送容量可用时允许非零短写，并以统一 Pipe readiness 完成 backpressure、EOF 与 `SIGPIPE`。
- AF_UNIX datagram receive queue 固定最多十条、单条最多 65,535 bytes；atomic limit 在完整 payload gather 前验证。nonblocking full send 返回 `EAGAIN`，blocking send 直接等待目标容量，connected `poll/epoll(POLLOUT)` 投影 live peer capacity。
- connected stream/datagram 与 socketpair 支持 `getsockopt(SOL_SOCKET, SO_PEERCRED)`；AF_UNIX endpoint 捕获 immutable effective identity，stream server 端在 connect transaction 捕获 caller 的实时 effective UID/GID，syscall 只编码 12-byte `struct ucred`。
- pathname bind 创建真实 ext2 socket inode并遵循目录 mutation/permission/umask；runtime namespace 以 filesystem+inode identity 解析 live endpoint，因此 rename/hardlink 保持连接 identity，unlink 后既有 endpoint 继续存活而旧 pathname 立即不可达，unlink/recreate 不会命中旧 socket。关闭 endpoint 后 socket inode继续存在，connect 稳定返回 `ECONNREFUSED`，直到显式 unlink。
- stream 与 datagram 支持 `SCM_RIGHTS`；sendmsg 在 transport byte/message commit 时原子附着 control batch，recvmsg 按 Linux `receive_fd()` 顺序逐个预留不可见 fd、copyout fd number、再发布。当前 fd copyout 失败只取消当前 reservation，保留已发布前缀并以 `MSG_CTRUNC` 丢弃后缀；`MSG_CMSG_CLOEXEC` 在发布前设置 descriptor flag。
- SCM inflight 按发送者 real UID 计数；超过发送者 `RLIMIT_NOFILE` 时同步执行无分配 cycle-safe AF_UNIX graph collection，回收后仍超限返回 `ETOOMANYREFS`。socket-buffer sockopts 尚未开放。

## 3. AF_INET 与 AF_PACKET

- UDP 支持既有 datagram/message/option 语义。IPv4 TCP 支持 active/passive stream、pselect/ppoll/epoll、`FIONBIO`、`TCP_NODELAY`、shutdown 与 close-time lifecycle。
- effective-root 可创建 `AF_INET/SOCK_RAW/IPPROTO_ICMP`：发送由内核补 IPv4 header 并经唯一 smoltcp route/ARP，接收返回重序列化的 IPv4 header；支持 bind、`MSG_PEEK/DONTWAIT/TRUNC`、`IP_TTL`、`SO_BROADCAST` 与 `SO_BINDTODEVICE`。
- `AF_PACKET/SOCK_DGRAM/ETH_P_IP` 服务 DHCP。单 VirtIO-net interface 由标准 ioctl 配置，smoltcp 0.13.1 唯一拥有 ARP/IPv4/ICMP/UDP/TCP 状态并使用 Reno。
- IPv6、其他 raw IP protocol、ARP packet protocol、multicast、TCP urgent data/keepalive/linger 与多 interface 尚未开放。

## 4. AF_NETLINK

- 精确支持 `socket(AF_NETLINK, SOCK_DGRAM[|SOCK_NONBLOCK|SOCK_CLOEXEC], NETLINK_KOBJECT_UEVENT)`、12-byte `sockaddr_nl` bind group 1、getsockname、read/recv 与 poll/epoll。未 bind 的 getsockname 返回 family+零 pid/groups；bind 的 port ID 在 live listener 中唯一，零 pid 由 syscall seam 投影 caller TGID。每个 OFD 固定拥有 16×256-byte queue；满时只替换最新 DRM hotplug，消息为标准 NUL-separated `ACTION/DEVPATH/SUBSYSTEM/HOTPLUG/SEQNUM`。
- registry 只持 listener Weak，dead entry 由创建/广播路径无分配回收；notification 只发布 empty→non-empty edge，因此 close 与 resize storm 不引入 registry-lock 自死锁、分配或重复唤醒。
- send/connect/peer/`MSG_PEEK`、其他 protocol/group、rtnetlink 与 userspace multicast publish 尚未开放。
