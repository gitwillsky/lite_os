# Packet 与 Netlink 契约

IPv4 路由由同一 NET 的 FIB owner 决定，支持本地、直连和默认路由；LOCAL 重定向与
reverse-FIB spec_dst 在实际收包时冻结，接收投影 O(1)，typed metadata 不增加每包分配。
实际 wire 回归、路由查找及协议 loan/readiness 成本随统一门禁执行，门槛保持不变。
产品只装配单物理 NIC 与 lo；完整 policy/rtnetlink、多地址、multicast 与设备迁移不在支持范围。

AF_PACKET 源码已根修NET/link归属：PacketRegistry 删除全局首张NIC与固定ifindex，实际PacketSocket共享facade的SocketMemory并捕获创建NET。全局目录只持真实Weak membership/绑定/队列，net_id是不可复用的查找key，不强持NET/
设备；发送与getsockname查询同一NET实际interface/device。Ethernet RX token从factory捕获NET id/interface index/MAC，L3解析前tap；实际IP-medium lo同一queue保存EtherType并tap，非IP packet不送入IPv4，设备未被复制。原生ifindex0/protocol0禁用
/ETH_P_ALL/重新绑定已接source；explicit send可覆盖本次binding，missing出口ENXIO、DOWN ENETDOWN，down bind保留一次SO_ERROR。队列与observer pins保持native fput退出顺序。

Packet deferred观察先提取scalar projection并释放registry，再无等待观察实际protocol/device；竞争返回None重投，不持spin guard睡眠。NetworkPoll用8项固定数组转交实际completion/lo队列false→true设备indices，guard外只向同NET/该index及
any-device bindings发布容量edge，无跨域/跨NIC全局唤醒。actual independent-wire域/接口收发、重新绑定、protocol0/ALL、lo非IP、close/OOM与negative围栏已补源码。benchmark决策：scope筛选只增加固定整数比较、无新增每帧分配
；既有payload Vec+Arc一次shared publication保持两次分配，统一门禁执行实际wire/OOM fanout成本与原热路径门槛。RAW/DGRAM 与 outgoing tap 共用下述路径；packet rings 与通用调优未开放，不宣称完整AF_PACKET。

AF_PACKET RAW/DGRAM与OUTGOING tap已补同一路径源码：实际SharedPacket保存一次payload Vec/Arc及固定link_header，RAW输出header+payload、DGRAM仅payload，不复制第二份queue。LO仍是实际IP-medium device，唯一queue的metadata保存原生link
header供packet projection，原始RAW header不被重建；非IP或OTHERHOST只供packet tap，不进入IPv4。packet_snd显式出口/协议不改binding；RAW缺省出口用实际binding，检查真实设备MTU加header；通用gather上界65550不把lo/RAW截为1500。硬件提交
和lo共同的codec/queue准入发表原生OUTGOING，排除发起该packet socket；incoming按原生再交付。14-byte metadata不新增每包分配，existing fanout两次payload Vec/Arc准备保持，实际wire/lo/MTU/RAW/DGRAM/origin-exclusion回归与negative围栏随统一门禁执
行。packet ring/fanout、通用缓冲调优与完整 RIB 不支持。NET publication 范围见下文。

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

本轮支持目标已收敛为学习型内核、一张物理网卡加 lo 的 IPv4 TCP/UDP。IPv6、多网卡、
rtnetlink/veth/设备迁移、多地址、组播、通用 raw protocol 与复杂 PMTU 策略不作为本轮
新增要求。已有入口须符合声明范围并通过集中门禁。
