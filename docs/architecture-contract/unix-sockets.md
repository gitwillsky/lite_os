# UNIX socket 与辅助数据契约

- send/receive publication 在 payload、control、fd reservation 与 queue capacity 全部验证后提交；partial stream progress 与 atomic message 失败必须区分。
- receive 失败或短读只能保留已 append 的 prefix；未初始化 capacity 永不成为 slice，也不得因错误路径或错误 byte count 被 copyout。
- epoll_ctl ADD 的任一节点预分配失败都返回 ENOMEM 且不留半发布
  membership；copyout 失败不消费 ET/ONESHOT。dup/fork 共享 OFD identity，
  最后 descriptor close 只消费该 OFD 的 reverse memberships，禁止扫描全局
  epoll registry。
- AF_UNIX stream listener 以 pending queue 与 RAII connect reservation 共同计入唯一 backlog
  capacity；backlog-full 必须在 transport factory 前返回，queue node、双向 Pipe 与 accepted endpoint
  全部在 listener/client lock 外准备，OOM 或并发失败由 reservation capability 自动回滚。
  `listen(2)` 的 signed 32-bit backlog 按 Linux unsigned comparison 截到 kernel-wide 4096 上限；
  因而 Rust `UnixListener` 使用的 `-1` 表示该上限，不得先归零。各 protocol backend 可继续收紧
  自身容量（当前 TCP 为 16），但不能改变 syscall 层的 ABI 归一化语义。
- hardirq 不分配且只确认设备并发布 Network bit；deferred 网络处理有 batch 上限，并且只从
  user-return/idle safe point 调用 `try_poll`。owner 竞争或存在 payload loan 时不得等待/自旋，必须
  O(1) 回投 Network bit；SocketSet 完整时 poll 持唯一可睡眠 owner 进入 VirtIO ordinary lock，其他
  task 竞争者由 `TaskMutex` handoff 唤醒，kernel SSIP 不得直接 poll 网络。readiness pending state
  在 poll owner 内提取为最多 64 个 endpoint Arc，释放 owner 后才进入 wait owner；满批次必须回投。
  actual OFD fput 以 `operation.take()` 消费 protocol registration，`InetSocket` Drop 仅处理未发布失败；
  close cleanup 只能 `try_lock`：失败时把唯一 endpoint identity 发布到与生产 SocketSet
  capacity 同为 1024 的 fixed pending-cleanup ring；下一轮 poll 在协议推进前 O(1) pop 最多 64 项，
  有余项则回投 Network bit。同一 endpoint 的 actual close 唯一且 poll 持 owner 时不能创建/复用 slot，
  每个 TCP facade 始终持至少一个真实 handle：取消 SYN_SENT 等无需 RST 的 transport 后，原 slot
  保留无 buffer 的 CLOSED socket，下一次 connect/listen 完整预分配 buffer 后替换同一 slot。
  listener 保留 first/至少一个 backlog，accept 先准备 replacement 再移交旧 handle；
  reap只删除CLOSED且remote tuple已退休的orphan registration；RST payload不持有TCP/端口引用，
  本地NIC背压只能延后有预算报文的交付，不能延后已关闭TCP身份的退休；
  普通 task 取得 protocol owner 时先消费已有 cleanup，保证 close 后重绑见到退休 port。
  源码围栏同时验证 admission/transfer/reap 与容量关系，
  因此 publish 不扫描查重，ring 不需要 heap 或 overflow fallback。
  close/drop 不得在 registry/graph lock 内反向调用 socket state。
- network-device receive seam 只消费 adapter 已完成 ownership transition 的 frame；VirtIO-net
  adapter 独占 RX slot/head mapping，畸形 completion 不得把 driver-owned slot 泄漏给协议层。
- smoltcp `Device` callback 无法直接返回 adapter error；`EthernetDevice` 因而独占首个 pending
  error latch，deferred poll按actual interface返回有界诊断和counter；不能把硬件fault
  投影成无关socket的SO_ERROR/POLLERR。AF_PACKET直接发送仍返回所选device同步typed错误。
  `WouldBlock` 不得进入 latch；TX reservation 只在 descriptor 成功发布后解除 RAII rollback。
- port membership 所需 outer/exact AVL node 必须在发布前预留；accept 在 replacement
  socket 进入 `SocketSet` 前预留 exact-address token，active connect 在 SYN 发布前预留
  wildcard→exact迁移token。OOM保留原membership且不留孤儿socket；正常FIN/TIME_WAIT
  orphan持有lease至纯协议时钟退休；abort只把一次原codec payload交给RouteReplies后释放身份。

