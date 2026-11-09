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
- netstack、netstack-unit 与 architecture-bench 的唯一协议栈源码是 `vendor/smoltcp`；原 release 每个文件的 SHA-256
  记录于 `UPSTREAM.sha256.json`，保留原许可证与 upstream commit。Cargo 不保留 registry 备选路径。
- 本地变化限定UDP/TCP/raw transport、UDP ingress及interface dispatch与IPv4地址mechanism十个文件（第十个只是 `wire/udp.rs` 里 6LoWPAN 专用函数的 feature 门控）：connected peer 在 ingress admission 前匹配，canonical local
  endpoint 修改保留已有 queue，bind/rehash order 选择单播目标，广播/组播交付实际匹配集合，
  TX packet 在 enqueue 时冻结source port/address/TTL/oif；TCP实际MSS cache约束广告及分段。
  bound device在ingress准入前筛选，kernel唯一route selector在dispatch前选设备；IPv4
  weak-host tuple不因当前iface source列表而reset。Linux bind/shutdown/rlimit/netns UAPI 归 linuxd；netstack 只拥有协议机制与 endpoint 状态。
- 只启用alloc、Ethernet、IP-medium实际loopback、IPv4、raw、UDP、TCP与Reno；Linux socket UAPI不由smoltcp定义，IPv6仍延期。

## 报文字节格式

- 批内阶段组织参考固定 VPP 25.02 的 [Scalar vs Vector packet processing](https://docs.fd.io/vpp/25.02/aboutvpp/scalar-vs-vector-packet-processing.html)
  与[处理图](https://docs.fd.io/vpp/25.02/aboutvpp/extensible.html)：同一阶段处理一组帧，格式借用不复制 payload；不据此声称已实现完整 VPP 图、SIMD 或无锁多核引擎。
- Ethernet Length/Type 按 IEEE 802.3 §3.2.6；固定一手说明见 IEEE 802.1 WG
  [2020-05 protocol identification](https://www.ieee802.org/1/files/public/docs2020/maint-seaman-protocol-ids-in-std-802-0520-v01.pdf)。
  C/S VLAN tag 使用 [IEEE 802.1Q-2022](https://1.ieee802.org/maintenance/p802-1q-rev/)；
  固定 Linux revision 的 `include/uapi/linux/if_ether.h`、`include/linux/if_vlan.h` 作为成熟格式对照，不把 Linux VLAN 配置 UAPI 带进 codec。
- ARP 可变宽地址与网络字节序取自 [RFC 826](https://www.rfc-editor.org/rfc/rfc826)；
  独立 host 对照使用固定 smoltcp 的 `wire/arp.rs`，不复制其邻居状态机。
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
  Jenkins lookup3 two-word final 对照 [jhash.h](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/linux/jhash.h) 中注明公有领域的原算法；不照搬 Linux secret seed、路径编号或 RCU。
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

## BGP

- 消息、UPDATE 与基本属性固定采用 [RFC 4271](https://www.rfc-editor.org/rfc/rfc4271)，独立错误处理以
  [RFC 7606](https://www.rfc-editor.org/rfc/rfc7606) 为约束；格式库不把单个坏属性自动升级为 session reset，整批裁决属于 BGP 协议 owner。
  AS 0 的各属性处置按 [RFC 7607](https://www.rfc-editor.org/rfc/rfc7607)：AS_PATH/AGGREGATOR 交 RFC 7606，AS4 辅助属性交 RFC 6793，不能把它们都当成同一种严重性。
- 能力按 [RFC 5492](https://www.rfc-editor.org/rfc/rfc5492)，普通/扩展 OPEN 参数按
  [RFC 9072](https://www.rfc-editor.org/rfc/rfc9072)，扩展消息按 [RFC 8654](https://www.rfc-editor.org/rfc/rfc8654) 的方向授权与类型限制。
- MP_REACH/MP_UNREACH 按 [RFC 4760](https://www.rfc-editor.org/rfc/rfc4760)，四字节 AS/AS4 属性按
  [RFC 6793](https://www.rfc-editor.org/rfc/rfc6793)，联盟段类型按 [RFC 5065](https://www.rfc-editor.org/rfc/rfc5065)。
- Communities/Extended/Large 的布局分别按 [RFC 1997](https://www.rfc-editor.org/rfc/rfc1997)、
  [RFC 4360](https://www.rfc-editor.org/rfc/rfc4360)、[RFC 8092](https://www.rfc-editor.org/rfc/rfc8092)；反射字段按
  [RFC 4456](https://www.rfc-editor.org/rfc/rfc4456)。列表非空/畸形属性的检查与该属性的传播/策略处置独立。
- 基础 ROUTE-REFRESH 按 [RFC 2918](https://www.rfc-editor.org/rfc/rfc2918)；subtype 保留
  [RFC 7313](https://www.rfc-editor.org/rfc/rfc7313) 的扩展空间，不据此宣称服务已接纳增强刷新或 ORF。
- Identifier 与同 ID 碰撞采用 [RFC 6286](https://www.rfc-editor.org/rfc/rfc6286)，FSM 错误数据按 [RFC 6608](https://www.rfc-editor.org/rfc/rfc6608)，Cease 子码按 [RFC 4486](https://www.rfc-editor.org/rfc/rfc4486)。
  MP 交集/无 MP 时的基础 IPv4 接纳同时对照固定 FRR 10.0 [`bgpd/bgp_open.c`](https://github.com/FRRouting/frr/blob/frr-10.0/bgpd/bgp_open.c)；不复制其状态容器或并发设计。
  路径规范化及分组件驻留对照同版 [`bgp_aspath.c`](https://github.com/FRRouting/frr/blob/frr-10.0/bgpd/bgp_aspath.c) 和 [`bgp_attr.c`](https://github.com/FRRouting/frr/blob/frr-10.0/bgpd/bgp_attr.c)；本项目索引复用可失败 AVL，引用计数不与领域引用人工同步。
  选择顺序按 [RFC 4271 §9.1.2.2](https://www.rfc-editor.org/rfc/rfc4271#section-9.1.2.2) 与 RFC 4456 §9；MED 分组/非传递风险同时对照
  FRR 10.0 [`doc/user/bgp.rst`](https://github.com/FRRouting/frr/blob/frr-10.0/doc/user/bgp.rst)。本地确定性不等于网络级无振荡，不声明 FRR 所有厂商策略兼容。
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

