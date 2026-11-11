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
- 验证节奏：以代码审查与范围内 Clippy 为主，跨架构变更覆盖两架构及 architecture-check；只有审查无法覆盖具体风险时才运行最窄的既有用例。
  运行时门禁与完整 `make verify` 在全部功能完成后经用户允许统一执行。
- 用户提供的 x86_64 PVE 可按需要用于后续平台支持及部署验收；不修改既有虚拟机，尤其家庭网关/OpenWrt，亦不修改宿主现有网桥、路由或防火墙。
  只使用专用测试实例；DHCP/RA、动态路由和二层环路等主动测试必须与家庭业务网隔离。连接凭据不进入源码、文档或日志。

## 服务拓扑

| 服务 | 职责 |
|---|---|
| `netstack`（重写） | 网卡队列、L2（网桥/VLAN）、L3 多 VRF 双栈转发、邻居（ARP/ND）、连接跟踪、NAT、防火墙分类器、流表快路径；主机协议栈（TCP/UDP/原始 IP/ICMP、多播加组、TCP-MD5/AO）；socket 共享环；镜像点 |
| `router-config` | 配置树 owner：schema、校验、事务、持久化、订阅；把配置下发给 netstack、routing-rib、协议进程与执行服务 |
| `routing-rib`（新） | 各来源路由的汇总与选路、递归下一跳解析、路由策略（前缀列表、团体属性等）、按 VRF 下发 FIB 与共享下一跳对象 |
| `bgp`、`ospf`（新） | 协议状态机；作为主机协议栈的普通 socket 使用者；向 routing-rib 提交路由 |
| `dhcp-client`（保留） | 由 router-config 驱动的 DHCPv4/DHCPv6-PD 客户端 |
| `dhcp-server`、`dns-cache`（新） | 由 router-config 驱动的执行服务 |
| `router-api`（新，Linux 侧） | WebSocket 网关：本地 Unix socket（`SO_PEERCRED`）与远程 TLS（Linux 账号登录）；按组授权（如 `netadmin` 可写、`netview` 只读）；持有 linuxd 发布的授权端点 `/dev/router-config` |
| 网络管理器（新，Linux 桌面应用） | 经典控件实现的图形化管理器：左侧菜单树、列表窗口、属性对话框、实时刷新、诊断工具；只连 router-api |

现有配置入口与权限见[路由配置契约](../architecture-contract/router-configuration.md)。

## 数据结构

- **FIB**：每 VRF 一张；IPv4 用 Poptrie 或 DIR-24-8，IPv6 用 Poptrie（Asai & Ohara, SIGCOMM 2015）；
  路由指向共享下一跳对象（含多路径组），下一跳变化 O(1) 影响所有引用它的路由。读侧无锁（RCU/epoch 式发布），
  写侧批量更新。
- **策略路由**：规则表（源地址、入接口、标记等）选择 VRF/路由表。
- **连接跟踪**：每核分片哈希表，按协议状态定时老化；NAT 映射与端口分配在同一 owner 内。
- **分类器**：规则链编译为决策树与哈希集合；地址集独立更新不触发整体重编译；按规则计数（每核计数，读时汇总）。
- **报文缓冲**：每队列预分配缓冲池，网卡 DMA 直接写入 netstack 的 VMO；批大小固定，热路径不分配。
- **状态寿命**：本期运行时 FIB、下一跳、接口表、连接跟踪随其 owner 存续，不宣称故障后恢复；配置树仍按版本持久化。跨重启保留及布局迁移属二期。

## 系统验收

整机验收待完成。按[构建与验证](../development/build-and-verify.md)验证 R1–R16 及必需交互。

- **主机与地址服务**。验证静态及动态通告、地址冲突、前缀变更、DNS、租约和认证。验证双栈 socket、并发分片、错误反馈及 TCP 丢包恢复，并与成熟系统互通。
- **路由与转发**。验证多网卡独立启停、邻居解析、多路径、跳数、分片和错误报告。覆盖 BGP/OSPF 与 FRR/BIRD 互通、BGP 反射及联盟，以及邻居、网卡或策略变化后的查询、宣告、收敛与实际转发。
- **二层与安全**。在与家庭业务网隔离的环境验证 VLAN 互通、环路抑制、链路切换，以及防火墙与 NAT 的双向行为和规则顺序。
- **配置与管理**。验证完整桌面操作、权限边界、配置生效时间，以及存储失败、断电后的配置加载与备份恢复。
- **性能与规模**。以同等 QEMU 配置对比 Linux 和 RouterOS CHR，测量小包及 IMIX 吞吐、NAT/防火墙开销、多核扩展与空闲 CPU。覆盖目标路由、连接、规则及列表规模下的收敛、内存和交互响应。

