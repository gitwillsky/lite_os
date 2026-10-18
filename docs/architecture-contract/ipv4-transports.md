# IPv4 与 TCP/UDP 传输契约

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
重写旧队列；bound-device在vendor ingress enqueue前过滤。完整SocketSet在egress前生成
容量预留的scratch route plan（每轮clear），每设备同一occupied-slot顺序O(1)消费；
未选中的设备不能触碰TCP/queue/neighbor state。source tuple按IPv4 weak-host保留，
不因另一设备或删地址重置TCP。8-device批次在整个域轮次完成后轮换首设备，共享64 RX/
completion预算，持续lo/eth0流量不饿死其他设备。配置/IFCONF/GETNAME/INDEX/MTU/
SO_BINDTODEVICE getter查询实际设备，删除syscall eth0/index常量。raw创建按实际NET
owning UserNS NET_RAW授权，普通NETLINK RAW不能按INET raw类型误授权。
固定 smoltcp 的十个明确修改文件及三个相同consumer加入medium-ip，
原inventory不变，其余upstream文件仍禁止漂移。真实无NIC LO UDP/TCP握手/数据、独立
双NIC frozen-oif/bound ingress/LOCAL source/weak-host SYN回归源码已写。
route selector 使用同一 FIB 的 33 个 prefix AVL roots，查找不扫描全部 route records。
NET、packet、uevent 捕获创建域；NEWNET publication 提供声明范围内的独立 transport/ports/lo。
完整 Linux NETNS/veth/设备迁移不支持。

- Inet final fput 的协议 registration 由 `operation: Mutex<Option<InetEndpoint>>` 独占；
  immutable endpoint key 只用于分派，已 take 的 observer 不再 lookup 可能复用的 SocketHandle。
  close 与全部 payload loans 同一 operation 生命周期；busy protocol owner 时只向实际固定 ring
  发布一次 command，无分配。新的 task transaction 必须先清空既有 commands，再允许 bind/new，
  因而已经关闭的 port lease 不会跨下一个 task transaction 存活。deferred poll 仍采用原固定 budget。
  actual Inet/smoltcp host fixture 覆盖 UDP slot+port 复用、listener 全 handles 退休、busy owner
  下 deferred close，以及 1024 个 fresh TCP 的真实 SocketSet 容量和 full cleanup ring。
  NIC、时钟与 deferred raise 是显式边界，protocol/port/ring 是生产实现；

SocketIoDirection/SocketTimeout 是向 syscall/文件 I/O caller 的语义超时接口。SocketTimeouts 的唯一 Mutex 只持 native receive/send policy，不持 backend state、wait membership 或 deadline cache；PreparedSocket 在 endpoint 副作用前 fallibly 预留该 Arc，OFD
façade、INET façade 与 TCP registry 引用同一个实际 owner。INET setter 按 operation → stack → policy 加锁，协议 poll 在 stack guard 下冻结 passive child 的值。wait_for_ofd/wait_for_socket_capacity 的 deadline 必须由 caller 在重试循环外冻结，
None 保留其他文件无限等待。已到期先无分配复查 level readiness，再返回 TimedOut；普通期限发布原有 WaitRegistry membership。socket::interrupted_wait 只根据本次期限决定内部 RestartSys 或用户 EINTR，不能重读后来改变的 policy。
timer::SCHEDULER_TICK_NS 是实际 timer cadence 的唯一投影，socket 禁止复制 HZ。native codec、owner、deadline、getter、guest 收发/accept 及既有 CPU benchmark 纳入集中门禁。

SocketPollState.read_closed 是实际协议/transport 半关闭的只读投影；file poll 仅按请求输出 POLLRDHUP，不从双向 HUP 猜测。TCP 使用实际 protocol FIN/closed state 和已有 shutdown_read，UDP 从实际 EndpointState.shutdown 投影 half close，raw/packet
不伪造。socket 与 OFD readiness 固定容量为三项，stream 观察两个实际 direction notifications 与 endpoint error/lifecycle notification；epoll 为这些实际 sources 预留三个 membership nodes，无 readiness cache。consume_wait_notifications 只排空三路通
知，不消费实际 chunks。无 requested data events 时仍观察 error/HUP 的实际 endpoint notification。

