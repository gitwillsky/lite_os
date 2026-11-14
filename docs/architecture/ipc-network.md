# IPC 与网络当前架构

Linux syscall、阻塞超时、namespace 授权、AF_UNIX/NETLINK 属于 `system/linuxd`；IPv4/TCP/UDP/RAW、FIB、端口与 AF_PACKET 队列属于 `system/netstack`。下述协议 owner 通过 `network-protocol` 交互，不共享内核状态。

## 网络服务拓扑

1. netstack 只提供协议栈：启动时创建只有回环（启用）的默认网络，经 `liteos.network.Control` 交出网络域与 socket，
   经 `liteos.network.Installer` 接受网卡会话并在授权的网络域中安装为 `eth<n>` 接口。默认网络随 netstack 存续。
2. router-config 是接口配置 owner（Fuchsia netcfg）：经原生 RIB 向 netstack 委托 Network 类目录能力与设备 ID；netstack 按自己的 worker 策略打开并独占队列；每个接口的配置依次取用户设置、
   bootfs 只读默认策略（`data/router-config/default.conf`，`iface * inet dhcp`）、DHCP。静态地址直接配置；DHCP 经
   dhcp-client 取得租约后由 router-config 向 RIB 提交接口意图；RIB 选默认路由，再经原生执行事务原子配置地址、掩码和 FIB；DNS 由配置/成功租约投影。
   执行服务确认前保留原 FIB 根与会话，避免逐项设置中途失败。用户设置持久化在 router-config 的私有数据目录，存储身份与进程名独立，保留既有设置。
   MTU 与地址/管理状态/默认路由由同一原生事务发布和撤销，单独修改 MTU 保留有效 DHCP 租约；批次经只读 VMO 传输。
   提交失败撤销原文件与可逆执行；持久决定后的确认丢失按新决定重试，快照明确投影提交状态，见[协调契约](../architecture-contract/router-configuration.md#原生接口执行事务)。
   一个事件循环线程以一个 Port 等待 netstack 断开（重连、重新安装网卡、重新配置）、设备目录变化（热插拔网卡）与
   各接口的租约事件；设置变化经 `liteos.network.Settings` 提交，先暂存可逆执行，再持久决定和确认。
   原生配置使用具名接口对象与独立设备匹配。配置模型和字段元数据归 router-protocol，router-config 将配置 JSON 与近期导入编号保存在同一私有文件。
   已发布配置是不可变 Arc 快照，导出/备份不复制整树。自动备份由同服务的后台执行线程处理周期、确认和轮转，不持配置锁等待存储。
   备份策略与目录管理见[配置备份契约](../architecture-contract/router-configuration.md#配置备份)，恢复仍走唯一整份导入。
   加载失败保留管理查询、禁用设备并拒绝局部覆盖；授权整份导入可以修复，不退回 DHCP，不使关键服务进入崩溃循环。
   原生配置导出/校验/字段元数据/对比及带代际的完整替换复用授权设置通道；接口修改与导入共用整批 NET/DHCP/持久化提交。
3. dhcp-client 为 router-config 指定的接口在默认网络的 datagram packet socket（EtherType IPv4）上收发 IPv4/UDP/DHCP 报文，
   按 RFC 2131 推进 INIT → SELECTING → REQUESTING → BOUND → RENEWING → REBINDING，向 router-config 报告租约取得、
   续租与失效。T1 使用租约服务器的 IP 和链路地址单播，T2 广播，两个阶段均不携带 Server Identifier。
   续租未改变地址、前缀与网关时，router-config 保留现有地址、路由与连接时长，并发布新的租约期限与生效 DNS。
   router-config 关闭租约事件 channel 即停止该接口的 DHCP。报文编解码使用无分配的 packet-wire；
   实际格式与失败边界见[报文格式契约](../architecture-contract/ipc-network.md#报文格式)。
4. linuxd 的初始网络域连上默认网络，新网络域在 netstack 中创建只有 DOWN 回环的独立实例。netstack 重启后 linuxd 重连并为
   全部存活网络域重建协议实例，旧 socket 的 channel 已随旧实例关闭，之后的操作以 `ENETDOWN` 失败；默认网络由 router-config
   重新配置。linuxd 重启不影响默认网络及其配置。

5. Linux 侧不运行 DHCP 客户端或接口配置脚本。`/etc/resolv.conf` 由 resolvd（init 监督）依据 router-config 的设置快照维护。
   `router-api` 向本机用户提供只读接口快照，按账户组开放观察及管理。控制面板经同一网关读取状态和修改设置。
   原生设置端点由网关私有使用。[网络管理入口](../architecture-contract/userspace-abi.md#网络管理入口)
   接口管理开关由 netstack 持有。以太网载波与速率由网卡驱动读取，并随接口快照投影到 Linux sysfs 和 rtnetlink；
   VirtIO 按协商的 STATUS 和 SPEED_DUPLEX 读取同一配置代际，未协商速率时显示未知。

获授权的 RIB 和配置服务可打开具名路由域。域内接口、转发表、连接和套接字由同一个 netstack 域持有，各域使用独立的 RIB 和通知对象。物理设备同一时刻只属于一个域，改绑时先撤下旧接口和相关路由，再在新域打开队列。
具名域的接口、静态路由、选路规则、防火墙、BGP 和 OSPF 随系统加载。动态路由协议接收同域网络连接与路由来源。授权本机程序可取得对应的网络命名空间描述符。[启动方式](../../user/libraries/network-config/README.md#在指定路由域启动应用)
配置服务可在打开设备前从目录读取 MAC。接口释放通过原有撤回路径清理旧域的接口和路由，网卡的独占会话结束后才可重新安装。

socket 控制与数据分离：低频配置/关闭走 channel，send/receive/poll 只走 `LSOCK001` 共享 SQ/CQ 与持久载荷槽。
复用现有 native VMO/Event/Port；调用者领队推进完成，不增加逐 socket 转发线程。netstack 的独立 dispatcher
也按需增长并收缩空闲 worker。完整布局、所有权、失败与顺序边界见[传输契约](../architecture-contract/ipc-network.md#socket-服务传输)。

## 当前设计

- IPv4 FIB 在唯一 RoutingTable 内共同保存连续有序记录、共享下一跳组与 prefix-index::Poptrie 实例，前缀直接引用候选区间，再按实际 alias/table/metric/设备裁决。
  内联路径按值共享，具名对象按配置身份独立；记录直接引用所属根内的槽，不增加报文锁或查询身份/路径树。
  完整配置交换候选根，具名独立更新只交换一个槽的成员数组，均复用原确认/撤销能力；热拔出后过滤实际失效路由。
  当前仍持 NET guard，新前缀与整表候选仍冷编译，按实际源/目的地址带权选择，成员按出口分组裁决；完整组快照经只读 VMO 传输，rtnetlink 输出标准多路径，不能宣称 ECMP 产品能力或 RCU 完成。
  生命周期与边界见[FIB 前缀索引契约](../architecture-contract/ipc-network.md#fib-前缀索引)。
- routing-rib 的不可变候选库与 FIB 共用 Poptrie；原生服务已装配并接管接口/静态意图与授权来源会话，属性/下一跳共享，替换不修改旧读者。
  冻结重选可撤回失效选择、采用备用并恢复递归依赖；实际接口/载波变化由 NET 直达 RIB，配置服务不再中转；独立来源替换/关闭撤回已接入，协议状态机和通用递归依赖订阅仍待实现。
  router-protocol 的双栈来源正文已接到该目录，resolution 对冻结选择做显式栈递归、环路剪枝和精确带权出口展开。
  结果组共享且退休无引用中间组；接口来源/RIB/NET 的 host 链路覆盖发布、撤销及热拔出备用切换，原生 RPC/VMO 尚未验收，见[候选目录契约](../architecture-contract/ipc-network.md#rib-候选目录)。
- 受管 IPv4 FIB 与接口计划可在一个授权 VMO 批次中共同预备，具名定义/内联/引用路由基于新接口事实准入，只构建一个最终索引。
  无 FIB 段保持原集合，显式空集合清除来源与定义；同一写租约、根交换与接口前像保证发布/确认/撤销一致。
  该执行入口不提供 RIB 选路，也不代表 routing-rib、ECMP 或双栈完成，见[集合契约](../architecture-contract/ipc-network.md#原生受管-fib-集合)。
- `ipc::IpcNamespace` 拥有实际 System V message/semaphore/shared-memory registry、标准 key/ID/权限与限额；
  Task 只持 membership。SemaphoreSet 原子处理多项 sembuf，SemUndoHandle 按实际共享者最后退出
  撤销；共享内存复用 memory 共享页和 VMA 退休，RMID 撤销 key 后按最后挂接延迟销毁。
  创建线程实际退出会退休独立 clist，`shm_rmid_forced` 按 orphan/VMA lifetime 清理。
  SHM_LOCK/UNLOCK 按实际 UserNS real UID/祖先限额收费，重复锁不重复计费，解锁/销毁
  归还原收费者。当前没有换出；backing 的唯一 lock policy 只维护 SHM_LOCKED/配额，不强制 fault-in。其余 POSIX IPC 接口未开放，支持范围见 syscall 矩阵。
- `ipc::Pipe` 唯一拥有 byte ring、endpoint、atomic write 与 readiness generation；notification pipe 只传递合并 edge，不复制 data readiness。
- epoll 在 ctl 阶段以持久 source index 精确更新 ready membership，wait 只向
  sharded WaitRegistry 发布单个 epoll notification key；ppoll/pselect 与 blocking I/O
  仍使用 transient source-key seam，两者在唤醒后都复查 backend level state。source notifier
  不等待 IPv4 `TaskMutex`；owner 竞争时先保守保留 ready membership，由 task-context delivery
  做精确 level recheck，避免 deferred callback 睡眠或丢失 edge。
- `net` façade 拥有 domain dispatch；AF_UNIX namespace/queue/SCM graph、IPv4 stack、
  AF_PACKET registry 与 kobject listener 各自拥有复合状态。IPv4 `TaskMutex` protocol owner 保持
  唯一 TCP/RAW `SocketSet` 与原生 UDP 目录；载荷操作临时借出实际传输对象，在 owner 外
  复制 payload；独立 endpoint 只共享 O(1) loan count，不共享互斥 guard、也不互相串行。poll 只在
  全部 loan 归还时 `try_lock` 完整 state，冲突立即回投而不 busy-wait。
- AF_UNIX 最后 OFD close 与 Arc observer 的 lifetime 分离：实际 Closed state 先发布，再撤销
  binding、关闭 stream/notification endpoints、清理 controls 与未 accept backlog；SHUT_RD 不让
  peer transmit 的 Direction pin 延长 rights lifetime。pending server 以真实 graph capability
  连到 listener，accept 消费该 capability 转到实际 OFD root，不将缺失 Weak 当成 live creator。

- 实际 fput 同时覆盖 Inet、AF_PACKET 与 KOBJECT_UEVENT：撤销协议/订阅注册和队列，
  backend observer 只能查看 closed 状态，不能延迟端口释放或重建绑定。AF_PACKET 每 endpoint
  在打开时锁外准备 64-slot queue；RX 仍仅分配一份共享 payload 的 Vec/Arc，packet registry
  内的 publication 不分配，receive 的 payload copy/最终退休也在该 guard 外。
  冷路径的新增容量准备由 OOM/关闭观察者回归覆盖；现有 packet cost gate 保持每 frame 两次
  payload allocation，并新增 spin guard 内无容量分配的证明，不增加热路径性能阈值。

- pipe、AF_UNIX、IPv4、AF_PACKET 与 kobject receive 共同写入 `ipc::ReceiveBuffer` 的
  initialized prefix；短读、control barrier 与错误路径不暴露未初始化 capacity，syscall 不保留
  另一个 zeroed staging 路径。两条 64KiB heap receive 的预清零成本由 131,072B 降为 0。
- `netstack` 独占双栈主机协议、邻接和设备收发。Linux socket UAPI、fd 与 errno 由 Linux 兼容层持有。[IPv4](../architecture-contract/ipv4-transports.md)、[IPv6](../architecture-contract/ipv6-link.md)
- UDP/TCP 各自的 `PortNamespace` 是 local tuple 占用唯一 owner：per-port summary
  与双族 exact 地址索引保留 wildcard/`SO_REUSEADDR` 语义，按地址空间选择的 ephemeral bitmap
  只投影“整个 port 完全空闲”。TCP listener claim、accepted exact tuple 与 active
  connect source-address 迁移都在该 owner 内 prepare/commit；raw socket local port 固定为 0，
  不参与 UDP/TCP port namespace。
- network hardirq 只确认设备并发布 deferred work；packet processing、completion reclaim 与
  waiter notification 在 user-return/idle safe point 的有界 deferred batch 中执行。deferred poll
  用一次 exclusive `TaskMutex` owner 推进 device completion、ingress/egress，并提取最多 64 个
  readiness endpoint Arc；竞争 task 睡眠而非自旋，存在 endpoint payload loan 时 poll O(1) 回投。
  释放 owner 后才进入 wait owner 发布 readiness。final socket cleanup 若与 poll 竞争，只发布到
  1024-slot fixed cleanup ring，并由下一轮 poll 在协议推进前按 64 项固定预算精确 drain，不存在旧
  protocol gate 或第二份 stack。

## Known limits

- 当前双栈 TCP/UDP 支持多接口与 IP-medium loopback；有限 raw ICMP、send-only IPPROTO_RAW、AF_PACKET 与 KOBJECT 保留声明范围。真实硬件和完整路由系统仍待验收。
- IPv6、多网卡、完整 rtnetlink/veth/设备迁移、multicast 及高级 TCP 扩展不作为本轮要求。
- IPv4 分片/重组未开放；UDP/raw 根据真实出口 MTU 在入队前拒绝超长报文。IPPROTO_RAW 使用实际队列和唯一 FIB/neighbor engine，默认 HDRINCL 且不接收；基本完整 header 发送保留用户 ID/TOS/DF/protocol，未开放形态明确拒绝。
  INET 保留固定有界 transport buffers，未开放的缓冲调优明确报错。

AF_UNIX MSG_PEEK 已接入实际 stream/datagram owner。stream 从同一实际 message chunk 队列按 consumed cursor 读取，peek 不推进 cursor/FIFO 或退款；datagram 借用首个实际消息，不摘 FIFO node，不释放容量。SCM_RIGHTS 的 peek 输
出是 UnixReceivedRights 中独立 OFD Arcs，不修改原消息的 graph edge/UID inflight 收费。复制 Vec 的 capacity 在 transport guard 外准备，再复查当前首记录长度；分配失败没有 byte/control progress。普通消费在 guard 外从 UnixRights 退休
graph ownership，将真实 files 转给相同接收 capability 类型。

AF_UNIX 显式 bind 与 family-only autobind 共用 namespace::register publication。自动地址是 NUL 加五个小写十六进制字符；以实际 endpoint node identity 为搜索 seed，逐个遍历全部 2^20 候选，唯一性由实际 namespace collision transaction 保证，
不另建占用计数或地址缓存。abstract key 包含 socket type，pathname key 仍是实际 VFS inode identity。预留一个 AVL node 后碰撞复用，重复 autobind 不分配；全部名称耗尽返回 ENOSPC。最终提交持实际 endpoint state guard，关闭后的
observer 无法重新发布 binding。

SocketTimeouts 是收发超时唯一 owner；linuxd OFD 与 INET facade 引用同一个 policy Mutex。Linux LP64 old/new timeval 在 syscall codec 验证：非法 usec 为 EDOM，零禁用，负秒立即到期，合法有限时长按实际 scheduler tick 上取整并在 KTIME_MAX
饱和。操作冻结一次 host-monotonic deadline，read/write/send/recv/accept/TCP connect 的实际等待全部交给已有 wait registry；源唤醒、队列容量变化和 option 后续修改不重置原期限。无限等待的 signal interruption 允许 SA_RESTART，有限超
时保持 EINTR。UNIX accepted endpoint 按 unix_create1 默认禁用超时；TCP 协议选项由 netstack 在握手完成时冻结；Linux 收发超时目前在 accept 时继承 listener，存在固定 Linux 语义差异，见 socket ABI。短 getsockopt 返回实际复制长度，负
容量拒绝且不消费 SO_ERROR。

AF_UNIX blocking connect 与 datagram send 共用 SocketCapacityWait 和既有 wait registry。Backlog/PacketQueue 是实际 owner 的等待条件，拒绝保留第二份容量缓存；终态或类型变化视为可重新检查，避免最后关闭的 edge 在注册前被消费后永
久阻塞。UNIX backlog 实际容量是 Linux pending > backlog 条件对应的 backlog + 1，已领取的未发布 reservation 同一 ledger 计数；accept 锁外通知容量变化。connect 满队列先归还实际 backlog reservation 再等待，重试重新解析实际
namespace/VFS identity，全部 transport allocation 仍在 admission 之后。一次 socket 调用冻结 O_NONBLOCK/MSG_DONTWAIT 与 deadline，后续 option/flags 修改不改变该调用。空 SeqPacket recv 也在本端 guard 外投影 peer 终态，消除两端同时 recv 的交
叉锁等待。

SCM_CREDENTIALS / SO_PASSCRED 已接通源代码。消息身份和 peer 身份分别在发送和连接时捕获；发送默认使用 real IDs，peer 使用 effective IDs。stream 用唯一 MessageFrame owner 同时保存字节范围、凭据和 FD，普通无
controls 的字节不分配 metadata；相同实际 PID/IDs 的无 FD frame 原地合并。接收时按真实凭据边界截断，peek 保留全部队列状态，FD 首次 consume 即转交或关闭而凭据保留至 span 消费完。native零长度 recvfrom/recvmsg 仍处理 controls
，普通空 read 和零 stream send 不提交新的控制消息。输出 credentials 先于 rights；实际文件表的单 FD reservation 在编号 copyout 后逐个发布，后续 control/name/msghdr fault 不撤销已安装 prefix。

SCM_RIGHTS native copyout 根修已补源码：删除整批 receive transaction 与重复 FD admission 接口，普通 open/clone/SCM 共用原实际 FileTable reservation。控制头故障保留已安装 FD 且不推进 cursor，FD 数组故障截断未接收尾部；
credentials/IP_PKTINFO 的 put_cmsg copy fault 按 native caller 忽略。controls 后依次写回 name length/name bytes、flags、controllen。跨页 guest 覆盖 scalar/header/name/flags/length 故障和 credentials fault 后继续 rights。benchmark 决策：删除额外 Vec
allocation，沿用实际 FileTable 成本诊断入口测单 FD reserve/publish/close，不新增或放宽阈值，随统一 benchmark 执行。

INET option 根修已补源码：TCP/UDP/raw 共用每个实际 endpoint 的 InetSocketOptions；raw 的旧 hop/broadcast/device fields 与重复 setter 已删除。TCP 的 Nagle/TTL mechanism 经 apply_tcp 在新建 listener handle、accept 补位、断开重建、握手
冻结处统一应用，已建立 child 不吸收 listener 后续设置。IP_TTL 接受 native int/单字节与 -1 默认；IP_PKTINFO flag 在所有 IPv4 protocol owner 保存，UDP/raw 输出真实 packet destination。标准 getter 支持实际
TTL/NODELAY/REUSEADDR/BROADCAST/PKTINFO/BINDTODEVICE，遵循 IP 短值、device buffer 最小长度、IP/TCP optlen 先写与 SOL_SOCKET value 先写语义。UDP 的 queued TX 冻结 TTL，后续 setter 不改旧包。当前默认 TTL 仍为固定 64，完整 NETNS/core/IPv4 sysctl
owner 待接入，不能将此阶段宣称为 namespace 支持。

全局 socket buffer 调优已接通源码：`/proc/sys/net/core/{wmem_default,rmem_default,wmem_max,rmem_max}`
直接访问 socket 的唯一全局 scalar owner。默认值 212992、最大值 4 MiB；各参数独立，调优
不改已有 endpoint 的预算。AF_UNIX 创建时直接捕获 default，后续 setsockopt 按当前 max 做
unsigned clamp、INT_MAX/2 上界和倍增，floor 来自实际 packet/frame allocation metadata。
SO_SNDBUFFORCE/RCVBUFFORCE复用同一预算实现：initial UserNS 的全局CAP_NET_ADMIN授权，
绕过全局max，负值归零，再应用INT_MAX/2和实际floor；仍以普通SO_SNDBUF/RCVBUF读取。
原有 ledger/admission/readiness 热路径未新增查表、锁或分配，沿用其实际 owner 成本门禁；
新增全局参数只在创建/调优处访问。NET policy 随实际创建域固定；非 UNIX 动态缓冲调优不支持。

optmem 源码已接通：每个 socket 的 SocketMemory 持其实际 NET policy 和唯一
in-flight 申请字节 AtomicUsize。初始网络域的 optmem_max 为128KiB，proc 文件捕获实际
网络域，调优不重定向旧fd。36-byte control栈路径免收费；堆路径在分配/user-copy前按
size<=max、used+size<max 原子预收费，heap snapshot保留独占token到协议send结束。
OOM、copy fault、解析错误、发送失败/成功均归同一drop退款；费用只pin memory owner，不
保活Task/backend。并发 admission、预算调低、OOM/fault和原生三种UNIX socket guest
回归已补源码；实际reserve/refund有独立成本诊断。NEWNET 创建独立 optmem/transport/ports 与 lo；不支持物理网卡迁移，不声明完整 Linux NETNS。

NET准备及UNIX名字范围已补源码：新NetNamespace持创建者/祖先的UserResource::Net
独占费用，OOM和最终实际对象释放归还；独立optmem初值不复制parent调优。抽象UNIX索引
以实际endpoint固定NET id/address/type为key，同一registry事务完成binding和getsockname；
pathname仍以真实inode全局查找，跨NET持同一实际pathname inode不会产生假名。新建endpoint
固定真实NET Arc，被动UNIX child捕获listener域；registry只有Weak，不保活NET/OFD。
proc inode key/owner捕获实际NET，既有open不随后续lookup变化；非initial NET的全局
wmem/rmem只读，optmem修改实际本域policy。实际owner/分配失败/寿命/名字/readonly回归
及 negative 围栏覆盖实际 NEWNET/setns 的准备、发布与回滚。
INET endpoint已有scope直接解析实际protocol owner，deferred与timer用实际域的Weak目录
及独立公平cursor有界推进，不在IO增加目录查找。interface配置/快照显式指定实际NET，
标准socket ioctl查询其捕获域并在user-copy之后按owning UserNS NET_ADMIN重新授权；
目录准备OOM/最后Arc退款、独立wire的实际ports/error/close/UDP/TCP及guest权限回归
覆盖实际 NET membership 的事务发布；非初始 NET 默认只有独立 lo，不开放物理设备迁移。

TCP_MAXSEG 已补源码：标准 0 或 88..32767 配置由同一 endpoint policy 接收。
actual transport 在 active open/收到 SYN 捕获路由及 user MSS，sender cache 约束 Nagle、数据分段和 Reno。
ESTABLISHED 修改仅影响 next-open policy，不改变旧 cache；标准 getter 区分 CLOSED/LISTEN 的
user override 与实际连接 cache。listener 的 cache 属 parent mode，不借 pending child 反推。
AF_UNSPEC 清空 rx_opt.user_mss、保留 sender cache，零 buffer placeholder/新 storage 原子移交 cache。
真实 SYN/ACK 与 payload host 回归、原生短参数 guest 和 negative source fence 已写，沿用既有 transport 热路径门槛。

benchmark 决策：没有新增 heap owner/global/锁；TCP apply_tcp 在现有真实 handle transition 扩充一次 TTL mechanism assignment，UDP enqueue 增加真实 packet TTL 字段及一次 O(1) 写入。沿用实际 TCP 生命周期/UDP wire benchmark 门槛，actual option
query 在同一 producer 添加 release 成本诊断。

KOBJECT源码已统一实际NET归属/端口索引、native group/rebind/autobind、内存预算FIFO及
once-per-overrun ENOBUFS。未tag事件按owning initial UserNS覆盖所有NET；广播不持registry
分配/通知或释放最后Arc。fput退休port/Weak/queue，观察者不能重绑或保留实际订阅。
标准DGRAM/RAW与跨NET同port、old-OFD、重bind/OOM、64条真实FIFO/peek/过载恢复源码
回归及成本诊断已写；标准 userspace send/connect 和其他协议未开放。

接口MTU源码已归实际device，ioctl配置同时更新protocol分段caps与packet长度准入；
lo低于68销毁IPv4成员，恢复MTU保留无地址状态，重新UP创建默认地址。新host回归从
真实SYN MSS和实际packet frame观察变化，guest覆盖LO收发与退化/恢复。
