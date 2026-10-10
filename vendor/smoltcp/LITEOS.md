# LiteOS transport source

此目录是 kernel、host 回归与 architecture benchmark 唯一使用的 smoltcp 0.13.1。
来源为 upstream commit `e347a1e2d3ac33c5ce2c0c114e24b85ae23c4897` 的原 crate；许可证为
原样保留的 [0BSD](LICENSE-0BSD.txt)。`UPSTREAM.sha256.json` 固定逐文件原始内容，
`scripts/verify_smoltcp.py` 拒绝原 inventory、未授权文件或第二个 registry consumer 的漂移。
四个不可变 upstream 文件自带行尾空白或末尾空行；根 `.gitattributes` 仅对这些确切文件
保留对应原始格式，SHA 校验仍逐字节裁决。本地九个 transport 文件不豁免空白检查。

允许变化的准确范围是 `src/socket/{udp,tcp,raw}.rs`、
`src/iface/interface/{mod,ipv4,udp,tcp}.rs`、`src/iface/mod.rs`、`src/phy/mod.rs`。
三个 consumer 使用相同 IPv4/TCP/UDP/raw、Ethernet/IP-medium feature profile；不启用 IPv6。

## Mechanism 与 policy owner

Linux UAPI、权限、namespace、端口 membership、设备配置、地址与 FIB 事务、队列预算由
kernel socket owner 实现。库拥有实际 transport queue、tuple、sequence、timer、neighbor
和 wire codec，不复制用户 ABI 或第二份 kernel route table。

- UDP connected peer/local tuple 在实际接收准入之前筛选源；解绑保留既有队列。
  单播按真实 bind/rehash 全序选择，广播交付全部匹配目标。
- UDP TX 入队冻结 source port/address/TTL；后续绑定不修改旧报文。UDP/raw 入队冻结 oif，
  RX metadata 冻结 iif/spec_dst/destination，kernel 从同一队列投影 PKTINFO。
- RAW binding 在入队前筛选地址和设备。RAW255 的 receive_enabled 控制真实接收准入，
  transmit_any_protocol 允许 HDRINCL header 选择协议；kernel 决定 RAW255、权限、
  HDRINCL codec、MTU 和零 source/ID 填充，不创建只有 ioctl 行为的空 endpoint。
- RAW 完整基础 IPv4 header 随原队列传给 dispatch_ip_frame；普通 transport 使用 Packet
  repr codec，两种输入进入同一 egress/neighbor engine。接收保存真实已验证的未分片 IPv4
  bytes，避免 Repr 重编码丢失 ID/TOS/DF/options；重组或其他地址族使用原 codec。
  TX 的 IP options、分片和 header/sockaddr 目的不同由 kernel 在入队前明确拒绝。
- Interface 接受实际 ifa_broadcast，覆盖 prefix 推导；limited broadcast/Class E 的原生
  local 配置不触发 unicast assertion，未开放的组播配置明确拒绝。
- IP-medium 承载真实 lo，不伪造 Ethernet/ARP；MTU 刷新 derived DeviceCapabilities，
  medium 不可改变。范围、实际设备配置及 Linux ioctl 属于 kernel owner。

## 唯一 FIB 与 dispatch

Kernel 的 33 个 prefix bucket 保存唯一 IPv4 FIB。IpRouteContext 只在本次
active-open/egress/RX guard 有效，向库借用 next-hop、logical iif、LOCAL/broadcast、
spec_dst、设备 MTU 和 route metrics，结束即清除。选中出口之前不改变 transport、queue
或 neighbor 状态。IPv4 默认 weak-host 保留真实 tuple source，不因当前物理 link 的地址
列表缺少它而重置 TCP。PacketMeta.interface_index 是实际 lo queue 的逻辑入口事实。

