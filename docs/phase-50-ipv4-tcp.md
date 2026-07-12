# Phase 50：IPv4 TCP stream 竖切

本阶段固定对照 Linux v7.1 socket UAPI、POSIX stream semantics 与 smoltcp 0.13.1 TCP state machine，在 Phase 49 唯一 `VirtIO-net → NetworkStack → Socket/OFD → syscall` 路径上增加 TCP。没有 TCP 私有 fd、第二个 protocol loop、BusyBox 特判或与 UDP 平行的 interface 配置 owner。

## Owner 与生命周期

- `NetworkStack` 唯一拥有 TCP handle、ephemeral allocator、listener backlog、pending error 和 close 后的 FIN/TIME_WAIT orphan；`InetSocket` 只保存稳定 endpoint identity 与统一 notification Pipe。
- listener 用最多 16 个 smoltcp listen handle 表达受限 backlog。listen 扩容先完成全部 buffer 分配再提交状态；accept 先分配 replacement，再把 established handle 原子转移给新 Socket/OFD，失败不会丢连接或留下半初始化 listener。
- accepted/connected OFD 最后关闭时不立即删除 handle：先发 FIN，再由 `poll_at` timer 推进 FIN/TIME_WAIT，只有 `Closed` 才从 SocketSet 回收。listener/fresh/connecting endpoint 关闭则 abort 并同步回收。
- TCP 使用 32 KiB RX/TX buffer 与 Reno congestion control；Reno 不要求 kernel FPU context。RX softirq budget、timer deadline、NetworkStack 锁序和 Pipe wait owner 继续复用 Phase 49 单一路径。

## Linux ABI 边界

- `AF_INET/SOCK_STREAM` 接受 protocol 0/6；支持显式 bind、`bind(0)` ephemeral allocation、blocking/nonblocking connect、listen、accept/accept4、getsockname/getpeername、read/write/readv/writev、sendto/recvfrom、sendmsg/recvmsg、ppoll/epoll、`SO_TYPE/SO_ERROR` 与 shutdown。
- active connect 以 `EINPROGRESS/EALREADY/EISCONN` 区分状态，blocking path 复用 OFD wait seam；RST/拒绝连接投影 `ECONNRESET/ECONNREFUSED`。peer FIN 在缓冲区排空后返回 EOF，local write shutdown 产生 FIN。
- backlog 固定上限 16；IPv6、urgent data、TCP Fast Open、reuse/keepalive/linger、精确 Linux retransmission timeout errno 与 network namespace 尚未开放，并在 ABI 矩阵保持 Partial。

## 运行验收事实

- passive：guest `echo LITEOS_TCP_REPLY | nc -l -p 5555` 经 QEMU TCP hostfwd 接收 `HOST_TCP_TRIGGER`，host 同时收到 guest reply。
- active：host listener 向 guest 返回 `HOST_ACTIVE_REPLY`，guest `echo GUEST_ACTIVE_TRIGGER | nc -w 5 10.0.2.2 16666` 完成 blocking connect 和双向 stream；host 收到 guest payload。
- closed port：BusyBox `nc -z -w 2` 返回非零，连接拒绝未误报成功或永久阻塞。
