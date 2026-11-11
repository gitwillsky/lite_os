# 原始 IP 报文

## 职责与可用范围

`netstack::inet::raw` 唯一拥有 IPv4/IPv6 RAW endpoint 的协议号、绑定、选项、接收/发送队列及 readiness。
两族使用同一个有界 datagram ring；Linux 编号、user-copy、CAP_NET_RAW 授权及 errno 仅属于 linuxd。
创建接受非零 8-bit IP 协议号；IPv6 不接收 mapped IPv4 地址，不借用 IPv4 协议栈。
原生创建仍使用 [SOCKET 版本 1](socket-transport.md#internet-endpoint-与格式版本)，没有另增 RAW 服务或私有应用 ABI。

当前提供 bind、connect/默认目标、地址查询、sendto/sendmsg、recvfrom/recvmsg、PEEK、截断、packet-info、设备绑定、半关闭及 ICMP 类型过滤。
IPv6 ASM 成员、发送控制与本机回送见[组播契约](ipv6-link.md#组播监听)，IPv4 ASM 成员、发送控制与回送见[IPv4 组播](ipv4-transports.md#ipv4-组播监听)。
分片接收沿[双栈重组](ip-reassembly.md)，IPv4 发送见[主机分片](ipv4-transports.md#ipv4-主机分片发送)；IPv6 发送见[主机分片](ipv6-link.md#主机分片发送)。错误交付见[数据报错误队列](socket-transport.md#数据报错误队列)。来源过滤与工具互通仍需补齐。

## 关联、解绑与半关闭

- connect 在同一 NET 事务中校验目标族、作用域、路由及广播授权后再发布默认目标与选源；失败保留原目标、绑定及队列。
  显式单次 sendto 可覆盖默认目标，不修改接收关联；关联后仅接收原对端来源，链路范围由唯一设备绑定限制。
- RAW IPv6 bind 的显式链路 zone 可覆盖此前设备绑定；单次 send 的 zone/设备冲突仍拒绝。两种操作不能共用错误的冲突策略。
- RAW 本地地址只保存 IP，不存另一份端口/zone；地址查询和默认发送从实际设备绑定派生作用域。IPv6 发送地址的非零 port 是协议提示，普通 RAW 必须与其协议相符；RAW255 可选其他上层协议。
- AF_UNSPEC 清除默认目标、本地收发地址及设备绑定；不丢弃已接收或已排队发送的报文。
  RAW 的专用 bind 不建立 UDP 的地址/端口锁，不能把 UDP 的锁保留策略直接移植进来。清除关联后 send 无默认目标返回 EDESTADDRREQ。
- SHUT_RD/WR/RDWR 合并到唯一关闭位；写关闭返回 EPIPE，读关闭先交付已有报文，队列空才返回无 sockaddr 的 EOF。
  未关联 shutdown 仍提交关闭位并唤醒等待者，再返回 ENOTCONN；取消关联和重连不隐式清除半关闭。

## ICMP 接收过滤

- 原生过滤器是八个逻辑 u32 的固定类型位图；位 1 阻止、位 0 放行，IPv4 ICMP 只使用第一个字。非 ICMP RAW 拒绝该能力。
  IPv4 类型小于 32 才受位图控制；IPv6 覆盖全部 256 种类型。过滤只影响 RAW 入队，不阻止系统 Echo/ND 或改变已入队数据。
- network-protocol 的 ICMP_FILTER(21) 版本 1 传输可选的 mask/blocked 更新，按 u32 编码，与 Linux 内存布局分开。
  GET 无更新；SET 在同一 owner 内合并 mask 选中的位，返回完整结果。请求无额外 handles，格式/尾部检查和回复预留均先于副作用。
- Linux ICMP_FILTER/SOL_RAW 和 ICMP6_FILTER/IPPROTO_ICMPV6 分别接受最多 4/32 字节；短长度只修改对应字节，零长度不访问 optval。
  syscall 层把机器字节布局转换成逻辑掩码，在 owner 原子修改，避免读后写覆盖并发更新。getter 先写实际 optlen，再写裁剪的字节，负容量拒绝。
  setsockopt 的 optlen 先按 Linux 32-bit signed ABI 转换，负数不进入截断或复制路径。

## 接收与报文边界

1. IPv4 在 IP 校验、地址/接口准入后，将去除链路 padding 的原始完整 IP bytes 借给 NET。
   RAW 按协议、绑定地址及逻辑入接口筛选，先于 TCP/UDP 校验；原始 ID、TOS、DF 与基础头均保留。
   匹配但接收队列满仍算已处理，未知上层协议不能再回复 protocol unreachable。
2. IPv6 沿原生本机目的及扩展头准入，在同一帧 guard 内投递 RAW。只交付 `upper_layer`，不含基本头或扩展头。
   未重组分片及 No Next Header 没有完整 upper layer，不伪装为有效负载。来源 sockaddr 保留真实链路地址作用域。
3. RAW 接收与上层协议处理独立：有 RAW 接收者不跳过 ICMPv6/ND 或 TCP/UDP 的协议工作。
   配置校验和的 endpoint 要求字段在有效载荷内，按实际源/目的、上层协议和负载长度验证伪头校验和；硬件资格不能绕过这一策略。

IPv4 IPPROTO_RAW(255) 只发送，默认 HDRINCL；不将它当作接收所有协议的通配符。
IPv6 的精确协议匹配及 HDRINCL 策略由其自身 socket 持有，不套用 IPv4 的接收禁用规则。
来源、目的和 packet-info 在入队时冻结；后续接口/选项修改不改写已接收报文。PEEK 不消费，短缓冲仍报告原长。

## 发送、选源与背压

- 发往显式目的的完整报文先沿唯一双族 FIB/选源/路径 MTU 准入；IPv4 广播仍要求 SO_BROADCAST。
  IPv6 packet-info 与 UDP 共用相同的顺序验证，peer zone、设备绑定和控制接口冲突返回 EINVAL；零源不清除之前的显式来源。
- IPv4 普通模式按实际 PMTU 策略分片；HDRINCL 接受标准 20..60-byte 选项及自定义 MF/offset，应用提供的每片须不超过实际接口 MTU。
  总长度/checksum 重算，零 source/ID 才填充；保留 TTL/TOS/flags/protocol/选项，sockaddr/header 目的不一致仍明确拒绝。
- IPv6 普通模式生成基础头，按 socket 校验和偏移填充 upper-layer checksum；未启用则保留调用者负载。
  普通模式超路径或应用 MTU 时按 IPv6 分片发送规则处理。HDRINCL 接受合法扩展头及用户提供的分片，按实际出口设备 MTU 准入。
  HDRINCL 保留源地址、流标签、扩展头和负载，拒绝头部长度与实际字节不一致的报文。
- 在 NET payload loan 内直接写未发布队尾：无临时报文分配或完整报文中转副本。
  长度、编码、首次存储或队列失败都不发布记录。实际队首保留已选源与逻辑接口，poll 时重新验证真实路由/MTU。
- RAW 输出按 endpoint ID 轮转并使用整轮预算；路径消失或禁止分片时的新 MTU 不容纳才退休，允许分片则仅重划尚未提交的区间；设备/邻居背压保留队首。
  TX completion/ARP/ND 活动可提前唤醒；ND 最终失败后按实际下一跳退休相关待发报文，释放队列容量。完整 IP 报文与即时回复复用接口的同一提交方法；只有实际提交成功才正常出队。

已关联 IPv4 RAW 可用 IP_MTU 查询共享路径，匹配的 ICMP Fragmentation Needed 可通过 SO_ERROR、send 或 receive 消费一次 EMSGSIZE。
IP_MTU_DISCOVER 的具体模式和学习边界见[IPv4 路径 MTU](ipv4-transports.md#ipv4-路径-mtu-反馈)。

## Linux IPv6 选项

- `IPV6_CHECKSUM(7)` 支持偶数非负偏移；负数禁用。普通 raw 默认禁用，ICMPv6 默认偏移 2，Mobility Header 默认偏移 4。
  `IPPROTO_IPV6` 层修改 ICMPv6 校验和返回 EINVAL；Linux 的 `IPPROTO_RAW` 层扩展可显式修改。查询从实际 endpoint 返回，不维护 facade 副本。
- `IPV6_HDRINCL(36)` 支持 IPPROTO_IPV6/IPPROTO_RAW 两个层级；只对 AF_INET6 SOCK_RAW 开放。
- `IPV6_MTU` 支持设置发送上限及查询已关联路径，`IPV6_DONTFRAG` 支持禁止普通模式分片。
- `IPV6_UNICAST_HOPS` 与 `IPV6_RECVPKTINFO` 复用现有标准选项。IPv6 RAW 的 V6ONLY 修改及未接通的高级选项明确返回错误。
- send gather 最多容纳 65535-byte IPv6 payload 加 40-byte 用户头；最终是否包含头及实际 MTU 由 RAW owner 裁决。
  receive 只交付至多 65535-byte upper layer，IPv4 仍交付完整至多 65535-byte IP 包。

固定语义来源：[RFC 3542 §3/ICMPv6 过滤](https://www.rfc-editor.org/rfc/rfc3542#section-3)、
[Linux v6.12 rawv6](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/raw.c)、
[同版 raw_v4_match/raw_rcv](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/raw.c)。

## 所有权、成本与验证边界

- 接收存储在 endpoint 发布前 fallibly 准备，IPv4 RAW255 只发送而不准备接收载荷；发送存储首次使用才准备。每方向八条记录、65575 字节，支持一个最大非 jumbo IPv6 包。
  相比原 RAW 小包队列扩大容量是为完整报文边界；常态未创建 RAW 时不占载荷。原每 NET 的预留旧 socket 槽和路由 scratch 已移除。
- payload loan 保持唯一队列归属；借出期间 poll 不进入不完整 owner。关闭沿既有 deferred cleanup 删除唯一登记，队列随之释放，不保留旧库 RAW socket。
- 热路径不新增锁、global、缓存或逐包分配；RX 只复制到匹配队列，TX 直接编码队尾并复制到最终设备窗口。
  过滤只增加固定字位测试，没有查表、每包分配或新锁。过滤修改为固定八字合并，仅控制面使用。
  benchmark 决策：复用既有 NET 收发/每 endpoint 扫描的成本测量；最终统一测量 RAW 活跃/空闲及 UDP 并发的分配量、内存和延迟，不增加独立窄阈值门禁。
- 宿主实际 NET/NIC 用例覆盖原 IPv4 头部/协议隔离，IPv6 回环 PEEK/原长/metadata、NIC 作用域、坏 checksum 丢弃、正确选源及关闭退休。
  另覆盖双族默认目标、来源过滤、失败重连、解绑保留队列、原子位更新、半关闭与 EOF。
  这些不证明 Linux 来宾 user-copy/授权、真实网卡、ping6/OSPFv3 或完整系统性能；仍待统一验收。