- AF_UNIX `UnixCredentials` 在 listen/connect/socketpair 的实际 transaction 捕获 canonical
  effective UID/GID 和真实 Process PID identity observer pin；普通 socket creation 不保存凭据。
  listener 的唯一 state 同时拥有 backlog 和 listen credentials；relisten 更新两者，旧 credential
  移出 guard 再析构。connect 在最终 listener commit guard 内冻结最新 credentials，不使用
  allocation 前的旧快照。stream peer 和配对 datagram 的历史凭据只属于其实际 state；普通
  datagram connect 不安装 peer 凭据；实际 peer 是 endpoint Arc observer，不保活 OFD。
  reconnect 与 AF_UNSPEC 在实际 state transaction 中替换 peer，peer 改变时摘除原 receive queue，
  向仍互连的旧 peer 报告消息丢失 RESET；旧 node/control/peer 在 guards 外退休。
  `SO_PEERCRED` 按读取方当前 PID/User namespace 投影；尚无 owner 时标准输出三个 -1。
  observer pin 不增加 graph registration，不延迟 PID 编号回收。socketpair 只发布尚未连接的
  私有 endpoints。relisten 缩小容量不丢弃 pending/reservations，只阻止新 admission。
  Arc clone 属于冷 listen/connect/getsockopt 路径，最终 guest 核对跨 view、凭据时机、空值和
  历史编号回收。

- AF_UNIX bind 在 endpoint address/namespace guard 外预留唯一节点，随后按 address→namespace
  锁序同时提交 actual visible address 与 Weak binding；同 endpoint 并发 bind 只有一次成功。
  仅当前 key 的 dead Weak 可被替换，不扫描无关 sockets；退休节点与 Weak 在两个 guard 外
  析构。最后 close 只摘除 dead binding，迟到的旧 close 不能删除已换代的 live endpoint。
  pathname 按 actual VFS filesystem/inode identity 检索，abstract 保留完整长度与 embedded NUL。
  benchmark 决策：删去 O(n) retain，实际 bind/lookup/close 为 AVL O(log n)，bind node 的一次
  冷分配保持；加入 OOM、并发 bind 和迟到 close owner 回归。

- SCM_RIGHTS graph preparation 在 transport/GRAPH/NodeState guard 外完成：唯一 PreparedAttach
  预留 node/edge/UID tokens 和必要 Tarjan scratch，实际 publication 在 GRAPH guard 内无失败
  commit。edge 以 (batch_id, descriptor ordinal) 索引，保留重复 source；detach 沿实际 keys
  摘除，不扫描其他 batch，node/edge/UID 退休 tokens 在 GRAPH guard 外释放。并发 topology
  超过 prepared scratch 时只返回 NeedsPreparation，caller 释放 transport guard 后重新准备，
  没有锁内分配 fallback。旧 alternative/prepared scratch 在 transport guard 外归还。
- stream barrier 和 datagram FIFO 都由实际 AVL admission nodes 拥有；queue node 在 guard 外
  预留。stream byte cursor 与 barrier 同 transaction 提交，零写/Full/Broken 在 guard 外
  detach；ordinary read 丢弃 rights 前先释放 direction guard。datagram capacity 与 graph/
  message publication 串行，pop 返回退休 node，guard 外才 into_value。FIFO 上限由实际 peer policy 输入；非互连/非 self 的 datagram 按 native max_dgram_qlen + 1
  admission，互连/SeqPacket/self 由 actual sender memory 约束。payload 边界保持，无兼容队列。
- Tarjan source→receiver dependency 按 sink→source 一次传播；自环读取当前已经持有的
  NodeState，不重入同一锁。root probe 一次 Weak upgrade 返回实际 OFD pin；Linux file_count
  判断只排除这一个 probe 和 outgoing inflight references。所有 OFD/Socket probe pins
  保留在已预留 scratch，完整 pass 之后在 GRAPH guard 外退休，避免最后 Drop 重入 detach。
  Tarjan scratch 在 GC pass 前由 actual attach 扩容，GC/exit/revoke 不再分配；容量不足的
  attach 只重做锁外 preparation，recovery 自身不通过分配恢复。
- SCM quota 沿 Linux v7.1 net/unix/af_unix.c too_many_unix_fds：检查本次 attach 之前的
  real-UID inflight > RLIMIT_NOFILE，允许当前 batch 越过边界；initial SYS_RESOURCE 或
  SYS_ADMIN 免除该限制，child UserNS capability 不豁免 initial quota。quota recovery 必须
  令当前 UID 真实 inflight 下降才重试，不因别的 UID cycle 或 stale candidates 自旋。
