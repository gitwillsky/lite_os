# IPC 与网络当前架构

## 当前设计

- `ipc::Pipe` 唯一拥有 byte ring、endpoint、atomic write 与 readiness generation；notification pipe 只传递合并 edge，不复制 data readiness。
- epoll、ppoll/pselect 与 blocking I/O 共用 IndexedWaitQueue 的 source-key seam，唤醒后必须复查 backend level state。
- `socket` façade 拥有 domain dispatch；AF_UNIX namespace/queue/SCM graph、IPv4 stack、AF_PACKET registry 与 kobject listener 各自拥有复合状态。
- smoltcp 只负责 Ethernet/ARP/IPv4/UDP/TCP protocol state，不定义 Linux socket UAPI、fd 或 errno。
- network hardirq 只确认设备并发布 deferred work；packet processing、completion reclaim 与 waiter notification 在有界 deferred batch 中执行。

## Known limits

- 当前网络只有单 VirtIO-net interface、IPv4、已声明的 UDP/TCP/raw ICMP/AF_PACKET 与有限 kobject netlink。
- IPv6、多 interface、network namespace、rtnetlink、multicast 和完整 advanced TCP option 尚未开放。