TCP passive child 的不可变 policy 属于实际 TcpHandle.passive；标准最终 ACK 将 smoltcp handle 转入 Established/CloseWait 时冻结 listener timeout 与 InetSocketOptions。accept 消耗该 handle，初始化已预留的新 policy 并移交端口引用；accept OOM 不
摘取 child 或重读 listener。TCP_NODELAY setter 不重写已经冻结的 pending child；未完成握手的失败复用实际监听 slot；已经建立的 child 即使随后 RST 仍留在该 handle 的 accept queue，连同握手地址与一次性 pending error 一起移交，无
平行 pending index。每次实际 ingress state change 后立即观察生命周期，保证同轮 ACK→RST 不丢 child；EthernetRxToken 只交付当前帧的 typed RST endpoints，只有协议库确实接受并转入 Closed 才消费终止事件。TcpConnection 的不可变
local/peer 是实际连接身份，协议库清除路由 tuple 不释放 local 绑定；peer 查询仍检查实际 TCP phase。terminal 记录唯一 Closed 事件已交付，防止 SO_ERROR 或 I/O 消费后重复重建 RESET。SHUT_RD 保留已排队字节；接收顺序为数据、
已收到 FIN 的 EOF、消费 pending error、半关闭 EOF，TCP recvmsg 不返回源 sockaddr。依据固定 Linux tcp_check_req → tcp_v4_syn_recv_sock → inet_csk_clone_lock/ sk_clone_lock。bench 决策：policy copy 无分配，沿用实际 timeout
snapshot 热路径门禁并加入双方向冻结成本；实际 wire 握手、最终 ACK 前后修改、OOM 重试、同批 ACK/RST、延迟 accept RESET、SHUT_RD queued data 和 FIN/RESET 优先级由共享 actual INET host fixture 覆盖，guest 增补无限/有限/禁用 timeout
的 SA_RESTART 对照。

TCP_MAXSEG 的 user_mss 是实际 InetSocketOptions policy，send_mss/advertised_mss 属唯一生产
smoltcp TCP mechanism。0 清配置，非零接受原生 88..32767。advertised_mss 在 active open/收到 SYN
固定；不能在 accept 或最终 ACK 用 parent 最新 policy 重建广告。send_mss 与 peer MSS/route 同时
形成，Nagle、分段、Reno 必须使用此实际结果；ESTABLISHED setter 不篡改协商。
parent listener 的 send_mss 是进入 LISTEN 时移交的不可变 scalar，children 各持自己的 cache；
getter 不从某个 pending child 估计 parent 值。disconnect 按 native memset(rx_opt) 清 user_mss，
保留 cache；placeholder 与可失败的新 storage 在实际 handle publication 前移交 cache。
新增机制无 global/lock/unsafe，受既有 protocol TaskMutex/payload loan 保护；policy mutation和机制
更新同一 guard，不能让 getter 与 SYN/数据出现两个值。源围栏覆盖握手捕获、真实分段与断开
归属，并提供删去捕获/分段约束、篡改 cache、保留旧 cap 的负例。

TCP lifecycle benchmark 复用完整实际 NetworkStack/SocketSet/端口 owner 与 NIC boundary，既有 200ns hot-path 门禁分别测量实际 pending child 观察和 1024-handle 有界扫描的每 handle 成本；真实 RST wire/NIC poll 的分配与收包冷成本独立报告。
未读数据的最后 OFD close 使用实际 tcp.abort，完整接收则 tcp.close，后续 orphan 保留直到真实协议退休。

active connect 的原始 IpListenEndpoint bind identity 随 Connecting→TcpConnection 移交，不能与协议选择的 source address 混同。握手失败保留 Connecting 与一次终止事件，SO_ERROR/读写消费不重新生成；blocking connect 或再次 connect 完成该失
败时，按 __inet_stream_connect 恢复原 bind 与未连接 read policy，错误已被消费则为标准 ECONNABORTED。PortNamespace::restore_bound_address 是原 wildcard→exact 的无分配 inverse，复用同一 occupancy/bitmap 与 port capability，explicit bind 地址不改
变。非阻塞失败尚未完成 connect 时 local source 仍保留。

- UDP 的实际 local/peer tuple 唯一属于 vendored smoltcp Socket；EndpointState 删除 peer 副本，
  只持 Linux user_bind_locks/sk_shutdown policy 与真实 port lease。connect 在实际协议对象中
  设置 source address/peer；disconnect 使用同一对象修改 tuple，不 close/recreate/flush queue，
  未锁定地址/端口归零，精确 bind 保留锁定部分，SO_BINDTODEVICE 解除，未锁定 lease 同事务退休。
  shutdown 未关联也提交位集合后返回 ENOTCONN；connect/disconnect 保留位集合。receive 先读
  实际 queue，空且 RCV_SHUTDOWN 才返回无 sockaddr 的 EOF（namelen 为零，name buffer 不写入）；后续 ingress 仍可排队，peek 不消费。
  所有直接/softirq edge poll 共用 poll_state_locked，RCV_SHUTDOWN 为 readable/RDHUP，双 shutdown
  为 HUP，writable 仍按真实 sender queue。操作锁外通知 waiters，没有平行生命周期队列。