- 唯一 unix-rights-gc kernel worker 由 main 装配；socket 下层只接收 clock/sleep/prepared
  TaskMutex gate seam，不反向导入 task。实际 publication/detach 发布一个合并 TaskEvent；
  graph 为空时 worker 无限休眠，存在 topology 时每 100 ms 复查 root retirement，低于
  RLIMIT/特权发送者的孤立 cycle 因此也会回收。启动时预留 collector contention waiter 和
  deadline owner；GC 不在 hardirq/deferred safe point 执行，last OFD/Socket Drop 位于 GRAPH
  guard 外。collector gate 为可睡眠 TaskMutex，detach 绕过此 gate以支持同步 revoke。
  benchmark 决策：ordinary send/recv 不走 graph；rights 冷 preparation 新增可分析 AVL
  tokens，publication/retirement 无分配；duplicate edge detach 为 O(k log e)，GC 为 O(v+e)，
  时间计量覆盖实际 graph 和 worker active/empty 行为，host/guest 用例随统一门禁执行。

- stream MessageFrame 的边界记录 actual carrying write 的 end_offset；recv/read 可取得完整
  carrying prefix，在下一条 control barrier 前停止。按首字节限读会让带 SCM_RIGHTS 的
  多字节 write 永远只返回一个字节。短读仍只消费一次 rights，剩余 bytes 可与下一条
  carrying write 合并，不重发 capability。host 用例覆盖两条完整 barrier、首字节短读和
  ordinary read 丢弃 rights。

AF_UNIX 生命周期与 GC：RightsOwner 的 Creator 是未发布构造事务 root，
Backlog 直接持实际 server→listener graph capability，Description 只持唯一 OFD Weak，Retired
没有 root。Backlog edge 不代表 file reference、不计 real UID inflight；Tarjan 的 OFD root
分母只计真实 SCM edges，拓扑 incident count 同时包含实际归属边。accept 在 GRAPH→root guard
内转移到实际 OFD，旧归属 token 在两把 guard 外移除；不以 missing Weak 猜测 creator。
connect 捕获 listener 的实际 file Arc，与完整 collector pass 共用 COLLECTOR TaskMutex gate，
避免 dead snapshot 与 revoke 之间通过历史 Weak 复活 root；该 Arc 只活到构造事务结束。
最后 OFD Drop 显式将 AF_UNIX state 置 Closed，撤销 root/binding，关闭实际 Pipe endpoints，
退休 stream controls/datagrams/backlog 并逐个关闭未 accept 的 child，全部最终 Drop 在 guards 外。
namespace 的 lookup/collision 检查实际 Closed state，Arc observer 不能压住 rebind；所有 syscall
持实际 OFD pin 到返回，不用裸 backend Arc 代替 active file lifetime。SHUT_RD 同时关闭 reader
和退休未消费 controls，即使 peer transmit 或 poll observer 仍持 Direction/StreamReceive Arc。


sendmsg native ancillary：ControlSnapshot 与固定 Linux ____sys_sendmsg 一样先
检查 INT_MAX，36-byte inline fast path 不分配，heap path 使用已有 UserInputStaging 的
MaybeUninit capacity；实际一次 copy_from_user_uninit 完整成功才 publish，fault 不暴露半份
initialized bytes。ControlList 只借 frozen bytes，后续 fd capture 不再读取 user header/data。
CMSG_NXTHDR 忽略不足完整 header 的 trailing bytes/partial padding；scm_fp_copy 按完整 int 个数
截断 SCM_RIGHTS，空或不足 int 的 payload 不产生 rights，合并最多253个descriptor。
AF_UNIX 忽略非 SOL_SOCKET level；unknown SOL_SOCKET type 为 EINVAL。IP_PKTINFO native payload
要求精确12 bytes。heap snapshot在分配/copy前取得实际SocketMemory独占charge，申请字节
不含allocator额外capacity，费用持有到协议send返回；字段顺序先释放allocation后退款。
SocketMemory唯一AtomicUsize以Relaxed CAS保证并发used+size严格小于捕获NET的optmem_max，
未发布payload，缺失原子admission会让并发申请突破预算。charge只pin memory/NET，不pin
Task/OFD/backend。NET policy初始128KiB，proc捕获同一实际owner并在每次write重新授权。
socket/accept的未发布preparation分配memory，accept使用listener域，其他创建使用caller
membership；dup/fork/SCM不复制policy。当前只装配initial NET对象，完整NEWNET和transport
隔离继续接通，不声明完整NETNS。新增copy/codec、quota并发/退款与allocation-free inline
源用例覆盖原生三种UNIX socket guest及实际reserve/refund成本诊断。

SO_*BUFFORCE 原生能力入口与普通buffer设置共享实际Unix ledger setter；仅pre-clamp按force
选择负值归零/绕过max，floor、倍增、通知和getter没有第二套实现。syscall先完整读取int，
再检查initial UserNS全局CAP_NET_ADMIN（Linux sockopt_capable→capable）；不能改成socket
NET UserNS的能力检查。既有fd的持有者每次set重新授权，EFAULT优先于缺权限。

