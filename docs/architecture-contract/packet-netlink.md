# Packet 与 Netlink 契约

Linux syscall、阻塞超时、namespace 授权、AF_UNIX/NETLINK 属于 `system/linuxd`；IPv4/TCP/UDP/RAW、FIB、端口与 AF_PACKET 队列属于 `system/netstack`。下述协议 owner 通过 `network-protocol` 交互，不共享内核状态。

同一镜像报文只在匹配至少一个可接收的端点后读取一次 UTC。时间戳与载荷同寿命，Linux OFD 仅保留控制项输出策略。抓包计数归端点，控制操作 22 原子读取清零。
普通接收与映射接收共享链路镜像点。每个端点只有一个接收目标。映射接收仅在至少一个匹配的普通队列需要载荷时分配 `SharedPacket`。
没有消费者时无时钟调用或分配。环发布、队列满和多观察者的成本随现有 packet tap 基准及整机小包转发验收。


IPv4 路由由同一 NET 的 FIB owner 决定，支持本地、直连和默认路由；LOCAL 重定向与
reverse-FIB spec_dst 在实际收包时冻结，接收投影 O(1)，typed metadata 不增加每包分配。
实际 wire 回归、路由查找及协议 loan/readiness 成本随统一门禁执行，门槛保持不变。
物理网卡和虚拟接口按所属路由域装配。Linux 查询与修改范围见 [socket ABI](../syscall-support/socket.md)。