## 二期：故障恢复

二期验收故障或重启后的运行状态恢复、恢复时限、连续转发，以及 BGP/OSPF 平滑重启。本期验收配置保存与开机加载，范围见 R12。

## TODO（待交付功能）

### 双栈主机网络

- [ ] **完整的数据报错误详情**。数据报发送失败时，应用可取得本机错误或远端返回的 ICMP 错误，并查询相关接口、地址和扩展信息。[错误反馈](../architecture-contract/socket-transport.md#数据报错误队列)
- [ ] **TCP 认证**。可为 TCP 连接启用 TCP-AO，并按路由域管理认证密钥。
- [ ] **Linux 网络查询**。可在各 Linux 网络命名空间内查询对应路由域的接口、地址、路由、邻居及规则。[网络查询](../architecture-contract/packet-netlink.md)

### 多网卡与路由

- [ ] **路由隔离**。可独立配置多个路由域，在不同域内复用地址，并为 Linux 网络命名空间指定对应的路由域。
- [ ] **双栈组播转发**。可配置 IPv4 和 IPv6 组播路由，让订阅者跨网段接收组播流量。
- [ ] **转发配置变更**。管理员可调整转发选项。链路、路由或策略变化时，受影响的流量按最新配置切换或停止转发。
- [ ] **转发负载管理**。可指定转发专用 CPU。高负载可利用多核，空闲时减少 CPU 占用，未配置专用 CPU 时保持桌面响应。

### BGP 与 OSPF

- [ ] **按 BGP 邻居配置路由策略**。可为每个邻居分别设置接收和发布策略，筛选路由并改写路由属性。[BGP](../architecture-contract/bgp.md#尚未接入的行为)
- [ ] **BGP 同网段多路径发布**。可向同一邻居发布同一网段的多条路径。本期是否交付待确认。[BGP](../architecture-contract/bgp.md)
- [ ] **OSPF 动态路由**。可配置 OSPFv2/v3 邻居与区域，发布和学习双栈路由。多区域拓扑变化或与其他来源路由并存时，实际转发随选路结果更新。

### 连接跟踪、NAT 与防火墙

- [ ] **地址转换**。可配置源地址转换、目的地址转换和随出口地址变化的转换规则。出口端口用尽时，系统拒绝建立新的映射。
- [ ] **防火墙规则管理**。可将规则分组并跳转到其他规则组。
- [ ] **有状态防火墙**。可按连接状态过滤分片流量，为已建立的连接配置双向放行、丢弃和地址转换，并决定新规则是否影响已有连接。

### 网桥、VLAN 与 RSTP

- [ ] **网桥与 VLAN**。可将多个端口接入网桥，让同一 802.1Q VLAN 内的设备互通，并按 VLAN 或端口隔离流量。
  网桥可在允许的端口转发目的位置未知的报文，并在设备换端口或离线后更新转发路径。
- [ ] **二层环路防护**。网桥默认启用 RSTP，可防止环路风暴，拓扑变化后更新转发并切换备用链路。MSTP 不属本期。

### 配置、管理与诊断

- [ ] **统一配置与变更通知**。管理员可在同一入口编辑、校验和审阅全部路由设置，一次提交多项修改并接收变更通知。提交失败或重连后可查询提交结果。
- [ ] **完整的配置保存与恢复**。全部路由设置均支持开机加载、导出、导入、对比、定时备份和恢复。升级后仍可加载旧版本配置。
- [ ] **本地及远程管理**。可按账号和组授予查看或修改权限，通过加密连接远程管理，断线重连后看到当前状态。其他管理会话响应较慢时仍可正常操作。
- [ ] **图形化管理**。桌面管理器提供经典菜单树、列表和属性对话框。管理器及控制面板网络页均可完成配置和备份操作，列表实时更新。
- [ ] **网络诊断**。可查看实时流量分解，使用 ping、traceroute、带宽测试与抓包诊断实际收发。

## Done（已有功能）

以下列出已有的可用能力。整机验收进度见[系统验收](#系统验收)。

### 地址分发与 DNS

- **DNS 转发与缓存**。可为下游客户端提供域名解析，并自动分发 DNS 地址。设置修改后生效，开机时自动加载。[DNS 转发](../architecture-contract/dns-forwarding.md)
- **IPv6 下游静态配置**。下游设备可自动获得管理员指定的 IPv6 前缀、默认网关和 DNS。配置修改或停用后，旧值会撤回。[下游路由通告](../architecture-contract/ipv6-link.md#下游路由通告)
- **DHCPv4 地址分发**。可为各下游配置地址池、租期、网关和 DNS，按接口查询租约。客户端可取得和续订地址，分配时默认避开已占用地址。[地址分发契约](../architecture-contract/ipc-network.md#dhcpv4-下游地址分发)
- **DHCPv6 前缀委派**。可让下游客户端使用上联分配的 IPv6 前缀，前缀到期或停用后撤回。可对服务器发来的重新配置请求启用认证。[委派会话](../architecture-contract/ipc-network.md#dhcpv6-前缀委派会话)

### 双栈主机网络

- **接口配置**。设置 IPv4 和多个静态 IPv6 地址、DNS、搜索域、MTU 与 IPv4 默认路由。IPv4 可续租并在失败后重试。[路由配置](../architecture-contract/router-configuration.md)
- **IPv6 主机网络**。可与本机、同链路和静态网关后的设备通信。接口可自动取得链路本地地址并检测地址冲突。
  可按接口选择是否从通告取得地址、路由、DNS 和链路参数，查看地址寿命和邻居状态。[IPv6 链路](../architecture-contract/ipv6-link.md)
- **双栈邻居查询**。应用可按接口和地址查询 IPv4、IPv6 邻居及其可达状态。[网络查询](../architecture-contract/packet-netlink.md)
- **双栈 TCP**。应用可通过 IPv4 或 IPv6 建立连接，一个监听端口可接收两族连接。应用停止发送后仍可接收。连接可适应路径 MTU 变化，并在部分丢包时快速恢复传输。[TCP 连接](../architecture-contract/socket-transport.md#tcp-连接)
- **原始报文**。获授权程序可收发双栈原始报文并筛选 ICMP。[原始 IP](../architecture-contract/raw-ip.md)
- **应用组播**。UDP 与原始 IP 可按接口订阅组播并控制发送和本机回送。[IPv4](../architecture-contract/ipv4-transports.md#ipv4-组播监听)、[IPv6](../architecture-contract/ipv6-link.md#组播监听)
- **双栈大报文**。应用可查询路径允许的报文大小、控制发送分片并接收分片报文。[分片接收](../architecture-contract/ip-reassembly.md)、[IPv4 传输](../architecture-contract/ipv4-transports.md)、[IPv6 链路](../architecture-contract/ipv6-link.md)
- **双栈数据报错误**。UDP 与原始 IP 可查询已报告的发送错误及来源。[错误反馈](../architecture-contract/socket-transport.md#数据报错误队列)

### 多网卡与路由

- **静态路由**。可配置及查询 IPv4 和 IPv6 路由，指定直连或经其他路由可达的网关，为多个出口设置权重，或丢弃及拒绝匹配流量。[路由配置](../architecture-contract/router-configuration.md)、[查询](../architecture-contract/packet-netlink.md)
- **主路由表准入**。可按路由来源和前缀决定是否采用 IPv4 和 IPv6 路由，并调整采用时的优先级。[路由准入](../architecture-contract/router-configuration.md#路由准入策略)
- **双栈路径查询**。可用 `ip route get` 查询本机发送或指定入接口的报文使用的路由表、出口和网关。[网络查询](../architecture-contract/packet-netlink.md)
- **双栈选路规则查询**。管理员可查看规则的生效顺序、匹配条件和目标路由表。[策略选路](../architecture-contract/ip-forwarding.md#策略选路)
- **IPv4 经 IPv6 网关**。IPv4 静态路由可指定直连 IPv6 网关，也可经其他 IPv6 路由选出出口。出口可只配置 IPv6。
  IPv6 路径切换或撤回时，依赖它的 IPv4 路由随之更新，可用 `ip route` 查询实际网关。[静态路由配置](../architecture-contract/router-configuration.md#静态路由配置)
- **双栈主备网关切换**。可配置主备静态网关。主网关失联时，受影响的流量可改走备用出口，主网关恢复后可自动切回。[递归选路](../architecture-contract/ipc-network.md#rib-候选目录)
- **双栈跨网卡转发**。独立启停两族单播转发并保存设置，可报告跳数和 MTU 错误；关闭转发后仍可访问本机服务。[跨网卡转发](../architecture-contract/ip-forwarding.md)
- **按流量来源选路**。管理员可按源网段为本机和转发流量选路，也可按转发流量的入接口选路。发往同一目的地的流量可使用不同出口，未匹配到可用路由时使用主路由表。[策略选路](../architecture-contract/ip-forwarding.md#策略选路)
- **按流量标记选路**。获授权的应用可标记本机发出的流量，管理员可按入接口和目的网段标记转发流量。
  两种流量都可按标记选路，管理员可查询指定标记使用的路径。[策略选路](../architecture-contract/ip-forwarding.md#策略选路)、[入站规则](../architecture-contract/ip-forwarding.md#预路由规则)

### BGP 与 OSPF

- **BGP 邻居**。配置、启停和查询双栈邻居，支持 TCP MD5 认证，连接不相容时可查看原因。[BGP](../architecture-contract/bgp.md)、[TCP 认证](../architecture-contract/tcp-authentication.md)
- **BGP 路由学习与发布**。双栈路由可按网段及属性筛选和修改，可查询采用结果；可发布、撤回和重发本机及指定来源路由，支持路由反射和 AS 联盟。[BGP](../architecture-contract/bgp.md)、[AS 联盟](../architecture-contract/bgp-confederation.md)
- **BGP 跨地址族下一跳**。可与支持该能力的邻居交换使用 IPv6 下一跳的 IPv4 单播路由。IPv4 流量可经 IPv6 网关转发，路由撤回后停止使用失效出口。[BGP 下一跳](../architecture-contract/bgp-next-hop.md)
- **BGP 路由自动恢复**。网关恢复可达后，系统可自动恢复此前因该网关失联而停用的 BGP 路由，即使期间没有业务流量或新的路由发布。[BGP 路由可达性](../architecture-contract/router-configuration.md#来源事务的安装后可达检查)
- **BGP 路由刷新**。管理员和邻居均可在保持连接的情况下请求重发 IPv4 或 IPv6 路由。双方支持增强刷新时，可清除对端已停止发布的路由。[路由刷新](../architecture-contract/bgp.md#邻居请求路由重发)

### 连接跟踪、NAT 与防火墙

- **双栈入站转发过滤**。管理员可按入接口和目的网段设置放行或丢弃规则，并按顺序配置例外。规则可与报文赋标一起保存、替换和撤销。[预路由规则](../architecture-contract/ip-forwarding.md#预路由规则)
- **查看防火墙命中次数**。管理员可按配置顺序查看每条预路由规则命中的报文数和 IP 字节数，识别未命中或因设备缺席而未生效的规则。[规则计数](../architecture-contract/ip-forwarding.md#查询防火墙命中次数)
- **复用双栈目的地址集**。管理员可让多条规则引用同一组 IPv4、IPv6 网段。修改成员后，引用规则一起生效，原有命中次数继续累计。[地址集](../architecture-contract/ip-forwarding.md#复用目的地址集)
- **按连接状态过滤**。管理员可针对双栈 TCP、UDP 和 ICMP Echo 分别过滤新连接、已有连接和相关的 ICMP 错误。连接关闭或空闲超时后，流量按当前连接状态重新判断。[转发连接状态](../architecture-contract/ip-forwarding.md#转发连接状态)
- **查询转发连接**。管理员可分批查看当前跟踪的双栈 TCP、UDP 和 ICMP Echo 连接、双方地址、状态和剩余空闲时间。[连接查询](../architecture-contract/ip-forwarding.md#查询转发连接)
- **清除转发连接**。管理员可清除列表中选定的一条连接，取消尚未发出的相关报文。旧列表中的标识不会影响后来建立的连接。[连接清除](../architecture-contract/ip-forwarding.md#清除转发连接)
- **实时跟踪转发连接**。管理员可查看当前连接，并持续接收双栈 TCP、UDP 和 ICMP Echo 连接的新增、状态变化和移除通知。来不及接收时重新加载当前列表。[实时跟踪](../architecture-contract/ip-forwarding.md#实时跟踪转发连接)

### 配置、管理与诊断

- **配置保存与备份**。保存、校验、对比、导入和导出当前配置。可定时备份，设置保留策略，查询、创建、导出、删除及恢复备份。[路由配置](../architecture-contract/router-configuration.md)