NET preparation源码：UserResource::Net进入同一actual UserQuotas ledger，NetNamespace
持独占UserResourceCharge，无第二份creator/ancestor计数。UnixSocket固定真实NET Arc，
这是与facade memory共享对象的只读引用，不保存policy镜像。NamespaceKey::Abstract包含
实际id/address/type，autobind和lookup同一范围，id单调不复用；Path仍只以实际inode为key。
被动child从listener捕获域，client当前Task不参与旧socket的解析。全局UNIX Weak索引不
保活namespace；操作/endpoint引用才保持实际NET费用。proc的canonical key包含NET id，
其Weak目标在open升级为实际Arc，Noninitial全局core buffers只读。
sysctl属性不进入全局registered entry表，scoped inode独占真实UID/GID/mode；非initial
core buffer mode为0444。net_ctl_permissions加proc_sysctl::test_perm按GLOBAL_ROOT_UID
或owning UserNS NET_ADMIN授权，不能用stat的mapped root owner代替permission规则。
此阶段只有领域准备与UNIX范围，未开放NEWNET：必须完成实际INET stack/interface归属后再publish task membership。
协议owner的持有已移交NetNamespace.protocol的唯一Once<Arc<NetworkStackOwner>>，全局
NETWORK_STACK已删除；Once只消费完整准备的owner，重复初始化不替换已有SocketSet/ports。
boot初始NET先于Task装配，设备发现后初始化真实protocol。InetSocket与facade共享实际
SocketMemory：TCP/UDP/raw操作、payload loan、poll/epoll observer、SO_ERROR和实际fput
均使用其捕获NET的protocol，accept消费预留的新memory，保持listener域。构造/rollback
不产生新namespace policy；协议owner引用借自实际memory，guard不借临时Arc或当前Task。
interface/loopback继续接通后才开放NEWNET；initial入口只保留composition，不作为既有
endpoint/configuration/deferred的fallback。interface配置与快照显式接收实际NET，socket
ioctl与IP_PKTINFO按OFD捕获域查询；proc采集按实际calling Task域。标准interface/route写在
user-copy后、name/参数查找前每次按owning UserNS NET_ADMIN授权，AF_UNIX也走generic
interface flags/MTU等generic ioctl；IPv4 address/route仍属于INET proto，其他domain按
generic ifreq copy后返回ENOTTY。不能用global/mapped uid0替代能力。固定Linux dev_ioctl/devinet_ioctl/
fib_frontend规则的原生guest权限/故障优先级回归已写。
SO_BINDTODEVICE按sock_setbindtodevice最多copy IFNAMSIZ-1，不由syscall私自解析eth0；
实际NET owner先解析名字，再在同一option事务按sock_bindtoindex_locked检查既有binding。
首次未绑定的普通actor可以绑定，已经绑定时同名/解绑都要求owning NET UserNS NET_RAW，
未授权失败保持原实际policy；global/mapped root及child UserNS能力不替代ancestor授权。
标准signed optlen在所有setter的user-copy之前拒绝负值。实际owner/原生跨页/故障优先级/
跨UserNS/negative fence源回归已补。
NetNamespace实际寿命拥有唯一Weak AVL目录项；创建先完整准备node，node OOM退还实际
ucounts，最后Arc摘项并在guard外退休Weak/protocol。deferred/timer各有独立严格后继cursor，
每批最多8域、目录及protocol只try_lock，竞争回投、域guard外通知与退休；不从initial反复
开始，不在socket IO增加目录查找。每域仍有64-frame/cleanup/notification预算，单轮固定
上限随NET数量不增长。packet通知每safe point只drain一次固定64项，不在每域重复drain。
实际目录公平性/OOM/Weak寿命、独立wire NIC的同端口UDP/TCP/error/close/deferred源码
回归及新增目录成本诊断已写；单物理 NIC/lo 与 NEWNET 的 publication 范围见 INET 契约。
policy/UNIX host fixture的未执行INET类型边界不安装protocol，不覆盖或冒充传输生命周期；
network_endpoint_fixture原样载入实际protocol owner和唯一NIC wire producer并显式boot NET。
名字索引未新增guard/copy/查表，仍沿用实际UNIX namespace/packet成本门禁。

