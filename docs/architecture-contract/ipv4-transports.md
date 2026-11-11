# IPv4 与双栈 TCP/UDP 传输契约

Linux syscall、阻塞超时、namespace 授权、AF_UNIX/NETLINK 属于 `system/linuxd`；IPv4/TCP/UDP/RAW、FIB、端口与 AF_PACKET 队列属于 `system/netstack`。下述协议 owner 通过 `network-protocol` 交互，不共享内核状态。

## TCP 连接机制

`libraries/tcp` 提供双栈纯连接机制，已接入产品 NET 的 IPv4/IPv6 TCP；产品与 host 均关闭旧库 TCP feature，没有备用实现。Linux IPv6 TCP、SACK 恢复与 MD5 认证已接通，TCP-AO 尚未提供。
1. Context 由调用者提供同次冻结的路径 MTU、逻辑接口、选源及运输度量；初始序号由实际 tuple 的秘密 PRF 与时钟生成。机制不查询设备/FIB，不拥有系统时钟、秘密或授权。
2. 连接唯一拥有序号、窗口、乱序重组和重传/关闭计时；MSS 缺省按族选择，路线 RTT seed 使用毫秒，不携带 Linux 缩放编码。绑定资格、地址族与链路 zone 的分派属于 NET。
3. Segment 只捕获 packet-wire 的已校验值，输出也只经其 TCP codec；stream payload 借用已有环，发送回调成功才推进流序号。即时 ACK/reset 是 best-effort，不保留额外连接或端口身份。
Reno 静态调用，接收窗口只归连接；拥塞避免按已确认字节累计，每窗口增加一个 SMSS。未使用 SACK 时第三个重复 ACK 进入快速恢复，下一次新 ACK 撤销膨胀；SACK 恢复见下节。
RTO 按真实未确认量计算阈值，同一丢失序号的重复超时不再次减半；丢失窗口为一个 SMSS。实际发送历史只用于空闲重启，ACK/keepalive 不伪装成数据活动。
流存储由调用者准备，接收固定四个乱序洞；没有新增锁、线程或全局状态。普通顺序收发不创建 SACK 存储；选择确认历史只属于同一连接，不是路由/就绪镜像。
双族机制字节用例覆盖无 MSS 握手、SYN/数据丢失、输出背压重试、顺序数据恢复与 FIN/TIME_WAIT；实际 NET 夹具覆盖双栈装配与生命周期，均不替代 Linux ABI、协议互通或整机吞吐验收。
认证 TLV 必须经密钥 owner 验证；MD5 配置、选择及签名见 [TCP 认证](tcp-authentication.md)，TCP-AO 尚未提供。固定来源见[标准基线](../standards-baseline/network.md#原生-tcp-机制)。
benchmark 决策：复用既有 TCP 生产路径与每 handle 成本测量，最终整机吞吐仍待统一验收；当前组件用例不作为跨服务性能结论。
NET 的 TcpHandle 直接拥有连接，不再在 SocketSet 保留 TCP 句柄；RAW 也由原生 endpoint 队列持有，旧表只以空集合满足链路库接口。普通 TCP 输入短路匹配实际节点并立即观察其状态，UDP 输入不扫描 TCP。
每域启动时从 CPRNG 创建唯一 HMAC-SHA256 秘密；每次开连接用实际 tuple 的 PRF 加 4 微秒时钟生成初始序号。固定软件 SHA-256 避免服务未保存的可选 SIMD/FPU 状态，不增加逐包随机调用、分配或全局 key。
输出复用真实 FIB、ARP 与 DMA token，直接用 packet-wire 编码并保留实际 checksum 卸载；提交失败不推进流序号。全域有界输出预算按 endpoint 轮转。
输出等待只限制时钟独立唤醒，不阻止已有 poll 机会重新检查设备；协议安排的拥塞重试不能被外层等待延后。路径失败与本地拥塞仍由同一连接计时器裁决。

## TCP 选择确认与丢包恢复

发送恢复按 RFC 6675：序号、未确认正文和拥塞窗口仍归 Socket；loss::Sack 只拥有选择确认区间与当前恢复阶段，不能凭 SACK 释放正文或推进 SND.UNA。

1. 只接受已协商 SACK，且位于实际已发送数据范围内的非空区间；忽略 DSACK、未来、逆序和覆盖累计确认点的无效证据。
   区间按 TCP 序号有序合并，支持 32 位回绕；未发送的排队数据不进入证据。实际窗口小于半个序号空间，比较不依赖宿主整数顺序。
2. 新 SACK 证据才增加重复确认计数；累计进度先清零。同一 ACK 即使带数据或更新窗口也可提供新证据。
   三个证据或 IsLost 判定进入恢复，冻结 RecoveryPoint；cwnd/ssthresh 按真实 FlightSize 减半，不使用 Reno 的重复 ACK 膨胀。
3. 先强制补发最早缺口，随后按 Pipe 约束依次选丢失洞、新数据、推测缺口和最高未确认字节的救援重传。
   仅真实 TX 提交后推进 HighRxt/RescueRxt；失败保留同一选择，不能提前消耗首次/救援机会。局部确认不退出恢复或增长窗口，累计确认覆盖 RecoveryPoint 才退出。
4. 超时清除旧证据并终止恢复，以真实 HighData 保留再次进入的屏障；后续新证据仍用于避开发送已接收区间，但不能对旧 flight 重复触发快速恢复。
   reset、关闭及 TIME_WAIT 退休恢复元数据；尚存描述符的 RX/EOF 不受影响。原 RTO/用户超时、退出及 NET 轮转预算仍适用。
5. 编解码完整保留四块 SACK；回复优先包含触发 ACK 的块，并填入其他当前块。有 Timestamp 时最多三块，避免超过 TCP 选项容量。
   实际 SMSS 从同次路径、已协商发送限制及 Timestamp 空间计算，正常输出与恢复共同使用；拥塞窗口限制整批在途字节，不能只限制单包。

benchmark 决策：新增控制热路径使用紧凑半开区间，Update 对每份最多四块做有序合并，Pipe/NextSeg 对区间线性扫描；不逐字节建表、复制 payload 或持另一份飞行窗口。
没有选择确认的连接不分配；新离散区间才 fallible 扩容，规模受实际发送缓冲约束。分配失败忽略该份新增建议，保留已知证据、正文与超时，不伪造确认。
双栈真实报文用例覆盖多个丢包、四块与 Timestamp 三块、序号回绕、IW10、TX 失败不推进及超时前恢复；范围用例覆盖无效证据和 RTO 屏障。
同一 NET 的 poll_at 继续观察实际恢复候选，背压仍由原输出等待约束；不增加 timer、线程或诊断入口。系统吞吐、乱序/丢包互通与每连接峰值内存沿统一验收评估。

## 双栈 TCP 装配

- TcpEndpointState 唯一保存创建族，生命周期身份使用标准 InetAddress；协议机制只保存规范化 wire tuple，不复制 flowinfo/zone policy。
  双栈 :: 监听同时覆盖 IPv4；V6ONLY 与具体 IPv6 地址按同一双族端口 owner 准入，IPv4 映射地址归同一 IPv4 身份。只绑定尚无端口的 socket 可修改 V6ONLY。
- link-local 绑定取显式 zone 或原设备绑定；connect 的 zone 必须与已有绑定一致。被动 link-local 握手固定实际入口，普通选项更新不能抹掉该范围；accept 的地址与 policy 带原接口。
  IPv4 IP_TTL 与 IPv6 IPV6_UNICAST_HOPS 独立，后者允许 0；pending child 在最终 ACK 冻结原 policy。标准地址与 ABI 边界见 [Socket 传输](socket-transport.md#tcp-连接)。
- route::output 拥有同次双族 FIB/选源/PMTU 投影，TCP/UDP 共用；拆借同一 NET 的权威字段支持连接可变借用，不缓存路由。IPv6 发送借已有 ND/NUD，不建立 TCP 邻居表。
  即时 TCP 回复沿原 64 报文/256 KiB 队列，保存完整独占 IPv4/IPv6 字节；真实出口、设备提交与失败退休继续共用预算。
- ICMPv6 在 IP owner 校验一次外层及引用，再按协议交付。TCP PTB 要匹配实际 tuple、绑定设备及 SND.UNA..SND.NXT，才可降低共享 PMTU；错误的引用不能影响其他连接。
  MTU 降低触发未确认字节重新分段，保持 SND.NXT，不减 cwnd、不退避 RTO；真正的拥塞/丢包仍走原 Reno/RTO。应用手工发送上限独立于路径查询。
- 两个实际 NET 夹具覆盖纯 IPv6/映射 IPv4 的监听、绑定冲突、流收发与半关闭，以及链路 scope、坏 checksum、主动/被动握手、合法/伪造 PTB 和实际重分段。
  成本决策：复用每连接/实际 poll benchmark；不新增锁、worker、跨服务消息、秘密副本或常驻帧。查路由与 ND 使用当前 owner，PTB 仅冷错误路径扫描；整机吞吐仍待统一验收。

## 当前 NET 传输与生命周期

- TCP shutdown/AF_UNSPEC 源码：SYN_SENT 取消不分配、不发送 RST；listener SHUT_WR
  保留 accept，SHUT_RD 停止监听并移除实际 child policy。disconnect 恢复原 bind，清除 shutdown，
  删除无需RST的transport；RST仅在唯一RouteReplies预算内保存实际payload，不保留handles、
  endpoint或port membership。queue_tcp_resets从actual socket一次消费tuple；无route/预算/OOM
  best-effort丢弃，符合tcp_send_active_reset/__tcp_close，不恢复连接。新connect/listen复用
  同一facade slot；完整buffer准备失败保留原bind，没有reset专用orphan/node/端口借用。
  CLOSED shutdown 提交 read flag 后返回 ENOTCONN；INET façade 在 operation/stack guards 外
  直接发出 notification，SHUT_RD 不依赖不存在的 egress，已经阻塞的 reader 因而可以重查 EOF。
  同时 open 的 SYN_RECV 写关闭先消费真实 SEND_SHUTDOWN permission；最终 ACK 进入 ESTABLISHED
  后才提交 FIN。权限属于 Connecting variant，在实际 ACK transition 中移交给 TcpConnection；
  SYN_RECV 不提前发布 Connected/完成 connect/POLLOUT。读关闭即时 EOF/RDHUP，两侧关闭即时
  HUP；这些投影不完成实际握手。该 flag 与 protocol FIN 的时序不同，不能由 `may_send()` 猜测。

单物理 NIC 与 lo 的源码归属：NetworkStack.interfaces唯一持NetworkInterface（index/name、实际设备、
smoltcp Interface及IPv4配置），boot只装配首张物理NIC；lo为真实IP-medium，不生成Ethernet/ARP。
每域lo先完整准备固定256KiB ring、独立64KiB TX/RX scratch与metadata，最后才移交；
新域初始DOWN/无地址，NETDEV_UP规则发表127.0.0.1/8。队列在actual protocol mutex下
串行，RX先复制到独立预留scratch并释放queue borrow再进入可能立即回复的callback；
TX拥塞按实际设备丢帧计数，原先队列不被覆盖，热路径不分配。library any_ip/self-router
仅作已由kernel实际LOCAL route选择的lo queue解码机制，不在kernel RIB暴露外部default。
实际IPv4 selector同时用于连接/发送准入和TCP egress，当前覆盖LOCAL/connected prefix/
default gateway；绑定保存真实设备index。UDP/raw准入metadata固定出口，后续binding不
重写旧队列；UDP 的 bound-device 在原生 owner、TCP 的绑定由原生连接筛选，RAW 同样在原生 owner 入队前过滤。TCP/UDP/RAW 各沿唯一 FIB 和实际队首选择出口，
不再为旧 RAW 表建立每轮路由 scratch；未选中的设备不触碰其队列或邻居。source tuple按IPv4 weak-host保留，
不因另一设备或删地址重置TCP。8-device批次在整个域轮次完成后轮换首设备，共享64 RX/
completion预算，持续lo/eth0流量不饿死其他设备。配置/IFCONF/GETNAME/INDEX/MTU/
SO_BINDTODEVICE getter查询实际设备，删除syscall eth0/index常量。raw创建按实际NET
owning UserNS NET_RAW授权，普通NETLINK RAW不能按INET raw类型误授权。
固定 smoltcp 的修改清单由 verify_smoltcp 管理，netstack 与 netstack-unit 使用相同 medium-ip features；
原inventory不变，其余upstream文件仍禁止漂移。真实无NIC LO UDP/TCP握手/数据、独立
双NIC frozen-oif/bound ingress/LOCAL source/weak-host SYN回归源码已写。
route selector 使用同一 FIB 的 33 个 prefix AVL roots，查找不扫描全部 route records。
NET、packet、uevent 捕获创建域；NEWNET publication 提供声明范围内的独立 transport/ports/lo。
完整 Linux NETNS/veth/设备迁移不支持。

### IPv4 邻接与 ARP

- 物理接口的原生邻居表独占 IPv4 链路映射、失败状态与时钟。ARP 和 IPv6 ND 共用邻接状态机，报文解析和地址准入各归本协议。回环直接提交 IP。
- ARP 只从本链路接收，使用当前 FIB 判定目标是否为本机地址。回复仍经收到请求的物理接口。零源地址探测只回复，普通组播和广播通过标准映射发送。
- IPv4 原生 TCP、UDP、RAW、ICMP 回复和转发共用实际下一跳解析。只有网卡接受探测才消耗次数。三次未应答后保留当前路由引用的失败网关，有效 ARP 回复重新赋予可用资格。
  依赖失败网关的路由由 RIB 重选。即使主路径已撤下，也按[路由事务](router-configuration.md#复合网络执行批次)继续复探。应用可通过 [RTM_GETNEIGH](../syscall-support/socket.md#netlink_route) 查询 IPv4 邻居。
- Benchmark 决策：已解析发送借用一个映射，直接写最终 TX 窗口。邻接节点与时钟仅在学习或首次解析分配，空表不预留固定槽。
  时钟按截止顺序有界推进，失败时查询活动路径组。路由规模和冷邻接抖动的 CPU 成本待整机验收。

- Inet final fput 的协议 registration 由 `operation: Mutex<Option<InetEndpoint>>` 独占；
  immutable endpoint key 只用于分派，已 take 的 observer 不再 lookup 可能复用的 SocketHandle。
  close 与全部 payload loans 同一 operation 生命周期；busy protocol owner 时只向实际固定 ring
  发布一次 command，无分配。新的 task transaction 必须先清空既有 commands，再允许 bind/new，
  因而已经关闭的 port lease 不会跨下一个 task transaction 存活。deferred poll 仍采用原固定 budget。
  actual Inet host fixture 覆盖 UDP 端口复用与不复用的 endpoint 身份、listener 全 handles 退休、busy owner
  下 deferred close，以及 1024 个 fresh TCP 的真实端点容量和 full cleanup ring。
  NIC、时钟与 deferred raise 是显式边界，protocol/port/ring 是生产实现；

SocketIoDirection/SocketTimeout 是向 syscall/文件 I/O caller 的语义超时接口。SocketTimeouts 的唯一 Mutex 只持 native receive/send policy，不持 backend state、wait membership 或 deadline cache；PreparedSocket 在 endpoint 副作用前 fallibly 预留该 Arc，OFD
façade 与 INET façade 引用同一个 linuxd owner；该 policy 不传入 netstack。wait_for_ofd/wait_for_socket_capacity 的 deadline 必须由 caller 在重试循环外冻结，
None 保留其他文件无限等待。已到期先无分配复查 level readiness，再返回 TimedOut；普通期限发布原有 WaitRegistry membership。socket::interrupted_wait 只根据本次期限决定内部 RestartSys 或用户 EINTR，不能重读后来改变的 policy。
time::scheduler_quantum_ns() 是 native scheduler cadence 的唯一投影，socket 禁止复制 HZ。native codec、owner、deadline、getter、guest 收发/accept 及既有 CPU benchmark 纳入集中门禁。

SocketPollState.read_closed 是实际协议/transport 半关闭的只读投影；file poll 仅按请求输出 POLLRDHUP，不从双向 HUP 猜测。TCP 使用实际 protocol FIN/closed state 和已有 shutdown_read，UDP 从实际 EndpointState.shutdown 投影 half close，raw/packet
不伪造。socket 与 OFD readiness 固定容量为三项，stream 观察两个实际 direction notifications 与 endpoint error/lifecycle notification；epoll 为这些实际 sources 预留三个 membership nodes，无 readiness cache。consume_wait_notifications 只排空三路通
知，不消费实际 chunks。无 requested data events 时仍观察 error/HUP 的实际 endpoint notification。

TCP 设备绑定与本地端口同一事务提交，完整范围及 pending child 租约移交见[端口契约](socket-transport.md#本地端口与地址空间)。
TCP passive child 的不可变 policy 属于实际 TcpHandle.passive；标准最终 ACK 将原生连接转入 Established/CloseWait 时冻结 InetSocketOptions。accept 消耗该 handle，移交协议选项与端口引用；accept OOM 不
摘取 child。linuxd 的收发 timeout 在 accept 时继承，尚未在握手时冻结。TCP_NODELAY setter 不重写已经冻结的 pending child；未完成握手的失败复用实际监听 slot；已经建立的 child 即使随后 RST 仍留在该 handle
  的 accept queue，连同握手地址与一次性 pending error 一起移交，无
平行 pending index。每次实际 TCP ingress 后立即观察匹配端点的生命周期，保证同轮 ACK→RST 不丢 child；终止原因由连接实际接受完整校验段后记录，不保留设备 RST 旁路事实。TcpConnection 的不可变
local/peer 是实际连接身份，协议库清除路由 tuple 不释放 local 绑定；peer 查询仍检查实际 TCP phase。terminal 记录唯一 Closed 事件已交付，防止 SO_ERROR 或 I/O 消费后重复重建 RESET。SHUT_RD 保留已排队字节；接收顺序为数据、
已收到 FIN 的 EOF、消费 pending error、半关闭 EOF，TCP recvmsg 不返回源 sockaddr。依据固定 Linux tcp_check_req → tcp_v4_syn_recv_sock → inet_csk_clone_lock/ sk_clone_lock。bench 决策：policy copy 无分配，沿用实际 timeout
snapshot 热路径门禁并加入双方向冻结成本；实际 wire 握手、最终 ACK 前后修改、OOM 重试、同批 ACK/RST、延迟 accept RESET、SHUT_RD queued data 和 FIN/RESET 优先级由共享 actual INET host fixture 覆盖，guest 增补无限/有限/禁用 timeout
的 SA_RESTART 对照。

TCP_MAXSEG 的 user_mss 是实际 InetSocketOptions policy，send_mss/advertised_mss 属唯一生产
原生 TCP mechanism。0 清配置，非零接受原生 88..32767。advertised_mss 在 active open/收到 SYN
固定；不能在 accept 或最终 ACK 用 parent 最新 policy 重建广告。send_mss 与 peer MSS/route 同时
形成，Nagle、分段、Reno 必须使用此实际结果；ESTABLISHED setter 不篡改协商。
parent listener 的 send_mss 是进入 LISTEN 时移交的不可变 scalar，children 各持自己的 cache；
getter 不从某个 pending child 估计 parent 值。disconnect 按 native memset(rx_opt) 清 user_mss，
保留 cache；placeholder 与可失败的新 storage 在实际 handle publication 前移交 cache。
新增机制无 global/lock/unsafe，受既有 protocol TaskMutex/payload loan 保护；policy mutation和机制
更新同一 guard，不能让 getter 与 SYN/数据出现两个值。源围栏覆盖握手捕获、真实分段与断开
归属，并提供删去捕获/分段约束、篡改 cache、保留旧 cap 的负例。

TCP lifecycle benchmark 复用完整实际 NetworkStack/原生 TCP/端口 owner 与 NIC boundary，既有 200ns hot-path 门禁分别测量实际 pending child 观察和 1024-handle 有界扫描的每 handle 成本；真实 RST wire/NIC poll 的分配与收包冷成本独立报告。
未读数据的最后 OFD close 使用实际 tcp.abort，完整接收则 tcp.close，后续 orphan 保留直到真实协议退休。

active connect 的原始 InetAddress bind identity 随 Connecting→TcpConnection 移交，不能与协议选择的 source address 混同。握手失败保留 Connecting 与一次终止事件，SO_ERROR/读写消费不重新生成；blocking connect 或再次 connect 完成该失
败时，按 __inet_stream_connect 恢复原 bind 与未连接 read policy，错误已被消费则为标准 ECONNABORTED。PortNamespace::restore_bound_address 是原 wildcard→exact 的无分配 inverse，复用同一 occupancy/bitmap 与 port capability，explicit bind 地址不改
变。非阻塞失败尚未完成 connect 时 local source 仍保留。
绑定与监听身份只由 TCP 生命周期 owner 保存，协议库地址类型仅在 listen/connect 边界转换，不跨状态转移持有。
TCP receive 只返回实际复制长度；INET façade 投影同值 full length，不制造无逐报文含义的 peer/local 地址。此标量接口精简不增加分配、锁或间接层，复用既有生命周期性能验收。
CLOSED 只保留真实句柄与状态，首次 connect/listen 才完整准备各 32 KiB 的 RX/TX；任一缓冲 OOM 保留原绑定和 policy，不能发布半开的连接。
活动连接、passive child 与补位 listener 保持原缓冲容量；已有打开准备路径完成延迟分配，不增加收发热路径分支。空闲端点每个少占 64 KiB，实际容量与 OOM 边界沿既有生命周期夹具覆盖。

- UDP 的 tuple、端口租约、user_bind_locks/shutdown、实际 RX/TX queue 及 readiness 唯一属于 netstack::inet::udp；
  NetworkStack.udp_endpoints 使用不复用的 ID，SocketSet 不再注册 UDP，产品及 host profile 均关闭库 socket-udp feature。
  connect、解绑、shutdown、广播授权与绑定精确化仍沿相同端口 owner；队列、PEEK、截断、EOF 与旧 TX source/oif/TTL 保持原语义。
- IP 层完成本地目的与 IP 校验后，原生 UDP 使用 packet-wire 校验长度与 checksum；只有真实硬件验证可省略求和，RAW 仍获得原 IP bytes。
  单播按 exact/peer/实际绑定顺序选择一个 endpoint，广播交付所有匹配者；接收队列满不产生错误 ICMP port unreachable。
  普通 UDP 帧不触发完整 TCP handle 维护扫描；TCP 时钟仍在每轮 poll 开始推进，真实 TCP ingress 仍逐帧观察状态变化。
- 每方向固定 8 条记录、128 KiB 载荷上限；create 不准备载荷，bind 准备 RX，第一次 send 准备 TX。
  连续 datagram ring 将 wrap padding 计入所属记录，不额外消耗 metadata slot；零长度包仍占一槽，消费最后一条归空闲。
  准备失败撤回新端口租约，RX/TX payload copy 继续经原 NET loan 在 owner 外执行；首用后 packet ring 不分配。
- TX 使用唯一 FIB/ARP，packet-wire 在最终 DMA 窗口加头；保留既有 DF=1、ID=0 的原子 IPv4 datagram，不为不可分片包增加 ID 计数。
  实际提交成功才正常出队；WouldBlock 保留队首等待设备活动/时钟，永久提交错误或准入后路径消失则计数丢弃并退休，不能无限占据发送预算或未来重放旧包。
  ARP、TX completion 与 lo RX 释放容量均可提前唤醒；共享 64 次 TX 尝试预算并按 endpoint 轮转，不能由单 socket 饥饿其他端点。
  冷端口/存储 OOM、queue wrap、PEEK、短读、零 datagram、metadata 满、NIC reserve 后 submit 背压及原生命周期由实际 host fixture 验证；
  跨服务/来宾和吞吐仍待统一验收；同一 UDP owner 的 IPv6 回环/链路范围见 [IPv6 契约](ipv6-link.md#udp-收发)，完整双栈仍未完成。

InetSocketOptions 是 INET endpoint 的唯一选项 policy；InetSocket::option 仅在 operation/stack guard 下读取实际 map，Socket::internet_option 是无状态 typed 查询 seam（InternetOption 不含 ABI number）。TCP 私有 handles 只由
tcp::update_transport_options/lifecycle/accept/io 修改，apply_tcp 无分配初始化 Nagle/TTL；已冻结 passive children 保持旧 policy。raw endpoint 也拥有同一 concrete options，receive 返回实际 IPv4 packet destination；不查询 Task 或复制 façade flags。UDP
transport 的 TX metadata 在实际 enqueue 冻结 source port/address/TTL，mechanism setter 只影响未来入队。测试只读 tcp::transport_options_match 在 cfg(test) 内观察实际 transport，不实现第二份 lifecycle。

IP_PKTINFO 源码归属已改为实际 queue metadata：UDP Receive 与 raw RawMetadata 在 ingress 入队冻结 interface_index/specific destination，raw 原始 IPv4 header 保持实际 destination。ReceivedMessage.internet_packet_info 是随本次接收移交
的 typed fact；唯一选项 owner 在 receive/PEEK 选择是否返回，默认关闭，linuxd 不另作选项 RPC。
syscall 只编码标准 in_pktinfo 的 iif/spec_dst/addr；peek 与接口重配置不能重新计算旧报文信息。raw 绑定地址只由实际 transport 持有，删除 RawEndpointState.bound_address 副本，raw_v4_match 式目的地址过滤发生在 enqueue 前。

发送 IP_PKTINFO 由 syscall 保留有序 InternetSendPacketInfo，实际目的族 owner 按固定 Linux ip_cmsg_send 折叠：非零 oif 更新本次请求、spec_dst 每条更新（零表示本次自动选源），不使用 ipi_addr 选源。send 的唯一 context 传入同一 facade/transport route owner，
选择/授权所在 NET 不变，不改变 socket bound-device/local identity；UDP/raw 实际 TX metadata 冻结选中的 device/source。未知接口 ENODEV、非本域 LOCAL 源 ENETUNREACH；失败不消费 payload。已有固定 lo/多 NIC 回归增加 actual queue peek/重配置
、raw 入队过滤、单次 override/后续 binding 保留、native sendmsg/recvmsg 源码，集中验证时执行。

Ethernet adapter的RX与TX准入已根修源码：库receive要求配套不可失败reply token，但硬件TX pool耗尽不能阻止RX/ACK。EthernetTxToken唯一持Option<NetworkTransmit>：正常egress只在reserve成功提供token；ingress在仅WouldBlock时仍读
取RX，reply沿同一codec但无reservation则丢弃并累积实际软件TXdrop，不提升为EIO。硬件不可恢复error仍锁存原typed error。缺失该区分会让ACK不能回收sender窗口并延长背压；没有第二套接收/发送engine。NetworkStatistics增加
transmitted_dropped，Ethernet投影硬件+实际software loss，lo投影实际queue drop；单一Cell仍受protocol mutex串行化。actual NIC边界TXfull/RXUDP/ARPdrop/TCPfinalACK/恢复发送源码已写，negative source fence拒绝恢复reservation?，集中执行，原热路径
门槛不降低。

IPv4 地址记录与原生 ioctl 源码已根修：首次或改地址按固定 devinet_ioctl 的
inet_abc_len 设置 /8、/16、/24、Class E /32、zeronet/limited broadcast /0；组播 EINVAL，
同一地址设置为 no-op，不重置显式 mask/broadcast。InterfaceState.broadcast 唯一保存
ifa_broadcast，netmask改变只在仍等于旧默认broadcast且新prefix<31时自动更新；显式非默认
地址保留。原生lo无IFF_BROADCAST，默认getter为零。无地址的ADDR/NETMASK/BRDADDR
查询为EADDRNOTAVAIL；setter先copy、capability、family校验，再实际name/address解析。
UDP/raw发送权限与各自实际 ingress/next-hop 分类消费同一配置投影，不在syscall重建broadcast。
额外配置输入只属于现有interface mechanism，保持单一protocol engine；无新增global/lock/
每报文分配，判定O(1)。真实NIC自定义broadcast收包/发包及原生classful/no-op/LO/error-order
回归已补源码，既有wire/loan/readiness性能门禁不降低，当前单 NIC FIB/MTU 沿同一 owner；多地址暂不扩展。

state Option是live/retired唯一事实；fput同时摘端口/Weak并take完整queue，全部deallocation、
Pipe close和最后NET Arc在所有guard外。poll blocking投影实际state，deferred用同一state的
try_lock；queue节点按真实allocation收费并遵守native首包例外。过载保留FIFO、一次ENOBUFS，
错误消费与拥塞独立；PEEK不消费/退费，实际排空解除拥塞。SO_RCVBUF/BUFFORCE委托同一
receive_limit，未实现的send budget仍返回EOPNOTSUPP，不能只修改getter。

### actual interface MTU

ProtocolDevice的实际Ethernet/lo对象唯一拥有配置L3 MTU；InterfaceState不新增镜像。
configure_mtu在已捕获NET protocol guard内准入native设备上下界、改actual值并刷新同一
vendor Interface的derived capabilities。snapshot/packet准入/device TxToken使用同一值，
RX scratch和物理DMA storage仍为硬件最大容量，缩小MTU不改变已有已排队frame的存储。
Ethernet帧最大长度冻结为当前L3+14，lo inject使用actual L3；库分段使用同步caps。
MTU低于68时销毁同一IPv4地址/broadcast及实际FIB成员，setaddr返回ENOBUFS；增长仅恢复
IPv4能力，不恢复地址。lo UP首次创建默认地址也检查actual MTU达到68；native重新DOWN→UP
发表127/8。既有OFD仍路由原NET，新getter无硬编码1500，setter不再只允许原值。

新增状态只是actual设备的一个usize，不增加hot锁、global或allocation；caps刷新属于冷配置
事务，沿用实际TCP/UDP/packet benchmark。真实不同NIC MTU/packet frame长度/SYN MSS及
lo 512→0→65536收发/地址退休host/nativeguest与negative围栏已写。
IPv4 UDP/RAW 路径 MTU 与分片行为见[发送契约](#ipv4-主机分片发送)。多地址与设备迁移仍需补齐。

### 同一NET唯一IPv4 FIB

NetworkStack.routing独占33前缀bucket的actual route records，LOCAL先于MAIN，每个prefix按
metric/alias publication顺序选择；used_prefixes只索引精确非空roots，与insert/remove/接口事务
同时更新，不缓存路由决定。InterfaceState.gateway与configure_gateway及旧线性selector已移除。
地址/mask/broadcast/UP事务先完整准备自动LOCAL/connected/broadcast节点，成功后一次移交
配置与FIB；OOM保留全部旧状态。DOWN保留HOST LOCAL并退休其他用户nexthop，UP只重新
创建actual地址的自动route，不复活旧static route；最后地址/IPv4销毁强制退休该IF的routes。

configure_route接收原生codec解出的Ipv4RouteRequest，在same NET TaskMutex验证actual
nexthop/gateway scope/prefsrc和CREATE/REPLACE/EXCL/APPEND，再准备唯一fallible scalar节点；
exact首alias replace无需分配。没有新的global/锁/unsafe。RouteSnapshot只抓取actual records与
同guard下的设备名字；ProcSource.route_snapshot按已open捕获NET读MAIN FIB，不从接口地址
合成路由；device snapshot只含名字/counters，不复制IPv4配置。

TCP即时回复捕获实际匹配socket bound device；same ingress FIB冻结unbound及logical iif-bound
两条反向MTU事实，socket admission保证只可能使用其一，SYN广告使用反向设备MTU而非
输入设备MTU。没有reverse route的listener SYN不发表半连接。ARP同一actual FIB判定目标
LOCAL，原生默认weak-host可答其他本域接口的地址；on-link neighbor不能被connected-prefix
过滤，RFC2131 0源只回复不缓存。Profile明确扩为九文件，新增iface/interface/tcp.rs只传递
实际socket的reply flow事实，原inventory未变。

IPv4 route metrics与TCP纯时间phase已补源码：actual FIB保存原生ADVMSS、WINDOW、
RTT，ioctl的MTU转换为ADVMSS而不修改设备MTU；proc从同records显示三个metrics。
IpRouteContext只在本次active-open/egress/RX guard有效，reverse route提供被动握手事实，
结束清除。接收广告默认下限256/上限65495，发送MSS受实际MTU/peer/握手user cap约束；
两者不能合并，否则小MTU会错误增大发送段或降低原生SYN广告。窗口clamp限制真实SYN/RX，
route RTT只在握手完成seed首个data RTO，不伪造srtt采样。pure maintain先于route/NIC筛选，
无route/设备DOWN不阻止timeout与60秒TIMEWAIT退休；真实FailureReason产生一次ETIMEDOUT，
退休保留queued RX/FIN。无新锁/global/unsafe/分配；沿用实际TCP每slot成本门禁。
规范依据同一固定Linux commit的
[`rtentry_to_fib_config`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/fib_frontend.c)、
[`ip_metrics_convert`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/metrics.c)、
[`tcp_connect_init/tcp_advertise_mss`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/tcp_output.c)、
[`tcp_init_metrics`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/tcp_metrics.c)
及include/net/tcp.h的TCP_RTO_MIN/MAX/TCP_TIMEWAIT_LEN。source/wire/nativeguest/negative围栏
未开放的 TCP 选项、PMTU 策略与重组不在本轮新增范围。

INET keepalive标准入口已补源码：SO_KEEPALIVE与TCP_KEEPIDLE/KEEPINTVL/KEEPCNT的
默认7200s/75s/9、native1..32767/1..32767/1..127准入经actual InetSocketOptions唯一policy，
apply_tcp配置所有未冻结handle；final ACK冻结child policy，parent后续setter不改变其deadline。
transport独占actual活动时刻、probe deadline 与实际探测尝试次数，已从重传Timer::Idle移除keepalive
并行字段；没有新锁/global/unsafe/分配。仅Established/CloseWait无unacked/queued数据时发送
snd_una-1空ACK；接收/普通发送重置活动周期，成功提交或实际路径失败才增 count 并安排 KEEPINTVL，local TX
失败安排原生500ms resource probe而不记peer失败，达到count在下一deadline产生一次ETIMEDOUT。
NetworkStack.poll_at是唯一protocol推进engine；普通poll只捕获一次当前timestamp委托，实际
wire回归使用明确事件时刻，不复制timer算法或sleep。wire背压/应答/断开/count/child继承、
nativeguest范围/短参数/坏指针、负围栏与200ns每slot clock benchmark源码已补。
PROBE0 budgets 的最终 owner 见下文；PMTU/fragmentation 不在本轮新增范围。非 INET SO_KEEPALIVE 未开放，
不能把该新增能力称为完整SOL_SOCKET。依据同一固定Linux tcp.c/tcp_timer.c原生实现。

TCP_SYNCNT/TCP_USER_TIMEOUT/TCP_LINGER2及reset退休已补源码。唯一transport纯时间
phase拥有实际pending sequence起点、RTO事件次数、前4次active SYN线性RTO及native模型预算；
无route/设备DOWN不冻结重试。USER_TIMEOUT仅约束未完成sequence，idle不会被强制关闭，
keepalive 配置该值时仍须至少进行一次探测尝试。数据orphan重试使用native默认安全预算8。
TCP_LINGER2默认getter60s、负值归一化-1、正值最多120s，期限仅在最后application fput后
生效；实际FIN ACK冻结FIN_WAIT2起点，最短3.5*RTO；正常期限静默退休，负值发一次reset。
RST原表示只由实际TCP构造，standalone dispatch 和 NET RouteReplies 均复用原生 TCP codec。
队列保留payload/bound oif，绝不持有TCP/port/endpoint；OOM、预算满和无route均不恢复身份，
符合tcp_send_active_reset的best-effort单次发布。删去reset专用orphan/node与retain_transport。
准入仍有64报文/256KiB实际预算，正常FIN/TIME_WAIT继续由真实transport持lease。
固定依据：[tcp.c](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/tcp.c)
的__tcp_close/tcp_done/TCP_LINGER2，[tcp_timer.c](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/tcp_timer.c)
的tcp_write_timeout/tcp_orphan_retries/tcp_keepalive_timer，以及
[tcp_output.c](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/tcp_output.c)
的tcp_send_active_reset/tcp_acceptable_seq及include/net/tcp.h的tcp_fin_time。
实际wire回归已补reset一次校验和/窗口0/端口提前复用、OOM/no-route释放，以及live SHUT_WR
无FIN期限、orphan -1/1/默认60/120s及IF DOWN退休；nativeguest option范围与负围栏已写。
PROBE0 budgets 的当前实现见下文，PMTU/fragmentation 暂缓。

PROBE0/USER_TIMEOUT 的当前实现如下。实际Timer::ZeroWindowProbe拥有
可选实际deadline、退避delay与一次pending intent；send无clock时不拿uptime ZERO冒充起点。
纯clock定首次期限并推进真实probe事件，无route仍按native负route错误退避；资源拥塞500ms
重试且不增加退避。发送复用原TcpRepr/Packet，实际snd_una-1空ACK，不发送或确认stream字节。
实际PROBE0 count和第一probe_since只属于transport；有效窗口0应答清count并刷新下一probe，
不重置USER_TIMEOUT起点。只有真实ACK推进SND.UNA才重置未完成sequence epoch。活socket
默认15次无人应答probe预算；orphan依native alive使用8/0。USER只在probe真正开始后计期，
原生持续零窗口回复在未配置USER时不因总尝试次数被误杀。实际host源码补空probe wire、
USER1000ms与非进展ACK、无route+IFDOWN探测预算、持续零窗口应答；负围栏已写。依据同一
固定Linux tcp_input.c tcp_ack_probe/tcp_ack以及tcp_timer.c tcp_probe_timer和tcp_output.c
的tcp_send_probe0/tcp_xmit_probe_skb。原生 TCP 不增加 lock/global/unsafe；
字段受actual transport/NET guard独占；沿用实际每slot clock/完整poll200ns benchmark门禁。
实际 NET 回归保留空 ACK、有效零窗口回复刷新期限、拒绝确认未发送字节覆盖。
实际 TCP 独占 `send_seq_no`（SND.NXT）和发送/重传 cursor `remote_last_seq`：只有成功提交
SYN/数据/FIN 才推进 SND.NXT，RTO 仅回退 cursor。ACK 准入上界、ACK-all、重复 ACK、
空 ACK/RST 的序号及 unacked 判断均读取 SND.NXT，不读取 queued byte 长度。重传不受
Nagle 对新小段的限制，纯 ACK 不重复发送 FIN 或推进 cursor。没有新增锁、全局状态、分配
或 unsafe；两个字段代表不同协议事实，不是需要同步的状态镜像。
`OutputFailure` 仅携带本次真实输出的路径失败或本地资源拥塞，kernel 对已有唯一 FIB
plan 中的失败调用 `output_unavailable`；邻居等待及 library 输出错误进入同一 transport
探测机制。没有新增 route selector 或 errno side table。探测 deadline 在有无 route 时均
可观察；路径失败消耗一次尝试，本地拥塞延后 500ms 且不增 count。standalone reset 在
提交前取走 tuple，一次 best-effort 发送失败也不重新建立重传身份。
新增实际单 NIC wire 回归覆盖未发送数据/FIN 的非法 ACK 与 RTO 回退后合法 ACK，双族机制
回归覆盖提交失败 SYN/数据；固定依据同一 Linux tcp_input.c 的
`tcp_ack`/`tcp_rcv_synsent_state_process` 及 tcp_output.c 的 `tcp_acceptable_seq`，沿用实际
TCP 每 slot 和完整 poll 的成本门禁，不改变阈值。

初始 NET 的创建与固定 ID 登记只在同一个 fallible Once 初始化事务内进行，避免并发 boot caller 发布重复目录 key；失败仍由实际 owner 退款与退休，无旁路登记。

Readiness 快照与 poll 后 transition capture 各自对 endpoint AVL 做一次零分配 O(N) 中序遍历。
原生 TCP/UDP/RAW 从实际连接或 queue/shutdown owner 取得 readiness，不逐项重新查树；通知提交继续使用有界 cursor batch，保持 false → true edge 语义。

双族 RAW 的原始报文、绑定、校验和、队列、背压、成本与验证边界由 [RAW 契约](raw-ip.md) 维护。
IPv4/IPv6 本机分片接收、超时与重叠规则由[双栈重组契约](ip-reassembly.md)维护，TCP/UDP/RAW 继续使用原接收 owner。

## IPv4 组播监听

`NetworkInterface::igmp` 唯一拥有接口 IGMP 查询器版本、组引用和报告义务；`inet::multicast::Memberships` 唯一拥有各 socket 的组/接口引用及 IPv4 地址选择意图。
IGMP 与 MLD 共用静态泛型 `multicast::listener::Listener<A>`；两族分别编解码协议、维护接口兼容时钟，不保存第二份成员状态，不开启旧链路库的 multicast feature。

- Linux `IP_ADD_MEMBERSHIP`/`IP_DROP_MEMBERSHIP` 接受 `ip_mreq`（8 bytes）与 `ip_mreqn`（至少 12 bytes），只读对应标准结构。
  原生请求用组地址、ifindex、接口地址与加入布尔传值；非零 ifindex 优先，其次按接口地址选择，均未指定时查询真实 IPv4 FIB；无设备/路径返回 ENODEV。
  重复成员 EADDRINUSE，非法组 EINVAL；TCP 成员操作 EPROTO。未找到退组对象返回 EADDRNOTAVAIL；未指定接口且无法选路返回 ENODEV。
  请求地址用于匹配地址移除后的退组，实际引用仍按不复用的 ifindex 释放；设备已退休时释放 socket 成员，不把它交给新设备。
- Socket 成员与接口引用在同一 NET guard 内提交；所有 fallible 容量在发布前准备，失败不留下半个引用，空成员容器失败/退出时释放。
  多 socket 共享一份接口报告义务。接口重启保留应用引用并重新报告；最后退出立即停止 IP 接收，报告重传不会延长资格。
  未曾报告且已零引用的记录立即退休；接口停止、协议重传结束也清理零引用记录，不积累不可观察的旧成员。
- `IP_MULTICAST_ALL` 可设/查，默认 1，只接受 0/1，不能以零长度设置；与 IPv6 对应选项分别持有。
  关闭后按实际入接口检查自身成员，开启时仍须先通过接口 IP 准入，并满足 UDP/RAW 协议、端口、绑定与关联过滤。
  IPv6 UDP 的 IPv4 映射收包使用 IPv4 成员与选项；创建族和 V6ONLY 仍决定实际接收资格。组播绑定只作为接收过滤，不成为发送源。
- IPv4 链路库只验证 IP 并消费当前帧的原生准入事实；加入的组送往同一 UDP/RAW owner，RAW 保留完整 IP 头和选项。
  IGMP（protocol 2）由原生接口处理，非法或无人监听的 IGMP 不产生 protocol-unreachable；RAW 观察不改变内建协议处理。
  `224.0.0.1` 永久接收、免报告；其他 ASM 组以 EXCLUDE 空集合报告。Loopback 只收敛本地资格，不发送链路 IGMP。
- IGMPv3 报告 TTL 1、DSCP CS6、IPv4 Router Alert 0，目的为 `224.0.0.22`，使用标准 `01:00:5e` 映射；直接编码到最终 NIC 窗口，不分配中间帧或借 UDP/raw 发送队列。
  v1/v2 报告发给对应组，v2 Leave 发给 `224.0.0.2`，v1 不发 Leave；只有旧代最后报告者执行退组报告。
  本机地址变化补报当前状态；无地址时按规范可用源 `0.0.0.0`，不复制接口地址或载波状态到协议对象。
- 查询校验完整 IGMP checksum/TTL、组/来源结构；v2/v3 缺 Router Alert、General Query 发向其他组、保留/非单播来源列表均忽略，不改变当前回复。
  v1 General Query 不要求 Router Alert，响应上限为 10 秒；v2 响应编码按 100 ms，v3 按标准浮点编码解码。
  两个接口级旧查询器寿命分别保存；v1 查询或 v2 General Query 刷新对应寿命，最旧版本优先；到期即使晚于预定时刻，也正确转回仍有效的版本并补报。
  旧查询器寿命按 RFC 9776 §8.12 计算；旧代报告可以抑制未提交的初次报告、重传及待回复查询，单次等价报告不重复发送。
- 当前代状态变化按 Robustness Variable 重传，默认间隔不超过 1 秒；旧代非 Leave 重复报告间隔按 IGMPv1/v2 的 10 秒规则。
  来源查询集合只属于短期回复，去重合并、按 MTU 分条发送，不变为应用来源过滤；合并仅保留未发送尾部，OOM/超额保留旧回复。
  共用机制最多保留 128 个来源，IPv4 为 512 bytes、IPv6 为 2 KiB；每组每次最多两份分段报告，每接口每轮最多四次提交，成功后轮转。
  设备背压等待活动，预算耗尽续调度；发送次数与重传时钟随实际提交推进，旧代允许的报告抑制独立消费待报义务；不新增全局状态、锁或 worker。
- `IP_MULTICAST_IF(32)` 接受 in_addr、ip_mreq、ip_mreqn；非零 ifindex 优先于地址，均为零清除默认接口/源意图，不隐式加入组。
  两个选择字段只由 `InetSocketOptions` 在 NET guard 内保存；地址是用户选择意图，不是设备地址副本。未知接口/地址 EADDRNOTAVAIL，设备绑定冲突 EINVAL，失败保留旧值。
  非零 ifindex 可携带其他地址意图，发送时仍验证源是本机地址；getter 返回该地址的四字节网络序（含零），不返回接口序号，短缓冲复制其实际前缀。
- `IP_MULTICAST_TTL(33)` 独立于 IP_TTL，默认 1，支持 0..255，-1 恢复 1；TCP 设置 IF/TTL 返回 EINVAL。
  `IP_MULTICAST_LOOP(34)` 默认开启，Linux 非零值归一为 true，TCP 也可设；TTL/loop 接受单字节或 int，空值 EINVAL，标量短 getter 返回一个字节。
  IPv6 UDP 向 mapped IPv4 目的发送时使用 IPv4 的三项策略，IPv6 单播/组播选项不覆盖它们。
- UDP 与 RAW 共用 IPv4 控制项准入：单次 packet-info/设备绑定优先于组播默认接口，显式源/绑定源优先于组播默认源；单播不使用组播默认项。
  实际选路、选源、TTL 与回送义务在发送发布时冻结，RAW HDRINCL 使用报头中的 TTL（IPv4 第 8 字节），不被 socket TTL 改写。
  回送复用 `multicast::loopback` 和原有 lo 队列，保留实际逻辑入接口，仍经当前成员、IP/UDP 校验及 socket 接收过滤；IGMP 查询/报告归原接口 owner。
  回送义务在首次尝试 NIC 前消费，NIC reservation/提交背压重试不重复投递；lo 自身只投递一次，满队列只丢失本机副本。
  TTL 0 可以回送但不上链路；回送关闭不改变链路发送。UDP 本机副本由同一最终编码器执行软件校验，NIC 仍可使用硬件校验卸载。
- INET `SET_OPTION` kind 18 为 IPv4 MulticastAll，kind 19 为成员请求；后者在通用布尔值/零选项字节后携带 u32 版本 1、IPv4 组、u32 接口与 IPv4 地址选择器。
  kind 20 为 IPv4 MulticastInterface，零值/零选项字节后携带 u32 版本 1、u32 接口、IPv4 地址；kind 21 为 TTL，kind 22 为回送布尔。
  选项查询仅追加编号 26..28，地址查询返回逻辑 u32，Linux 自己编码网络序。
  全请求/句柄/尾部验证和回复预留先于副作用，新增查询项只追加既有选项表。Linux 将成员准入的资源不足投影为 ENOBUFS，原生调用者仍得到领域分配错误。

验证扩展现有 NET/NIC 夹具，覆盖标准报告、查询器代际切换/晚到期、旧报告抑制、无地址报告、GSS 合并/分段、IPv4 UDP/RAW 准入、共享引用、失败与启停清理。
另覆盖双接口选择/单次覆盖、mapped UDP、单播隔离、软件/卸载 checksum、TTL 0、HDRINCL、发送冻结、背压回送去重及原接口 IGMP 消费。
benchmark 决策：泛型机制没有运行时分派，来源保留各自地址宽度；单播不查询成员表，组播按组/接口二分查询。控制报文沿原有 NIC 预算，无逐数据包新分配。
发送控制不新增全局状态、锁或队列；仅有成员且开启回送时，在既有 lo 容量中增加标准所需的一份副本，回送不再准备第二个临时包。
不新增独立 benchmark 门禁；最终统一测量双族组播/单播并发、回送开启/关闭和 NIC 背压成本。
统一系统验收仍须覆盖交换机 snooping、真实网卡接收过滤、Linux user-copy/互通及转发与桌面并发成本；宿主测试不代替这些结论。
应用来源筛选、组播路由和查询器/代理服务仍未提供；不宣称完整双栈组播交付。IPv6 范围见[链路契约](ipv6-link.md#组播监听)。


## IPv4 主机分片发送

- 原生 UDP/RAW 在各自原发送队首保存已提交的 IP 载荷游标及冻结的发送策略，不建立分片队列或临时完整 UDP 包。
  `packet_wire::ipv4::fragment::header` 是主机、转发再分片及本机回复共用的无状态窗口：保持 ID、绝对 offset/MF、复制选项，非首片移除不带 copy 位的选项并重算 checksum。
  正常未分片主机报文不调用窗口划分，不增加完整载荷复制或重复 header checksum。
- UDP 分片在实际需要时计算并保存一次完整 checksum；元数据和载荷发布后不可变，后续片/背压不重复扫描完整数据。
  第一片包含 UDP header，后续片直接借用原载荷区间；普通报文继续请求 NIC checksum 卸载，最终 adapter 自己安装伪头种子。
  multicast 回送仍只交付一次完整包；物理分片不会把本机回送重放成多份数据报。
- `ProtocolDevice::reserve` 返回真实 reservation 错误；标准 Device 的 Option 适配不决定原生发送成败。
  `transmit_ipv4`/`transmit_ipv6` 共用最终 Ethernet/IP commit；IPv4 仍借用唯一 ARP owner，IPv6 借用唯一 ND owner。
  只有实际提交成功才推进游标，WouldBlock 保留队首/同一 ID，硬错误退休报文并沿原设备诊断及丢包计数处理。
  旧库 dispatch_ipv4_packet/parts 已无调用方并移除，不能保留只观察编码完成的另一条原生发送入口。
- 每轮 UDP/RAW 仍共享各自原 64 次尝试预算；未完成分片继续调度，邻居/设备背压等待原事件或重试期限。
  出口 MTU 变化后只重新划分尚未提交的区间，不能重新发送已提交的前缀；已排队包不采用之后修改的 socket 策略。
- 本机 codec 产生的 ICMP 回复由 IP owner 清除库默认 DF；RouteReplies 及 native TCP 回复共用原队列，IPv4 回复同样保存提交游标。
  非原子且 ID 为零的本机回复在输出前取得稳定 ID。小 MTU 不抑制本机错误，也不依赖转发启停；各片仍按实际路由/ARP 发送。

### 标准选项与 RAW

- `IP_MTU_DISCOVER(10)` 交给实际 UDP/RAW owner，默认 WANT(1)，支持 0..5；负值/超范围 EINVAL，支持单字节/int，零长度等同 DONT(0)。
  DONT/WANT/OMIT 允许主机分片；DO/PROBE/INTERFACE 超过所选上限返回 EMSGSIZE。
  PROBE/INTERFACE/OMIT 使用实际接口上限，其他模式使用当前路由 MTU；WANT 小包设置 DF，大包本机分片时清除 DF，DO/PROBE 保留 DF。
  当前路由无 MTU lock 控制。TCP 使用动态路径 MTU 分段，设置 IP_MTU_DISCOVER 返回 ENOPROTOOPT。
- INET `SET_OPTION` kind 23 为逻辑 u8 模式，值域 0..5；查询在选项表追加编号 29，不改变已有编号。
  全请求/句柄/尾部验证及回复预留先于副作用；Linux 只处理 UAPI 编号、user-copy 和 errno，不保存 mode 镜像。
- RAW 不带自定义 header 时按同一策略自动分片；HDRINCL 则由应用负责分片，每次完整提供的片必须不超过实际接口 MTU，不能自动再分片。
  HDRINCL 支持完整 20..60-byte 标准选项和 MF/offset，校验合法 flags、非末片八字节边界及最大长度；总长度/checksum 重算，源/ID 只在零值时填写。
  保留用户 TTL/TOS/protocol/选项和显式源/ID。IPv4 header 目的与 sockaddr 不同仍明确拒绝，不把路由目标与线上目标混为一个影子字段。

### 成本及仍需完成

实际 NET/NIC 用例覆盖超过单轮预算的大报文、真实 submit 背压、游标/ID 连续性、后改策略不影响旧包、NIC 校验卸载、
另一 NET 完整重组、RAW 自动/自定义分片及选项、自定义超 MTU 拒绝、组播逻辑接口回送、小 MTU 本机 ICMP 分片。
原有超 MTU 用例明确设置 DO 验证失败前不入队；接收用例保留不完整/重叠拒绝，不再把“拒绝一切分片”作为预期。

benchmark 决策：不新增 payload 池、每片分配、锁或服务往返；UDP 仅增加每数据报一次必要完整 checksum，普通发送保留卸载。
最终统一测量小 MTU、背压、正常报文与分片并发的 CPU/吞吐及元数据内存，不新增独立窄阈值门禁。

### 非原子报文标识

- `NetworkStack.identification` 拥有两族自动分片标识。IPv4 按线上源、目的和协议精确区分；端口、接口及 socket 不分割编号空间。
  UDP、RAW 自动编号及本机 ICMP 共用；DF=1 且无分片的自动报文保留 ID=0，转发及应用显式提供的 RAW ID 原样保留。显式 RAW ID 的唯一性由调用者负责。
- 按 RFC 6864 保留在途报文编号，随机初值参考 RFC 7739 §5.1。每组三元组的序号分为 64 个块，跨入下一块前等待旧记录全部退休并经过 120 秒 MDL。
  只在一次进入期间顺序分配不同 ID；整块等待可保守延后使用，绝不依赖哈希碰撞或提前回绕。
- 队列记录持有编号直到最后一片提交或整条报文取消，排队超过 MDL 不允许回收。入队失败、路由/发送失败、超时与关闭均释放实际记录，退休时开始隔离。
  RAW 若在首次发送前因 MTU 下降需要编号，与本机回复复用头部补写；UDP 普通 DF 发送不保留编号。
- 元数据上限 4 MiB/NET，按实际 AVL 节点大小计费；无预分配，每个新三元组一次取熵和节点分配，已有键单次树查找。
  原网络 timer 按固定预算回收安全过期的条目，不为容量驱逐活跃项。准入资源耗尽返回 ENOBUFS，节点分配失败返回 ENOMEM。
  已排队报文因 MTU 变化晚于准入取号失败时计发送丢弃；连接 UDP 还可读取异步错误。本机控制回复尽力发送。
- 单个三元组的非原子速率受 16 位空间与 MDL 约束。块隔离以少量保守等待换取固定历史大小；普通 DF 报文与 transit 快路径不查表。
  统一验收测量分片并发、回收预算和实际内存，不能用回绕用例证明目标规模吞吐。

### IPv4 路径 MTU 反馈

- `NetworkStack.path_mtu` 是双栈唯一估计表。按逻辑出接口和线上源、目的地址共享，端口不参与路径键。未关联时没有唯一查询目标。
  两族共用 1024 项上限和十分钟有效期。有效反馈只降低估计，设备 MTU 始终是上限。地址失效或出口停用时清理相关项。
- ICMP 类型 3/代码 4 需通过整条校验和及原 IPv4 头验证。引用允许截断，但须包含至少八字节原载荷并来自 DF 报文。
  引用源须是外层目的且仍属本 NET。当前路由须是经 ICMP 接收接口的单播出口，错误入口或路径不符时忽略。
  非零反馈至少 68 且小于原报文及实际设备 MTU。零值从 RFC 1191 的档位选择小于原报文长度的值，重复反馈不会逐次降低档位。
- UDP 核对原地址与端口，RAW 核对原协议、地址和设备约束，TCP 还校验 SND.UNA ≤ SEQ < SND.NXT。
  UDP/RAW 已关联时可通过原异步错误槽消费 EMSGSIZE，DONT 模式只学习上限。INTERFACE/OMIT 忽略路径估计，PROBE 按设备上限发探测报文。
  同一已排队 UDP/RAW 报文按新上限分片或失败。TCP 立即按降低的上限重分段并重发未确认数据。
- Linux `IP_MTU(14)` 从已关联 IPv4 路径读取上限，未关联返回 ENOTCONN。原生查询追加编号 31。
  `IPV6_MTU` 继续查询实际 IPv6 或 mapped IPv4 路径。UDP/RAW 错误记录见[数据报错误队列](socket-transport.md#数据报错误队列)。
  TCP 的 IP_MTU_DISCOVER 控制和跨接口返回的 IPv4 反馈仍需补齐。
- benchmark 决策：路径表按有效反馈懒分配，输出只查询同一有界表。普通 IPv4 接收不查询反向路径估计，TCP 接收用当前估计生成回应上下文。
  宿主网卡用例核对坏引用、协议间共享、探测模式、旧路由器反馈与 TCP 重分段。整机吞吐和来宾 ABI 仍待统一验收。

IPv6 主机发送见[分片发送契约](ipv6-link.md#主机分片发送)。
真实来宾 ABI、成熟系统互通及目标规模尚未验收。