TCP listener 只匹配 unbound 或与 iif 相同的 bound socket；反向 FIB 提供实际握手 MTU，
无反向 route 不创建半连接。即时 IP reply 通过唯一有预算 kernel queue、原 Packet codec
和同一 FIB/neighbor dispatch；ARP 仍在当前 link 内响应。ARP LOCAL/on-link 来自同一 FIB，
0 源 DHCP probe 仅回复，不缓存 0 邻居。库不维护 per-interface kernel route mirror。

Route ADVMSS/WINDOW/RTT 进入真实握手和发送机制：接收广告与发送 MSS 分开，发送受实际
MTU/peer/user cap 限制；Nagle 准入、Reno 单位和分段使用同一 sender MSS。ESTABLISHED
setter 不篡改旧握手；断开保留 sender cache，kernel 清除原生 user_mss policy。窗口 clamp
限制 SYN/RX，route RTT 只 seed 握手完成后的首个 data RTO，不伪造 srtt 样本。

## TCP sequence、期限与退休

真实 SND.NXT 仅在成功提交 SYN/数据/FIN 时推进，与 RTO 可回退的 retransmit cursor 分开。
ACK 上界、ACK-all、empty ACK/RST 和 unacked 判定读取同一 SND.NXT，不使用 queued bytes
冒充发送事实。Nagle 不阻止旧小段重传，纯 ACK 不重复 FIN。

Pure maintain 先于 route/NIC 筛选，无 route/设备 DOWN 不冻结重试、USER_TIMEOUT、
keepalive、PROBE0、FIN_WAIT2 或 TIME_WAIT 退休。OutputFailure 将路径不可用与本地资源
拥塞送入同一 transport timer；不创建额外 route selector 或 timer engine。

Keepalive 独占实际活动时刻、deadline 和 count；只在 Established/CloseWait 无未完成
sequence 时发送 snd_una-1 空 ACK。接收/普通发送重置活动周期，成功提交或路径失败才增加
count，本地资源失败使用 500ms 重试且不计 peer 失败。USER_TIMEOUT 仍需至少一次探测。

PROBE0 的实际 deadline、退避和 pending intent 由 transport 拥有；无 clock 的 send 不将
uptime ZERO 冒充起点。Probe 使用原 TcpRepr/Packet 空 ACK，不发送或确认 stream 字节。
零窗口应答刷新 count/deadline，但不重置 USER_TIMEOUT 起点；只有 ACK 真正推进 SND.UNA
才重置未完成 sequence epoch。持续零窗口应答不因累计尝试误杀未配置 USER_TIMEOUT 的连接。

TCP_LINGER2 只在最后 application fput 后约束 FIN_WAIT2；FIN ACK 冻结起点，正常期限
静默退休，负值发布一次 reset。Reset 沿原 Packet codec 和 FIB dispatch，只保留有预算的
payload/oif，不持有 TCP/port/endpoint 身份；OOM、预算满、无 route 均不恢复已释放身份。
Queued RX/FIN 不因 timeout 退休而丢失。

## 规范与验证

Linux 语义依据仓库固定 v7.1 commit
[`8cd9520d35a6c38db6567e97dd93b1f11f185dc6`](https://github.com/torvalds/linux/tree/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/net/ipv4)：
`udp.c`、`raw.c`、`route.c`、`fib_frontend.c`、`fib_trie.c`、`arp.c`、`metrics.c`、
`tcp.c`、`tcp_input.c`、`tcp_output.c`、`tcp_timer.c`、`tcp_metrics.c` 及 `include/net/tcp.h`。

Host wire 回归载入实际 production owners，native musl guest 验证标准 syscall ABI；source
围栏保留负向 mutation，provenance inventory 和准确九文件 profile 不可漂移。实际每 slot
clock 与完整 NET poll 保持 200ns 成本门禁。上述机制无新增 global/lock/unsafe，沿用
transport/NET guard；RAW codec 冷发送使用既有有界 copy，不引入另一个排队路径。

IPv6、多网卡、完整 raw protocol、multicast、复杂 PMTU 和分片/重组不在本轮支持范围内。
