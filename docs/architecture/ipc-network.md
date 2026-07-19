# IPC 与网络当前架构

## 当前设计

- `ipc::Pipe` 唯一拥有 byte ring、endpoint、atomic write 与 readiness generation；notification pipe 只传递合并 edge，不复制 data readiness。
- epoll 在 ctl 阶段以持久 source index 精确更新 ready membership，wait 只向
  sharded WaitRegistry 发布单个 epoll notification key；ppoll/pselect 与 blocking I/O
  仍使用 transient source-key seam，两者在唤醒后都复查 backend level state。
- `socket` façade 拥有 domain dispatch；AF_UNIX namespace/queue/SCM graph、IPv4 stack、
  AF_PACKET registry 与 kobject listener 各自拥有复合状态。IPv4 protocol owner 保持唯一
  `SocketSet`，endpoint data-plane 通过稳定 placeholder slot 临时借出真实 socket，在全局 stack
  mutex 外复制 payload；独立 endpoint 只共享 reader membership，不互相串行。
- pipe、AF_UNIX、IPv4、AF_PACKET 与 kobject receive 共同写入 `ipc::ReceiveBuffer` 的
  initialized prefix；短读、control barrier 与错误路径不暴露未初始化 capacity，syscall 不保留
  另一个 zeroed staging 路径。两条 64KiB heap receive 的预清零成本由 131,072B 降为 0。
- smoltcp 只负责 Ethernet/ARP/IPv4/UDP/TCP protocol state，不定义 Linux socket UAPI、fd 或 errno。
- UDP/TCP 各自的 `PortNamespace` 是 local tuple 占用唯一 owner：per-port summary
  与 exact IPv4 嵌套索引保留 wildcard/`SO_REUSEADDR` 语义，ephemeral bitmap
  只投影“整个 port 完全空闲”。TCP listener claim、accepted exact tuple 与 active
  connect source-address 迁移都在该 owner 内 prepare/commit；raw socket local port 固定为 0，
  不参与 UDP/TCP port namespace。
- network hardirq 只确认设备并发布 deferred work；packet processing、completion reclaim 与
  waiter notification 在 user-return/idle safe point 的有界 deferred batch 中执行。deferred poll
  取得独占 protocol membership 后把唯一 stack 临时移出 mutex slot，再执行 device completion、
  ingress 与 egress，并在同一 RAII loan 内提取最多 64 个 readiness endpoint Arc。归还 stack、释放
  membership 后才进入 wait owner 发布 readiness，满批次重新投递 deferred work；设备 I/O 不持有
  stack mutex，也不会在 poll loan 期间从第二条路径访问空 slot。

## Known limits

- 当前网络只有单 VirtIO-net interface、IPv4、已声明的 UDP/TCP/raw ICMP/AF_PACKET 与有限 kobject netlink。
- IPv6、多 interface、network namespace、rtnetlink、multicast 和完整 advanced TCP option 尚未开放。
