# 计划：桌面上的路由系统（路由、OSPF、BGP 与图形化管理）

Status: Active

执行期间存在；完成后把持久事实迁入架构/契约/ABI 矩阵/性能文档并删除本文件。

## 目标

在 LiteOS 桌面上提供与 RouterOS 同等的路由功能与图形化管理体验：多网卡双栈转发、策略路由与 VRF、
NAT 与有状态防火墙、网桥/VLAN、OSPFv2/v3、BGP，以及实时刷新的桌面管理器与诊断工具。
性能在同等 QEMU 配置下对标 Linux 内核转发与 RouterOS CHR，并为真实网卡留好驱动接口。

面向终态设计：可以重写现有网络实现（netstack、netcfg、network-protocol 等），不保留过渡路径或双轨实现。

## 决策（2026-11-07 用户确认）

| # | 决策 | 参考 |
|---|---|---|
| R1 | 先在 QEMU（virtio-net 多队列）上实现，驱动接口按多队列、批量收发设计，之后接真实网卡 | Linux NAPI、DPDK PMD |
| R2 | 控制面是原生 Rust 服务，不经 Linux ABI | holo-routing |
| R3 | 重写 `netstack`：唯一拥有网卡队列，转发数据面与双栈主机协议栈同进程；每核 run-to-completion + RSS，向量化批处理 | VPP、Fuchsia netstack3 |
| R4 | 配置由唯一的 `router-config` 服务持有：自研 schema（不对齐 RouterOS 路径/属性，不做 .rsc 导入）、事务、版本化持久化、变更订阅 | VyOS/SONiC 的配置层思路 |
| R5 | 权限只用 Linux 用户和组；管理入口统一为 WebSocket API，经 Linux 侧网关接入 | 现有 `/dev/network-settings` 授权端点模式 |
| R6 | Linux 侧只读投影：rtnetlink/sysfs 可读，写操作返回 EPERM（记入 ABI 矩阵）；Linux 网络命名空间对应 VRF；tcpdump 经数据面按需镜像点 | — |
| R7 | 第一个里程碑：基础转发（多网卡、静态/策略路由、VRF、NAT、有状态防火墙、DHCP 服务端、DNS 缓存）、二层（网桥、VLAN）、动态路由（OSPFv2/v3、BGP 多协议与路由策略）；双栈一起做（含 ND/RA、DHCPv6-PD） | — |
| R8 | 规模按运营商边缘级：IPv4 约 100 万、IPv6 约 25 万条路由（多路径），百万级连接跟踪，数千条防火墙规则 | — |
| R9 | `netcfg` 并入 `router-config` 后删除；`dhcp-client` 保留为执行服务，由 router-config 驱动；控制面板网络页改为经 API 网关读写配置树 | — |
| R10 | 数据面 CPU：默认自适应轮询（高负载忙轮询批处理，空闲回到中断）；配置树可指定独占核。未开独占核时转发让位于桌面交互 | Linux NAPI busy-poll、VPP adaptive mode |
| R11 | 路由进程：`routing-rib` 负责选路、下一跳解析与策略并下发 FIB；`bgp`、`ospf` 各自独立进程，崩溃只影响自身并由 component-manager 重启 | FRR zebra + bgpd/ospfd |
| R12 | 本期优先完整路由功能，不要求故障/重启后恢复此前的运行时状态；跨重启 VMO 交还、转发接续及 BGP/OSPF Graceful Restart 放入二期。正常配置持久化与开机加载仍属本期 | 2026-11-08 用户修订；二期参考 RFC 4724、RFC 3623/5187 |
| R13 | 防火墙与 NAT 规则对用户呈现为有序规则链（匹配条件 + 动作，支持跳转链、地址集、连接状态、命中计数）；提交时编译为决策树加哈希集合的分类器，原子替换；已建立连接走流表快路径 | nftables flowtable、DPDK ACL |
| R14 | 本计划与已落地的[热路径优化](../development/performance-coverage.md)分开 | — |
| R15 | 第一个里程碑包含 RSTP（802.1D-2004），不做 MSTP；网桥端口状态（discarding/learning/forwarding）与 BPDU 上送从数据面设计之初就具备 | IEEE 802.1D-2004；RouterOS 网桥默认 RSTP |
| R16 | 报文格式库 `libraries/packet-wire`：批处理（整批校验后直接读字段）、前部预留空间原地加头、对接网卡卸载标志、覆盖 TCP-MD5/AO 等选项；netstack、dhcp-client、dhcp-server 统一使用，彻底移除 smoltcp 依赖 | Fuchsia netstack3 `packet`/`packet-formats` |

