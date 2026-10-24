# IPC 与网络契约

## Owner

- `ipc::Pipe` 独占 byte ring、endpoint count、atomicity 与 readiness generation。Fresh/Live/Retired 与 endpoint counts 在同 state 锁线性化；端点锁外预分配，typed open
  一次冻结 prior counts/counters、判 nonblock writer peer requirement，并登记完整方向集合。
  每个 PipeEnd 的 registered AtomicBool 是唯一一次 open capability：未发布 unique Arc 在
  admission guard 内置 true；close/shutdown/fallback Drop 以 AcqRel swap 消费为 false，再在
  Pipe guard 内归还 count。Acquire read/write 拒绝已关闭端点，观察者 Arc 不延长打开阶段。
  最后 close 同锁退休、锁外销毁 ring；增加的 atomic load 纳入既有 Pipe CPU benchmark，阈值不变。
- named/anonymous pipe 共用 FifoFile 实际 I/O 与 poll；只 named 执行 peer rendezvous，只“named+nonblock+read+prior 无writer”保存 HUP suppression baseline，其余读端按真实 writer EOF/HUP。FifoFile allocation 发生在 endpoint admission 前；OOM 不留下半次 RW open。
- 全部 Pipe 共用 task 在 `task::initialize` 安装的唯一 `PipeScheduler`（唤醒与阻塞）；任何领域都
  直接调用 `Pipe::pair`/`Pipe::notification_pair` 创建 endpoint，并只经 `Pipe::wait` 阻塞，禁止恢复
  composition root 注入的 pipe 工厂或领域直接调用 task 的 pipe wait。安装前不存在任何 task，因此
  不存在可唤醒的 waiter 或 poller。
- `ipc::ReceiveBuffer` 独占 kernel receive staging 的 initialized prefix；heap storage 只保留 capacity，backend 只能通过 append 扩展可读取前缀。
- `file::Epoll` 独占 interest、incremental ready membership、ET/ONESHOT 与 nesting state；
  持久 source index 把 Pipe/console edge 精确路由到 interest，OFD reverse index 独占
  final-close detach membership；sharded WaitRegistry 只独占实际 task wait membership。
- AF_UNIX socket、rights graph、IPv4 NetworkStack、AF_PACKET registry 与 kobject registry
  分别独占各自 namespace、queue 和 protocol state。`NetworkStackOwner` 的
  `TaskMutex<NetworkStackState>` 是 IPv4 protocol state 的唯一 owner；普通 task 竞争时睡眠，
  deferred poll 只 `try_lock`。state 内的 payload-loan count 只证明 SocketSet 是否完整，
  不复制 protocol state。
- `NetworkStack.udp_ports/tcp_ports` 分别独占 UDP/TCP local tuple membership；
  `PortLease` 是 endpoint 释放的唯一 capability。per-port/exact-address 索引与 ephemeral
  bitmap 必须同一 transaction 更新，禁止恢复 endpoint 扫描或平行占用表。
- AF_UNIX rights graph 的 node state 独占 incoming、outgoing 与两者之和的 incident reference
  count；attach/detach 在同一 graph lock transaction 更新三者。detach 只检查 batch 的唯一
  source endpoints 与 target，访问上界为 `unique_sources + 1`；reference 归零才从 topology
  index 精确摘除，禁止全图 retain。

- `SO_ACCEPTCONN` 只由 `sys_getsockopt` 编解码；Socket::is_listening 投影实际 backend 状态。
  AF_UNIX 在已有 endpoint state guard 内读取 Listening，IPv4 TCP 在已有 NetworkStack guard 内读取 TcpMode，UDP/RAW/packet/netlink 返回 false。
  新 scoped getter 的 production caller 只有该 syscall 投影及下一层 façade；不增加依赖、缓存、锁对象或可修改状态。

## Interface

- 地址投影区分不存在的 source 与未命名 UNIX 地址：recvmsg/recvfrom EOF 的 name 长度为零；
  UNIX getsockname/getpeername/accept 则由实际 backend 投影 family-only 地址，通用编码器不猜 domain。

- notification pipe 只承载 edge；epoll_ctl ADD 在发布 interest 前预分配
  ready/source/reverse 节点，wake 只回收并重用节点。epoll_wait 只消费
  ready index 并等待单个 epoll notification，禁止每次 wake clone/poll 全部 interests
  或重建 source keys；所有 poll/epoll/blocking caller 仍必须在 wake 后复查
  backend level readiness。source notifier 持 epoll owner 时只能无等待地观察 backend；
  IPv4 owner 竞争或 payload loan 使状态暂不完整时，先保守发布 ready membership，再由
  task-context delivery 精确 level recheck，禁止在 deferred context 注册 `TaskMutex` waiter
  或吞掉唯一 edge。
- eventfd 每次成功 write/read 都必须分别发布 `EPOLLIN`/`EPOLLOUT` edge，而不只在
  counter 的 0/满边界通知。Mio 的 epoll waker 不读取 eventfd counter；若重复 write
  不推进 readiness generation，Tokio task 可已入队但所有 worker 仍睡眠。
