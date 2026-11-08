# IPC 与网络契约

Linux syscall、阻塞超时、namespace 授权、AF_UNIX/NETLINK 属于 `system/linuxd`；IPv4/TCP/UDP/RAW、FIB、端口与 AF_PACKET 队列属于 `system/netstack`。下述协议 owner 通过 `network-protocol` 交互，不共享内核状态。


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

## Socket 服务传输

- `network-protocol` 控制 channel 只承载低频配置、创建、关闭和一次 QUEUE 协商；send/receive/poll 的旧 RPC codec 已删除。
  数据面版本 `LSOCK001`：固定 8 槽 SQ/CQ、128 字节记录与每槽最多 256 KiB 载荷，两个 Event 合并请求和完成通知。
  VMO 非 RESIZABLE，metadata/载荷偏移固定且校验长度；双方只作同宽对齐原子访问。记录小端、字节载荷保留原序；
  索引 Release/Acquire 交接，载荷字 Relaxed，不将可变对端存储形成 Rust 非原子引用。
- 客户端一个本地锁独占 SQ producer/CQ consumer、ticket 与 credit；持锁只操作有界记录/复制，不跨等待。
  credit 在领取 CQ 后仍保留，复制完载荷才归还；同槽单调 ticket 拒绝重放，计数距离不能超过容量。
  多调用者由当前调用者之一推进 port/CQ，其余等本地 epoch；等待人数与谓词同锁，单调用者不发多余 futex wake/credit 通知。
  无逐 socket 分派线程、每请求 VMO 或消息 Vec。
  协议损坏后 abort 并停止复用；载荷已复制到共享 VMO，没有借给服务的调用者地址，失败不需要伪造 I/O 完成。
- 服务端一个锁独占 SQ consumer/CQ producer、ticket 与发送暂存，netstack dispatcher 串行化同 socket 的控制与数据操作。
  每轮至多 16 条控制消息、8 条数据请求，剩余请求保留电平通知；两个来源分别记录一次性等待状态，不能重复武装。
  worker 从一个空闲读者起步、繁忙时至多四个、多余空闲 5 s 退休；最后一个读者永久等实际通知。
- 协议状态仍只有 netstack 一个 owner。短读写、整条 datagram 错误、PEEK、截断长度与 packet info 由实际引擎产生；
  poll 经共享环查询真实状态，不引入需要人工同步的就绪缓存，也不宣称消除了所有服务切换。
  就绪 Event 仍只提示复查；对端关闭也经同一 EventHub 活动回调唤醒已阻塞的 poll/epoll。
  CLOSE 回复后立即退休服务端登记，进程退出由 channel PEER_CLOSED 退休；两条路径均公布 queue closed 并唤醒等待者。
- 两端映射复用同一 VMO，不预提交载荷；每 socket 载荷提交上限 2 MiB，发送暂存上限 256 KiB，均随连接退休。
  控制/数据并发调用仅保证各自发布顺序与同 socket 串行执行，不另创跨来源的先后承诺；已完成调用的后继操作保持顺序。
  共享服务传输不新增应用 ABI、syscall 或内核 Linux 概念；适用于 linuxd 和现有原生 DHCP 客户端。

## Interface

- 安装协议只授权给接口配置 owner：请求携带设备目录能力、ID 和静态能力描述，netcfg 不预先打开硬件会话。
  netstack 的 worker owner 选择非零 pair 数，经目录 OPEN 的设备类 options 交给 driver controller；硬件超限请求失败，不静默截断。
  当前主机协议引擎单 worker 明确请求一对，最终每核/RSS 数据面仍属路由计划。目录转发不复制队列策略或配置状态。


- DHCP 租约事件带服务器标识、发出请求的单调时刻及租期。netcfg 只在地址配置成功后发布租约，失效或切换配置时一并清除。
  只读快照用当前 UTC 与单调时钟的偏移投影取得和过期时刻；配置文件不包含租约，系统时钟校正不改变续租或失效期限。
- DHCPv4 选项 15 提供租约后缀，netcfg 以手动连接后缀覆盖它。没有生效地址时不发布连接后缀。
  显式全局搜索列表覆盖自动搜索。自动模式依次尝试主 DNS 后缀、各接口后缀、主后缀的父级，去重且父级至少保留两个标签。
  netcfg 持有系统主后缀及父后缀开关，并在同一次持久化提交中保存一并修改的网络设置。只改搜索策略时保留现有租约与路由。
  resolvd 与 `ipconfig /all` 使用同一有效搜索顺序。
  搜索列表（含分隔空格）最多 247 字节，以完整读入 musl 的 256 字节行缓冲。域名使用 ASCII 标签，国际化域名使用 A-label。
- 用户租约操作走当前接口的 DHCP 会话。续租在原地址上发出 DHCPREQUEST，只有新租约应用后才回复成功；释放尽力发送
  DHCPRELEASE，随后撤销本地配置并暂停该会话的自动获取。完成句柄跟随租约事件回到 netcfg，避免把排队当成应用成功。
  此状态随会话结束而消失，操作不写入持久设置。替换接口或配置会关闭旧会话及其在途操作。
- netcfg 的默认路由以 ifindex 定义稳定优先级。新增路由不会替换同前缀的其他接口路由，删除限定出口、网关和配置协议。
- 局域网连接的自定义显示名由 netcfg 按接口名持久化，和 IP 配置共同保存在原生设置文件。名称读取随公开状态快照发布，
  写入沿用网络设置端点的 `netdev` 权限，更新仅发布新的设置代际，接口身份、地址和 DHCP 会话继续存续。
- 物理载波与速率只由设备提供，管理开关由 netstack 持有。设备会话的只读 LINK 请求取当前配置，netstack 仅在管理面接口快照中调用，
  不加入收发、完成队列或 socket 唤醒路径。读取失败没有链路事实，消费者不得把接口已启用当成物理连接。

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