每个网络域拥有一个 `PacketRegistry`，其中记录该域端点的弱引用、绑定和接收队列。物理接口、VLAN 与回环在安装时共享该目录，报文 token 只借用它。
端点归属与设备撤销行为参考 [Linux v6.12 AF_PACKET](https://github.com/torvalds/linux/blob/v6.12/net/packet/af_packet.c#L4006-L4052)。
端点捕获创建时的网络域。`bind` 在域协议锁内检查接口并更新绑定，接口退出也在同一顺序下撤下绑定并登记 `ENETDOWN`。通知在协议锁和目录锁外发送。
`ifindex=0` 可匹配任意接口，`ETH_P_ALL` 匹配所有协议。显式发送可覆盖本次出口，发送到缺席或停用的接口分别返回 `ENXIO` 或 `ENETDOWN`。
停用接口仍可绑定，`SO_ERROR` 可消费一次 `ENETDOWN`。最后的文件描述释放时移除目录成员，即使仍有观察引用也不保留订阅。

首个端点登记后在目录锁内 Release 发表非空状态，最后一个端点删除时清除。镜像入口 Acquire 检查该状态，空目录时不解析报文或取得锁。
Packet poll 在目录锁内读取接收状态，释放后查询出口。发送容量恢复和接口退出只通知该域相关绑定，每轮有界交付。直接 packet 发送产生新可读通知时唤醒本域。
性能验收分别测量无订阅转发、单域抓包时其他域的转发、以及同域多个订阅者的扇出成本。目录为空时每个镜像点只增加一次原子读，无逐包分配。

AF_PACKET RAW/DGRAM与OUTGOING tap已补同一路径源码：实际SharedPacket保存一次payload Vec/Arc及固定link_header，RAW输出header+payload、DGRAM仅payload，不复制第二份queue。LO仍是实际IP-medium device，唯一queue的metadata保存原生link
header供packet projection，原始RAW header不被重建；非IP或OTHERHOST只供packet tap，不进入IPv4。packet_snd显式出口/协议不改binding；RAW缺省出口用实际binding，检查真实设备MTU加header；通用gather上界65550不把lo/RAW截为1500。硬件提交
和lo共同的codec/queue准入发表原生OUTGOING，排除发起该packet socket；incoming按原生再交付。14-byte metadata不新增每包分配，existing fanout两次payload Vec/Arc准备保持，实际wire/lo/MTU/RAW/DGRAM/origin-exclusion回归与negative围栏随统一门禁执
行。Linux 映射接收范围见 [socket ABI](../syscall-support/socket.md)。NET publication 范围见下文。

标准NEWNET publication源码已接实际owner：socket::create_net_namespace先完成
ucounts/NET identity准备，再使用同一prepare_network factory构造独立SocketSet/ports及真实
DOWN、无IPv4的lo；成功才返回给Task的PreparedCloneNamespaces，不复制旧设备、地址或协议。
所有fallible allocation位于Task credential/membership publication前，失败回收实际域与收费。
标准NamespaceOwner由NetNamespace实现，nsfs/proc net selector及PIDFD_GET_NET_NAMESPACE
保活同一Arc；initial NET使用标准inode映射。setns双SYS_ADMIN授权后仅替换该Thread唯一
net membership，retired Arc在guard外释放；旧OFD/accepted socket/proc fd不会跟随当前Task。
clone/clone3/unshare、NEWUSER|NEWNET的actual新User授权均接入同一prepare→commit，无
备用namespace/协议engine。新增功能使用既有锁/Once/ucounts/8域deferred目录，无新热路径
allocation；完整构造是冷路径，OOM/退费/nsfs pin/真实loopback与原生权限回归。
当前 FIB、MTU、transport budgets 与 KOBJECT 域归属见对应 owner；完整 rtnetlink、多地址、veth/设备迁移暂不扩展。

### KOBJECT netlink实际域与接收owner

KobjectSocket固定持facade同一SocketMemory，既有OFD不随setns重定向。Registry仅有
endpoint Weak与(NET id,port)→endpoint id的标量索引；端口准入、binding和fput同一事务，
显式冲突EADDRINUSE，0以calling visible TGID自动选择，冲突负port不全表升级socket Arc。
标准RAW/DGRAM共享一条endpoint路径，不要求INET NET_RAW。group0、同port重bind及组mask
截断由实际owner完成；已绑定重bind不分配，OOM不能留下半个绑定或改旧订阅。

全局SEQNUM归registry，但广播只捕获固定publication上界并逐个取Arc后释放guard；
enqueue、allocation、readiness及临时Arc释放均在registry外。未tag DRM事件按owning UserNS
筛选所有NET，不只发initial NET，也不向child UserNS泄漏。新增queue node allocation属于
事件实际payload，receive锁外释放；已有TCP lifecycle benchmark增加绑定与广播/消费成本诊断，
不放宽热路径门槛。仅声明当前完整的协议子集，其他 netlink 协议与完整 rtnetlink 暂不扩展。

UDP/raw queue冻结source及logical oif；TCP/socket准入与本轮egress_routes scratch共享routing.select。
每slot选择在transport/neighbor mutation前向library Context传当前next-hop/logical iif/broadcast；
库派生上下文在poll结束清除。LOCAL实际经lo交付，PacketMeta和唯一lo queue记录logical iif，
AF_PACKET继续观察真实物理lo。Ingress读取actual header边界，全部校验/协议解析仍是原库engine；
FIB LOCAL和reverse-FIB spec_dst在实际RX enqueue之前冻结，不从recv时的新地址重算。

RouteReplies是同NET独占的即时IP回复队列，最多64records、实际Vec capacity+tree-node总计
256KiB，allocation失败计入actual device TX drop；没有资源Arc/新锁/unsafe。每批最多8个回复
沿相同FIB/neighbor/codec输出，未准备neighbor/TX时保持payload，1秒重试调度而非另一个ARP
状态机；timer读取同一queue deadline。ARP保持link内回复。队列不能覆盖原packet，也不能
在OOM退回输入NIC发送。新2048-route lookup benchmark采用现有200ns门槛，不降低已有阈值；
配置/OOM/alias/local/down、真实gateway/asymmetric ICMP/native ioctl/proc回归和negative围栏
纳入集中验证。完整 policy/rtnetlink、多地址和 veth 暂不扩展；metrics 源码见传输契约。

硬件 fault 只归属实际 device owner，与 socket protocol error 分开。NetworkPoll唯一完整outcome同时持backlog、
8项actual-link TX-ready及8项actual-link首个typed诊断；没有“一个device错误覆盖整个Result”
分支。退出NET guard后同轮所有成功link的AF_PACKET writer edges仍交付，RX/cleanup/通知
backlog全部保留。删去stack入口任意设备take_error、全域pending_device_error与各socket
readiness上的global EIO。设备RX/TX callback错误只属于actual PHY counter/diagnostic；
Socket SO_ERROR必须来自实际协议事件。AF_PACKET直接发送依然获得所选device同步错误。
Ethernet错误/oversize-drop/submit-failure counter沿实际NET mutex独占的Cell写入，与底层
成功RX/TX counter一起投影标准/proc/net/dev；oversize未交付帧不计成功RX，不双计drop。
无新锁/global/unsafe；NetworkPoll栈上固定array，事件扫描仍沿8-interface批次。
依据同一固定Linuxcommit的
[virtio_net.c](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/drivers/net/virtio_net.c)
receive_buf/start_xmit：丢帧与FIFO故障计device stats，TX失败best-effort drop，不给任意socket
制造sk_err。实际host源码补A RX fault同时B completion/RX65budget、一次诊断/独立counter/
不污染UDP或TCP/SO_ERROR，proc标准16栏实际errors/drop；新增negative fence拒绝丢backlog/
固定零counter/不消费诊断。完整actual NET poll1024-slot每handle成本沿既有200ns门禁，不
更改阈值；设备terminal recovery/限频诊断和
多 NIC、完整 rtnetlink、veth 与迁移不在本轮支持范围。

主机 IPv6 UDP、[自动链路地址/ND](ipv6-link.md)及其状态已接通；地址/DAD 由 RTM_GETADDR 查询，IPv4 ARP 和 IPv6 ND 均可通过 RTM_GETNEIGH 枚举或单条查询。
IPv6 多地址、双栈应用组播及路径 MTU 的支持范围见[链路网络契约](ipv6-link.md)和[传输契约](socket-transport.md)。IPv4 接口保留单地址模型。veth 与设备迁移待实现。
已有入口须符合声明范围，局部宿主用例不能代替集中系统门禁。

### NETLINK_ROUTE actual 域与 owner

`RTM_GETLINK` 通过实际接口索引配对链路元数据与 `IFLA_STATS64`，只返回两份快照中都存在的接口。同名接口被移除后重建时，旧链路不会取得新接口的计数。
原生网络域 `NETWORK_SNAPSHOT` 行以 u32 接口索引开头，后接原有带长度前缀的 16 字节接口名和收发计数。索引与逻辑计数在同一次 NET 读中取得。

`RouteSocket` 与 `KobjectSocket` 是两个独立 backend，共享同一张 `(协议, NET, port)` 端口表（`kobject::with_ports`/`choose_port`），
加锁顺序固定为 endpoint state -> 端口表，close 不在持有端口表时取 state。报文 codec 全部在 `net/rtnetlink/codec.rs`，
不读内核状态；快照只来自 `interface_snapshots`/`network_snapshot`/`address_snapshot`/`route_snapshot`/`neighbor_snapshot`，不保存第二份接口或 FIB，
`ifi_flags` 与 `SIOCGIFFLAGS` 共用 `InterfaceSnapshot::flags`。dump 不预先排队：endpoint 只持有冻结快照与游标，回复内存有界。
查询固定在打开 socket 时所属的网络域。进程切换命名空间或接收传递的 fd 后，已打开的 socket 仍读取原域。sysfs 视图随挂载者的域建立。
[Linux v6.12 rtnetlink](https://github.com/torvalds/linux/blob/v6.12/net/core/rtnetlink.c)
双族路由快照以完整组字典经只读 VMO 传输；AF_UNSPEC 在同一 NET guard 捕获两族，各族沿同一类型化 codec。Linux 每组只保留不可变表示布局，不按前缀复制成员。边界见
[路由快照契约](ipc-network.md#路由只读快照) 与 [UAPI 范围](../syscall-support/socket.md#netlink_route)。
非 dump 的 RTM_GETROUTE 默认查询本机输出。RTA_IIF 选择输入路径，RTA_OIF 仅约束本机输出。两族均可指定 u32 RTA_MARK 并在单条 RTM_NEWROUTE 回复中读取非零标记。
回复给出实际表、出口和立即网关；输入回复附 RTA_IIF 并保留源地址，输出可返回自动选源。异族网关沿用 RTA_VIA。查询不创建邻居、启动复探或建立逐流 FIB 缓存。
路由查询的原生错误经 Linux 网络域的 `socket_errno` 映射；netlink 与 socket syscall 共用该映射，接口不存在与规则拒绝分别返回 ENODEV 和 EACCES。
规则 dump 从同一个 NET guard 冻结生效规则及实际入接口名。地址族与优先级顺序稳定，接口消失后无法匹配的绑定不进入新快照。内建本地表和主表规则由 NET 投影。
Linux 按游标输出 RTM_NEWRULE 及 FRA_SRC、FRA_IIFNAME、FRA_PRIORITY、FRA_TABLE 和适用的 FRA_FWMARK/FRA_FWMASK，最后发送 NLMSG_DONE。快照只有一份正文，逐行投影不分配或查询接口。
RTM_GETRULE 接受标准 fib_rule_hdr，以及 BusyBox 使用的 rtgenmsg。规则写操作返回 EPERM；协议布局参考 [Linux v6.12 fib_rules](https://github.com/torvalds/linux/blob/v6.12/net/core/fib_rules.c#L952)。
点查在冷路径编码固定大小的请求和回复，实际发送和点查共用选表及选源。规则扫描和自动选源后的重选见[策略选路](ip-forwarding.md#策略选路)。
接收先在 guard 外预留 chunk 和输出容量，重取锁后核对需求，随后才推进游标；准备失败不丢消息，退休快照在 guard 外释放。

默认域与具名路由域由安装能力管理。linuxd 对这些域的接口及路由修改返回 EPERM，NET 在普通域配置入口检查同一边界。安装事务可发布完整配置。
检查仅发生在管理写请求，查询和数据转发路径没有新增锁、分配或查找。
应用自行创建的独立域支持已有链路、IPv4 地址及路由修改。syscall 层按 socket 所属 NET 的 owning UserNS 检查发送者的 CAP_NET_ADMIN，随后由同一 FIB 应用路由操作。

具名域的本机授权入口经 `SCM_RIGHTS` 返回真实 nsfs 文件。linuxd 按原生实例身份复用存活的 Linux 命名空间对象，所有者是初始 UserNS，进入仍走 `setns` 的双 `CAP_SYS_ADMIN` 检查。
nsfs 提供命名空间 inode 与设备后端，文件描述层统一打开并通知。`SCM_RIGHTS` 和 nsfs ioctl 的调用者各自负责发布文件描述。
域删除后已有 fd 和进程保留原对象。重新创建同名域或重启 netstack 后，重新打开得到新实例。旧受管域不会由 linuxd 重建为空的独立域。[Linux `setns(2)`](https://man7.org/linux/man-pages/man2/setns.2.html)
netlink 与 ioctl 共用 `Ipv4RouteError::errno`。现有来宾夹具在独立域执行写操作，并通过旧 socket 检查默认域的接口及地址、路由隔离。范围见[ABI 矩阵](../syscall-support/socket.md#netlink_route)。
原生路由配置回复追加错误码 10 表示无直接配置权，现有错误码保持原值。

地址 dump 统一使用原生 AddressSnapshot 的冻结行，按请求族逐条生成，不把每接口固定成两个地址槽。
IPv4 与 IPv6 共用 RTM_NEWADDR 投影；::1/128、以太网自动/静态地址的前缀及资格来自实际 owner，无直连意图的 IPv6 地址带 NOPREFIXROUTE；接口号不能表示时在整批发布前返回 EOVERFLOW。
轻量 InterfaceSnapshot 独立于完整地址列表。地址消息与接口 codec 见[地址查询契约](ipv6-link.md#地址查询与版本)。
