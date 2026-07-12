# Phase 46：AF_UNIX 与统一事件等待

Phase 46 以 Linux v7.1 与 musl 1.2.6 为固定语义源，在现有 OFD 和 IndexedWaitQueue 上完成本地事件 I/O 竖切，不建立 socket 私有 fd 表或第二套 wait registry。

## Owner 与 seam

- `socket::UnixSocket` 唯一拥有 abstract AF_UNIX namespace、stream connection、listen queue 与 datagram message boundary；Pipe 作为内部全双工 transport，复用背压、EOF、SIGPIPE 与 wake 证明。Phase 49 将其从 IPC module 迁入统一 socket facade，owner 语义不变。
- `fs::OpenFileDescription` 统一承载 inode、character、Pipe、Socket 与 Epoll。dup/fork 共享 OFD，exec/close 释放 descriptor；最后一个外部 descriptor close 清除 epoll interest，避免 fd reuse ABA。
- `fs::Epoll` 唯一拥有 `(fd, OFD)` interest、ET generation、MOD revision 与 ONESHOT disable state；`ppoll` 与 epoll 调用同一个 OFD readiness seam 并注册到同一 Poll wait membership。
- syscall 只解析 RV64 sockaddr/epoll_event、执行 user-copy 与 errno translation。

动态 musl probe 覆盖 stream/datagram socketpair、LT/ET/ONESHOT、abstract bind/listen/connect/accept4、blocking client/server、双向传输、fork 与 CLOEXEC。

pathname AF_UNIX 需要 VFS socket inode 与 unlink lifetime，本阶段明确返回 `EOPNOTSUPP`，不以仅存内存 map 冒充；SCM_RIGHTS/credentials、AF_INET、VirtIO-net 与网络协议栈属于后续阶段。

Phase 47 进一步修复 fork 后最后 descriptor close、fd reuse identity、并发 MOD/delivery、嵌套 epoll 与 ET 事件代际；Phase 48 又把 `EPOLLEXCLUSIVE` source wake-one 收敛进同一 IndexedWaitQueue。ABI 矩阵仍以当前 OFD event-mask 的精确边界为准。
