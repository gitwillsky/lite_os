# 原生 Socket 服务传输契约

从 [IPC 与网络契约](ipc-network.md#socket-服务传输)进入；本文件拥有 `network-protocol` 的控制地址编码、共享数据队列、生命周期及顺序边界。
Linux sockaddr/cmsg/errno 只由 linuxd 编解码，native-abi 不增加应用接口。

## 共享队列与所有权

- `network-protocol` 控制 channel 只承载低频配置、创建、关闭和一次 QUEUE 协商；send/receive/poll 的旧 RPC codec 已删除。
  数据面版本 `LSOCK006`：固定 8 槽 SQ/CQ、128 字节记录与每槽最多 256 KiB 载荷，两个 Event 合并请求和完成通知。
  AF_PACKET CQ 的 word 8 保存 checksum-state(bits 0..1，0 未验证/1 待补全/2 已验证，3 非法) 与 network-offset(bits 16..31)，其余位及 words 9..15 为零；
  帧入队即冻结，PEEK/短读取保持原信息。Linuxd 仅在 OFD 启用标准 PACKET_AUXDATA 时输出 cmsg，不改变捕获队列。
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

### 定长帧接收环

Packet endpoint 控制操作 23 接收版本 1、是否启用和启用时的布局及一个 VMO 句柄。Linuxd 分配定长匿名 VMO，拥有 Linux 选项、槽布局和 VMA 租约。
netstack 校验大小及对齐、提交物理页并建立本地映射，成功后用环替换队列，旧队列内容和卸下的环在 registry 锁外释放。
每槽的对齐所有权字为零时可写。生产者先写入长度、偏移、时间、地址和载荷，Release 发布所有权。消费者 Acquire 读取后以 Release 归还。
写入位置由已验证布局与服务端游标决定。消费端可写的数据只用所有权字判断槽是否空闲。成功发布后沿用 endpoint 就绪通知，读取不经过 SQ/CQ。
环映射保留真实 socket 和缓冲租约。活动环不能替换，最后一份映射及其继承释放后可撤销，随后恢复普通接收队列。

## Internet endpoint 与格式版本

- 原生 `InetAddress` 使用标准 `core::net::SocketAddr`，分开保留 IPv4 与 IPv6，不通过截断或 0.0.0.0 回退伪装另一族。
  IPv6 包含完整 128-bit 地址、16-bit 端口、32-bit zone 与原样 flowinfo；zone 是逻辑接口标识，不是 hardware identity。
  TCP/UDP 使用同一双族端点与端口 owner，各自独占协议队列；RAW 使用独立精确协议号，双族范围由 [RAW 契约](raw-ip.md) 维护。
  域的 SOCKET 请求携带 u32 版本 1、地址族 u8（4/6）、传输类型 u8（0 UDP、1 TCP、2 RAW）；RAW 再携带非零 u8 IP 协议号。
  拒绝未知版本、类型、零 RAW 协议、额外 handles、缺失或尾随字段；副作用前预留回复空间。两端同步升级，不接受旧格式。
- 控制地址由 `network-protocol::address` 编解码：首 u8 为 IP 版本 4/6，IPv4 后接原生 u32 地址值和 u16 端口；
  IPv6 后接长度为 16 的 bytes 地址、u16 端口、u32 flowinfo 与 u32 zone。字段使用现有 Writer 小端数值编码，地址 bytes 保持网络序。
  解码拒绝未知族、地址宽度错误或截断，不补齐缺失字段。
- SQ/CQ Internet 地址 word 4 为 0/4/6：0 表示无地址，此时 words 5/6/7/12 全零。
  IPv4 的 word 5 为 u32 地址值，word 7 为端口，words 6/12 为零；IPv6 的 words 5/6 为两个网络序地址半部的 u64 值，
  word 7 为端口低 16 位加 zone 的随后 32 位，word 12 为 u32 flowinfo。所有超宽或保留高位拒绝，记录本身仍按 AtomicU64 小端交接。
- SEND 的 word 8 是有序控制项数，words 9..11 为零；payload 槽先放每项 24-byte 控制记录，再放应用数据。
  word 2 只计应用数据；前缀与正文合计不得超过 256 KiB，溢出在发表 SQ 前拒绝。记录 tag 为 4/6（IPv4/IPv6）或 0x84/0x86（该族控制内容无效），
  三个保留字节为零，随后 u32 小端接口与 16-byte 源地址；IPv4 只用地址前四字节，余下必须为零。无效项除 tag 外全零。
  无效项保留顺序，由实际目的族决定是否适用；不能提前合并而丢失中途错误，也不能把 Linux cmsg 布局传入原生服务。
- RECEIVE 的 words 8..11 是请求的接收信息：tag 0 无信息（余字段全零）、1 IPv4、2 IPv6、3 双栈 IPv4。
  word 9 是非零 u32 ingress 接口；IPv4 的 words 10/11 是 specific/destination 两个 u32，IPv6 为目的的两个网络序半部；
  双栈只存一次 IPv4 事实，投影得到 mapped IPv6 目的。接口及目的在 ingress 冻结，返回哪些投影由 receive/PEEK 时的真实选项决定。
- AF_PACKET RECEIVE 在 word 9 携带镜像点冻结的 UTC 纳秒。地址和校验状态字段保持原布局。SEND 不允许 Internet 控制项，不能借 word 12 携带 flowinfo。
  SEND 不能带 peek；RECEIVE 不能偷带目的地址/发送元数据；POLL 只能带操作和 ticket。未知 tag、超宽或保留字段均拒绝。
- 版本 6（LSOCK006）替换版本 5。两端同步构建和升级，只接受当前 VMO 标识。控制和数据继续使用同一地址值和错误码表。

## 本地端口与地址空间

UDP 与 TCP 各有一个 PortNamespace，IPv4、IPv6-only 和双栈通配在同一 owner 内准入、发布和释放；不是分别申请两个端口再回滚。
PortBinding 规范化地址空间：0.0.0.0 只覆盖 IPv4，V6ONLY 的 :: 只覆盖 IPv6，双栈 :: 覆盖两族；具体 IPv6 地址只占 IPv6。
IPv4-mapped 地址使用对应 IPv4 身份；V6ONLY 与 mapped 地址组合拒绝为 InvalidBinding，由原生错误投影为 EINVAL。
每端口按族、地址与逻辑接口判断交集：未绑接口覆盖全部接口，不同非零接口互不重叠。
计数包含全接口投影、未绑接口子集与按设备索引，均在同一个 NET transaction 更新；具体地址规范化为 IpAddr。
两端都启用 REUSEADDR 才允许重叠；现有 listener 拒绝重叠 bind，先前已绑定的重叠成员不能再 listen。

PortLease 是一份精确 capability。connect 精确化地址、accepted child 取得实际本地地址、disconnect 恢复绑定、reuse 重新分类及 close 均沿同一 owner；
精确节点先准备，随后发布与 inverse 不分配。准备失败不改变既有 membership 或 listener，不能留下半个双栈占用。
port 0 不扫描 endpoint；分别投影 IPv4、IPv6 与两族并集的空闲端口，每次最多读取 257 个 bitmap word。
隐式分配选择对应族在全部地址/设备均空闲的端口；指定端口的准入才按地址与设备复用，不宣称按接口穷尽所有可共享的 ephemeral tuple。
一族耗尽不能阻止另一族申请端口；精确化释放的一族以及最后一次 close 都必须更新对应投影。
TCP 主动打开及 UDP 关联和发送在选定实际源地址后与 [转发 NAT](ip-forwarding.md#自动源地址转换) 协调。隐式绑定在空闲位图内跳过已被同一远端转换占用的元组。
显式绑定的端口若与已确认转换产生相同的线上连接，连接或发送返回 EADDRINUSE。仅 bind 仍使用原地址空间准入。

AF_INET6 UDP 已使用同一地址冲突 owner；双栈 :: 同时占用两族，V6ONLY 的 :: 允许独立 IPv4 绑定，具体 IPv6 绑定同时固定 V6ONLY。
IPV6_V6ONLY 只能在端口尚未绑定时修改；mapped 绑定与该选项冲突返回 EINVAL。IPv6 TCP 已接通，RAW 仍未开放。
实际 IPv4 UDP/TCP 的显式、隐式绑定及 connect 精确化均使用当前 SO_BINDTODEVICE；不同网卡可以共用同一端口。
设备 setter 按固定 Linux 行为迁移已持有 membership，不重新执行 bind 冲突准入；新 bind/listen 仍查询迁移后的全部占用。
新接口所需节点在 policy 发布前准备，OOM 保留原接口、端口及接收顺序；解除绑定复用全局精确节点，不在 disconnect 分配。
TCP 的已建立 pending child 保留最终 ACK 时的设备。parent 改设备前才为其取得独立精确 membership，失败撤销本次全部 child 租约；
accept 移交该租约，停止监听/关闭释放它，不为每次握手增加端口分配。尚未建立的监听槽随 parent 更新。
IPv6 link-local bind 使用显式 zone 或已有设备绑定，缺少接口返回 EINVAL，未知接口返回 ENODEV；显式 bind zone 可以替换先前设备。
connect 的非零 zone 必须与既有绑定一致，成功后与端口精确化同一事务发布；查询只对链路地址输出当前实际接口，不保留第二份 zone 状态。

## TCP 连接

AF_INET6 的 SOCK_STREAM/IPPROTO_TCP（含 protocol 0）与 IPv4 使用同一原生 TCP；bind、connect、listen、accept、收发、shutdown、close 与 poll 已接通。
:: 监听可接入 IPv4，接受的本地/远端地址按创建族输出映射形式；V6ONLY 排除 IPv4，绑定端口后不可修改。scope 的实际设备归同一连接与租约，getter 仅对链路地址返回它。
IPV6_UNICAST_HOPS 支持 -1/0..255，IPV6_MTU 查询当前路径及设置发送上限；TCP 不提供 UDP 的 IPV6_RECVPKTINFO。其余未列选项仍明确拒绝。
IPv6 经现有本机/直连/静态或 BGP 路由收发；ND、路径 MTU 与 UDP 共用 IP owner。完整机制、scope 与 PTB 验证边界见 [TCP 装配](ipv4-transports.md#双栈-tcp-装配)。
BGP 已接入双栈邻居和 MD5 认证。原生与 Linux TCP 的密钥管理及 BGP 认证范围见 [TCP 认证](tcp-authentication.md)。发送侧 SACK 恢复见 [TCP 恢复契约](ipv4-transports.md#tcp-选择确认与丢包恢复)。其余 TCP ICMP 错误处理仍待补齐，成熟系统互通仍待验收。

### TCP 发送统计

- 原生 INET socket 操作码 25 读取版本 1 的运行时事实，无句柄。成功后返回阶段、连续超时次数、SACK、时间戳、两向窗口移位各一个字节，再返回十个 u32 字段。窗口缩放缺席用两个 255 表示。
- 状态来自连接、RTT 估计器和 Reno 的现有 owner。监听器只读取等待接收的连接数和上限。累计重传按成功提交的重传段计数，在连接重建时清零，包含超时、快速重传和 MTU 变化后的重分段。
- 查询在原 operation 与 NET guard 冻结字段，不推进计时器、消耗错误或读写队列。Linux 投影及未提供字段见 [TCP_INFO](../syscall-support/socket.md#tcp_info)。
- 成本沿既有 `tcp-lifecycle-bench` 与整机 TCP 吞吐验收。查询无逐包 IPC，发送只在已成功提交的占用序号段上比较旧 SND.NXT，实际重传时增加本连接计数。

## 数据报错误队列

- UDP/RAW 的 `inet::error_queue` 拥有错误记录及待消费的普通错误值。ICMP 入站先检查外层 checksum、原 IP 头和实际发送端点。
  UDP 使用引用中的地址与端口，RAW 使用协议、地址及接口。IPv4 还按当前 socket 标记和设备绑定核对出接口。错误引用不携带发送时的标记，标记更改后沿用当前路径核对。IPv6 首片的截断引用可交付已见的上层前缀。
- 选择 `IP_RECVERR(11)` 或 `IPV6_RECVERR(25)` 后，相关 ICMP 不可达、超时、参数错误及 MTU 反馈可排队。同步本机超 MTU 也记录 LOCAL 与发送上限。
  已入队报文因当前路由失效、分片失败或 ARP/ND 探测耗尽而被退休时，交付原目的和本机错误。存在备用路径时，等待报文可沿当前路由重试。
  每端点最多八条、64 KiB 引用容量。只在启用并收到错误时分配，满队列保留先到记录。关闭选项清空队列，关闭端点释放其所有状态。
- `SO_ERROR`、普通收发只消费普通错误值。错误记录仍由 `recvmsg(MSG_ERRQUEUE)` 或 `recvfrom(MSG_ERRQUEUE)` 消费。
  读出 ICMP 记录后由下一条 ICMP 恢复普通错误值。记录存在时持续报告 POLLERR，空队列立即返回 EAGAIN。
- UDP 的数据部分是实际引用中的 UDP 载荷。RAW 返回上层前缀，启用 HDRINCL 时包含原 IP 头。
  `msg_name` 给出原目的。控制消息携带 `sock_extended_err` 和报告方地址，本机错误的报告方为 AF_UNSPEC。
  短 iovec 和控制缓冲分别标记 MSG_TRUNC 与 MSG_CTRUNC。带 MSG_ERRQUEUE 时仍按实际复制字节数返回，MSG_PEEK 不保留记录。
- ICMP 错误在入站时冻结外层报文的接收接口和目的地址，读取时按当前 `IP_PKTINFO`、`IPV6_RECVPKTINFO` 投影，可同时返回 IPv4 和映射后的 IPv6 信息。
  本机错误的控制消息只包含发送失败信息。提交时的帧超长返回 EMSGSIZE，其余设备提交失败由设备计数反映。TCP 继续通过流错误和路径 MTU 处理反馈。
- 具有明确边界的 [RFC 4884](https://www.rfc-editor.org/rfc/rfc4884.html) 扩展随引用及填充原样交付。
  启用同族 `IP_RECVERR_RFC4884(26)` 或 `IPV6_RECVERR_RFC4884(31)` 后，新错误在 `sock_extended_err.ee_rfc4884` 给出相对数据起点的扩展位置。
  扩展格式或校验无效时置 `SO_EE_RFC4884_FLAG_INVALID`，仍交付原错误。引用长度字段为零时按普通引用处理，位置以完整数据计算，短缓冲仍可报告截断。
  Linux 结构和语义基线见 [v6.12 UAPI](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/errqueue.h) 和 [ICMP 错误投影](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/icmp.c#L1271-L1293)。
- INET 数据环操作 4 复用 payload window 和响应长度。原目的及 flowinfo 用现有地址槽。word 8 高位携带扩展位置和标志，words 13..15 存接收信息，首 word 的低/高 32 位分别是 tag 和接口。
  SET_OPTION kind 25/27 设置同族错误队列及扩展选项，查询编号分别为 32/33 和 35/36。原生传输使用领域错误，Linux 层编码 errno 与 cmsg。
- benchmark 决策：普通数据无额外分配、锁或 IPC。扩展仅在 ICMP 错误路径按报文大小校验和复制。ARP/ND 失败每批扫描一次待发队列，错误存储沿用固定记录与字节预算。
  宿主验证双栈队列、截断、SO_ERROR、RAW 引用、首片 PTB、扩展校验、邻居失败及原生编码。Linux 来宾 copyout 与互通待系统验收。

## Linux 边界与当前限制

Linux sockaddr_in 仍是 16 字节，端口网络序、地址原 bytes；sockaddr_in6 读取完整 128-bit 地址，支持 24-byte 基础布局与 28-byte 含 zone 的布局。
输出使用 28 字节，端口网络序；flowinfo 与 scope 保持标准 Rust SocketAddrV6 与系统 sockaddr 转换的原样数值语义，不将它们误当 u16 端口或截断地址。

AF_INET6 的 SOCK_DGRAM/IPPROTO_UDP（含 protocol 0）已接通创建、绑定、关联、收发、关闭与 poll；当前 IPv6 目的支持本机地址、链路地址及显式 on-link 前缀，含已通过 DAD 的静态全局/ULA 地址。
双栈 UDP 继续收发 IPv4，向 IPv6 使用者报告 mapped 来源；IPV6_UNICAST_HOPS 与 IP_TTL 独立，前者允许 0，已排队报文保持原跳数。
单播 IPv6 的默认跳数归所选出口。TCP 的语义 IP 描述保留显式设置或委托 IP 层选择，避免将默认值提前编码为固定跳数。关联 socket 的查询与发送复用同一路径事实。
flowinfo 编解码保持原样；尚未开放 IPV6_FLOWINFO_SEND，因此实际发送使用默认 Traffic Class/Flow Label，名称查询输出零 flowinfo。
RAW 及其选项范围见 [RAW 契约](raw-ip.md)。
IPv6 UDP 空端口按本机/直连/静态网关回程生成标准不可达，已关联 endpoint 的异步端口错误由 send/receive/SO_ERROR 一次消费，poll 同时报错；
不改变绑定、peer 或数据队列。未关联 socket 未启用 RECVERR 时不交付该错误，具体引用/限速/清理由 [IPv6 契约](ipv6-link.md#udp-空端口与错误交付)拥有。
IPv4 UDP/RAW/TCP 已接通 Fragmentation Needed 和 IP_MTU 查询。关联与反馈边界见[IPv4 路径 MTU](ipv4-transports.md#ipv4-路径-mtu-反馈)。
IPv6 UDP 已学习有效 PTB 并按新路径上限决定分片或报错。IPV6_DONTFRAG 可禁止分片。IPV6_MTU 支持路径查询及手工发送上限，过期、共享与资源边界见[路径 MTU 契约](ipv6-link.md#路径-mtu)。
IPV6_RECVPKTINFO 默认关闭，与 IP_PKTINFO 独立；实际 IPv6 收到目的及接口，双栈 IPv4 可按两个选项同时收到 mapped IPv6 和 IPv4 信息。
发送 IPV6_PKTINFO 逐项验证：非零接口须与本次已有出口一致且实际存在，link-local 源须有接口，非零源须是该接口可用的 LOCAL 地址；零源保留此前选择。
显式目的的 link-local zone 优先于 socket 绑定设备；未给 zone 时可用本次控制项指定出口。单次覆盖不修改绑定或 peer。
映射到 IPv4 的发送按 IPv4 规则折叠：IPv6 控制源必须是 mapped 地址，零 IPv4 源清除覆盖，最终非零源验证本域 LOCAL；native IPv6 忽略 IPv4 控制项。
完整 Linux 范围见 [Socket ABI](../syscall-support/socket.md)，链路收发与邻居失败清理见 [IPv6 契约](ipv6-link.md#udp-收发)。

## 验证与成本

固定来源见 [Socket 地址扩展基线](../standards-baseline/network.md#socket-地址扩展)。协议私有测试对照固定字节和记录值，覆盖双族、端口、zone/flowinfo、截断、未知族、宽度和保留字段；
netstack-unit 继续执行既有双栈 UDP/TCP 及 IPv4 RAW、端点生命周期和数据路径，不新建另一套协议模型。跨服务、来宾 ABI 与原生队列生命周期的系统验收仍待统一执行。

benchmark 决策：保持 128-byte 记录、槽数、VMO 容量、同步和载荷归还时机不变；不新增锁、worker 或消息往返。
linuxd 借用唯一冻结 cmsg snapshot 迭代，重试不另建控制缓冲；producer 直接原子写入同一 payload 槽，不拼接复制正文。
服务端沿既有 scratch 读取，控制 Vec 仅首次实际使用 fallible 准备，随后复用容量；无控制项时不分配。单次控制项数量由同一槽的 24-byte 前缀上界约束。
接收不再为查询 packet-info flag 额外发 OPTION RPC，由唯一网络 owner 随 RECEIVE 返回所需投影。
逐条地址 decode 的固定字段与分支成本在完整双栈数据面就绪后沿既有性能验收测量，不新增只对编码函数设墙钟门限的测试。

端口回归沿已有 kernel-unit owner 夹具覆盖两族/mapped/通配交集、REUSEADDR 与 listener、child 移交、地址精确化与 inverse、OOM 和单族耗尽。
同一 netstack-unit UDP 夹具验证 connect 后其他本地地址可绑定同端口、disconnect 恢复通配及保留接收队列，以及 connect 广播授权；这些仍不替代双栈系统验收。
端口位图从每协议一个扩为三个派生投影，UDP/TCP 合计每 NET 增加 8 KiB，换取两族与双栈均保持原有 257-word 上界；
按族/设备计数与精确键只在端口生命周期更新；设备节点按使用惰性准备，最后一份 membership 释放时回收。
benchmark 决策：接口变更属于冷配置，沿既有端口成本围栏保持 257-word 上界与零 endpoint probe；收发、握手不增加锁、分配或扫描。
已有交集用例扩展设备维度，并沿实际 UDP/TCP 夹具检查共用端口、改绑、冻结 child、OOM 回滚与退休；整机成本仍待统一验收。

原生 UDP 的完整 owner 与当前验证边界见 [IPv4/UDP 契约](ipv4-transports.md)。普通创建不再分配两方向合计 256 KiB 载荷，
每方向首次实际使用准备 128 KiB 并保留以复用；metadata 固定 8 槽，不把 per-packet 分配转移到 RX。原有 NET payload-loan 仍会为 restore 预备 waiter，
这项调用成本未在本次迁移中消除，不宣称整个 send/receive 零分配。无接收者仍沿库 ICMP 路径重复验证 UDP；完整原生 IP/ICMP 迁移仍在计划范围。
DMA adapter 不再预清零整个提交窗口：每个 emitter 与 IP-medium 一样负责完整帧，编码失败不提交，IP 错误回复先在独立已初始化正文中完成编码。
现有 wire fixture 用非零 DMA 内容验证同一生产路径；当前没有增加锁或 IPC 跳转。
benchmark 决策：本轮审查分配、复制与扫描边界，普通收发在统一验收沿既有 benchmark 测量；区分冷存储准备、UDP demux/队列与实际 NIC 提交，阈值不放宽。