交互体验必须项：列表按订阅增量实时刷新（计数器、邻居、路由、连接跟踪，十万级条目流畅）；诊断工具（实时流量分解、
ping、traceroute、带宽测试、抓包）；导出、导入、配置对比与定时备份。Safe Mode 与撤销历史本期不做，
但配置树按事务与版本化设计，后续加入不需要返工。

## 约束

- 遵守 AGENTS.md：内核不出现路由或 Linux 概念；新增服务协议以版本化协议 crate 加契约文档交付，只走 channel/VMO 或标准 socket，
  不新增私有 syscall/ioctl；Linux 语义（rtnetlink 投影、EPERM、netns）只属于 linuxd。
- 每个复合状态只有一个 owner：接口配置归 router-config，运行时转发状态归 netstack，路由选择归 routing-rib。
- 新增热路径、锁、分配或 codec 都按[性能验证范围](../development/performance-coverage.md)作 benchmark 决策；
  整机吞吐、收敛时间只作诊断，不作窄阈值 blocking gate。
- 范围缩减（例如 Linux 侧写操作、未实现的 TCP 选项）在 ABI 矩阵精确记录。
- 不复制 RouterOS 或 WinBox 的名称、图标与品牌；只借鉴交互方式，界面使用现有经典桌面控件。
- 验证节奏：每阶段做两架构编译/clippy、architecture-check 与相关单元测试；运行时门禁与完整 `make verify`
  在全部阶段完成后经用户允许统一执行。
- 用户提供的 x86_64 PVE 可按需要用于后续平台支持及部署验收；不修改既有虚拟机，尤其家庭网关/OpenWrt，亦不修改宿主现有网桥、路由或防火墙。
  只使用专用测试实例；DHCP/RA、动态路由和二层环路等主动测试必须与家庭业务网隔离。连接凭据不进入源码、文档或日志。

## 服务拓扑

| 服务 | 职责 |
|---|---|
| `netstack`（重写） | 网卡队列、L2（网桥/VLAN）、L3 多 VRF 双栈转发、邻居（ARP/ND）、连接跟踪、NAT、防火墙分类器、流表快路径；主机协议栈（TCP/UDP/原始 IP/ICMP、多播加组、TCP-MD5/AO）；socket 共享环；镜像点 |
| `router-config`（新） | 配置树 owner：schema、校验、事务、持久化、订阅；把配置下发给 netstack、routing-rib、协议进程与执行服务 |
| `routing-rib`（新） | 各来源路由的汇总与选路、递归下一跳解析、路由策略（前缀列表、团体属性等）、按 VRF 下发 FIB 与共享下一跳对象 |
| `bgp`、`ospf`（新） | 协议状态机；作为主机协议栈的普通 socket 使用者；向 routing-rib 提交路由 |
| `dhcp-client`（保留） | 由 router-config 驱动的 DHCPv4/DHCPv6-PD 客户端 |
| `dhcp-server`、`dns-cache`（新） | 由 router-config 驱动的执行服务 |
| `router-api`（新，Linux 侧） | WebSocket 网关：本地 Unix socket（`SO_PEERCRED`）与远程 TLS（Linux 账号登录）；按组授权（如 `netadmin` 可写、`netview` 只读）；持有 linuxd 发布的授权端点 `/dev/router-config` |
| 网络管理器（新，Linux 桌面应用） | 经典控件实现的图形化管理器：左侧菜单树、列表窗口、属性对话框、实时刷新、诊断工具；只连 router-api |

现有 `netcfg`、`netcfg-protocol`、`/dev/network-settings`、`/dev/network-status`、`user/libraries/network-config`
及控制面板网络页在对应阶段迁移到新链路后删除，不保留兼容入口。

## 数据结构

- **FIB**：每 VRF 一张；IPv4 用 Poptrie 或 DIR-24-8，IPv6 用 Poptrie（Asai & Ohara, SIGCOMM 2015）；
  路由指向共享下一跳对象（含多路径组），下一跳变化 O(1) 影响所有引用它的路由。读侧无锁（RCU/epoch 式发布），
  写侧批量更新。
- **策略路由**：规则表（源地址、入接口、标记等）选择 VRF/路由表。
- **连接跟踪**：每核分片哈希表，按协议状态定时老化；NAT 映射与端口分配在同一 owner 内。
- **分类器**：规则链编译为决策树与哈希集合；地址集独立更新不触发整体重编译；按规则计数（每核计数，读时汇总）。
- **报文缓冲**：每队列预分配缓冲池，网卡 DMA 直接写入 netstack 的 VMO；批大小固定，热路径不分配。
- **状态寿命**：本期运行时 FIB、下一跳、接口表、连接跟踪随其 owner 存续，不宣称故障后恢复；配置树仍按版本持久化。跨重启保留及布局迁移属二期。

