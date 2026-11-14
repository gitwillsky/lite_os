# 双栈本机分片重组

## Owner 与接收边界

`netstack::inet::NetworkStack.reassembly` 持有每 NET 的本机重组状态。`inet::reassembly` 维护包键、区间、载荷、计费及固定期限；
`reassembly::packet` 解释分片和归一化报头，`reassembly::reply` 把超时/格式错误交给既有 IP 回复路径。
有状态转发复用区间和格式检查，持有独立预算和原始分片，见[分片流量过滤](ip-forwarding.md#分片流量过滤)。
不启用旧链路库的分片 feature，不创建第二套 TCP/UDP/RAW endpoint、设备队列或 Linux 选项副本。

1. 原 RX token 只借当前实际帧作识别；本机目的须通过当前地址/成员资格及链路准入，IPv4 checksum 和 IPv6 每片扩展头先验证。
   IPv4 转发片仍由原转发入口校验，IPv6 非本机流量不执行终点扩展链，不进入本机重组。
   已确认结构的 IPv4 无片报文、IPv6 无可见扩展链报文复用同一 token 的事实，域线程直接进入正常接收。
   无结构事实、IPv4 分片及以可解析扩展头开始的 IPv6 仍由重组入口判断，包括可能需要报告的扩展头格式错误。[RFC 791](https://www.rfc-editor.org/rfc/rfc791.html#section-3.1)、[RFC 8200](https://www.rfc-editor.org/rfc/rfc8200.html#section-4)
2. 各片载荷复制到唯一重组缓冲；NIC loan 随原 token 消费归还。原始物理抓包只有一次，不把重组结果伪造成第二帧。
3. 完整结果按值移交 `ProtocolRxToken::Reassembled`，继续使用同一 IP、TCP、UDP、RAW 接收入口。
   保留实际完成接收的逻辑接口；重组后 transport 必须执行完整软件 checksum，分片级硬件验证不能替代它。
   本机 IPv6 完整结果可大于接收 MTU；该例外仅属于重组结果，不放宽物理帧或转发的 MTU 检查。
   本机目的可能参与 NAT 时，按原有预算保留合格物理单播的原片。选中转换时仅同一物理入口的完整原片集可转发，逐片沿原出口 MTU 规则发送。
   未选中转换的普通本机重组继续交付完整报文，包括原有的跨接口全局单播语义。

`ipv6::fragment_extensions` 只给重组 owner 复用不可分片扩展头准入，不开放底层 packet module。
完整包仍检查后续扩展头和当前接收资格；Routing/AH 等尚未提供的终点语义不会因重组而被接受。
IPv6 ND 保留原有禁分片语义：即便 Fragment header 已移除，真实重组结果仍不能触发邻居学习或地址状态变化。

## 包身份、区间与首片

- IPv4 键为源、目的、协议、Identification；IPv6 为源、目的、32-bit Identification。
  组播与链路作用域加实际接口 zone，普通全局单播可跨接口到达；NET 之间完全隔离。
- 外层包表与每包区间均复用 fallible AVL。插入只检查相邻区间，乱序不会退化为线性移动或重复写载荷。
  区间互不重叠，已收字节数与最终长度相等且收到首片才完成；缺洞的零初始化字节绝不作为数据交付。
- 精确范围、M 标志和载荷均相同的重复片被忽略，不刷新期限、不再写缓冲；其他重叠、冲突末尾、非法 ECN 混合整体退休当前包。
  不能用后一片覆盖前片数据来拼出协议检查与应用看见不同内容的报文。
- 以偏移零的首片决定最终报头，其他片的不可分片头可以不同。IPv6 去掉 Fragment header 并修复前驱 Next Header、Payload Length；
  IPv4 清除 MF/offset、更新 Total Length 和 header checksum；保留首片选项/DSCP，并按 RFC 3168 保留 CE。
  Not-ECT 与 ECT/CE 混合拒绝；ECT(0)/ECT(1) 混合沿首片，出现 CE 时向完整报文传播 CE。
- IPv6 atomic fragment 独立于重组表，同标识的在途重组不吞掉原子包，也不会被原子包清除。
  首片必须包含完整扩展链和可辨认的上层头；TCP 包括其 Data Offset 声明的全部头、UDP/ICMPv6 至少共同头。

## 生命周期、容量与错误

- 首个到达片启动固定 60 秒期限，后续片或重复片不延长。期限 AVL 与包表同时增删，网络计时器只查看队首。[RFC 8200 §4.5](https://www.rfc-editor.org/rfc/rfc8200.html#section-4.5)
  每轮至少处理一个到期包，最多 16 个，到软截止后余项续调度。接口停用、移除或载波代际变化时退休本机和转发分片。
- 每 NET 最多 128 个在途包、4 MiB 重组存储，按实际 Vec capacity 与 AVL 节点布局计费；空表不预留载荷池。
  载荷按需倍增扩容并限制 IP 最大长度，避免连续最小分片导致二次方搬移；最终完整包只准备一个有界连续输出。
  输出移交前旧重组状态立即退休，输出分配的短暂峰值受单个最大 IP 包约束，不保留缓存副本。
- 所有分配可失败。容量、节点、载荷或首片引用准备失败时退休该包，释放实际存储及计费；不保留半个区间，也不把接收分配失败伪装成任意 socket 的错误。
  物理接收的重组拒绝进入原接口 RX drop；尚未收齐的有效片不计作丢包。
- 有首片的超时可产生 IPv4 Time Exceeded(11/1) 或 IPv6 Time Exceeded(3/1)；引用保留原始首片，未取得首片则静默释放。
  IPv6 不完整首片产生 Parameter Problem(4/3)，对可报告的非法长度/偏移产生 4/0 并保留字段位置；重叠不回复错误。
  错误只对仍为本机的单播目的、合法单播来源且能安全辨认的引用产生，排除 ICMP 错误、组播/广播及非首片等反射风险。
- `route_reply::ErrorBudget` 是从原 IPv6 接口错误桶移来的同一 owner，主机重组错误与 IPv6 端口不可达共享限速；不另增限速状态。
  `RouteReplies::enqueue_packet` 消费独占完整包，与原 codec/TCP 回复共用最终容量准入及 AVL 提交；实际路由、邻居和背压沿原发送路径处理，转发开关不抑制本机错误。

## 依据与验证边界

依据 RFC 791、RFC 1122 §3.3.2、RFC 8200 §4.5、RFC 5722、RFC 6946、RFC 7112、RFC 6980、RFC 3168 及 RFC 4443。
成熟区间、重复片与 ECN 语义对照固定 Linux v6.12 的 inet_fragment.c、ip_fragment.c、reassembly.c；来源链接见[网络基线](../standards-baseline/network.md)。
不采用 RFC 815 的重叠覆盖语义：保留其单一数据缓冲原则，以现代重叠拒绝及 AVL 区间保护替代旧覆盖规则。

现有 NET/NIC 夹具覆盖大于 MTU 的双栈 UDP/RAW 接收、乱序/精确重复、重叠/冲突末尾、ECN、真实分配失败、
完整 checksum、原子片隔离、不完整首片错误、固定超时引用与 ND 禁分片。未据此宣称真实网卡、来宾 ABI 或成熟系统互通已验收。

benchmark 决策：分别对比网卡队列记录结构与域内判断的 CPU 成本。普通双栈 TCP/UDP 只读取同帧结构事实，无额外锁、分配或跨线程通信。
每个在途包新增一个计入容量的期限节点。正常轮次查看队首，超时轮次按实际到期项消费。重组载荷倍增增长摊销为线性总搬移。
整机比较无到期包和集中超时时的域锁时长与输入延迟，覆盖正常/逆序小片、容量压力、目的转换并发及内存峰值。

本契约只覆盖主机重组接收。IPv4 主机分片、RAW 自带片和小 MTU 本机回复已接通[统一发送](ipv4-transports.md#ipv4-主机分片发送)，本机自动编号的复用保护见[非原子报文标识](ipv4-transports.md#非原子报文标识)。
IPv6 主机发送见[分片发送契约](ipv6-link.md#主机分片发送)。数据报错误交付见[错误队列](socket-transport.md#数据报错误队列)，转发范围见[转发契约](ip-forwarding.md)。