SCM sender 的 RLIMIT：Process.resource_limits 是唯一
Arc<Mutex<ResourceLimits>>；Thread 共用该 allocation，fork 新分配 values 的副本并重置
CPU cadence，exec 保留。UnixFileLimit 只把同一 allocation 的 RLIMIT_NOFILE 投影为只读标量，
不捕获 Process/Task/MM/PID 或 namespace registration。未豁免的 UnixRights 持这一实际 Arc，
每次 try_attach 从当前 owner 读取 limit；PreparedAttach 只持图 storage，不保存 limits 副本。
因此阻塞期间 remote prlimit 的升降会在下一次 commit 生效，queued controls 不延长进程
资源 membership。limits guard 在进入 GRAPH 前释放，setter 不取得 socket/GRAPH guard；
COLLECTOR recovery 也读取当前 limit，再以实际 UID inflight 的下降判进展。
新增 quota 升高后复用准备对象、准备后降低 limit 的源码用例。
独立 limits allocation 是 cold process 创建成本；SCM 增加的只读 trait/lock 查询已纳入现有
200ns gate 的 exact UID quota benchmark，不新增宽松门槛。

SCM limits 的 host CPU fixture 直接加载生产 resource_limits/unix_file_limit.rs getter 与
indexed_slots 的 fd domain 常量；只提供 values 输入、setter 与 controller handle，不能用
Atomic/固定常量代替生产 Mutex/clamp。hot graph quota、真实 getter 和原有门槛共同验证。

- `/proc/sysvipc/msg,sem,shm` 在 actual open 捕获 caller IPC namespace、active PID namespace
  和 f_cred user namespace，之后 setns/unshare/SCM 不重定向已有文件。IPC namespace 的
  next_description 沿真实 ID slot cursor 每次提取一条 metadata；snapshot/native stat/proc
  共用同一对象字段投影，文本与 UID/PID munging 在 proc codec 完成，formatter 不持 registry
  guard、不取得全局 Task snapshot。查询不检查对象 DAC，SHM DEST 对象仍在实际 registry
  中时继续可见，rss/swap 输出实际 backing 的字节数。
  每个 actual open 的 IpcReadCursor 拥有 next actual slot、固定512字节记录与未交付 suffix；OFD position
  仍归 file owner。打开对应 IPC 表时通过 Box::try_new 预留其唯一 cursor，OOM 不发表文件；其他 proc OFD 不为该 cursor 预留空间。reader TaskMutex 串行化连续 read/pread 的 seq buffer，metadata registry guard
  不跨格式化/user-copy。顺序小 read 不重新渲染已消费行，且 record codec 无 heap allocation；
  不连续 seek/pread 从 header 重新 traverse，同一 codec 处理，user-copy fault 只推进真实 copied prefix。
  benchmark 决策：有序 neighbor 查找不扫描无关 slot，无全表 metadata snapshot/输出 Vec；16/1024
  actual IPC 对象的 full read/stack codec 冷诊断成本纳入 architecture-bench，保留既有热路径阈值。
  固定依据：Linux v7.1 ipc/util.c sysvipc_proc_open/ipc_seq_pid_ns 与 msg/sem/shm proc_show。

- UnixReceivedRights 是 transport 完成后唯一的接收 file capabilities，只有 Vec<Arc<dyn UnixPassedFile>>；不带 UID fee、graph node 或 sender limit。消费先在 guard 外 detach 原 UnixRights，再移交原 Vec；MSG_PEEK 在 guard 外预留 Vec capacity、锁内复
  查当前记录并 clone 实际 OFD Arcs，不 detach 原消息。新的 Arcs 是实际 syscall roots，GC 必须按 strong-reference probe 看见它们；不引入 fd/payload 镜像。
- StreamReceive::read 的 peek 参数复用实际 message chunks；Peek 不推进 consumed cursor、FIFO node 或任何 readiness generation，普通零进度也不产生 write edge。DatagramQueue::front 仅在实际 endpoint guard 下借用首消息；peek 失败或成功均不摘
  node、不通知 full→non-full。file/syscall 接收类型改用 UnixReceivedRights，发送仍由 UnixRights 独占 graph publication。
- benchmark 决策：无 control 的实际 chunk peek 使用既有 2M 次/5 样本/200ns 门禁；SCM peek 的 1/253 OFD 实际分配/clone/fput 为原生不可避免的冷 preparation，20k 次报告成本。host 回归覆盖重复 peek、OOM、未变 generation/UID 费用和真正
  消费；guest 同一 stream/datagram/seqpacket 消息两次 peek、一次 consume，并检查每次独立 descriptor、CLOEXEC、inode 与 readiness，随统一门禁执行。

AF_UNIX BindingRequest 是唯一 namespace 发布输入，Named 传 VFS/abstract identity，Automatic 传冷绑定搜索 seed。address → namespace → state 固定锁序冻结 live endpoint 与 getsockname publication；close 必须先在 state 发布 Closed 并释放 guard，再
撤销 address。碰撞 Arc 与退休 AVL node 在全部 guards 外 Drop；自动碰撞逐轮释放 guards，复用同一预留 node。abstract identity 含 socket type；所有 connect/send 查找传自身实际 type。syscall read_address 仅 bind 允许 family-only 地址，其他
目的地址返回 EINVAL。新增 NoSpace 只映射标准 ENOSPC。新增节点、锁序与 codec 的验证归属集中门禁。