## 阶段与验证点

### 0. 前置能力（内核与驱动）

- 已有前置：高精度定时器、登记 VMO 块/DMA 数据面、截止时间调度类与 socket 共享环；
  owner 与验收边界见[性能覆盖](../development/performance-coverage.md)。
- 新增：CPU 隔离（独占核不调度普通线程）与中断亲和设置的原生接口；virtio-net 多队列与每队列 MSI-X；
  驱动 trait 改为多队列批量收发（RSS 配置、队列数协商、校验和/TSO 等卸载能力位）。
- **契约**：执行域契约（CPU 隔离、IRQ 亲和）、设备契约（网络驱动 trait）。
- **验证点**：kernel-unit 调度与 IRQ 测试；virtio-net 多队列单元测试；两架构编译。

### 1. 协议 crate 与 router-config

- 定义版本化协议 crate（配置 schema、事务、订阅、FIB/RIB 下发、诊断）；schema 以类型化描述生成校验与 API 元数据，
  桌面管理器与未来的 Web 界面据此生成列表和对话框。
- router-config 实现事务（校验 → 预备 → 提交到各执行服务 → 失败回滚）、版本化持久化、导出/导入、配置对比、定时备份。
- 并入 netcfg 的默认策略与用户设置；dhcp-client 改由 router-config 驱动；删除 netcfg 与 netcfg-protocol。
- **契约**：新增 router-config 协议契约；`docs/architecture-contract/dependency-owners.md` 的服务拓扑与能力路由。
- **验证点**：router-config 单元测试（事务原子性、部分失败回滚、持久化崩溃一致性、schema 版本升级）。

### 2. netstack 重写：数据面骨架与主机协议栈

- 每核 worker、RSS、批处理图（收包 → 解析 → L2 → L3 → 本机/转发分流 → 发包），自适应轮询与独占核配置。
- `libraries/packet-wire`（R16）：以太网、VLAN、ARP、IPv4/IPv6（含扩展头）、ICMP/ICMPv6（含 ND/MLD）、TCP（含 MD5/AO 选项）、
  UDP、DHCPv4/v6 的解析与构造；每种格式 fuzz 测试，并与 smoltcp 及 Linux 抓包样本做对照测试（对照只在测试中，产品代码不依赖 smoltcp）。
  dhcp-client 在本阶段同步迁移，移除 workspace 中的 smoltcp 依赖。
- 双栈主机协议栈：IPv4/IPv6、ICMP/ICMPv6、ND/RA、TCP（含 MD5/AO、窗口缩放、SACK）、UDP、原始 IP、多播加组；
  socket 沿用性能计划落地的共享 SQ/CQ + 事件通知，迁移到新协议引擎，不恢复逐次数据 RPC。
- linuxd：socket 接入新协议；rtnetlink/sysfs 只读投影（接口、地址、路由、邻居、规则），写操作 EPERM；
  netns 对应 VRF。
- **契约**：`docs/architecture/ipc-network.md` 与 IPC/网络契约整体改写；ABI 矩阵（socket、rtnetlink 写操作范围）。
- **benchmark 决策**：批处理图单级处理与 socket 环收发加 host blocking benchmark（单包 CPU 成本、热路径零分配）。
- **验证点**：协议栈单元测试；现有 socket/网络运行时门禁（统一阶段执行）。

### 3. L3 转发、VRF 与策略路由

- FIB（Poptrie/DIR-24-8）、共享下一跳与多路径（ECMP 哈希）、规则表、邻居解析、ICMP 错误生成、TTL/hop limit、分片处理。
- 共享 FIB 的布局按当前读写与发布语义设计；本期不引入跨重启交还布局或恢复流程。
- **benchmark 决策**：FIB 查找与更新加 host blocking benchmark（100 万前缀查找、批量更新吞吐）。
- **验证点**：FIB 单元测试（最长前缀匹配对照参考实现的随机测试）；多网卡转发运行时测试。

### 4. 连接跟踪、NAT 与防火墙

- 连接跟踪（TCP 状态、UDP/ICMP 超时）、srcnat/dstnat/masquerade、规则链编译器与分类器、流表快路径、规则计数。
- **benchmark 决策**：分类器（数千条规则）与连接跟踪查找/插入加 host blocking benchmark。
- **验证点**：编译器对照逐条线性执行的等价性随机测试；NAT 端口耗尽与超时回收测试。

