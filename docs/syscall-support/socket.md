# Socket syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 198 | `socket` | Partial | AF_UNIX、AF_INET、AF_INET6 TCP/UDP、AF_PACKET、有限 AF_NETLINK（`NETLINK_KOBJECT_UEVENT` 与只读的 `NETLINK_ROUTE`） |
| 199 | `socketpair` | Complete | AF_UNIX stream/datagram/seqpacket |
| 200 | `bind` | Partial | 支持 domain 的 address scope |
| 201 | `listen` | Partial | AF_UNIX/双栈 TCP backlog；Linux signed-32 ABI 与 4096 kernel cap |
| 202 | `accept` | Partial | AF_UNIX/双栈 TCP |
| 203 | `connect` | Partial | AF_UNIX/双栈 INET endpoint |
| 204 | `getsockname` | Partial | 支持 domain |
| 205 | `getpeername` | Partial | connected endpoint |
| 206 | `sendto` | Partial | stream/datagram/raw/packet scope |
| 207 | `recvfrom` | Partial | short buffer、TRUNC/PEEK/DONTWAIT |
| 208 | `setsockopt` | Partial | 已声明 SOL_SOCKET/IP/TCP options |
| 209 | `getsockopt` | Partial | 已声明 SOL_SOCKET/IP/TCP options |
| 210 | `shutdown` | Partial | 声明范围内 stream/packet 半关闭 |
| 211 | `sendmsg` | Partial | iovec、SCM_RIGHTS、atomic message limits |
| 212 | `recvmsg` | Partial | iovec、cmsg、CLOEXEC/CTRUNC/TRUNC/PEEK |
| 242 | `accept4` | Partial | NONBLOCK/CLOEXEC |

## Domain scope

- AF_UNIX 支持 abstract/pathname stream/datagram、peer credential、SCM_RIGHTS、pathname inode
  identity、actual sender memory/peer queue policy 与 cycle-safe inflight collection。
- `SOCK_SEQPACKET` 已加入 bind/listen/connect/accept、accept 前排队、half-close drain、
  native 显式地址忽略、历史 peer 地址和未读/未 accept close RESET 源码与回归。
  sender 内存预算、并发 admission 与关闭错误优先级由同一 endpoint owner 判定。
- AF_INET 当前提供多接口及真实 IP loopback 的 IPv4 TCP/UDP、按协议号选择的 RAW 与已声明的 AF_PACKET tap。AF_INET6 TCP/UDP 支持回环、直连/静态网关单播及 mapped IPv4，RAW 提供原生 IPv6 收发；完整 IPv6 路由与真实多网卡验收尚未完成。
- AF_NETLINK `NETLINK_KOBJECT_UEVENT`标准DGRAM/RAW、按实际NET隔离的端口、group0/1订阅与重bind、visible TGID自动绑定、实际接收预算/FIFO/PEEK/ENOBUFS已补源码；userspace send/connect及其他协议未开放。
- blocking、nonblocking、pselect/ppoll/epoll 共用 backend level recheck；notification edge 不是第二份 readiness state。
- AF_PACKET直接发送的所选adapter同步`Device`错误映射EIO，无TX容量为EAGAIN，
  超长frame为EMSGSIZE。INET协议回调的硬件fault归属实际设备诊断/counter，不能由
  任意socket消费为SO_ERROR；socket错误只来自实际协议事件。

## 已知缺口

TCP_MD5SIG 与 TCP_MD5SIG_EXT 的主机/前缀密钥、添加/替换/删除及双栈报文签名已接通；密钥不提供 getter。
EXT IFINDEX 的 L3 master/VRF 作用域当前返回 EOPNOTSUPP，TCP-AO 尚未提供；范围与验证边界见 [TCP 认证契约](../architecture-contract/tcp-authentication.md)。

完整 IPv6 地址配置/路由/转发、错误队列附带信息、组播来源过滤与路由、完整 rtnetlink/veth/设备迁移、userspace netlink publish 与高级 TCP 扩展仍未完成。
当前多接口及 IPv6 UDP 的具体范围由下述传输契约维护；NEWNET 只提供其中已声明的能力。

原生 netstack 可独立重启（Linux 没有对应情形）：重启前创建的 AF_INET/AF_PACKET socket 之后的操作以 `ENETDOWN` 失败，
已建立的 TCP 连接不保留；新 socket 进入重建后的网络域。非初始网络域重建后与新建的 NEWNET 一样从 DOWN 回环开始。

