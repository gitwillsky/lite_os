# LiteOS transport source

此目录是 netstack、host 回归与 architecture benchmark 唯一使用的 smoltcp 0.13.1。
来源为 upstream commit `e347a1e2d3ac33c5ce2c0c114e24b85ae23c4897` 的原 crate；许可证为
原样保留的 [0BSD](LICENSE-0BSD.txt)。`UPSTREAM.sha256.json` 固定逐文件原始内容，
`scripts/verify_smoltcp.py` 拒绝原 inventory、未授权文件或第二个 registry consumer 的漂移。
四个不可变 upstream 文件自带行尾空白或末尾空行；根 `.gitattributes` 仅对这些确切文件
保留对应原始格式，SHA 校验仍逐字节裁决。本地明确授权的 transport 文件不豁免空白检查。

允许变化的准确范围是 `src/socket/{tcp,raw}.rs`、
`src/iface/interface/{mod,ethernet,ipv4,ipv6,udp,tcp}.rs`、`src/iface/mod.rs`、`src/phy/mod.rs`、`src/wire/udp.rs`。
`src/wire/udp.rs` 只给 `Repr::emit_header` 加上与其唯一调用方（6LoWPAN）相同的 `proto-sixlowpan` 门控，
消除未启用该 feature 时的 dead-code 警告，不改变任何运行代码。
产品与 host 的 transport 依赖使用相同 IPv4/raw、Ethernet/IP-medium profile，关闭库 TCP/UDP socket 与 IPv6；host 仅为独立 wire 对照追加 proto-dhcpv4。

## Mechanism 与 policy owner

Linux UAPI、权限、namespace、端口 membership、设备配置、ARP、地址与 FIB 事务、队列预算由
netstack owner 实现。库保留部分 IPv4 接收适配。
原生 TCP/UDP/RAW 的 tuple、队列、接收筛选、时钟与 readiness 已迁入 netstack 和 libraries/tcp，不复制用户 ABI 或第二份 FIB。

- IP 校验/目的准入后，以借用 Ipv4Ingress 将完整 TCP/UDP upper-layer 字节交给各自原生 owner；
  原生 packet-wire 校验 UDP 长度/checksum，硬件验证事实只省略求和。匹配后队列满仍消费，不能错误回复 port unreachable。
  无接收者保留原 ICMP 回复路径；RAW tap 仍在 transport 投递之前。
- 以太网和 IP-medium 只传递同一个 callback；库可选 IPv6 分派的签名同步，不改变其原有接收算法。
  socket/udp.rs 已恢复固定 upstream 字节并重新受 SHA 校验，不保留旧本地 UDP 状态机或 fallback feature。
- 原生 TCP/UDP 通过 netstack 邻居表解析实际下一跳，直接在最终 DMA 窗口用 packet-wire 加头。
  native token 返回实际提交结果，背压保留队首。DMA 不由 adapter 预清零，完整写入责任与原有 IP-medium 相同；
  原生 codec 写全帧。ARP 与 IPv4 主机发送、转发均在 native token 观察真实提交后推进。RAW 入队冻结 oif，RX metadata 冻结 iif/spec_dst/destination。
- 本机片由 NET 唯一重组 owner 完成后，同一次 callback 借出完整 IPv4 bytes 与 upper-layer 字节，不启用库内第二套重组状态。
  NET 先投递原生 RAW，再交付 TCP/UDP；未知上层协议已有接收者时不回复 protocol unreachable。
  NET 不向库 SocketSet 注册任何实际 socket，不保留 RAW 路由扫描或第二份队列；具体契约见
  [原始 IP 报文](../../docs/architecture-contract/raw-ip.md)。

## 逐帧 checksum metadata

`PacketMeta.tx_checksum` 只由实际 TCP/UDP/DHCP emitter 在跳过软件 transport checksum 时设置。
Device capabilities 的 TX checksum 关闭不代表所有 RAW 字节都能重写：RAW metadata 保持 false，
netstack 的设备 adapter 只为明确请求的帧构造伪头 seed，VirtIO 驱动依逐帧描述符执行 partial checksum。
超出出口 MTU、将进入分片的包先强制完整软件 checksum，不把整包 seed 错当成分片 checksum。
`PacketMeta.rx_checksum_valid` 由实际 RX token 提供；只在本次 ingress 作用域内跳过 TCP/UDP RX 验证，
TX 策略和 IP/ICMP 验证保持不变，正常返回（包括 closure 提前返回）后恢复原 capabilities。
NEEDS_CSUM 已由 driver-host 在交付只读帧前补全，不把未完成 checksum 暴露给 RAW 或普通 socket。
实际 TCP/UDP/RAW 收发均使用 packet-wire 与 NET 原生 owner；库 SocketSet 为空。

## 唯一 FIB 与 dispatch

netstack 拥有唯一 IPv4 FIB。IpRouteContext 只在本次
active-open/egress/RX guard 有效，向库借用 next-hop、logical iif、LOCAL/broadcast、
spec_dst、设备 MTU 和 route metrics，结束即清除。选中出口之前不改变 transport、queue
或 neighbor 状态。IPv4 默认 weak-host 保留真实 tuple source，不因当前物理 link 的地址
列表缺少它而重置 TCP。PacketMeta.interface_index 是实际 lo queue 的逻辑入口事实。

TCP ingress 只向原生 owner 借出同一帧的逻辑入口、RAW 已处理事实及本次冻结的两种反向路径（不绑定/绑定入口）。
库不匹配 TCP 连接、不验证第二次 TCP checksum，也不保留 RST 旁路或备用分派。
原生 TCP 直接用 packet-wire 输出并借唯一 ARP 机制；TCP 序号、MSS、期限及退休契约只由
[传输契约](../../docs/architecture-contract/ipv4-transports.md)维护。此目录的旧 TCP 源仅保留固定来源，不编译为产品或 host 备用引擎。
ARP 仍在当前 link 内响应；LOCAL/on-link 来自同一 FIB，0 源 DHCP probe 仅回复、不缓存 0 邻居。库不维护每接口的路由镜像。

## 规范与验证

Linux 语义依据仓库固定 v7.1 commit
[`8cd9520d35a6c38db6567e97dd93b1f11f185dc6`](https://github.com/torvalds/linux/tree/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4)：
`udp.c`、`raw.c`、`route.c`、`fib_frontend.c`、`fib_trie.c`、`arp.c`、`metrics.c`、
`tcp.c`、`tcp_input.c`、`tcp_output.c`、`tcp_timer.c`、`tcp_metrics.c` 及 `include/net/tcp.h`。

Host wire 回归载入实际 production owners，native musl guest 验证标准 syscall ABI；source
围栏保留负向 mutation，provenance inventory 和明确授权文件/feature profile 不可漂移。实际每 slot
clock 与完整 NET poll 保持 200ns 成本门禁。上述机制无新增 global/lock/unsafe，沿用
transport/NET guard；RAW codec 冷发送使用既有有界 copy，不引入另一个排队路径。

此库的产品执行路径保留部分 IPv4 接收处理。raw feature 用于空 SocketSet 的类型定义。原生 IPv4 邻接与 IPv6 分别见[传输契约](../../docs/architecture-contract/ipv4-transports.md#ipv4-邻接与-arp)及[IPv6 契约](../../docs/architecture-contract/ipv6-link.md)。