SocketCapacityWait 替代原 send 专用 blocker，socket::capacity 只持 target Arc 与 CapacityKind 输入，unix::capacity 只从实际 PacketQueue/ConnectionBacklog/state 投影；旧 send module 已移除，无兼容入口。SocketConnectError::BacklogFull 转交同一等待
capability，syscall connect 才决定 EAGAIN、等待、timeout 或 restart。close 状态的 capacity recheck 必须为 true，让重试获得实际连接错误；不是恢复 writable 的平行状态。accept 的 node/endpoint 仍锁外退休并发布 capacity edge。namespace/VFS
lookup、invocation flags/deadline 与 ConnectGuard allocation ordering 的 fence 均纳入集中门禁。

SCM_CREDENTIALS / SO_PASSCRED 源码：UnixSocket.pass_credentials 是 endpoint 唯一 AtomicBool policy；没有队列、OFD 或 façade 镜像。Atomic 只控制 native 消息保存/输出，不发布其他数据，因此 Relaxed 足够；队列内容、cursor 与
graph 仍受原 owner guards 保护。connect 的实际 commit 把 listener policy 安装进唯一 staged child。自动凭据只从 syscall 冻结的 actual TGID/real UID/GID 取得；显式 native 12-byte claim 使用 actual credential authority 和 PID namespace Weak association，
无 live Task 选择器。发送方或接收方 PASSCRED、实际 Creator/Backlog 未打开 owner 才要求默认凭据；显式 claim 总是保留。connect 与 datagram/seqpacket send 触发共用 autobind，stream send 不触发。

stream DirectionState.frames 唯一拥有 end_offset、可选 credentials 和可选 rights；普通无 controls 字节为真实 cursor 间隙，不分配记录。相邻相同 actual PID Arc/UID/GID 的无 rights span 在 guard 内延长原 node，不用可复用 PID 数字合并。节
点 admission 在 direction guard 外预留并重新检查 actual tail；bytes/frame/graph 一次无失败发布。PASSCRED read/recv 在真实凭据边界停止；未启用时可跨 credentials spans，遇到 carrying rights 的完整 prefix 停止。ReadPlan 只保存 keys，peek
的 FD Vec 仅为实际读取 prefix 会遇到的 rights 在 guard 外预留，随后重算当前 plan。Consume 摘除 rights 一次，剩余 span 保留凭据；完成 frame 的实际 nodes 在 guard 外退休。Datagram 的同一实际消息保存 credentials、bytes、source 和
rights，不另建 side table。

ReceiveMode 的 Read 与 Message 分别对应 VFS read 和 recvfrom/recvmsg protocol 调用：普通 read 空 iterator 返回零，零长度 Message 在首实际 payload 上仍 peek/copy 或消费/关闭 rights；不接收控制输出的 recvfrom 同样消费该 ancillary。零 stream
send 校验完 cmsg 后不发布 payload/control。recvmsg 按 native 顺序先编码 credentials，再使用真实剩余 capacity 逐个预留 FD；不足 header 不输出，足够 header 可截断 ucred payload，cmsg_len 与实际消费空间分别遵循 native长度/对齐规则
。canonical ID/PID 只在 copyout 投影接收者 namespace；无保存凭据输出 PID 0 与 overflow IDs。SCM_RIGHTS 复用实际 OpenFdReservation：每个 FD 编号 copyout 成功即发布，之后的 scalar fault 只撤销当前未发布 slot；header fault 不撤销 prefix，
也不推进 cursor。credentials 的 put_cmsg fault 不推进 cursor，caller 忽略该错误并继续 rights。native name length/name bytes、flags、controllen 在 controls 后输出，故其 EFAULT 保留已发布 FD。UnixSendCredentials 和 UnixCredentials 仅是调用输入和
实际 transport 历史身份，既不拥有 Task，也不延迟 PID registration 退休。

FallibleMap::last_key_value 新增只读 O(log n) 查询供实际 frame tail coalescing，沿真实 tree 读取，未增加全局、cache、计数或镜像。新 codec/plan/锁路径的 benchmark 决策是沿用 unix-rights-bench 的实际 hot CPU 200 ns 阈值并报告 credential
frame cold admission，所有 source/host/guest/perf 用例待全部实现结束后统一执行。