双栈分片接收已接通[原生重组](../architecture-contract/ip-reassembly.md)。IPv4 UDP/RAW 提供分片发送及 IP_MTU_DISCOVER(10) 的 0..5 模式控制；IPv6 UDP/RAW 提供分片发送及 IPV6_DONTFRAG(62) 开关。禁止本机分片时超 MTU 返回 EMSGSIZE。
AF_INET SOCK_RAW 支持协议号 1..=255，按实际协议精确接收；IPPROTO_RAW(255) 默认 IP_HDRINCL、只发送。
创建仍要求目标网络域的 CAP_NET_RAW；零 RAW 协议返回 EPROTONOSUPPORT，INET 协议号超出 0..=255 返回 EINVAL。IPv6 RAW 的报文边界、校验和与已提供选项见 [RAW 契约](../architecture-contract/raw-ip.md)。
RAW 的 connect、AF_UNSPEC、半关闭与 ICMP_FILTER/ICMP6_FILTER 已接入同一 owner；过滤器短更新保持未覆盖位，实际接口/来源、队列与验证边界由该契约维护。
IP_HDRINCL 支持完整 20..60-byte IPv4 header/options 及应用自带的标准分片；每片须不超过接口 MTU，保留合法 TTL/TOS/flags/protocol，总长度/checksum 重算，source/ID 只在零值时填充。header destination 不同于 sockaddr 仍返回 EOPNOTSUPP。
IPv4 UDP/RAW/TCP 通过 IP_MTU(14) 查询关联路径上限。Fragmentation Needed 的匹配、错误和发送行为见[IPv4 路径 MTU](../architecture-contract/ipv4-transports.md#ipv4-路径-mtu-反馈)。
AF_PACKET 出口 tap 在网卡之前；设备启用 TX partial checksum 时，内核生成的 TCP/UDP 帧可能仍含伪头 seed，而非线上最终 checksum。
标准 SOL_PACKET/PACKET_AUXDATA 支持 i32 开关与短 getter；recvmsg 输出 20-byte tpacket_auxdata，卸载请求标明 TP_STATUS_CSUMNOTREADY。
USER、原长、捕获长度与 L3 偏移来自实际帧，PEEK 保持信息；用户缓冲截断不改变捕获长度，短控制缓冲沿既有 MSG_CTRUNC 规则。
RX checksum 已验证或驱动补全后标明 CSUM_VALID；未验证包保持无校验标志。未协商 GSO/VLAN 剥离时不伪造其状态位；不支持 packet ring/fanout/BPF。
AF_PACKET 自行发送的帧不请求卸载，内容保持调用者字节。
TCP/UDP/raw 当前使用固定有界协议缓冲，INET 的 SO_SNDBUF/SO_RCVBUF 调优明确返回 EOPNOTSUPP，不伪造 getter 或设置成功。

新增 AF_UNIX 与 KOBJECT netlink MSG_PEEK 已有原始队列消费模式及回归源码SCM_RIGHTS peek 复制实际 OFD、保留原 inflight 与 readiness。family-only autobind 与按 socket type 隔离的 abstract bind 已修改源码及回
归。SO_PASSCRED/SCM_CREDENTIALS 已接通源码（包括 actual PID/real IDs、显式 claim、namespace 输出、partial control、零长度 Message 与 MSG_PEEK）；NEWNET 保留独立 protocol/ports/lo 的声明范围；完整 IPv6 与多网卡系统验收尚未完成。

SO_RCVTIMEO/SO_SNDTIMEO 的 LP64 OLD/NEW option、标准 timeval 错误、getter 短缓冲区、实际收发/accept/TCP connect deadline 与 signal restart policy 已改源码。AF_UNIX listener 满时的 blocking connect、native backlog + 1
admission、关闭竞态与固定调用 flags/deadline 已补源码和回归；SeqPacket 生命周期已补源码与回归。TCP 协议 options 在最终 ACK 建立 child 时冻结，accept OOM 保留快照；收发 timeout 当前在 accept 时继承，与
  Linux 握手时冻结存在差异；无限/有限/重新禁用超
时的 SA_RESTART 回归已补源码。

完整 108-byte 非 NUL pathname 的 getsockname/recvmsg address 输出按 Linux 追加终止 NUL，storage 容纳标准 111-byte 输出，短 name buffer 仍回报完整长度。此前该场景可导致内核 slice 越界；原 codec 已根修并补 guest 绑定/连接/accept/短输
出回归。

AF_UNIX pathname 队列 source/peer 地址按实际 endpoint 查询，支持发送后 bind/close 的历史地址。live binding/accepted child 持 actual VFS inode pin，最后 OFD close 释放；新增 unlink/recreate guest 和完整实际 owner host 回归。datagram
AF_UNSPEC disconnect/reconnect 与已关闭 connected peer 的清除已接通源码；stream SHUT_RD drain、SO_ERROR/RESET 和独立 POLLRDHUP 已补源码与回归。通用 sockaddr 输入先复制一次最多 128 bytes，再按 family 长度校验，AF_UNIX 保持
110-byte 输入上限。

packet sender memory、native 非互连 queue + 1、互连/SeqPacket/self 的不同 admission，以及按 actual front packet 分配的 receive staging 已补源码/host 回归。AF_UNIX 长度由 actual sndbuf-32 校验，支持超过 IPv4 u16 长度；SO_SNDBUF/RCVBUF
的 unsigned clamp、倍增、actual metadata floor、共享 OFD policy 与 Unix accepted child 默认值已接通源码及回归。stream 已以实际 chunk FIFO 拥有 payload/control/cursor/整块 credit；部分读取保持 allocation fee，完整 chunk/GC/close 才退款。没有
固定 64 KiB ring 上限，connect 只准备 direction notifications；peek/partial/OOM/close 已补回归。网络 sysctl 与控制消息 optmem 的现有源码范围见下文；INET 固定有界缓冲不支持动态 SO_SNDBUF/SO_RCVBUF；不声明完整 Linux 预算能力。

补充源码：全局`/proc/sys/net/core/wmem_default,rmem_default,wmem_max,rmem_max`
已接入唯一实际owner，default212992、max4MiB；SO_SNDBUFFORCE(32)/RCVBUFFORCE(33)
复用实际AF_UNIX预算路径，initial UserNS全局CAP_NET_ADMIN授权，负值归零/绕过max。
`optmem_max`初始128KiB，socket创建/accept捕获实际NET policy，heap control在分配前预收费，
协议send结束及OOM/fault/解析失败退款；36-byte栈路径不收费。各 NET 使用独立 optmem policy；INET 缓冲调优不支持。

TCP 已补同批最终 ACK/RST 的真实 child 发布、已建立 reset child 的延迟 accept、local binding 保留、一次 pending error、SHUT_RD queued drain 与 FIN EOF 优先；新建/关闭 poll、active refusal 与 recvmsg 不发布源 sockaddr，以及 TCP 对已导入的
目的 name 忽略语义已改源码。

TCP AF_UNSPEC 与 shutdown 的 SYN_SENT/listener/CLOSED 状态转换、读关闭主动唤醒、取消后原 bind 恢复，以及RST实际payload移交有界报文队列后立即退休transport/端口身份，已补源码和实际 owner 回归。AF_INET UDP AF_UNSPEC/半
关闭已接通同一实际 transport queue 与 native policy owner 的源码。

同时打开 SYN_RECV 的 shutdown 权限保留在实际握手中，最终 ACK 后才移交连接并发 FIN；ACK 前 connect 仍 InProgress、send 仍 Again 且无 POLLOUT。真实 SYN→SYN/ACK→ACK→FIN 报文回归已补源码。

UDP tuple、connected peer 与队列由唯一原生 UDP owner 持有，在 ingress 准入前匹配地址/端口；
重连与 AF_UNSPEC 保留既有消息，依据真实 user bind locks 归还未锁定 local port/address。
未关联 shutdown 仍提交位集合后返回 ENOTCONN，空 RX 的读返回无源地址 EOF，后续 ingress
仍可入队；connect/disconnect 不清除 shutdown。packet EPIPE 不生成 stream SIGPIPE。
source wire/host 回归与原生 musl bind/shutdown/EOF/SIGPIPE 用例已补。
SO_REUSEPORT、UDP error queue 与 INET 动态预算尚未开放；多接口和当前 IPv4 RIB 范围见网络契约，不声明完整 UDP 兼容性。

SCM_RIGHTS copyout 已补 native 源码/回归：每个 FD 复制编号后发布，scalar fault 截断尾部；control header fault 保留 FD 且不推进 controllen，name/flags/controllen fault 返回 EFAULT 并保留已安装 prefix。MSG_CMSG_CLOEXEC 同时作用于实
际 descriptor 与输出 msg_flags。recvmsg 初始负 namelen（非 NULL name）返回 EINVAL，输出重新读取原 namelen 并先写回实际长度。

现有 INET option 源码已完善：IP_TTL 的 TCP/UDP/raw、-1 默认、短 input/getter；TCP_NODELAY 与 REUSEADDR/BROADCAST/PKTINFO/BINDTODEVICE getter；TCP passive child 及重建 handle 的选项一致性；raw PKTINFO 输出实际 destination。UDP TX 按 enqueue
时 TTL 发送。IP/TCP getter 的 optlen 先于 value、SOL_SOCKET getter 的 value 先于 optlen；SO_BINDTODEVICE bound name 要求 input capacity ≥ IFNAMSIZ，unbound 输出长度零。NEWNET 与 sysctl 仅开放下述范围，其他 TCP options 不支持。

SO_MARK(36) 可在 IPv4/IPv6 TCP、UDP、RAW socket 查询与设置。值为 32 位，缺省零。设置要求 socket 所属网络命名空间的
owning user namespace 中具有 CAP_NET_RAW 或 CAP_NET_ADMIN，读取无需该权限。非 INET socket、每消息标记和 SO_RCVMARK 尚未接入。
能力检查对照 [Linux v6.12](https://github.com/torvalds/linux/blob/v6.12/net/core/sock.c#L1425)。

IP_PKTINFO 的实际 RX iif/spec_dst/destination、单次 TX oif/source 与 raw 入队前绑定过滤已补源码/回归。IPv4 静态 FIB 与 LOCAL 经 lo 投递使用同一 owner；多物理接口已接入同一选择路径；multicast 完整主机收发与设备迁移仍未完成。
IP_ADD_MEMBERSHIP(35)/DROP_MEMBERSHIP(36) 接通 IPv4 ASM 成员和 UDP/RAW 接收，支持 ip_mreq/ip_mreqn；IP_MULTICAST_ALL(49) 可设/查，默认 1。
IP_MULTICAST_IF(32)/TTL(33)/LOOP(34) 接通 UDP/RAW 发送与本机回送；IF 支持标准地址及成员结构输入，getter 返回地址，TTL/loop 支持短标量。mapped UDP 使用 IPv4 策略。
原生 IGMPv1/v2/v3 共享成员、报告与查询处理，接口选择、错误投影、生命周期及未提供范围见[IPv4 组播契约](../architecture-contract/ipv4-transports.md#ipv4-组播监听)。


回归覆盖：`/proc/net` 标准self/net符号链接，task net目录选择目标NET，
实际network fd固定该域并展示全部接口及发送丢包计数；普通proc snapshot不查询网络。
IPv4接口首次地址采用原生classful掩码，同地址no-op保留mask/broadcast，显式广播在mask
改变后保留并用于真实UDP/raw收发；lo默认broadcast零，无地址查询EADDRNOTAVAIL。
静态 FIB、单地址 MTU 与 NEWNET/setns 在下述范围开放；多地址未开放。


NEWNET标准入口已接真实独立protocol/ports/LO的源码；旧socket/OFD创建域保持，proc文件
固定其目标域，setns不重定向已有socket。clone3/NEWUSER权限、UDP/abstract隔离、实际LO收发
及 nsfs/PIDFD net 引用有对应回归；veth/迁移、完整 rtnetlink 与 INET 动态预算不支持。

SIOCSIFMTU已改为actual捕获NET接口配置：Ethernet68..1500、lo0..65536，
getter投影actual设备，protocol分段与packet长度校验使用相同配置。lo低于68退休IPv4地址，
SIOCSIFADDR返回ENOBUFS；增长MTU不恢复旧地址，再次DOWN→UP发表127/8。真实wire SYN
MSS 与设备 MTU 限制通过收发及恢复用例覆盖。TCP 使用已学习的路径 MTU 重分段，IP_MTU_DISCOVER 设置仍未支持。


IPv4静态FIB接入已补源码：删除gateway专用双轨；SIOCADDRT/SIOCDELRT按标准
LP64 rtentry family、连续mask/HOST、metric非0减1、直接/GW/REJECT和存在/删除错误进入
同一NET实际FIB。proc route读MAIN实际records；LOCAL经lo保留逻辑iif，IP_PKTINFO反向FIB
选源及即时跨NIC回复沿同一FIB。新增actual wire/OOM/native ioctl/captured OFD回归与
2048-route 成本由现有门禁验证；完整 rtnetlink、IPv4 多地址/veth 未开放；metrics 范围见后续段。

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
未声明的高级 TCP 选项仍未支持。

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
zero-window budgets 见下文。非 INET SO_KEEPALIVE 暂未接，
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
zero-window budgets 由同一 transport clock owner 管理。

NET 链路硬件 fault 由实际设备 owner 记录；单物理 NIC 与 lo 共用一套 outcome。NetworkPoll唯一完整outcome同时持backlog、
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
多网卡、完整 rtnetlink/veth/迁移不在当前支持范围。

PROBE0/USER_TIMEOUT继续补了源根修，全部未编译/未测试。实际Timer::ZeroWindowProbe拥有
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
ACK 准入上界是发送 high-water `send_seq_no`，与可回退的重传游标 `remote_last_seq` 分离；未发送的数据、SYN 与 FIN 不能被确认。keepalive 遇 route 失败时消耗一次 probe count，并安排后续 deadline，不伪造成功的 wire 发送。
zero-window 使用下述有界原生 timer。TCP 扩展支持范围见本节。

原生 endpoint 已使用双族地址值，系统服务的编码与生命周期见 [Socket 传输契约](../architecture-contract/socket-transport.md)。
Linux sockaddr_in6 保留地址、端口、zone 和 flowinfo；AF_INET6 SOCK_DGRAM 的 protocol 0/17 已接通同一原生 UDP，支持本机、显式直连与原生受管路由单播（含静态全局/ULA、链路地址）及 mapped IPv4。
IPV6_V6ONLY 在未绑定时可设，具体 IPv6 bind 固定为仅 IPv6；IPV6_UNICAST_HOPS 支持 -1/0..255，且不影响 IP_TTL 或旧 TX。
未显式设置时，关联 socket 查询所选出口的默认单播跳数。无关联路径时返回系统默认 64。主机通告可更新出口默认值，组播和 ND 等控制协议仍使用各自的跳数策略。
IPV6_RECVPKTINFO（49）可查询/设置，默认关闭；recvmsg 的 IPV6_PKTINFO（50）给出 ingress 接口和目的地址，双栈 IPv4 可同时请求 IP_PKTINFO。
发送 IPV6_PKTINFO 接受至少 20 字节，IP_PKTINFO 恰好 12 字节；按实际目的族及 cmsg 顺序处理，不能跳过中间无效控制项。PEEK 保留 ingress 事实并应用当前选项。
IPv6 RAW 提供精确协议、作用域、payload 收发及校验和选项，见 [RAW 契约](../architecture-contract/raw-ip.md)；IPv6 TCP 已接通标准连接/监听/收发/关闭。
IPV6_JOIN_GROUP(20)/LEAVE_GROUP(21) 接通 UDP/RAW 的 ASM 成员；IPV6_MULTICAST_ALL(29) 可设/查，默认 1。
IPV6_MULTICAST_IF(17)/HOPS(18)/LOOP(19) 接通 UDP/RAW 发送和本机回送，可设/查；默认值、接口优先级、scope 与生命周期见[组播契约](../architecture-contract/ipv6-link.md#组播监听)。
来源过滤、可配置组播路由及其他未列选项仍未接通；不宣称完整 IPv6 socket 兼容或来宾互通已验收。
IPv6 UDP 查询同一 NET 的本机/显式直连/受管 FIB；经网关发送保持远端 IP，ND 解析实际下一跳，失败交付一次 EHOSTUNREACH。
IPv6 静态路由经统一配置接入，BGP IPv6 来源已接通，其他动态协议仍待完成；无路由返回 ENETUNREACH。Jumbo 和 IPV6_MTU_DISCOVER 模式仍未实现。
IPv6 UDP 已生成回环、直连与静态网关回程的端口不可达；已关联 socket 收到匹配错误时 POLLIN/POLLERR 置位，send/recvmsg（含 PEEK）/SO_ERROR 一次消费 ECONNREFUSED。
不取消关联或释放端口，未关联且未启用 RECVERR 时不交付异步错误；有效 PTB 仍可更新共享路径 MTU。
IPV6_MTU（24）查询已关联 socket 的当前路径 MTU，未关联返回 ENOTCONN；设置 0 清除手工发送上限，1280 以上限制新发送，其他值 EINVAL。
主机采用路由通告的 IPv6 MTU 后，查询与分片使用同一链路上限。`ip link` 中的设备 MTU 仍反映实际设备设置。
PTB Type 2/Code 0 经匹配后收紧路径；已关联 endpoint 异步报 EMSGSIZE，同路径其他端口与后续发送使用实际路径限制。
低于 1280、坏引用/校验或不匹配 tuple 不改变估计；不因更大 PTB 增长估计。IPv4/IPv6 UDP 与 RAW 可设/查 IP_RECVERR(11)/IPV6_RECVERR(25)。MSG_ERRQUEUE 取原目的、可见引用和错误控制消息，空队列返回 EAGAIN。
详细 errno、offender、截断及未支持的附带信息见[数据报错误队列](../architecture-contract/socket-transport.md#数据报错误队列)。
flowinfo 暂未启用发送选项，实际发送/名称查询使用默认值；详细范围与系统验收边界见[传输契约](../architecture-contract/socket-transport.md#linux-边界与当前限制)。

## NETLINK_ROUTE

`socket(AF_NETLINK, SOCK_RAW|SOCK_DGRAM, NETLINK_ROUTE)` 让 `getifaddrs`（musl/libuv/Node `os.networkInterfaces`）与 iproute2 的
`ip link/addr/route` 工作，`ip neigh` 可查询双栈邻居。

**查询**：`RTM_GETLINK`（dump 与单设备查询）、`RTM_GETADDR` 双族 dump，以及双族 `RTM_GETROUTE` dump、输入和输出路径点查。
`RTM_GETRULE` 支持 IPv4、IPv6 和 AF_UNSPEC dump。规则的添加与删除返回 EPERM，沿原配置入口管理策略。
路由 dump 按 AF_INET/AF_INET6 筛选，AF_UNSPEC 返回两族冻结路由；完整目的、网关、首选源、出口及带权多路径按本族地址宽度编码，不把 IPv6 返回为空或截成四字节。
路由点查支持 `from`、`oif`、`iif` 和 `mark`，回复实际表、出口、源和立即网关。IPv4 单播输入须提供合法源；IPv6 未指定源时仅匹配无源前缀限制的规则。
组播、IPv4 有限广播输入、传输层键、显式 UID 和 `fibmatch` 暂返回 EOPNOTSUPP。
IPv6 链路地址输出 /64、link scope 及真实 tentative/dadfailed 标志；实际可用的 ::1 输出 /128、host scope；静态地址输出实际前缀/资格，无直连意图时带 NOPREFIXROUTE。AF_UNSPEC/AF_INET6 均可查询。
SLAAC 地址按动态来源输出，优选期结束时带 IFA_F_DEPRECATED。IFA_CACHEINFO 提供快照捕获时的剩余优选期和有效期。通告学习的单播路径标记为 RTPROT_RA。
原生直连/静态网关 ping 回复、邻居解析与限制见[链路网络契约](../architecture-contract/ipv6-link.md)；UDP 支持范围见上文，地址/邻居变化多播尚未开放。
`RTM_GETNEIGH` 支持 AF_INET/AF_INET6/AF_UNSPEC dump，以及按 ndm_ifindex/NDA_DST 的单族单条查询；严格检查头及属性，未知接口 ENODEV、未命中 ENOENT。
dump 接口过滤取 NDA_IFINDEX，按标准设置 NLM_F_DUMP_FILTERED；ndm_ifindex 不作为 dump 过滤。NDA_MASTER 非零当前返回 EOPNOTSUPP，不能伪装成未过滤结果。
原生 ARP/ND 状态映射 NUD_INCOMPLETE/REACHABLE/STALE/DELAY/PROBE/FAILED。已失败网关无可用 MAC，当前探测阶段可查询 NDA_PROBES，IPv6 NTF_ROUTER 由邻居 owner 提供。
单条查询与 dump 均不触发解析；代理邻居集合为空，代理查询未命中。FDB 和邻居变化订阅尚未接通。NEWNEIGH/DELNEIGH 受统一配置约束返回 EPERM。
以太网接口的 `IFLA_CARRIER`、`IFLA_OPERSTATE` 和 `IFF_LOWER_UP` 取驱动载波状态；`IFF_UP` 仍独立反映管理开关。
`/sys/class/net/<接口>/carrier` 与查询共用快照，`speed` 以 Mbps 为单位，驱动未报告时返回 `-1`。
dump 惰性生成：请求只冻结快照与游标，`recvmsg` 时按接收缓冲容量逐包生成，最后以 `NLMSG_DONE` 结束；被截断的数据报余下部分丢失（`MSG_TRUNC`）。
完整 FIB 快照经服务只读 VMO 捕获。多成员路由使用 RTA_MULTIPATH；单成员使用 OIF。同族网关使用 RTA_GATEWAY，IPv4 路由的 IPv6 网关使用 RTA_VIA。
原生权重先按公因数约分；比例仍超出 rtnh_hops 的 1..256 范围则查询回复 EOVERFLOW，完整多路径属性超出 u16 长度则 EMSGSIZE，不裁剪成员或修改执行状态。
/proc/net/route 按 Linux 约定只列首成员。接收存储准备失败不推进 dump 游标，peek 不消费；单条大消息仍完整生成并应用用户缓冲截断。
首次发送自动 bind；port 空间按（协议，NET）隔离。

**修改**（需要 socket 所属 NET 的 owning UserNS 里的 `CAP_NET_ADMIN`，否则 `EPERM`，检查先于分发）：
`RTM_NEWLINK`/`RTM_SETLINK` 修改 MTU 与 UP/DOWN（`ifi_flags/ifi_change` 按 Linux 语义，`NLM_F_EXCL` 得 `EEXIST`）；
`RTM_NEWADDR`/`RTM_DELADDR` 设置或删除 IPv4 地址、前缀与广播地址；`RTM_NEWROUTE`/`RTM_DELROUTE` 直接进入同一 NET 的 FIB，
`NLM_F_CREATE/EXCL/REPLACE/APPEND` 原样交给 FIB，`RTN_UNSPEC` 删除匹配任意类型。`NLM_F_ACK` 返回 `error = 0` 确认。
blackhole/prohibit/unreachable 保留独立类型；未指定 oif 的本机发送分别返回 EINVAL/EACCES/EHOSTUNREACH，明确 oif 沿固定 Linux route_output 的直接出口 fallback。

**明确不支持，返回 `EOPNOTSUPP` 而不伪造成功**：一个接口的第二个 IPv4 地址、点对点 peer、`eth0:1` 式别名 label；设备创建、删除、重命名、
改 MAC/txqlen 等其他链路属性与混杂/全组播等标志位；anycast/multicast 路由类型、多路径修改、源地址/TOS 路由、`RTAX_MTU` 等其他度量；
邻居、规则、qdisc 等其他对象；非 dump 的 `RTM_GETADDR/GETROUTE`（含 `ip route get`）；多播订阅（bind 的 `nl_groups` 非零，`ip monitor`）；IPv6 地址/路由修改（`EAFNOSUPPORT`）。

网卡移除（驱动宿主终止）按 NETDEV_UNREGISTER：接口及其地址/路由从 dump 中消失，绑定该 ifindex 的 AF_PACKET socket 解绑并报告 `ENETDOWN`（`POLLERR`）；
ifindex 不复用，`SO_BINDTODEVICE` 到已移除接口的 socket 无路由可选。因不支持组播订阅，不发 `RTM_DELLINK`/`RTM_NEWLINK` 通知。

`SOL_SOCKET/SO_ACCEPTCONN` 对照固定 Linux `net/core/sock.c::sk_getsockopt` 的 TCP_LISTEN 判定，支持四字节整数查询：仅实际处于 Listening 的 AF_UNIX/双栈 TCP endpoint 返回 1，
新建 stream、已 accept 的连接及无监听语义的协议返回 0；短输出长度仍沿用通用 getsockopt codec。
会话门禁的 Linux C consumer 覆盖 UNIX/IPv4 TCP 的初始、listen、accept 与 UDP/socketpair 查询。

系统服务边界：INET/AF_PACKET 通过 network-protocol channel 访问 netstack。epoll 无等待通知路径把它们保守作为候选，任务上下文经 RPC 精确复查。
stream send 每次服务调用最多传递 256 KiB，可返回部分发送；Linuxd 保留阻塞、信号、重试与 timeout 政策。