- syscall socket 层只处理 sockaddr/iovec/msghdr/cmsg/option codec、user-copy 与 errno；不得匹配或泄漏 concrete protocol adapter。
- protocol message limit 与 stream/atomic classification 由 `socket::message_limits` 唯一提供。
- pipe 与所有 socket backend 只向 `ipc::ReceiveBuffer` 追加实际取得的 bytes；64KiB heap staging 只 reserve、不预清零，stream control barrier 通过 bounded append 保持，syscall 只 scatter initialized prefix。不得取得未初始化 capacity 的 Rust slice，
  也不保留 slice/zeroed 双轨。
- smoltcp、VirtIO-net 与 Linux socket ABI 通过 network-device 和 socket façade 分隔，任何一层不得复制另一层状态。
- 每个 `InetSocket` 独占自己的 operation membership；send/receive 通过
  `NetworkStackOwner::with_payload_loan` 在两个短 `TaskMutex` transaction 中用同类型 closed
  placeholder 保持 `SocketHandle` slot 稳定，把真实 smoltcp socket 借到 owner 外完成 payload
  copy 后原位归还。不同 endpoint 只共享 O(1) loan count，不共享互斥 guard；deferred poll 只在
  loan count 为零时取得一次必要的 exclusive owner，否则 O(1) 回投。禁止恢复全局 data-plane
  mutex、reader/writer spin gate、staging 协议副本或旧的锁内 fallback。
- AF_PACKET RX tap 对一个 Ethernet frame 只构造一个不可变 `SharedPacket` owner；匹配 endpoint
  queue 只克隆 Arc membership。每个 endpoint 的 64-slot capacity 在 constructor 锁外一次准备，
  payload Vec/Arc 在 packet registry spin guard 外准备，重新取得 guard 后只重验绑定/容量并发布。
  队列满独立丢包；payload OOM 丢弃该 frame；RX publication 和 receive 均不增配 queue。
  receive 取得实际 Arc 后锁外复制和退休，禁止恢复逐 endpoint payload 分配与复制。
- local tuple 冲突必须区分 wildcard 与 exact IPv4：不同 exact address 可共用
  port，重叠 tuple 只有双方 `SO_REUSEADDR` 时可 bind。未实现 `SO_REUSEPORT`，
  因此 wildcard/同 exact address 的第二个 TCP listener 始终拒绝；accepted 与 active
  connect 必须持有 smoltcp 权威 local endpoint 的 exact lease，不得长期保留 listener/bind wildcard。

- 实际 Socket facade fput 统一撤销 AF_PACKET、KOBJECT_UEVENT 与 Inet 的注册；backend Arc
  observer 不延长绑定或接收队列。packet/netlink backend Drop 不反入 registry，避免 broadcaster
  在 guard 内丢弃最后观察 Arc 时自锁。Kobject bind/receive/poll 在同一 registry admission 上判断
  closed；端口立即可由新 OFD 使用，旧 observer 不得重新绑定或继续等待空队列。

## Failure and cleanup

- MSG_COPY 在 ID/permission lookup 前预留并从用户 buf 起点读取 min(bufsz,msgmax) 的实际
  storage，保留至 completion；实际 queue selection 仍只读不可变 Message Arc。NOERROR
  跳过 E2BIG 后仍由 copy capacity 检查返回 EINVAL，不截断副本，不改计数/时间/历史 PID。
  普通 receive 不受随后降低 msgmax 影响；copy fault 前的普通 dequeue 按 Linux 消费消息。
  send/receive 等待中断输出 ERESTARTNOHAND typed policy，捕获 handler 即返回 EINTR。

FD receive admission 的 bookkeeping/backing OOM 按固定 scm_detach_fds 语义截断 controls；已成功 copyout 的 FD prefix 立即安装到实际文件表，MSG_CTRUNC 标记剩余未发表 capabilities，在锁外释放。不会因后续 FD 扩容失败把已消费的 payload
变为 ENOMEM；当前 scalar copy fault 只撤销当前单 FD reservation。

connect 前的 stream SHUT_WR 只终止自身后续 writer admission，不向尚不存在的新 peer 重放 EOF；DirectionState.write_disabled 属于真实 writer permission，而 DirectionState.writer_closed 拥有 peer 可观察 EOF。显式 connected shutdown 消费真实 writer
capability；两个事实不能用同一关闭动作合并，否则 connect 前关闭写侧会错误地令 accepted peer 立即 EOF。该控制状态在真实 direction guard 下读取，未增加第二套 transport 或兼容分支。

新增热路径 Mutex/capacity 比较的 benchmark 沿用现有 hot 200ns 门禁；packet 的 native 逐消息 node/payload allocation 作为 cold roundtrip 独立报告，不降低阈值。host 回归已补 >11 SeqPacket、sender watermark/refund、100000-byte deferred staging、
peek/OOM 不退款、不互连第十二条拒绝和 self-send；native guest 与实际 packet benchmark 纳入集中门禁。

## 具体 owner 契约

- [IPv4 与 TCP/UDP 传输契约](ipv4-transports.md)
- [Packet 与 Netlink 契约](packet-netlink.md)
- [UNIX socket 与辅助数据契约](unix-sockets.md)
- [System V IPC 对象与资源契约](systemv-ipc.md)
- [Pipe 与 readiness 契约](pipe-readiness.md)