AF_UNIX oriented lifecycle 与 pathname 源码：Stream/SeqPacket 共用 ConnectionBacklog 与 ConnectGuard；准备 transaction 只持 listener capacity，client 保持实际状态；最终双端 publication 重查 client、listener SHUT_RD 与当前 shutdown，竞
争失败不会恢复/覆盖另一个成功连接。没有临时 Connecting state。失败 rollback 在 listener guard 外销毁 staged server，再释放真实 reservation 并通知 waiter。packet 不分配 stream byte Pipes。accepted child 在实际 connect commit 继承
SO_PASSCRED，connect 捕获 listener 最新 peer credentials。Stream/Datagram 的 peer 和 queued packet source 直接持实际 endpoint Arc observer，最后 OFD close 消费真实 state，断开 cycles，保留历史地址值；peer close/error 投递必须匹配实际 peer node
identity，未发布竞争失败的 child 不能污染胜出连接；观察者不保活 transport。pathname binding 保留由 VFS 创建/授权的实际 inode Arc，仅提供 lifetime capability，不向 socket 暴露 storage 操作。accepted child 克隆同一 inode pin；实际最
后 OFD close 在 address guard 外释放 pin。unlink/recreate 按新 inode identity 路由，不能复用仍被 live endpoint 持有的旧 inode identity。

未读消息或实际 RightsOwner::Backlog 的未 accept child 关闭时，向 oriented peer 发布 ECONNRESET；pending_error 仅属于 Stream/Datagram 实际 state。packet receive 在队列观察/peek 分配前消费错误，stream 先 drain queued byte prefix；SO_ERROR 原子取
走同一错误。错误发布先于 direction retirement 唤醒，避免先读到 EOF 而遗失 reset。StreamReceive 的 read_closed 只属于实际 DirectionState：原生 SHUT_RD 禁止后续 write admission，但保留当前 actual chunks 供读取。SHUT_WR 消费真实 writer
capability，EOF 来自同一 DirectionState.writer_closed。Initial/listener 的 shutdown 属于其实际 variant，accept 先取已有 child，read-shutdown 空队列返回 EINVAL。

benchmark 决策：半关闭与 close/reconnect 为冷控制路径；实际 stream read/write/poll 多一次已持有 DirectionState 内的 boolean 判断，沿用 unix-rights-bench 原有门禁。OFD/epoll 增加一个 fixed source slot 与冷 admission node，不另设较宽阈值
；保留原 epoll owner/runtime gate。host 完整 endpoint fixture 与 native guest 覆盖未读 RESET、SO_ERROR 一次消费、stream/packet 不同错误顺序、half-close drain、datagram disconnect/消息丢失、pathname inode pin/accepted 继承/observer 释放。执行结果
待全部实现完成后集中验证。

packet sender budget 源码：UnixSocket.send_memory 的唯一 Mutex<SendMemoryState> 拥有 native send limit 和 actual used；创建直接捕获 CoreBufferLimit 全局 default。core_buffers 的四个 AtomicU32 是原生全局调优唯一值，Relaxed scalar 对
应 READ_ONCE/WRITE_ONCE，不发表其他对象；max 初始 4 MiB，default 初始212992。proc actual per-open read/getter 不保存副本，open 与每次 write 检查权限；非零 read offset EOF。SO_SNDBUF/RCVBUF 在同一 owner 应用 unsigned clamp、倍增及本域实际
payload/frame metadata floor。RCVBUF 保持 native Unix hint，不伪造 receiver byte limit；NETNS 与其他 backend 继续接入。SenderCharge 同时持 actual sender endpoint observer 与实际 payload capacity/FIFO node allocation 费用；stream DirectionState.messages 的每
个 MessageFrame 直接持 actual payload Box slice、consumed cursor、SCM 与完整 SenderCharge；部分 consume 不提前退款，完整实际 chunk 退休才退 payload capacity 和 node fee。相邻同一凭据的 chunk 在 read plan 中无分配合并，writer identity 是 immutable
node id 而非 empty transport self-cycle；准备/排队期间收费，OOM、队列拒绝、consume、disconnect、GC/fput 全部在 endpoint/GRAPH guards 外退款并通知。peek 只克隆 metadata observer，不克隆或退还 charge。send admission 按 used < sndbuf，允许单
条分配跨越 watermark；POLLOUT 按同一 used 的 native quarter watermark。send allocation 使用 SendMemory capacity wait，不依赖更严格的 POLLOUT 门槛。receiver queue wait 记录实际 sender identity，peer 改变或 terminal/shutdown 时允许重查错误。

DatagramQueue 只拥有实际 AVL nodes/sequence，push/pop 的 maximum 是当前 peer policy 输入，无保存的 bound mirror；非互连 datagram 默认 max_dgram_qlen=10 在 qlen>10 时阻塞，因此允许第十一条。互连 packet/SeqPacket/self 不使用这个消息数限制
。AF_UNIX packet 取消 IPv4 u16 的固定 65535-byte 长度限制，由 actual sndbuf-32 校验。ReceiveBuffer.deferred 保留单一用户 logical limit，Vector capacity 是另一真实 storage 事实；actual front packet 决定需要预留的 bytes，reserve/peek FD copy
preparation 在 guard 外完成后重新查看队列。append 必须已有足够实际 storage，永不隐式分配；空队列不按巨大用户 capacity 分配，也不凭 initial staging 截断大消息。