### 5. 网桥与 VLAN

- 网桥（MAC 学习与老化、泛洪）、802.1Q VLAN 过滤、端口隔离。
- RSTP（R15）：端口角色与状态机、BPDU 收发（经数据面上送）、拓扑变化时刷新 MAC 表；网桥默认启用。MSTP 不在本期。
- **验证点**：网桥学习、老化、VLAN 过滤单元测试；RSTP 状态机单元测试；多网卡二层运行时测试
  （人为制造环路时不形成广播风暴，主链路断开后备份链路在 RSTP 收敛时间内接管）。

### 6. routing-rib、OSPF 与 BGP

- routing-rib：多来源选路（管理距离/优先级）、递归下一跳、路由策略引擎、FIB 批量下发。
- ospf：OSPFv2（RFC 2328）、OSPFv3（RFC 5340），区域、LSA 泛洪、SPF（增量 SPF 视收敛指标决定）。
- bgp：BGP-4（RFC 4271）、多协议（RFC 4760）、团体属性、路由反射（RFC 4456）、ADD-PATH 视需要；
  Adj-RIB-In/Loc-RIB 内存按 100 万前缀设计（属性去重共享）。
- **验证点**：各协议单元测试（状态机、报文编解码 fuzz）；与 FRR/BIRD 在 QEMU 中互通的运行时测试；
  BGP 全表加载与 OSPF 收敛诊断。

### 7. DHCP 服务端、DHCPv6-PD 与 DNS 缓存

- dhcp-server（RFC 2131、DHCPv6 RFC 8415）、DHCPv6-PD 客户端与下游 RA 前缀分发、dns-cache（转发与缓存）。
- **验证点**：协议单元测试；Linux 客户端在 QEMU 中取得地址与解析域名。

### 8. router-api 网关与网络管理器

- router-api：WebSocket（本地 Unix socket、远程 TLS）、Linux 账号认证、按组授权、订阅增量推送、背压。
- linuxd 发布限组的 `/dev/router-config` 授权端点，删除 `/dev/network-settings` 与 `/dev/network-status`。
- 网络管理器（Rust UI SDK）：菜单树、虚拟化列表（只绘制可见行）、属性对话框、实时刷新、诊断工具（实时流量分解、
  ping、traceroute、带宽测试、抓包，抓包经数据面镜像点）。控制面板网络页改为经 router-api 读写。
- **契约**：用户态与 ABI 契约（授权端点、组）；UI 框架契约（新控件需求若有）。
- **验证点**：网关单元测试（授权、断线、背压）；管理器在十万级条目下的绘制与刷新诊断。

### 9. 二期：故障恢复（本期不实施）

- 依 R12 的用户修订，本期不要求 netstack 故障或重启后恢复此前的路由、接口运行状态、连接跟踪及 NAT 映射。
- 跨进程状态交还、恢复时限、转发连续性及 BGP/OSPF Graceful Restart 在二期设计和验收，不作为本期完成声明。

### 10. 评测、文档迁移与整体验证

- 在同样的 QEMU 配置下对比 Linux 内核转发与 RouterOS CHR（诊断，不设阈值）：
  - 64 字节小包 pps、IMIX 吞吐，开启 NAT 与防火墙后的衰减，按核数的扩展；
  - BGP 全表加载、OSPF 收敛、FIB 更新速率；
  - 每 100 万路由与每 100 万连接的内存，空闲时 CPU 占用；
  - 管理界面在十万级条目下的响应、配置提交到生效的延迟。
- 持久事实迁入 `docs/architecture/`、契约、ABI 矩阵与性能文档；完整 `make verify`（经用户允许）。

## 进度

- `micro_kernel` 已于 2026-11-08 快进合入本地 `main`，实现分支为 `codex/router`，起点 `c070532f`。
- 阶段 0 开始；故障恢复依 R12 修订移至二期，其余阶段与用户可用目标保持完整。前置 socket 环边界见[性能覆盖](../development/performance-coverage.md)。
- 阶段 0：原生中断对象已接通在线逻辑 CPU 亲和配置，GIC/PLIC 保持屏蔽与 ack；CPU 独占接口已接入配置准入、调度分派与退出归还，
  见[执行契约](../architecture-contract/execution.md#cpu-分区)。两架构静态检查与 scheduler-unit 覆盖不代替 SMP 硬件验收；多队列与队列授权传输仍在实施。