- 实际 UDP ingress 在 transport admission 匹配 peer 后入队；单播按 exact local tuple/connected
  peer 与实际 bind/rehash publication order 选一个目标，广播/组播交付全部匹配 sockets。
  order AtomicUsize 只签发不可重用全序、不发表 protocol 内容，NetworkStack guard 仍为唯一
  publication owner；缺失该 order 会按创建顺序而不是实际 bind 顺序误选 SO_REUSEADDR 收件人。
  TX metadata 在实际 enqueue 冻结 source port/address，disconnect/rebind 不修改旧 packet，
  source 地址由当前 actual interface/local tuple 准入，不能在 dispatch 借新绑定改写。
  新扫描无分配，与现有 SocketSet demux 同一界限；实际 wire peer reject、connect/reconnect、
  queue-preserving disconnect、bind-lock、shutdown/EOF 后新 ingress 和 TX source 冻结回归
  属于集中门禁；wire demux/loan/readiness 热路径沿用实际 benchmark，不降低门槛。

InetSocketOptions 是 INET endpoint 的唯一选项 policy；InetSocket::option 仅在 operation/stack guard 下读取实际 map，Socket::internet_option 是无状态 typed 查询 seam（InternetOption 不含 ABI number）。TCP 私有 handles 只由
tcp::update_transport_options/lifecycle/accept/io 修改，apply_tcp 无分配初始化 Nagle/TTL；已冻结 passive children 保持旧 policy。raw endpoint 也拥有同一 concrete options，receive 返回实际 IPv4 packet destination；不查询 Task 或复制 façade flags。UDP
transport 的 TX UdpMetadata 在实际 enqueue 冻结 source port/address/TTL，mechanism setter 只影响未来入队。测试只读 tcp::transport_options_match 在 cfg(test) 内观察实际 transport，不实现第二份 lifecycle。

IP_PKTINFO 源码归属已改为实际 queue metadata：UDP UdpMetadata 与 raw RawMetadata 在 ingress 入队冻结 interface_index/specific destination，raw 原始 IPv4 header 保持实际 destination。ReceivedMessage.internet_packet_info 是随本次接收移交
的 typed fact，syscall 只编码标准 in_pktinfo 的 iif/spec_dst/addr；peek 与接口重配置不能重新计算旧报文信息。raw 绑定地址只由实际 transport 持有，删除 RawEndpointState.bound_address 副本，raw_v4_match 式目的地址过滤发生在 enqueue 前。

发送 IP_PKTINFO 由 syscall 按固定 Linux ip_cmsg_send 解析到单次 InternetSendPacketInfo：非零 oif 更新本次请求、spec_dst 每条更新（零表示本次自动选源），不使用 ipi_addr 选源。send 的唯一 context 传入同一 facade/transport route owner，
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
UDP/raw发送权限与实际vendored ingress/next-hop分类消费同一配置投影，不在syscall重建broadcast。
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
通用 UDP/raw PMTU/fragmentation、RIB、多地址与设备迁移不在当前支持范围。

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
RST原表示只由实际TCP构造，standalone dispatch和kernel RouteReplies均复用原Packet codec。
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
的tcp_send_probe0/tcp_xmit_probe_skb。九文件vendor profile未扩大，无新lock/global/unsafe；
字段受actual transport/NET guard独占；沿用实际每slot clock/完整poll200ns benchmark门禁。
原 vendor probe 回归已按空 ACK、有效零窗口回复刷新期限、拒绝确认未发送字节维护，未删除覆盖。
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
新增实际单 NIC wire 回归覆盖未发送数据/FIN 的非法 ACK 与 RTO 回退后合法 ACK，vendor
回归覆盖提交失败 SYN/数据；固定依据同一 Linux tcp_input.c 的
`tcp_ack`/`tcp_rcv_synsent_state_process` 及 tcp_output.c 的 `tcp_acceptable_seq`，沿用实际
TCP 每 slot 和完整 poll 的成本门禁，不改变阈值。

初始 NET 的创建与固定 ID 登记只在同一个 fallible Once 初始化事务内进行，避免并发 boot caller 发布重复目录 key；失败仍由实际 owner 退款与退休，无旁路登记。

Readiness 快照与 poll 后 transition capture 各自对 endpoint AVL 做一次零分配 O(N) 中序遍历。
分离只读 SocketSet 与可变 endpoint 借用，不逐项做后继查询或二次树查找。TCP/raw readiness 仅借用 native SocketSet；通知提交继续使用有界 cursor batch，保持 false → true edge 语义。

RAW ICMP 与 RAW255 由已验证的 InetProtocol 类型进入同一构造入口。RAW owner 保存唯一
不可变 protocol 和可变 HeaderIncluded codec policy；RAW255 默认启用并禁止接收。发送
在实际 FIB/MTU 准入后填 total length/checksum，仅补零 source/ID，再向原 transport queue
提交完整 header。接收保存真实未分片 IPv4 bytes；PKTINFO 仍由同一 queue metadata 投影。
smoltcp 的 preserved frame 输入与普通 Packet repr 进入同一 egress/neighbor engine；无额外
路由/队列/状态 owner。基础 header 之外的 options/fragments、目的不一致显式拒绝。
新增 RAW codec 的 benchmark 决策：冷发送多一次既有有界 packet copy，完整 NET poll
沿用每 slot 200ns 成本门禁；无新增锁/global/unsafe。
