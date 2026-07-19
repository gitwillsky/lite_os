# IPC 与网络契约

## Owner

- `ipc::Pipe` 独占 byte ring、endpoint count、atomicity 与 readiness generation。
- `ipc::ReceiveBuffer` 独占 kernel receive staging 的 initialized prefix；heap storage 只保留 capacity，backend 只能通过 append 扩展可读取前缀。
- `fs::Epoll` 独占 interest、incremental ready membership、ET/ONESHOT 与 nesting state；
  持久 source index 把 Pipe/console edge 精确路由到 interest，OFD reverse index 独占
  final-close detach membership；sharded WaitRegistry 只独占实际 task wait membership。
- AF_UNIX socket、rights graph、IPv4 NetworkStack、AF_PACKET registry 与 kobject registry 分别独占各自 namespace、queue 和 protocol state。`NetworkStackOwner` 的 `Option` 只表示独占 poll loan，不是第二份协议状态；loan 必须在 protocol writer membership 释放前原样归还。
- `NetworkStack.udp_ports/tcp_ports` 分别独占 UDP/TCP local tuple membership；
  `PortLease` 是 endpoint 释放的唯一 capability。per-port/exact-address 索引与 ephemeral
  bitmap 必须同一 transaction 更新，禁止恢复 endpoint 扫描或平行占用表。
- AF_UNIX rights graph 的 node state 独占 incoming、outgoing 与两者之和的 incident reference
  count；attach/detach 在同一 graph lock transaction 更新三者。detach 只检查 batch 的唯一
  source endpoints 与 target，访问上界为 `unique_sources + 1`；reference 归零才从 topology
  index 精确摘除，禁止全图 retain。

## Interface

- notification pipe 只承载 edge；epoll_ctl ADD 在发布 interest 前预分配
  ready/source/reverse 节点，wake 只回收并重用节点。epoll_wait 只消费
  ready index 并等待单个 epoll notification，禁止每次 wake clone/poll 全部 interests
  或重建 source keys；所有 poll/epoll/blocking caller 仍必须在 wake 后复查
  backend level readiness。
- syscall socket 层只处理 sockaddr/iovec/msghdr/cmsg/option codec、user-copy 与 errno；不得匹配或泄漏 concrete protocol adapter。
- protocol message limit 与 stream/atomic classification 由 `socket::message_limits` 唯一提供。
- pipe 与所有 socket backend 只向 `ipc::ReceiveBuffer` 追加实际取得的 bytes；64KiB heap staging 只 reserve、不预清零，stream control barrier 通过 bounded append 保持，syscall 只 scatter initialized prefix。不得取得未初始化 capacity 的 Rust slice，也不保留 slice/zeroed 双轨。
- smoltcp、VirtIO-net 与 Linux socket ABI 通过 network-device 和 socket façade 分隔，任何一层不得复制另一层状态。
- 每个 `InetSocket` 独占自己的 operation membership；send/receive 在 shared protocol
  membership 下用同类型 closed placeholder 保持 `SocketHandle` slot 稳定，把真实 smoltcp
  socket 借到 stack mutex 外完成 payload copy 后原位归还。namespace mutation 与 deferred poll
  使用 exclusive membership，禁止恢复全局 data-plane mutex、staging 协议副本或旧的锁内 fallback。
- AF_PACKET RX tap 对一个 Ethernet frame 只构造一个不可变 `SharedPacket` owner；匹配 endpoint
  queue 只克隆 Arc membership。queue capacity/OOM 仍按 endpoint 独立丢包，禁止恢复逐 endpoint
  payload 分配与复制。
- local tuple 冲突必须区分 wildcard 与 exact IPv4：不同 exact address 可共用
  port，重叠 tuple 只有双方 `SO_REUSEADDR` 时可 bind。未实现 `SO_REUSEPORT`，
  因此 wildcard/同 exact address 的第二个 TCP listener 始终拒绝；accepted 与 active
  connect 必须持有 smoltcp 权威 local endpoint 的 exact lease，不得长期保留 listener/bind wildcard。

## Failure and cleanup

- send/receive publication 在 payload、control、fd reservation 与 queue capacity 全部验证后提交；partial stream progress 与 atomic message 失败必须区分。
- receive 失败或短读只能保留已 append 的 prefix；未初始化 capacity 永不成为 slice，也不得因错误路径或错误 byte count 被 copyout。
- epoll_ctl ADD 的任一节点预分配失败都返回 ENOMEM 且不留半发布
  membership；copyout 失败不消费 ET/ONESHOT。dup/fork 共享 OFD identity，
  最后 descriptor close 只消费该 OFD 的 reverse memberships，禁止扫描全局
  epoll registry。
- AF_UNIX stream listener 以 pending queue 与 RAII connect reservation 共同计入唯一 backlog
  capacity；backlog-full 必须在 transport factory 前返回，queue node、双向 Pipe 与 accepted endpoint
  全部在 listener/client lock 外准备，OOM 或并发失败由 reservation capability 自动回滚。
- hardirq 不分配且只确认设备并发布 Network bit；deferred 网络处理有 batch 上限，并且只从
  user-return/idle safe point 取得 exclusive protocol membership。poll 必须先把唯一 NetworkStack
  从 mutex slot 借出、释放 mutex，再进入 VirtIO queue ordinary lock，结束后归还；kernel SSIP 不得
  直接 poll 网络。poll loan 必须由 RAII owner 固定为“归还 stack 后释放 writer”，禁止暴露独立
  take/restore 操作。readiness pending state 在 loan 内提取为最多 64 个 endpoint Arc，随后归还 loan
  才进入 wait owner；满批次必须回投 Network bit。缺少前半段会与下一轮 poll loan 竞态访问空 slot，
  持 writer 进入通知则会和持 wait owner 复查 level readiness 的线程形成反向等待。close/drop 不得
  在 registry/graph lock 内反向调用 socket state。
- network-device receive seam 只消费 adapter 已完成 ownership transition 的 frame；VirtIO-net
  adapter 独占 RX slot/head mapping，畸形 completion 不得把 driver-owned slot 泄漏给协议层。
- smoltcp `Device` callback 无法直接返回 adapter error；`EthernetDevice` 因而独占首个 pending
  error latch，deferred poll 只发布 error readiness，socket façade 统一投影 typed `Device` error。
  `WouldBlock` 不得进入 latch；TX reservation 只在 descriptor 成功发布后解除 RAII rollback。
- port membership 所需 outer/exact AVL node 必须在发布前预留；accept 在 replacement
  socket 进入 `SocketSet` 前预留 exact-address token，active connect 在 SYN 发布前预留
  wildcard→exact 迁移 token。OOM 保留原 membership 且不留孤儿 socket；FIN/reset
  orphan 只在 egress 观察完成后同 endpoint 一起释放 lease。
