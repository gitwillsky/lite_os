# Socket syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 198 | `socket` | Partial | AF_UNIX、AF_INET、AF_PACKET、有限 AF_NETLINK |
| 199 | `socketpair` | Complete | AF_UNIX stream/datagram |
| 200 | `bind` | Partial | 支持 domain 的 address scope |
| 201 | `listen` | Partial | AF_UNIX/IPv4 TCP backlog |
| 202 | `accept` | Partial | AF_UNIX/IPv4 TCP |
| 203 | `connect` | Partial | AF_UNIX/IPv4 endpoint |
| 204 | `getsockname` | Partial | 支持 domain |
| 205 | `getpeername` | Partial | connected endpoint |
| 206 | `sendto` | Partial | stream/datagram/raw/packet scope |
| 207 | `recvfrom` | Partial | short buffer、TRUNC/PEEK/DONTWAIT |
| 208 | `setsockopt` | Partial | 已声明 SOL_SOCKET/IP/TCP options |
| 209 | `getsockopt` | Partial | 已声明 SOL_SOCKET/IP/TCP options |
| 210 | `shutdown` | Partial | connected stream endpoint |
| 211 | `sendmsg` | Partial | iovec、SCM_RIGHTS、atomic message limits |
| 212 | `recvmsg` | Partial | iovec、cmsg、CLOEXEC/CTRUNC/TRUNC |
| 242 | `accept4` | Partial | NONBLOCK/CLOEXEC |

## Domain scope

- AF_UNIX 支持 abstract/pathname stream/datagram、peer credential、SCM_RIGHTS、pathname inode identity、固定 datagram queue 与 cycle-safe inflight collection。
- AF_INET 支持单 interface IPv4 UDP/TCP 与 effective-root raw ICMP；AF_PACKET datagram 提供当前 DHCP 路径。
- AF_NETLINK 只开放 `NETLINK_KOBJECT_UEVENT` group 1 的只读 DRM hotplug multicast。
- blocking、nonblocking、pselect/ppoll/epoll 共用 backend level recheck；notification edge 不是第二份 readiness state。
- AF_INET/AF_PACKET 的 adapter `Device` failure 经 socket façade 稳定映射为 `EIO`；暂时无包或
  无 TX capacity 仍为 `EAGAIN`，frame 超长仍为 `EMSGSIZE`。

## 已知缺口

IPv6、其他 raw protocol、ARP packet protocol、multicast、多 interface、network namespace、rtnetlink、userspace netlink publish 与 advanced TCP options 尚未开放。
