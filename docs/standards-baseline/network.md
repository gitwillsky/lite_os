# 网络与报文固定基线

本文件从[固定规范总表](../standards-baseline.md)进入；只维护网络规范与对照源码，不声明路由计划已经完成。

## 配置格式与字段元数据

- 配置类型复用 [Serde 1.0.229](https://docs.rs/serde/1.0.229/serde/)、[serde_json 1.0.154](https://docs.rs/serde_json/1.0.154/serde_json/)
  与 [Schemars 1.2.3](https://docs.rs/schemars/1.2.3/schemars/)，Cargo 精确固定版本并禁用默认 std；Serde 开启 alloc/derive，serde_json 开启 alloc，Schemars 开启 derive。
- 持久格式是 JSON；元数据显式选择 [JSON Schema Draft 2020-12](https://json-schema.org/draft/2020-12)，不依赖库的默认 draft。
  字段类型、枚举、范围和描述由相同 Rust 声明生成。设备匹配重叠、跨对象唯一性等语义仍由配置类型验证，不能把生成元数据当成完整策略执行器。
- no_std 的 Schemars 不自带 core::net IP 格式：仅在 Schema 中声明同一标准 IPv4 字符串格式，实际编码/解析继续使用 Serde 的 core::net 实现，不引入另一套地址类型或解析器。
- 原生 DNS 容量不取自 Linux libc；只有 resolvd 消费者按固定 musl 1.2.6 的 MAXNS/行缓冲容量投影。
- 配置文档的 Linux fd 传递沿同一固定 Linux revision 的 `include/linux/socket.h` 与 `net/unix`，
  使用标准 SCM_RIGHTS、memfd/seals；接收同时拒绝 MSG_TRUNC/MSG_CTRUNC，原生传输只使用既有 VMO 能力。
- 审阅变更的字段路径采用 [RFC 6901](https://www.rfc-editor.org/rfc/rfc6901) §3/4 的 JSON Pointer，先转义 `~` 为 `~0`，再转义 `/` 为 `~1`。
  比较结果不是 JSON Patch，不增加第二条修改/校验路径；数组保留完整顺序含义。

- 备份身份使用 [ulid 3.0.0](https://docs.rs/ulid/3.0.0/ulid/)，精确固定版本并禁用默认 std/rand；原生服务提供 CPRNG 随机位，复用上游编解码/排序。
  [ULID 规范](https://github.com/ulid/spec)定义 48-bit 时间与 80-bit 随机位；备份使用现有存量的逻辑高水位避免墙钟回退，实际时刻仍归文件系统。
  from_parts 会截断溢出位，调用方先拒绝超出 48-bit 的时间；不依赖默默截断。创建使用同一固定 Linux/FUSE 的 RENAME2_NOREPLACE 语义，不覆盖已有目标。

## smoltcp

- crate `0.13.1`；source commit `e347a1e2d3ac33c5ce2c0c114e24b85ae23c4897`。
- 来源：[release 文档](https://docs.rs/crate/smoltcp/0.13.1)、[固定源码](https://github.com/smoltcp-rs/smoltcp/tree/e347a1e2d3ac33c5ce2c0c114e24b85ae23c4897)。
- netstack、netstack-unit 与 architecture-bench 对 smoltcp 的引用固定为 `vendor/smoltcp`；原 release 每个文件的 SHA-256
  记录于 `UPSTREAM.sha256.json`，保留原许可证与 upstream commit。Cargo 不保留 registry 备选路径。
- 本地变化限定UDP/TCP/raw transport、UDP ingress及interface dispatch与IPv4地址mechanism十个文件（第十个只是 `wire/udp.rs` 里 6LoWPAN 专用函数的 feature 门控）：connected peer 在 ingress admission 前匹配，canonical local
  endpoint 修改保留已有 queue，bind/rehash order 选择单播目标，广播/组播交付实际匹配集合，
  TX packet 在 enqueue 时冻结source port/address/TTL/oif；TCP实际MSS cache约束广告及分段。
  bound device在ingress准入前筛选，kernel唯一route selector在dispatch前选设备；IPv4
  weak-host tuple不因当前iface source列表而reset。Linux bind/shutdown/rlimit/netns UAPI 归 linuxd；netstack 只拥有协议机制与 endpoint 状态。
- 只启用 alloc、Ethernet、IP-medium 实际 loopback、IPv4 与保留空 SocketSet 所需的 raw 类型；实际 TCP/UDP/RAW 收发均归原生 owner；Linux socket UAPI 不由 smoltcp 定义；原生 IPv6 链路地址独立实现，完整 IPv6 socket 仍待补齐。

## 双栈本机重组

- 分片/超时依 [RFC 791](https://www.rfc-editor.org/rfc/rfc791)、[RFC 1122 §3.3.2](https://www.rfc-editor.org/rfc/rfc1122#section-3.3.2)、[RFC 8200 §4.5](https://www.rfc-editor.org/rfc/rfc8200#section-4.5)。
- 重叠拒绝、原子片隔离、完整首片和 ND 禁分片依 [RFC 5722](https://www.rfc-editor.org/rfc/rfc5722)、[RFC 6946](https://www.rfc-editor.org/rfc/rfc6946)、[RFC 7112](https://www.rfc-editor.org/rfc/rfc7112)、[RFC 6980](https://www.rfc-editor.org/rfc/rfc6980)。
- 区间及重复/ECN 语义对照 [Linux v6.12 inet_fragment.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/inet_fragment.c)、[ip_fragment.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/ip_fragment.c)、[reassembly.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/reassembly.c)。
  单一缓冲思想参考 [RFC 815](https://www.rfc-editor.org/rfc/rfc815)，不沿用其覆盖重叠数据的旧语义；实际 owner、容量、期限、错误范围与成本由[重组契约](../architecture-contract/ip-reassembly.md)维护。

- IPv4 主机分片与再分片依 RFC 791；模式值与 DF/本机分片区分对照固定 Linux v6.12
  [ip_sockglue.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/ip_sockglue.c)、[ip_output.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/ip_output.c) 和 include/net/ip.h 的 ip_sk_ignore_df/ip_sk_use_pmtu。
  RAW 自定义头依同版 [raw.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/raw.c) 的 raw_send_hdrinc；当前限制与标识分配缺口见[主机分片契约](../architecture-contract/ipv4-transports.md#ipv4-主机分片发送)。

## 原生 TCP 机制

- `libraries/tcp` 的连接、重组与流缓冲算法源自上述固定 smoltcp revision，保留 0BSD 许可证；机制不依赖旧 Interface 或 smoltcp crate。产品 IPv4/IPv6 TCP 均使用此唯一机制。
- TCP 以 [RFC 9293](https://www.rfc-editor.org/rfc/rfc9293.html) 为准：§3.4.1 初始序号由调用者提供时钟加秘密 PRF，§3.7.1 缺省发送 MSS 区分 IPv4 536 与 IPv6 1220。
- NET 的初始序号按 [RFC 6528](https://www.rfc-editor.org/rfc/rfc6528.html) 组合秘密 PRF 与时钟；使用固定 [hmac 0.12.1](https://docs.rs/hmac/0.12.1/hmac/)
  与 [sha2 0.10.9](https://docs.rs/sha2/0.10.9/sha2/)，禁用默认特性并固定软件 SHA-256；秘密及路径所有权见 TCP 契约。
- RTO 对照 [RFC 6298](https://www.rfc-editor.org/rfc/rfc6298.html)；Reno 对照 [RFC 5681](https://www.rfc-editor.org/rfc/rfc5681.html) 的字节计数、实际 FlightSize、快速恢复和超时丢失窗口。
- 选择确认及发送恢复对照 [RFC 2018](https://www.rfc-editor.org/rfc/rfc2018.html) 和 [RFC 6675](https://www.rfc-editor.org/rfc/rfc6675.html) §4–5.1；实际发送 MSS 按 [RFC 6691](https://www.rfc-editor.org/rfc/rfc6691.html) 扣除选项占用。
- MD5 报文依据 [RFC 2385](https://www.rfc-editor.org/rfc/rfc2385.html)，双栈伪头与密钥选择/UAPI 对照固定 Linux v6.12 的 tcp_ipv4.c/tcp_ipv6.c；AO 仍未实现。
- MAC 使用固定 [md-5 0.10.6](https://docs.rs/md-5/0.10.6/md5/)，禁用默认特性并启用 force-soft；常数时间比较用 subtle 2.6.1，秘密退休用 zeroize 1.9.1，均禁用默认特性。所有权与验证边界见 [认证契约](../architecture-contract/tcp-authentication.md)。
- 初始窗口采用 [RFC 6928](https://www.rfc-editor.org/rfc/rfc6928.html) 的实验性 IW10；超时回退至一个 SMSS。机制/调用者及当前验证范围见 [TCP 契约](../architecture-contract/ipv4-transports.md#tcp-连接机制)。

## 报文字节格式

- 批内阶段组织参考固定 VPP 25.02 的 [Scalar vs Vector packet processing](https://docs.fd.io/vpp/25.02/aboutvpp/scalar-vs-vector-packet-processing.html)
  与[处理图](https://docs.fd.io/vpp/25.02/aboutvpp/extensible.html)：同一阶段处理一组帧，格式借用不复制 payload；不据此声称已实现完整 VPP 图、SIMD 或无锁多核引擎。
- Ethernet Length/Type 按 IEEE 802.3 §3.2.6；固定一手说明见 IEEE 802.1 WG
  [2020-05 protocol identification](https://www.ieee802.org/1/files/public/docs2020/maint-seaman-protocol-ids-in-std-802-0520-v01.pdf)。
  C/S VLAN tag 使用 [IEEE 802.1Q-2022](https://1.ieee802.org/maintenance/p802-1q-rev/)；
  固定 Linux revision 的 `include/uapi/linux/if_ether.h`、`include/linux/if_vlan.h` 作为成熟格式对照，不把 Linux VLAN 配置 UAPI 带进 codec。
- ARP 可变宽地址与网络字节序取自 [RFC 826](https://www.rfc-editor.org/rfc/rfc826)；
  独立 host 对照使用固定 smoltcp 的 `wire/arp.rs`，不复制其邻居状态机。
- IPv4 缓存更新与探测节奏对照 [RFC 1122 §2.3.2](https://www.rfc-editor.org/rfc/rfc1122.html#section-2.3.2)。
  ARP 与 ND 共享邻接状态机的分层参考 [Fuchsia netstack3 NUD](https://fuchsia.googlesource.com/fuchsia/+/9ff064b30848159a70fce9f5026d148338657afe/src/connectivity/network/netstack3/core/src/ip/device/nud.rs)。
- IPv4 头与选项布局取自 [RFC 791](https://www.rfc-editor.org/rfc/rfc791)，UDP 长度与零 checksum 语义取自
  [RFC 768](https://www.rfc-editor.org/rfc/rfc768)；IPv6 传输伪头及 checksum 取自 [RFC 8200 §8.1](https://www.rfc-editor.org/rfc/rfc8200#section-8.1)。
- IPv6 固定头、扩展头接收次序、选项动作及分片字段取自同一 RFC 8200；Jumbo Payload 与 UDP 长度规则取自
  [RFC 2675](https://www.rfc-editor.org/rfc/rfc2675)，AH 长度与对齐取自 [RFC 4302](https://www.rfc-editor.org/rfc/rfc4302)。
  固定 Linux revision 的 `net/ipv6/exthdrs_core.c`、`net/ipv6/exthdrs.c` 作成熟解析机制参考，不据此宣称原生引擎已经支持 IPv6。
- DHCPv4 消息布局与客户端状态取自 [RFC 2131](https://www.rfc-editor.org/rfc/rfc2131)，选项/overload 取自
  [RFC 2132](https://www.rfc-editor.org/rfc/rfc2132)，重复选项拼接与分段顺序取自 [RFC 3396](https://www.rfc-editor.org/rfc/rfc3396)。
- DHCPv6 格式固定采用 [RFC 9915](https://www.rfc-editor.org/rfc/rfc9915)（2026-01，STD 102，替代 RFC 8415）；
  客户端/服务端四字节信封、中继 34-byte 信封、IA_NA/IA_PD、地址/前缀与标准选项共享该基线。
  重复选项保留独立实例，DUID 仅按不透明 3..130-byte 值处理；IA_TA/Server Unicast 在接收时保留以供忽略，类型化输出拒绝产生它们。
  成熟实现的布局审查参考 ISC Kea `Kea-2.6.3`（commit `1e695954be4d7d60036a11e338fef07b29177c27`）的
  [`option6_ia.cc`](https://github.com/isc-projects/kea/blob/1e695954be4d7d60036a11e338fef07b29177c27/src/lib/dhcp/option6_ia.cc)、
  `option6_iaprefix.cc`、`pkt6.cc`；host 夹具使用同 revision 的 IA/Prefix 固定字段向量，不宣称实际运行 Kea 或已取得 DHCPv6-PD 租约。
- DHCPv6 DNS 地址/搜索域按 [RFC 3646](https://www.rfc-editor.org/rfc/rfc3646)，与 DNSSL 共用
  [RFC 1035 §3.1](https://www.rfc-editor.org/rfc/rfc1035#section-3.1) 的非压缩名称机制；label 最大 63、编码名称最大 255 字节。
  DHCPv6/DNSSL 不允许压缩指针，DNS 消息里的压缩机制、resolver、IDNA 与名称配置策略不属于此处的格式实现。
- host 格式对照使用上述固定 smoltcp 源的 `wire/{ipv4,udp,dhcpv4}.rs`；DHCP 编解码特性仅由 host 夹具的
  dev-dependency 启用，不把该对照实现重新带入 DHCP 客户端产品依赖。
- TCP 固定字段及必需选项取自 [RFC 9293](https://www.rfc-editor.org/rfc/rfc9293)，WS/TS 取自
  [RFC 7323](https://www.rfc-editor.org/rfc/rfc7323)，SACK 布局取自 [RFC 2018](https://www.rfc-editor.org/rfc/rfc2018)。
  MD5 与 AO 的格式/互斥取自 [RFC 2385](https://www.rfc-editor.org/rfc/rfc2385)、[RFC 5925](https://www.rfc-editor.org/rfc/rfc5925)；
  固定 smoltcp `wire/tcp.rs` 作常规 TCP 格式/校验和 oracle，不能用其三块 SACK 截断代替标准四块的格式范围。
- ICMPv4 的基本布局取自 [RFC 792](https://www.rfc-editor.org/rfc/rfc792)，错误类型集合按
  [RFC 1122 §3.2.2](https://www.rfc-editor.org/rfc/rfc1122#section-3.2.2)，Address Mask 固定字段按
  [RFC 950](https://www.rfc-editor.org/rfc/rfc950)；ICMPv6 按 [RFC 4443](https://www.rfc-editor.org/rfc/rfc4443)。
  错误引用/扩展分界及版本 2 对象链按 [RFC 4884 §4/§7/§8](https://www.rfc-editor.org/rfc/rfc4884#section-4)：两族单位不同，零长度不猜测扩展，未知对象保留；不把引用交给完整 IP parser。
  独立扩展 checksum 允许省略，输出负零区分实际提供的校验；不支持扩展的 v6 MTU/Pointer 不被当作长度。固定 smoltcp `wire/icmpv4.rs` 作 host 基础格式对照，扩展使用标准布局/独立校验向量。
- ND 五类固定消息与基础选项采用 [RFC 4861](https://www.rfc-editor.org/rfc/rfc4861)；framing 与已知选项内容分别验证。
  RA/Prefix 的 flags 原值留给扩展 owner；RDNSS/DNSSL 按 [RFC 8106](https://www.rfc-editor.org/rfc/rfc8106)，
  Route Information 按 [RFC 4191](https://www.rfc-editor.org/rfc/rfc4191)。接收允许标准定义的较宽 RIO 前缀字段，
  输出选最短宽度并清零非前缀位；保留 preference 10 仅使该选项 decode 失败。配置/路由/lifetime 行为仍待协议引擎。
- 组播格式固定采用 [RFC 9776](https://www.rfc-editor.org/rfc/rfc9776)（2025-03，替代 RFC 3376）与
  [RFC 9777](https://www.rfc-editor.org/rfc/rfc9777)（2025-03，替代 RFC 3810）；旧代布局按
  [RFC 2236](https://www.rfc-editor.org/rfc/rfc2236)、[RFC 2710](https://www.rfc-editor.org/rfc/rfc2710)。
  Query/Report 的扩展 flags 保留，来源数和辅助区边界按完整接收长度验证；额外尾部参与 checksum 后忽略。
  基础输出禁止辅助数据和额外尾部。固定 smoltcp `wire/igmp.rs` 仅作 v1/v2 host 格式对照，不能作为 v3 或 MLD oracle。

## 前缀索引

- 六位 stride、vector/leafvec、连续子节点与叶 run 的 rank 查找采用 Asai 与 Ohara 的
  [Poptrie，SIGCOMM 2015](https://conferences.sigcomm.org/sigcomm/2015/pdf/papers/p57.pdf) §3.2–3.3。
  当前叶值是前缀身份，保留父前缀链以裁决绑定设备、scope 和 metric alias；不是复制一份路由结果。
  前缀对应多个 table/metric alias 的裁决语义对照固定 Linux 7.1
  [`fib_trie.c` 的 leaf_new/fib_find_alias](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/fib_trie.c)：
  LiteOS 用前缀区间直接引用自己的连续记录，不照搬 LC-trie、链表或 Linux RCU。
  本阶段不采用论文的 direct-pointing 表或宣称完成增量 COW/RCU、IPv6 协议栈和整机吞吐。

- 下一跳与前缀分离、按实际解析出口/网关比较的成熟语义，对照同一固定 Linux revision 的
  [`fib_semantics.c` 的 fib_nh_match/fib_find_info](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/fib_semantics.c)
  与 [`nexthop.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/nexthop.c)。
  同一 `nexthop.c` 的 fib_check_nexthop/fib_check_nh_list 对照具名对象更新时的引用范围检查；LiteOS 用所属根内的范围计数判断，不逐前缀扫描。
  LiteOS 的匿名路径去重、具名身份、引用/范围计数及单槽交换同属 FIB，沿既有配置能力确认/撤销；
  组选择采用 [RFC 2992 §1](https://www.rfc-editor.org/rfc/rfc2992#section-1) 的 hash-threshold；权重区间对照同一固定 fib_semantics.c 的 fib_rebalance。
  源/目的地址键对照固定 [route.c 的 fib_multipath_hash policy 0](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/route.c)。
  IPv4 Jenkins lookup3 two-word final 与 IPv6 八字 jhash2 对照 [jhash.h](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/linux/jhash.h) 中注明公有领域的原算法；不照搬 Linux secret seed、路径编号或 RCU。
  当前组选择已接通真实发送及完整组快照，不宣称 Linux next-hop UAPI、完整 ECMP 产品能力或 RCU 已完成。
- 多路径只读投影依据同一 revision 的 [rtnetlink.h](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/linux/rtnetlink.h)
  与 fib_semantics.c：RTA_MULTIPATH 包含对齐的 rtnexthop，rtnh_hops 为 weight-1，网关属于成员嵌套属性。
  原生大权重先精确约分；无法表示的权重/属性长度明确返回 EOVERFLOW/EMSGSIZE，不以截断掩盖边界。
  [/proc 的 fib_route_seq_show](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/fib_trie.c)
  只取 fib_info_nhc(fi, 0)，因此该文本投影仅列首路径，完整观测使用 rtnetlink。

- 终止动作依据同一固定 Linux revision 的 [fib_props](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/fib_semantics.c)：
  本机 blackhole/prohibit/unreachable 分别产生 EINVAL/EACCES/EHOSTUNREACH；fib_trie 先处理终止动作，明确 oif 再按同树 route.c 的 route_output 回退到目的地址，不能先跳过动作选默认网关。
  rtnetlink 采用固定 UAPI 的 RTN_BLACKHOLE=6、RTN_PROHIBIT=8；/proc 的 fib_flag_trans 只为 unreachable/prohibit 置 RTF_REJECT，不为 blackhole 伪造该标志。
  当前新增的是本机选路及观测语义，不能据此宣称完整转发 ICMP、策略路由或防火墙已经完成。

## RIB 来源与候选

- 来源/RIB/FIB 分工、路由 preference 与下一跳间接引用参考固定 [RFC 8430](https://www.rfc-editor.org/rfc/rfc8430.html)
  （2018-09，Informational）的信息模型；不宣称实现其完整读写/通知协议。
- 候选保留与来源撤销对照固定 Holo revision `a5573626d59d4b3b1a4245b4e9fc3e601c0bbed4` 的
  [holo-routing/src/rib.rs](https://github.com/holo-routing/holo/blob/a5573626d59d4b3b1a4245b4e9fc3e601c0bbed4/holo-routing/src/rib.rs)。
  LiteOS 采用不可变来源、去重字典和紧凑引用，等 preference 跨来源用稳定身份裁决，metric 只在同源比较；不复制 Holo 的容器或相同距离的插入顺序。
  此 revision 的递归只处理一层，不作为完整递归解析依据；LiteOS 已实现冻结选择集多层展开及授权来源会话，协议内状态机仍未接入。

- 递归目标与立即出口的区别，以及安装后仍须成立的可达性条件，以固定 [RFC 4271 §5.1.3/§9.1.2.1](https://www.rfc-editor.org/rfc/rfc4271#section-9.1.2.1)
  （2006-01）为约束：自身/相互依赖不能作为有效证明，不能借将被替换的旧条目证明新路由。
  冻结求解剔除祖先环路分支；直接相连叶采用本层 NEXT_HOP、已有立即网关保持。失效候选重选后逐次复核安装集合仍有效；IPv4 跟踪事件只在执行确认后发表。
  组内相对权重的递归组合参考同一固定 [RFC 8430 §2.4.2/§7.2.3](https://www.rfc-editor.org/rfc/rfc8430#section-2.4.2) 的 weighted-list 模型；
  LiteOS 用精确有理数规范为 u32 权重，无法表示即拒绝，不声称实现该 RFC 的完整下一跳语法或传输协议。
- IPv6 link-local 的 zone 语义依据固定 [RFC 4007 §6/§11.2](https://www.rfc-editor.org/rfc/rfc4007#section-6)（2005-03）；
  当前 Direct 用显式接口确定链路，Recursive 未携带 zone，故拒绝未限定链路的 link-local 递归声明，不选择默认链路。
  来源 framing 为 LiteOS 原生系统协议，不声明是 BGP、I2RS 或 Linux rtnetlink 的线格式。

## IPv4 转发

- 转发、TTL、分片与错误抑制固定采用 [RFC 1812](https://www.rfc-editor.org/rfc/rfc1812.html)、RFC 791 与 RFC 792；协议格式仍由 packet-wire 拥有。
  分片中非 copied 选项的 NOP 替换对照 [Linux v6.12 ip_options.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/ip_options.c)；实现范围、所有权与验证由[转发契约](../architecture-contract/ip-forwarding.md)维护。
  IPv4 经 IPv6 网关的 ND、原 IPv4 帧与 RTA_VIA 对照 Linux v6.12 的 [ip_output.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/ip_output.c) 和 [fib_semantics.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/fib_semantics.c)。
  IPv6 单播转发按 RFC 8200 §4/§5、RFC 4443 §2.4/§3 与 RFC 4007，成熟对照 [Linux v6.12 ip6_output.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/ip6_output.c)；路由器不处理终点扩展头、不分片，按实际 MTU 返回 PTB。

## BGP

- eBGP 没有显式入口/出口政策时不采用或发布路由，依据 [RFC 8212](https://www.rfc-editor.org/rfc/rfc8212)；该要求不扩大到 iBGP。
- 消息、UPDATE 与基本属性固定采用 [RFC 4271](https://www.rfc-editor.org/rfc/rfc4271)，独立错误处理以
  [RFC 7606](https://www.rfc-editor.org/rfc/rfc7606) 为约束；格式库不把单个坏属性自动升级为 session reset，整批裁决属于 BGP 协议 owner。
  AS 0 的各属性处置按 [RFC 7607](https://www.rfc-editor.org/rfc/rfc7607)：AS_PATH/AGGREGATOR 交 RFC 7606，AS4 辅助属性交 RFC 6793，不能把它们都当成同一种严重性。
- 能力按 [RFC 5492](https://www.rfc-editor.org/rfc/rfc5492)，普通/扩展 OPEN 参数按
  [RFC 9072](https://www.rfc-editor.org/rfc/rfc9072)，扩展消息按 [RFC 8654](https://www.rfc-editor.org/rfc/rfc8654) 的方向授权与类型限制。
  RFC 5492 §3/§5 的参数拒绝重试与必需能力不相容暂停以实际本地 OPEN 为准；AS4 双方声明才用四字节路径（RFC 6793 §4.1），不以持久配置冒充本次授权。
- 基础路由刷新按 [RFC 2918 §3–4](https://www.rfc-editor.org/rfc/rfc2918.html)；增强协商、刷新边界、陈旧路由清理和长度错误按 [RFC 7313 §3–5](https://www.rfc-editor.org/rfc/rfc7313.html)。
  按邻居和地址族发起请求对照 [FRR 10.0 BGP 命令](https://docs.frrouting.org/en/stable-10.0/bgp.html#clicmd-clear-bgp-ipv4-ipv6-unicast-PEER-soft-in-out)，能力检查和调度对照同版 [bgp_packet.c](https://github.com/FRRouting/frr/blob/frr-10.0/bgpd/bgp_packet.c)。
- MP_REACH/MP_UNREACH 按 [RFC 4760](https://www.rfc-editor.org/rfc/rfc4760)，四字节 AS/AS4 属性按
  [RFC 6793](https://www.rfc-editor.org/rfc/rfc6793)，联盟段类型按 [RFC 5065](https://www.rfc-editor.org/rfc/rfc5065)。
  IPv6 的非 link-local/辅助 link-local 下一跳及共链路条件按 [RFC 2545 §3](https://www.rfc-editor.org/rfc/rfc2545#section-3)，没有接口范围不能生成可安装的 link-local 目标；出口共链路时携带实际 32-byte 下一跳，否则剥离辅助地址，缺失必要证明时拒绝发布。
- IPv4/IPv6 TCP 与 MP 地址族独立，接口范围按 RFC 4007。IPv4 单播的跨族下一跳按 [RFC 8950 §3–5](https://www.rfc-editor.org/rfc/rfc8950.html)，
  能力与 MP 宣告/撤回对照 FRR 10.0 [`bgp_open.c`](https://github.com/FRRouting/frr/blob/frr-10.0/bgpd/bgp_open.c)、
  [`bgp_updgrp_packet.c`](https://github.com/FRRouting/frr/blob/frr-10.0/bgpd/bgp_updgrp_packet.c)。MP 属性首位按 RFC 7606 §5.1，范围限于 IPv4 单播。
- Communities/Extended/Large 的布局分别按 [RFC 1997](https://www.rfc-editor.org/rfc/rfc1997)、
  [RFC 4360](https://www.rfc-editor.org/rfc/rfc4360)、[RFC 8092](https://www.rfc-editor.org/rfc/rfc8092)；反射字段按
  [RFC 4456](https://www.rfc-editor.org/rfc/rfc4456)。列表非空/畸形属性的检查与该属性的传播/策略处置独立。
- ROUTE-REFRESH 保留完整帧以报告标记长度错误；按协商解释 subtype。ORF 范围见 [BGP 契约](../architecture-contract/bgp.md#邻居请求路由重发)。
- Identifier 与同 ID 碰撞采用 [RFC 6286](https://www.rfc-editor.org/rfc/rfc6286)，FSM 错误数据按 [RFC 6608](https://www.rfc-editor.org/rfc/rfc6608)，Cease 子码按 [RFC 4486](https://www.rfc-editor.org/rfc/rfc4486)。
  MP 交集/无 MP 时的基础 IPv4 接纳同时对照固定 FRR 10.0 [`bgpd/bgp_open.c`](https://github.com/FRRouting/frr/blob/frr-10.0/bgpd/bgp_open.c)；不复制其状态容器或并发设计。
  路径规范化及分组件驻留对照同版 [`bgp_aspath.c`](https://github.com/FRRouting/frr/blob/frr-10.0/bgpd/bgp_aspath.c) 和 [`bgp_attr.c`](https://github.com/FRRouting/frr/blob/frr-10.0/bgpd/bgp_attr.c)；本项目索引复用可失败 AVL，引用计数不与领域引用人工同步。
  选择顺序按 [RFC 4271 §9.1.2.2](https://www.rfc-editor.org/rfc/rfc4271#section-9.1.2.2) 与 RFC 4456 §9；MED 分组/非传递风险同时对照
  FRR 10.0 [`doc/user/bgp.rst`](https://github.com/FRRouting/frr/blob/frr-10.0/doc/user/bgp.rst)。本地确定性不等于网络级无振荡，不声明 FRR 所有厂商策略兼容。
- AS 路径表达式的 `_` 边界与 AS-plain 段格式分别对照同版 [`bgp_regex.c`](https://github.com/FRRouting/frr/blob/frr-10.0/bgpd/bgp_regex.c) 和 [`bgp_aspath.c`](https://github.com/FRRouting/frr/blob/frr-10.0/bgpd/bgp_aspath.c)。
  解析与单方向 DFA 固定采用 [regex-automata 0.4.18](https://docs.rs/regex-automata/0.4.18/regex_automata/) 的无 std 实现；显式资源预算、流式 EOI 及不支持语法见[筛选契约](../architecture-contract/router-configuration.md#bgp-as-路径筛选)。
- 本机网段宣告参考 [FRR 10.0 Networks/import-check](https://docs.frrouting.org/en/stable-10.0/bgp.html#networks) 与 RFC 4271 §9.4；显式网络须有精确 RIB 依据，不能用父/子前缀替代，不把自身宣告回装为存在性依据。
- 其他来源重分发对照 [FRR 10.0 Redistribution](https://docs.frrouting.org/en/stable-10.0/bgp.html#redistribution) 与同版
  [`bgp_route.c`](https://github.com/FRRouting/frr/blob/frr-10.0/bgpd/bgp_route.c)：由 RIB 的实际来源/度量生成 INCOMPLETE/本地 MED，撤回随来源失效；不剥去 BGP 路径再重导入。
- 出口多目标合批采用 [RFC 4271 §4.3/Appendix F.1](https://www.rfc-editor.org/rfc/rfc4271#appendix-F.1)：共享实际出口属性，不合并不同政策结果；逐目标间隔与整帧完成记账保持。
- ORIGIN/MED 的值、缺失语义和选路顺序采用 RFC 4271 §5.1.1/§5.1.4/§9.1.2.2；入口/出口有序规则的动作参考同版 FRR Route Maps。
  LiteOS 的标量条件匹配改写前值；MED 条件要求属性实际存在，本地优先条件使用已计算 degree，跳数复用 RFC 4271/5065 选路定义；外部 MED Set 为本地声明。保留原始接收并共享后政策属性，不声明 FRR 完整 CLI/政策兼容。
- 安装后的自递归准入按 [RFC 4271 §9.1.2.1](https://www.rfc-editor.org/rfc/rfc4271#section-9.1.2.1)：当前能解析不等于候选安装后仍可解析；BGP 以冻结图反事实检查反馈重选，整批一致后才确认来源。
  无法解析的候选仍保留在接收表，下一跳变化后重选，依据 [RFC 4271 §9.1.2](https://www.rfc-editor.org/rfc/rfc4271#section-9.1.2)。
  失败依赖的存续按来源跟踪，ND 确认仍遵循 [RFC 4861 §7.3](https://www.rfc-editor.org/rfc/rfc4861#section-7.3)。
- 同一可见代际的提案环检测采用 [Brent, BIT 20 (1980), 176–184](https://maths-people.anu.edu.au/~brent/pub/pub051.html) 的倍增检查点方法。
  临时只保留一份来源检查点；拒绝重复提案不等于证明网络级 BGP 无振荡，也不宣称该控制面已达到规模性能目标。
- 出口 AS prepend、MED/LOCAL_PREF 边界、分割水平与宣告间隔按 RFC 4271 §5.1/§9.1.3/§9.2.1.1；§5.1.3 禁止下一跳为本机，Local 因而不提供递归转发证明。
  本机网段存在检查对照固定 [FRR 10.0 network import-check](https://docs.frrouting.org/en/stable-10.0/bgp.html#clicmd-bgp-network-import-check)；本机交付与禁止路由分开，未通过 DAD 不作为发布依据。
  本地路径前置保留序列顺序及重复，255 项线上分段与 AS4 迁移复用 RFC 4271/RFC 6793 的唯一 codec；成员身份与边界另按 RFC 5065。
  指定 AS 排除对照同版 bgp_aspath.c 的 aspath_filter_exclude：删除全部出现、移除空段并允许空路径；协议去环同时保留原始接收路径检查。
  指定/全部 AS 替换对照 [FRR 10.0 AS Path Route Map](https://docs.frrouting.org/en/stable-10.0/bgp.html#using-as-path-in-route-map)；本地显式配置目标 ASN，按排除、替换、前置投影，不声明正则政策完整支持。
  IPv4 路由反射按 [RFC 4456 §6–10](https://www.rfc-editor.org/rfc/rfc4456#section-6)：手工指定客户端、保留起源标识、前置簇列表及返回簇拒绝。
  反射域的 iBGP 政策可筛选和编辑团体，不覆盖关键路径属性；簇为不透明四字节身份，缺省采用本地 Identifier，不要求是接口地址。
  团体的本地编辑和集合语义按 RFC 1997/RFC 4360/RFC 8092；规范值直接参与出口匹配，空结果移除属性。
  先检查 Loc-RIB 保留团体、后执行出口编辑，对照固定 FRR 10.0 [`bgpd/bgp_route.c`](https://github.com/FRRouting/frr/blob/frr-10.0/bgpd/bgp_route.c) 的 bgp_announce_check/社区过滤调用。
  出口新加的标签约束接收者的后续传播；不把新标签误当成本次本地拒绝，也不让出口删除绕过已选路由上的限制。
  NO_ADVERTISE/NO_EXPORT/NO_EXPORT_SUBCONFED 按 RFC 1997，非跨 AS 扩展团体按 RFC 4360 §6 过滤；普通 permit 不能绕过协议限制。
  超过已授权消息上限时不发送并撤回旧宣告，按 RFC 8654 §4；逐目标拒绝不能扩大为整个路由域重启。
- 逐前缀展示接收候选与协议最佳选择，对照固定 [FRR 10.0 路由观察](https://docs.frrouting.org/en/stable-10.0/bgp.html#displaying-bgp-routes)。
  判定来自实际 RFC 4271 选路过程并随来源确认发布；协议最佳不等于统一 RIB/FIB 安装，不在查询时复制另一个选择算法。
- AS 联盟身份、路径修改与去环、LOCAL_PREF/MED 和选路按 RFC 5065 §4–5；RFC 7606 约束坏路径按撤回处理。
  团体传播边界按 RFC 1997/RFC 4360 §6；不可映射成员 AS 的过渡约束按 RFC 6793 §7，不用 AS4_PATH 搬运联盟段。
  成员/公开号码相同及角色判定对照固定 [FRR 10.2 bgpd.c](https://github.com/FRRouting/frr/blob/frr-10.2/bgpd/bgpd.c)，
  选路和成员 LOCAL_PREF 缺省对照同版 [bgp_route.c](https://github.com/FRRouting/frr/blob/frr-10.2/bgpd/bgp_route.c)。
- 当前格式范围、所有权与未接入的协议语义由 [BGP 契约](../architecture-contract/bgp.md) 唯一维护；没有 FRR/BIRD 实际互通结论。

## 配置事务参考

RIB 选路与执行 FIB 的分工参考固定 [FRR 10.0 Zebra](https://docs.frrouting.org/en/stable-10.0/zebra.html)；
有序首匹配和前缀长度条件参考同版 [Route Maps](https://docs.frrouting.org/en/stable-10.0/routemap.html)
及 [Filtering](https://docs.frrouting.org/en/stable-10.0/filter.html)；LiteOS 使用显式缺省动作，不声明 FRR CLI/route-map 完整能力兼容。
下一跳跟踪对照同版 [Nexthop Tracking](https://docs.frrouting.org/en/stable-10.0/zebra.html#nexthop-tracking)：默认路由不自动作为可达证明，LiteOS 由每查询显式允许；传输使用自己的授权能力，不声明 ZAPI 兼容。
LiteOS 用已有 Fuchsia 风格的服务能力路由授权接口来源与执行权，不声称实现 Zebra ZAPI 或 FIDL 线格式。确认与持久决定仍按下述事务约束实现。

候选配置、提交前验证和失败回滚的设计参考固定 [RFC 6241 §8.3/§8.5](https://www.rfc-editor.org/rfc/rfc6241#section-8.3)
（2011-06）：执行服务先预备完整变更，发布前排斥竞争的配置写入，未确认时保留可恢复的原资源。
LiteOS 使用授权的原生 channel 协议，不提供 NETCONF 端点；接口来源已接通跨服务提交，完整配置树仍由路由计划完成，
不声称实现 NETCONF 的全部 capability 或 §8.4 的超时 confirmed-commit。

## Linux 网络语义参考

原生IPv4地址/proc NET归属补充依据同一固定Linux commit的
[`devinet_ioctl/inet_abc_len`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4/devinet.c)、
[`get_proc_task_net/seq_open_net`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/proc/proc_net.c)、
[`dev_seq_show`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/core/net-procfs.c)
及kernel/nsproxy.c、kernel/kthread.c的真实引用创建/退出；不引入第二个规范版本。

NETLINK_KOBJECT_UEVENT的域、端口和过载以同一固定Linux commit的
[`netlink_bind/autobind/broadcast_deliver/overrun`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/netlink/af_netlink.c)
与[`uevent_net_init`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/lib/kobject_uevent.c)为依据。
端口按NET/协议独立，0首先使用调用者可见TGID，碰撞在[-2147483648,-4097]选择空位；
group0及同port重bind合法，protocol group数以外的mask被截断。
无tag内核事件发往所有owning initial UserNS的NET；非initial UserNS的NET不订阅该全局广播。
actual allocation收费覆盖每个receiver FIFO节点；overrun一次发表ENOBUFS，已有记录不替换，
SO_ERROR消费不解除拥塞，实际queue排空后恢复。

原生接口配置的缺省 MTU 1500 与通用以太网范围 68..65535 对照同一固定 Linux revision 的
[if_ether.h](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/linux/if_ether.h)
的 ETH_DATA_LEN/ETH_MIN_MTU/ETH_MAX_MTU，以及 [RFC 894](https://www.rfc-editor.org/rfc/rfc894) 的以太网 IP 封装。
这不是对实际设备可用范围的承诺；执行预备仍检查 adapter，且仅修改 MTU 不重新运行 DHCP。

接口MTU继续使用上述固定dev_ioctl.c的dev_set_mtu调用与devinet.c的
inetdev_event/NETDEV_CHANGEMTU语义：lo允许0..65536，实际Ethernet设备允许68..1500
（本驱动frame storage上限1514）。低于IPv4最低68时销毁地址成员；仅恢复MTU不还原旧
地址，lo再次DOWN→UP才发表127.0.0.1/8。smoltcp interface caps只是该实际设备配置的
分段mechanism投影，同一NET事务同步刷新，不作为第二份可配置状态。


## IPv6 自动链路地址

- 地址、最小 MTU、以太网组播映射和 modified EUI-64 取自 [RFC 4291](https://www.rfc-editor.org/rfc/rfc4291)、
  [RFC 2464](https://www.rfc-editor.org/rfc/rfc2464) 与 [RFC 8200 §5](https://www.rfc-editor.org/rfc/rfc8200#section-5)。
- DAD 的初始延迟、tentative/duplicate 与 NS/NA 接收采用 [RFC 4862 §5.3/5.4](https://www.rfc-editor.org/rfc/rfc4862#section-5.3)，
  ND 校验和回应采用 [RFC 4861 §7](https://www.rfc-editor.org/rfc/rfc4861#section-7)；拒绝含分片头的 ND 依 [RFC 6980](https://www.rfc-editor.org/rfc/rfc6980)。
- 主机 RS、RA 与前缀寿命采用 [RFC 4861 §6.3](https://www.rfc-editor.org/rfc/rfc4861#section-6.3)。自动地址、两小时保护及弃用采用 [RFC 4862 §5.5](https://www.rfc-editor.org/rfc/rfc4862#section-5.5)，
  默认及指定网段路由的寿命、覆盖和优先级采用 [RFC 4191 §3](https://www.rfc-editor.org/rfc/rfc4191#section-3)。当前覆盖与限制见[链路网络契约](../architecture-contract/ipv6-link.md#主机路由发现与自动地址)。
- RA 链路 MTU、可达时间及重传间隔按 [RFC 4861 §6.3.4](https://www.rfc-editor.org/rfc/rfc4861#section-6.3.4)，计时下限对照 [Linux v6.12 ndisc.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/ndisc.c)。
- 每接口接收与请求策略对照 [Linux IPv6 accept_ra](https://docs.kernel.org/networking/ip-sysctl.html)，包括仅主机、禁止和转发时仍允许三种行为。设置经 LiteOS 统一配置交付。
- 下游 RS 验证、组播 RA、定时、限速及网关撤回依据 [RFC 4861 §6.1.1、§6.2](https://www.rfc-editor.org/rfc/rfc4861#section-6.2)。
- 下游静态前缀运行期撤回及寿命对照 [RFC 9096 §3.5、§4](https://www.rfc-editor.org/rfc/rfc9096.html#section-3.5)，跨系统重启保留历史属后续故障恢复范围。
- 通告 DNS 的顺序、作用域及寿命按 [RFC 8106 §5–6](https://www.rfc-editor.org/rfc/rfc8106#section-5)，解析文件投影核对固定 musl 1.2.6 的 `resolvconf.c` 和 `lookup_ipliteral.c`。
- 手动地址同样执行 DAD，按 RFC 4862 §5.4；地址与 on-link 前缀分离遵循 [RFC 5942 §3–4](https://www.rfc-editor.org/rfc/rfc5942#section-3)，
  管理配置显式提供 on_link，不能以地址前缀或收到数据帧推断直连。
- 静态 IPv6 网关遵循 RFC 4861 的 next-hop/on-link 规则与 [RFC 4007 §6/10](https://www.rfc-editor.org/rfc/rfc4007#section-6) 的 zone；链路网关使用具名 direct 接口，不为递归地址猜默认链路。
- 地址源选择按 [RFC 6724 §2.1/2.2/5](https://www.rfc-editor.org/rfc/rfc6724#section-5)，包括适当 scope、避免弃用地址、默认 label 与最长共同前缀（不超过源前缀长度）。
  home/临时地址和可配置策略表尚未接入。网关路由的当前覆盖由[链路网络契约](../architecture-contract/ipv6-link.md)维护。
- 回送 nonce 与额外探测采用 [RFC 7527 §4](https://www.rfc-editor.org/rfc/rfc7527#section-4)，单次 nonce 六字节、使用原生 CPRNG；不据此宣称 SEND 认证。
- 监听侧采用 [RFC 9777](https://www.rfc-editor.org/rfc/rfc9777)（2025-03，替代 RFC 3810），含 §6.2 查询合并、§8 v1 互通与 §10.1 来源存储限额。
- 应用 ASM 成员依 [RFC 3493 §5.2](https://www.rfc-editor.org/rfc/rfc3493#section-5.2)；成员生命周期、重复/缺失错误与 MulticastAll 对照
  [Linux v6.12 mcast.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/mcast.c) 和 [ipv6_sockglue.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/ipv6_sockglue.c)。
  IF/HOPS/LOOP、独立默认跳数和本机回送依同一 RFC §5.2；回送先于链路发送、scope/跳数边界对照同版
  [ip6_output.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/ip6_output.c)，默认发送接口的 fallback 对照同版 [udp.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/udp.c)。
  接口统一持有查询器版本/参数，all-nodes、scope 0/1 和回环免报告；当前默认接口选择与未实现范围由[组播契约](../architecture-contract/ipv6-link.md#组播监听)维护。
  初始未指定源及正式地址报告遵循 [RFC 3590](https://www.rfc-editor.org/rfc/rfc3590)，v1 报告/抑制/Done 依 [RFC 2710](https://www.rfc-editor.org/rfc/rfc2710)。
- IPv4 监听采用 [RFC 9776](https://www.rfc-editor.org/rfc/rfc9776) 的 ASM、查询合并、§7 旧代兼容、§8.12 查询器寿命及 §9 查询防护；IGMPv1/v2 的抑制、Leave 与旧代重复间隔依
  [RFC 1112](https://www.rfc-editor.org/rfc/rfc1112) 和 [RFC 2236](https://www.rfc-editor.org/rfc/rfc2236)。两族共用成员机制，协议差异各归接口适配。
  ip_mreq/ip_mreqn 选择、退组和资源 errno 对照 [Linux v6.12 igmp.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/igmp.c) 与
  [ip_sockglue.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/ip_sockglue.c)；范围及验证边界由[IPv4 组播契约](../architecture-contract/ipv4-transports.md#ipv4-组播监听)维护。
  IPv4 IF/TTL/LOOP、输入结构宽度和 getter 编码依同版 ip_sockglue.c；源/接口覆盖次序对照同版
  [udp.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/udp.c) 与 [raw.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/raw.c)，
  本机回送先于链路及 TTL 0 边界对照同版 [ip_mc_output](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/ip_output.c)，默认值与成员独立性依 RFC 1112 §6.1/§6.2。
- Linux 只读状态映射核对固定 Linux v6.12 的 `include/uapi/linux/if_addr.h` 与 `net/ipv6/addrconf.c`；
  IPv4/IPv6 地址按实际 owner 枚举，回环 ::1 的 /128 与 host scope 对照同版 init_loopback；不由接口行数猜测地址数量。
  全局目的经 NET 本机地址判断、链路目的限制入接口，对照同版 [ip6_route_input_lookup](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/route.c)。
  ::1 不能从线缆接收，仍按 RFC 4291 §2.5.3；本机地址不意味着接收接口应替其他接口代理 ND。
  本机 DAD/组播契约见[IPv6 自动链路地址](../architecture-contract/ipv6-link.md)，不声明完整 IPv6 或真实交换机互通已经通过验收。

- 原生同链路 Echo 执行 [RFC 4443 §4.1/4.2](https://www.rfc-editor.org/rfc/rfc4443#section-4.1)，atomic fragment 处理依 [RFC 6946](https://www.rfc-editor.org/rfc/rfc6946)；
  IPv6 TC 随请求与源选择核对 [Linux v6.12 icmpv6_echo_reply](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/icmp.c)。
- 邻居主动解析、SLLA/TLLA 更新和 NUD 依 RFC 4861 §7.2/7.3 与附录 C；默认间隔/次数及资源上限参考
  [Linux v6.12 nd_tbl](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/ndisc.c)，缓存 scope 与压力回收策略由本项目契约明确，不宣称复制 Linux 全部回收/路由语义。

## Socket 地址扩展

- endpoint 字段采用 [RFC 3493 §3](https://www.rfc-editor.org/rfc/rfc3493#section-3) 与 [RFC 4007 §6](https://www.rfc-editor.org/rfc/rfc4007#section-6) 的地址/端口/zone 分工。
  值类型复用标准 `core::net::SocketAddr`，Linux sockaddr 的长度与字段归 syscall 层，不泄漏其结构布局到原生服务或内核。
- flowinfo/scope 与标准 Rust 转换核对 [Rust 1.91.0 socket_addr_v6_to_c/from_c](https://github.com/rust-lang/rust/blob/1.91.0/library/std/src/sys/net/connection/socket/mod.rs)；
  保留 SocketAddrV6 的原样数值，端口单独转换网络序，不为 flowinfo 擅自建立另一种数值解释。此来源只固定转换语义，不替换项目工具链基线。
- Linux IPv6 bind 的 24-byte RFC2133 基础长度及可选 scope 使用[固定 Linux v6.12 af_inet6.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/af_inet6.c)；
  原生传输版本与当前应用范围见 [Socket 传输契约](../architecture-contract/socket-transport.md)。

- 设备绑定的端口交集与 listener 准入核对 [Linux v6.12 inet_bind_conflict](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/inet_connection_sock.c)；
  设备 setter 的权限、名称解析顺序及不重新 bind 准入核对 [同版 sock_bindtoindex_locked](https://github.com/torvalds/linux/blob/v6.12/net/core/sock.c)。
- 双族端口重叠与 mapped 地址身份对照 [Linux v6.12 inet_rcv_saddr_equal](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/inet_connection_sock.c)；
  V6ONLY 与 mapped 地址的 EINVAL 以及具体 IPv6 地址的族隔离按上述 af_inet6.c。接口与 zone 的实际准入、绑定后不可改 V6ONLY 及具体 IPv6 bind 固定仅 IPv6，均按该源码核对。
- UDP connect 选源、广播授权与 rehash 对照 [Linux v6.12 datagram.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/datagram.c)；
  取消关联的地址/端口锁与 rehash/unhash 按 [同版 udp.c 的 __udp_disconnect](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/udp.c)，保留已接收和待发数据报。

- 原生 UDP 的接收长度、IPv4 可选/IPv6 必须 checksum、报文原子性以 [RFC 768](https://www.rfc-editor.org/rfc/rfc768) 与
  [RFC 1122 §4.1](https://www.rfc-editor.org/rfc/rfc1122#section-4.1) 为约束；零 datagram 保留实际 source，队列满不能伪造无人监听。
  netstack 生命周期与重用选择仍按上文固定 Linux 版本。收发格式由 packet-wire 提供，固定 smoltcp 保留部分 IPv4 接收适配，不注册 RAW endpoint。

- IPv4 RAW 的协议范围、创建授权顺序及 RAW255 默认 HDRINCL 按 [Linux v6.12 inet_create](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/af_inet.c)；
  精确协议/地址/接口匹配与完整 IP 头交付按 [同版 raw_v4_match/raw_rcv](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/raw.c)。
  RAW 的关联/取消关联与半关闭同时核对同版 datagram.c、udp.c 的 __udp_disconnect 与 af_inet.c 的 inet_shutdown；专用 raw bind 不建立 UDP bind locks。
  类型过滤与短字节更新按同版 raw.c/rawv6 的 ICMP_FILTER/ICMPV6_FILTER；过滤器的机器端序转换只属于 Linux syscall 层。
  本地协议不可达仅在上层与 RAW 都无接收者时产生，按 [同版 ip_protocol_deliver_rcu](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/ip_input.c)。

- IPv6 RAW 的去头交付、checksum 偏移与 ICMPv6 默认规则依 [RFC 3542 §3](https://www.rfc-editor.org/rfc/rfc3542#section-3)，
  Linux 扩展层级与 errno 对照 [Linux v6.12 rawv6](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/raw.c)；实现范围见 [RAW 契约](../architecture-contract/raw-ip.md)。

- IPv4 UDP 保留 DF=1 的原子报文与 ID=0，对照 [RFC 6864 §4.1](https://www.rfc-editor.org/rfc/rfc6864#section-4.1)；
  不可分片的 datagram 不占标识保留；超所选 MTU 仍在发送准入返回 EMSGSIZE。
- 非原子 IPv4 自动标识按 [RFC 6864 §4.3](https://www.rfc-editor.org/rfc/rfc6864#section-4.3) 的源/目的/协议与 MDL 保留，
  随机初值参考 [RFC 7739 §5.1](https://www.rfc-editor.org/rfc/rfc7739#section-5.1)，120 秒取自 [RFC 1122 §3.2.1.8](https://www.rfc-editor.org/rfc/rfc1122#section-3.2.1.8) 的推荐重组期限上限。

- IPv6 UDP 的 :: 自连解释、link-local zone 与既有设备一致性、mapped 来源和默认 flowinfo 核对
  [Linux v6.12 __ip6_datagram_connect](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/datagram.c) 与上述 af_inet6.c；
  单播跳数 0/-1 与 V6ONLY 的绑定时机核对 [同版 ipv6_sockglue.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/ipv6_sockglue.c)。

- IPv4 Fragmentation Needed、旧路由器零 MTU 档位及最小 68 字节遵循 [RFC 1191 §3/§5/§7](https://www.rfc-editor.org/rfc/rfc1191)。
  UDP/RAW 错误消费参考 Linux v6.12 [udp.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/udp.c) 和 [raw.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/raw.c)。
- 数据报错误队列的域边界、地址、消费与截断参考 Linux v6.12 [ip_sockglue.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/ip_sockglue.c) 和 [datagram.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/datagram.c)。
  错误控制消息布局见 [errqueue.h](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/errqueue.h)，ICMP errno 按同版 IPv4/IPv6 的错误映射表。
- IPv6 主机分片按 [RFC 8200 §4.5](https://www.rfc-editor.org/rfc/rfc8200#section-4.5) 和 RFC 7112 保留完整首片头。
  IPV6_DONTFRAG 及 HDRINCL 的 MTU 语义对照 Linux v6.12 的 [ip6_output.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/ip6_output.c) 与 [raw.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/raw.c)。
- IPv6 packet-info 的接口/源地址及接收目的按 [RFC 3542 §6](https://www.rfc-editor.org/rfc/rfc3542#section-6)；
  有序验证、零源保留与接口冲突核对 [Linux v6.12 ip6_datagram_send_ctl](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/datagram.c)。
  mapped IPv4 的 IPv6/IPv4 控制折叠按 [同版 ip_cmsg_send](https://github.com/torvalds/linux/blob/v6.12/net/ipv4/ip_sockglue.c)，
  mapped 接收同时投影及顺序按 [同版 udpv6_recvmsg](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/udp.c)。

- 本机 IPv6 UDP 空端口反馈、禁止反射范围、引用长度及 token bucket 按 [RFC 4443 §2.4/3.1](https://www.rfc-editor.org/rfc/rfc4443#section-2.4)；
  Type 1/Code 4、已关联 UDP hard error 交付对照 [Linux v6.12 __udp6_lib_rcv/__udp6_lib_err](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/udp.c)。
  不完整 ICMP 引用与完整接收分开；IPv6 零 UDP length 的接收推导仍按 [RFC 2675 §4](https://www.rfc-editor.org/rfc/rfc2675#section-4)，不能误以发送要求拒绝此接收兼容规则。

- 邻居查询的 ndmsg、NUD/NTF 与 NDA 编号固定于 [Linux v6.12 neighbour.h](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/neighbour.h)；
  单条 GET 校验、NDA_IFINDEX dump 过滤及代理查询按 [同版 neighbour.c](https://github.com/torvalds/linux/blob/v6.12/net/core/neighbour.c)。
  Router 位随被接受 NA 更新、Override 拒绝条件及状态投影按 [RFC 4861 §7.2.5](https://www.rfc-editor.org/rfc/rfc4861#section-7.2.5)；不据查询补造年龄或历史计数。

- IPv6 路径 MTU 与增长探测时限按 [RFC 8201 §4–5](https://www.rfc-editor.org/rfc/rfc8201#section-4)，低于 IPv6 最小 MTU 的 PTB 丢弃。
  路径共享、过期与减小规则对照 [Linux v6.12 route.c](https://github.com/torvalds/linux/blob/v6.12/net/ipv6/route.c) 的 __ip6_rt_update_pmtu。
  UDP PTB 异步错误按同版 udp.c 的 __udp6_lib_err，IPV6_MTU 查询/手工上限按同版 ipv6_sockglue.c；具体范围见[路径 MTU 契约](../architecture-contract/ipv6-link.md#路径-mtu)。