显式 Unix destination 在 lookup/publication 间 close：实际 peer 匹配才按 native 断开并返回 ECONNREFUSED；非 current peer 的死亡显式目标返回 opaque DestinationChanged，让 syscall 在固定 flags/deadline 下重新解析真实 VFS/abstract 目的，不把
race 当容量 EAGAIN。connect 使用同一实际重解析语义；普通未找到仍返回原 errno，不重试。SocketSendError::Capacity 统一表达 sender memory 或 receiver queue 水位，旧 PeerFull 名称已移除，无兼容入口。

Socket scalar/writev/sendto/sendmsg 的唯一发送 engine 是 syscall::socket::message；fs sequential 分支只传 actual Task/OFD/Socket/iovec 输入，无第二份 gather/progress/timeout/restart/SIGPIPE。成功短写只推进实际 cursor，blocking 继续 suffix；nonblocking
在真实 backpressure 返回 prefix，signal/timeout/OOM 同样以已发送 prefix 优先。原 Socket::write 与 UnixSocket::write wrapper 移除，无兼容路径。sendmsg raw name/control copy 先于协议语义；SeqPacket 的 pending error/established preflight 先于 frozen
SCM 解析。Unix stream name 仅保留 ABI 已 copyin 的 presence，connected EISCONN/unconnected EOPNOTSUPP，不按 family/VFS 解析；SCM 错误仍优先于 stream name 错误。零长度 stream 不检测单独 peer SHUT_RD，但 own SHUT_WR/真正 peer fput 的 writer
permission 仍返回 EPIPE。Native vfs_readv/writev 的 empty iterator 在 access/import 后返回零，scalar read/write 继续使用实际 file operation 的 minimum-record/零长度语义。

AF_UNIX stream 的唯一 payload owner 已改为实际 fallible chunk FIFO。固定 64 KiB byte ring 与单独 ancillary span map 均已移除；每个 chunk 的 payload capacity、node、SCM、consumed cursor 与整块费用同生共退，无 byte/fee 平行账本。connect 仅分配
两个 one-byte direction notification，首次 send 才 fallibly 分配 actual chunks。read plan 按 actual PID/UID/GID 及 FD barrier 合并可见 prefix；部分读取保留整个 allocation 的费用，只有完整 chunk 消费/GC/fput 在 direction guard 外退款。read/write
half 权限和 EOF 属于 DirectionState；notification 只发布 edge，所有 I/O/poll level 从实际 owner 重查。三个 notifications 均可 drain，任何 waiter 不消费 payload。原 IPC Pipe 的 stream/peek 专用 mode/入口已移除，无兼容路径。Native payload
allocation 热成本纳入原 unix-rights-bench 门禁，不放宽阈值。

stream 的 read/write completion 直接使用 scoped StreamRead/StreamWrite，移除旧 Pipe transport/Full 分支；IPC Pipe 仅供该 domain 的 notification primitive。聚焦 rights host fixture 只做 immutable result 类型投影，完整 actual endpoint fixture 使用生产
seam，无 I/O 算法副本。

Stream chunk 队列使用单一链式 FIFO：准备完整 Box 在 guard 外完成，追加/摘首 O(1)，read plan/peek 按消息顺序 O(N) 扫描。
首 chunk 直接提供凭据；SCM 控制只按本次 plan 已扫描的位置访问，不维持第二份索引。
退休移交同一 Box/费用至 guard 外，队列 Drop 迭代断链避免递归耗尽栈。尾指针只指向队列拥有的稳定 Box，所有 mutation 受独占 borrow 约束。

Stream payload 使用 exact-length Box slice，在发布前完整初始化；其 immutable 长度就是收费的 allocation layout。
Chunk 不需要 growable Vec 的 capacity 字段或追加能力，payload/节点仍分别在 guard 外分配并由同一 SenderCharge 退休。

SCM_RIGHTS capability 只在携带控制消息时拥有独立 Box，普通 chunk 不内嵌整个 graph preparation union。
控制 Box 在 guard 外 fallible 预留，同一 SenderCharge 按实际 presence 计费；graph 提交成功后无分配移交唯一 rights，OOM/close 保留 caller rights 并退款。控制及费用仍随实际 chunk 在锁外退休。

Control Box 内的 Option 在 FD 移交时清空，allocation 本身保留至整个 chunk 退休。因此部分读取不提前释放已收费存储，后续读也不会重复交付控制对象；无第二份 usage 或退款路径。
