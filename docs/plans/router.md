# 计划：桌面上的路由系统（路由、OSPF、BGP 与图形化管理）

Status: Active

文末的 [TODO](#todo待实现功能) 和 [Done](#done已实现功能) 按用户可使用的功能记录进度。[系统验收](#系统验收) 单独记录实际验证范围。计划完成后将持久事实迁入对应领域文档并删除本计划。

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

## 领域文档

- 转发与选路见[双栈转发契约](../architecture-contract/ip-forwarding.md)、[路由选用与下发](../architecture-contract/ipc-network.md#rib-候选目录)和[网桥与 VLAN](../architecture-contract/switching.md)。
- 配置与管理见[路由配置契约](../architecture-contract/router-configuration.md)、[网络管理入口](../architecture-contract/userspace-abi.md#网络管理入口)和[桌面网络管理器](../../user/programs/network-manager/README.md)。

## 系统验收

整机验收待完成，覆盖以下实际使用场景。验证入口见[构建与验证](../development/build-and-verify.md)。

- **已验证**。具名路由域可双向转发 IPv4 和 IPv6，使用共享或专用 CPU 均可。使用多队列网卡时，增加专用 CPU 可提高单个路由域的双栈吞吐。
  [多核吞吐验证](../development/desktop-verification.md#单个路由域的多核吞吐)
- **待验收**。转发期间可打开菜单、移动窗口和调整窗口大小，高负载时响应仍会变慢。仍需确认满载时的桌面响应及网卡重新连接后的双向转发。
  [桌面响应验证](../development/desktop-verification.md#转发负载下的桌面响应)

- **主机与地址服务**。验证静态及动态通告、地址冲突、前缀变更、DNS、租约和认证。验证双栈 socket、并发分片、错误反馈及 TCP 丢包恢复，并与成熟系统互通。
- **路由与转发**。验证多网卡独立启停、邻居解析、多路径、跳数、分片和错误报告。覆盖 BGP/OSPF 与 FRR/BIRD 互通、BGP 反射及联盟，以及邻居、网卡或策略变化后的查询、宣告、收敛与实际转发。
  核对 OSPF 邻居发现、双向通信与失联后的状态变化，再验证同一区域和跨区域的路由学习与转发。
  通过 OSPF 上联访问 LiteOS 的 IPv4 和 IPv6 局域网，核对局域网只通告网段，并验证移除地址或停用链路后的对端路由。
- **路由域隔离**。在两个域中配置相同的地址和网段，验证各域独立选路、转发及认证。设置不同的防火墙和 NAT 规则，核对双向流量与命中计数。
  把应用放入指定域，核对其通信和查询结果，并验证重启后保存的接口归属与路由配置重新生效。
  从桌面创建域、分配网卡、编辑域内设置并提交。核对网卡移回默认域和删除具名域后的设置与转发，验证取消编辑时当前配置保持原样。
  用只读账户在两个具有重叠地址的域之间切换，核对各页只显示所选域的数据；用管理账户清除其中一条连接，确认其他域连接保持有效。
- **二层与安全**。在与家庭业务网隔离的环境验证 VLAN 互通、与第三方网桥的环路抑制和链路切换，以及防火墙与 NAT 的双向行为和规则顺序。
  从桌面配置接入和带标签上联，验证同 VLAN 互通、隔离端口、跨 VLAN 路由，以及撤销改动后的转发路径。
  从桌面修改规则顺序、地址集和组合 NAT，核对命中、两方向流量及已有连接处理。
- **配置与管理**。验证完整桌面操作、权限边界、配置生效时间，以及存储失败、断电后的配置加载与备份恢复。覆盖全部路由功能的审阅、提交、变更通知和备份恢复。
  在桌面一次提交备选表、静态路由和选路规则，确认实际路径随优先级变化，并验证系统拒绝删除仍被引用的表。
  在桌面修改网卡地址和转发开关，确认两族分别生效，重启后按保存的设备绑定加载。
  从桌面配置物理下游与 VLAN，验证客户端取得预期地址、委派前缀、网关和 DNS，并通过配置的解析器查询域名。
- **流量分析与抓包**。验证桌面显示的主要流量、字节占比、近期速率及采集丢包。用 `tcpdump` 实时查看和筛选接口报文，保存后重新读取抓包文件。
- **性能与规模**。以同等 QEMU 配置对比 Linux 和 RouterOS CHR，测量小包及 IMIX 吞吐、NAT/防火墙开销、多核扩展与空闲 CPU。
  覆盖目标路由、连接、规则及列表规模下的收敛、内存和交互响应。使用目标规模的配置验证导入、查找变更、审阅和提交。
  在至少三颗逻辑 CPU 的系统上分配、替换及释放转发专用 CPU，确认桌面仍有可用 CPU、转发继续工作、重启后重新应用。验证提交失败保留上一份配置。
  保持网卡配置和流量条件一致，分别测量多个路由域同时转发和单个路由域内多条连接的吞吐，比较增加转发可用 CPU 前后的结果。
  在共享 CPU 上持续转发时测量桌面输入和窗口调整延迟，随后停止流量，测量空闲 CPU 用量，并检查空闲后首包和协议重传。

## 二期：故障恢复

二期验收故障或重启后的运行状态恢复、恢复时限、连续转发，以及 BGP/OSPF 平滑重启。本期验收配置保存与开机加载，范围见 R12。

## TODO（待实现功能）

- [ ] **在持续转发时流畅操作桌面**。桌面与转发共享 CPU 时，用户可流畅打开菜单、移动窗口和调整窗口大小。

## Done（已实现功能）

### 路由域隔离

- **建立独立的静态路由域**。管理员可导入配置，为具名域分配网卡、设置双栈地址和静态路由，分别启停两族转发。各域可使用重叠地址，设置随系统启动加载。[配置方法](../../user/libraries/network-config/README.md#建立独立静态路由域)
- **为隔离网络设置选路、防火墙和 NAT**。管理员可在各域设置备选路由表、选路规则、路由准入和有状态防火墙及 NAT。获授权的用户可按域查看规则命中数。[配置与查询](../../user/libraries/network-config/README.md#按路由域设置选路和防火墙)
- **在隔离网络使用 BGP**。管理员可为各域配置独立邻居、认证及收发策略，按域查询会话和路由，并请求指定邻居刷新路由。[配置与查询](../../user/libraries/network-config/README.md#在具名路由域配置-bgp)
- **在隔离网络使用 OSPF**。管理员可为各域分别配置 OSPFv2、OSPFv3 和认证，按域查看邻接、认证结果和实际采用的路由。[配置与查询](../../user/libraries/network-config/README.md#在具名路由域配置-ospf)
- **让 Linux 应用使用指定路由域**。管理员可在已配置的域中启动程序。程序可通过标准 Linux 接口通信，并查看域内接口、地址、路由、邻居和选路规则。[启动与查询](../../user/libraries/network-config/README.md#在指定路由域启动应用)
- **在桌面管理独立路由域**。管理员可创建和删除域、跨域分配网卡，并一起审阅和提交多个域的网络设置。获授权的用户可切换并查看各域的运行状态。
  [路由域设置](../../user/programs/network-manager/README.md#独立路由域)、[按域查看状态](../../user/programs/network-manager/README.md#按域查看运行状态)

### 地址分发与 DNS

- **DNS 转发与缓存**。可在物理接口或 VLAN 网关提供双栈域名解析，下游客户端可自动取得解析器地址。设置修改后生效，开机时自动加载。[DNS 转发](../architecture-contract/dns-forwarding.md)
- **IPv6 下游静态配置**。下游设备可自动获得管理员指定的 IPv6 前缀、默认网关和 DNS。配置修改或停用后，旧值会撤回。[下游路由通告](../architecture-contract/ipv6-link.md#下游路由通告)
- **DHCPv4 地址分发**。可为各下游配置地址池、租期、网关和 DNS，按接口查询租约。客户端可取得和续订地址，分配时默认避开已占用地址。[地址分发契约](../architecture-contract/ipc-network.md#dhcpv4-下游地址分发)
- **DHCPv6 前缀委派**。管理员可为物理下游和各 VLAN 分配不同的 IPv6 子网。客户端可自动使用上联提供的前缀，前缀更换、到期或停用时会收到更新。
  可对上联服务器发来的重新配置请求启用认证。[委派会话](../architecture-contract/ipc-network.md#dhcpv6-前缀委派会话)
- **在桌面管理地址分发与 DNS**。管理员可为物理接口及 VLAN 设置 DHCPv4 地址池、IPv6 通告和委派子网，也可配置 DNS 监听、上游和系统搜索域。[地址分发与 DNS](../../user/programs/network-manager/README.md#地址分发与-dns)

### 双栈主机网络

- **查询应用所在的网络**。应用可查看默认网络或自行创建的 Linux 网络命名空间中的接口、双栈地址、路由、邻居和规则，并按接口和地址查询邻居的可达状态。[网络查询](../architecture-contract/packet-netlink.md)
- **接口配置**。设置 IPv4 和多个静态 IPv6 地址、DNS、搜索域、MTU 与 IPv4 默认路由。IPv4 可续租并在失败后重试。[路由配置](../architecture-contract/router-configuration.md)
- **IPv6 主机网络**。可与本机、同链路和静态网关后的设备通信。接口可自动取得链路本地地址并检测地址冲突。
  可按接口选择是否从通告取得地址、路由、DNS 和链路参数，查看地址寿命和邻居状态。[IPv6 链路](../architecture-contract/ipv6-link.md)
- **双栈 TCP 连接**。应用可通过 IPv4 或 IPv6 建立连接，并在同一端口接收两族连接。连接支持单向关闭，并可在路径 MTU 变化或部分丢包后继续传输。[TCP 连接](../architecture-contract/socket-transport.md#tcp-连接)
- **从对端探测 LiteOS 的 IPv4 地址**。对端可以 `ping` LiteOS 的单播地址并收到应答，关闭 IPv4 转发后仍可探测本机。[IPv4 主机网络](../architecture-contract/ipv4-transports.md)
- **为 TCP 连接启用 TCP-AO 认证**。客户端可为对端配置认证密钥，在连接期间轮换密钥并查看认证统计。[TCP 认证](../architecture-contract/tcp-authentication.md)
- **收发原始报文**。获授权程序可收发 IPv4 和 IPv6 原始报文并筛选 ICMP。[原始 IP](../architecture-contract/raw-ip.md)
- **应用组播**。使用 UDP 或原始 IP 的应用可按接口订阅组播、选择组播发送接口，并控制本机是否接收自己发出的组播。[IPv4](../architecture-contract/ipv4-transports.md#ipv4-组播监听)、[IPv6](../architecture-contract/ipv6-link.md#组播监听)
- **收发双栈大报文**。应用可查询路径允许的报文大小、控制是否分片发送，并接收对端发来的分片报文。[分片接收](../architecture-contract/ip-reassembly.md)、[IPv4 传输](../architecture-contract/ipv4-transports.md)、[IPv6 链路](../architecture-contract/ipv6-link.md)
- **定位双栈数据报发送失败**。UDP 与原始 IP 应用可查询报文超长、路由失效和邻居不可达等本机错误。
  收到远端报错时，可查看原目的、报告方、接收接口、本地地址及扩展详情。[错误反馈](../architecture-contract/socket-transport.md#数据报错误队列)

### 多网卡与路由

- **为转发分配专用 CPU**。管理员可在桌面或导入配置时指定、更换或释放转发专用 CPU。多个路由域可同时转发，桌面仍有 CPU 可用。设置在系统启动后继续生效。
  [桌面转发 CPU 设置](../../user/programs/network-manager/README.md#转发专用-cpu)
- **同时转发多个路由域的双栈流量**。在有多颗可用 CPU 的共享模式下，多个路由域可同时转发 IPv4 和 IPv6 流量。[转发行为](../architecture-contract/ip-forwarding.md#转发唤醒与共享-cpu-调度)
- **提高单个路由域的双栈吞吐**。在网卡支持多队列时，管理员可增加转发专用 CPU，提高同一路由域并发转发 IPv4 和 IPv6 流量的总吞吐。[多核吞吐验证](../development/desktop-verification.md#单个路由域的多核吞吐)
- **空闲时降低转发的 CPU 占用**。网络空闲时转发的 CPU 占用自动下降，收到流量后自动恢复转发。[转发行为](../architecture-contract/ip-forwarding.md#转发唤醒与共享-cpu-调度)
- **静态路由**。可配置及查询 IPv4 和 IPv6 路由，指定直连或经其他路由可达的网关，为多个出口设置权重，或丢弃及拒绝匹配流量。[路由配置](../architecture-contract/router-configuration.md)、[查询](../architecture-contract/packet-netlink.md)
- **按来源和前缀选用路由**。管理员可决定哪些 IPv4 和 IPv6 路由进入主路由表，并调整采用时的优先级。[路由准入](../architecture-contract/router-configuration.md#路由准入策略)
- **查询双栈选路结果**。管理员可查看生效的选路规则，并查询本机发送或从指定接口进入的报文将使用的路由表、出口和网关。
  [网络查询](../architecture-contract/packet-netlink.md)、[策略选路](../architecture-contract/ip-forwarding.md#策略选路)
- **IPv4 经 IPv6 网关**。IPv4 静态路由可指定直连 IPv6 网关，也可经其他 IPv6 路由选出出口。出口可只配置 IPv6。
  IPv6 路径切换或撤回时，依赖它的 IPv4 路由随之更新，管理员可查询实际网关。[静态路由配置](../architecture-contract/router-configuration.md#静态路由配置)
- **双栈主备网关切换**。可配置主备静态网关。主网关失联时，受影响的流量可改走备用出口，主网关恢复后可自动切回。[主备网关行为](../architecture-contract/ipc-network.md#rib-候选目录)
- **双栈跨网卡转发**。可分别启停 IPv4 和 IPv6 转发并保存设置，关闭转发后仍可访问本机服务。链路或选路设置变化后，流量切换到可用出口，系统可报告跳数和 MTU 错误。[跨网卡转发](../architecture-contract/ip-forwarding.md)
- **静态双栈组播转发**。管理员可按组及来源指定入接口和出口，让物理网段及 VLAN 中的订阅者接收跨网段的组播。撤销规则后停止跨网段转发，本机订阅仍可接收。[组播转发](../architecture-contract/ip-forwarding.md#静态组播转发)
- **按流量来源选路**。管理员可按源网段为本机和转发流量选路，也可按转发流量的入接口选路。发往同一目的地的流量可使用不同出口，未匹配到可用路由时使用主路由表。[策略选路](../architecture-contract/ip-forwarding.md#策略选路)
- **按流量标记选路**。获授权的应用可标记本机发出的流量，管理员可按入接口和目的网段标记转发流量。
  两种流量都可按标记选路，管理员可查询指定标记使用的路径。[策略选路](../architecture-contract/ip-forwarding.md#策略选路)、[入站规则](../architecture-contract/ip-forwarding.md#预路由规则)
- **在桌面管理路由接口**。管理员可浏览网卡与已保存的配置，修改双栈地址、MTU、管理状态和默认路由优先级，并按地址族启停转发。[路由接口与双栈转发](../../user/programs/network-manager/README.md#路由接口与双栈转发)
- **在桌面管理静态路由**。管理员可在主表和已有备选表内查找、新增、编辑和删除双栈静态路由，管理带权下一跳并审阅提交。[静态路由](../../user/programs/network-manager/README.md#静态路由)
- **在桌面管理选路规则**。管理员可新建或删除备选表，调整选路规则的优先级、匹配条件和目标表，并与静态路由一同提交。[备选表与选路规则](../../user/programs/network-manager/README.md#备选表与选路规则)
- **在桌面查看生效路由**。获授权的用户可实时查看所选路由域的 IPv4 和 IPv6 路由、出口、下一跳与权重。[路由表](../../user/programs/network-manager/README.md#路由表)
- **在桌面查看双栈邻居**。获授权的用户可实时查看所选路由域的邻居、接口、地址与可达状态。[邻居](../../user/programs/network-manager/README.md#邻居)

### 网桥与 VLAN

- **网桥与 VLAN**。管理员可让同一 VLAN 内的设备互通，配置接入口、带标签的上联和端口隔离。设备换端口后，流量会转到新端口。[网桥契约](../architecture-contract/switching.md)
- **二层环路防护**。网桥默认抑制冗余链路造成的环路，主链路断开后可使用备用链路。管理员可指定优先使用的网桥和链路。[生成树](../architecture-contract/switching.md#生成树)
- **静态双栈 VLAN 网关**。管理员可在各 VLAN 配置 IPv4 和 IPv6 网关，查询接口与邻居，并启停 VLAN 之间的双栈转发。[VLAN 网关](../architecture-contract/switching.md#vlan-网关)
- **在 VLAN 自动分发网络设置**。管理员可按 VLAN 设置 IPv4 地址池和静态 IPv6 前缀。设备可自动取得地址、默认网关及 DNS，管理员可按 VLAN 查询 IPv4 租约。[VLAN 网关](../architecture-contract/switching.md#vlan-网关)
- **按 VLAN 接口管理流量**。管理员可指定 VLAN 接口作为静态路由出口，按流量进入的 VLAN 选择路由表或过滤流量，并查询生效路径和规则命中数。[VLAN 网关](../architecture-contract/switching.md#vlan-网关)
- **在桌面管理网桥与 VLAN**。管理员可配置网桥成员、快速生成树、端口隔离、VLAN 标签和双栈网关，与路由和地址分发一起提交。[网桥与 VLAN](../../user/programs/network-manager/README.md#网桥与-vlan)

### BGP

- **管理 BGP 邻居及认证**。管理员可配置、查询和单独启停双栈邻居。停用时保留该邻居的设置，系统重新选择受影响的路由，其余会话继续运行。
  可使用 TCP MD5 或 TCP-AO 并查看连接失败原因。使用 TCP-AO 时可在会话保持期间轮换密钥并查询当前密钥 ID。
  [BGP 配置](../../user/libraries/network-config/README.md#bgp-配置)、[TCP 认证](../architecture-contract/tcp-authentication.md#bgp-配置与操作)
- **按 BGP 邻居筛选和修改路由**。管理员可分别设置从各邻居接收哪些路由、向各邻居发布哪些路由，以及如何调整这些路由的属性。
  [入口策略](../architecture-contract/bgp.md#入口政策反馈选路与发表)、[出口策略](../architecture-contract/bgp.md#出口策略与实际发送)
- **BGP 路由学习与发布**。管理员可查询学习到的双栈路由及选用结果，向邻居发布、撤回和重发本机及指定来源的路由，也可配置路由反射与 AS 联盟。[BGP](../architecture-contract/bgp.md)、[AS 联盟](../architecture-contract/bgp-confederation.md)
- **BGP 跨地址族下一跳**。可与支持该能力的邻居交换使用 IPv6 下一跳的 IPv4 单播路由。IPv4 流量可经 IPv6 网关转发，路由撤回后停止使用失效出口。[BGP 下一跳](../architecture-contract/bgp-next-hop.md)
- **网关恢复后继续使用 BGP 路由**。网关重新可达后，受影响的 BGP 路由自动恢复参与选路。管理员可查询最新的选用结果。[路由表](../../user/programs/network-manager/README.md#路由表)
- **BGP 路由刷新**。管理员和邻居均可在保持连接的情况下请求重发 IPv4 或 IPv6 路由。双方支持增强刷新时，可清除对端已停止发布的路由。[路由刷新](../architecture-contract/bgp.md#邻居请求路由重发)
- **与同一 BGP 邻居交换多条路径**。管理员可按邻居启用双栈多路径收发，查看已发布路径和选路结果。一条路径撤回后，其他合格路径仍可参与选路。[使用说明](../../user/libraries/network-config/README.md#bgp-多路径)
- **在桌面配置 BGP 本机身份和宣告**。管理员可设置本地 AS、路由器 ID、反射簇和 AS 联盟，选择宣告的本机网段和允许重分发的来源。[BGP 设置](../../user/programs/network-manager/README.md#bgp-设置)
- **在桌面管理 BGP 邻居与认证**。管理员可新增、编辑、删除和单独启停双栈邻居，设置地址族与多路径，管理认证密钥并换钥。
  [BGP 设置](../../user/programs/network-manager/README.md#bgp-设置)、[BGP 会话和路由](../../user/programs/network-manager/README.md#bgp-会话和路由)
- **在桌面配置 BGP 路由策略**。管理员可按邻居编辑入口和出口规则、调整顺序和默认动作，决定收发哪些路由及如何调整路由属性，再审阅提交。[BGP 路由策略](../../user/programs/network-manager/README.md#bgp-路由策略)
- **在桌面查看 BGP 会话和路由交换**。获授权的用户可查看所选域的会话、认证和路由计数，按 IPv4 或 IPv6 网段查看收到的路径、选用结果和发布情况。
  管理员可按邻居及地址族请求重发路由。[BGP 会话和路由](../../user/programs/network-manager/README.md#bgp-会话和路由)

### OSPF

- **建立并查看 OSPF 邻接**。管理员可在普通区域的点对点链路启用 OSPFv2 或 OSPFv3，查看邻居发现、同步进度和同步完成状态。邻居失联后会从列表消失。[使用说明](../../user/libraries/network-config/README.md#ospf-邻居发现)
- **使用学到的区域内 OSPF 路由**。管理员可查询系统采用的 IPv4、IPv6 路由及出口。等价路径可同时使用，邻居或链路失效后撤回受影响的路径。[使用说明](../../user/libraries/network-config/README.md#ospf-邻居发现)
- **在 OSPF 区域内发布本机网络**。管理员可让同区域路由器学习 LiteOS 的 IPv4 和 IPv6 直连网段，并将局域网接口设为仅通告。链路或地址撤销后，对端可更新转发路径。[使用说明](../../user/libraries/network-config/README.md#在-ospf-区域内通告本机网络)
- **跨 OSPF 普通区域选路**。管理员可通过骨干连接多个普通区域并交换双栈路由。跨区路径失效后，系统更新转发，管理员可查询选用结果。[使用说明](../../user/libraries/network-config/README.md#跨-ospf-区域选路)
- **启用 OSPF 邻居认证**。管理员可分别为 OSPFv2 和 OSPFv3 要求链路邻居通过认证，查询拒绝原因，并为轮换同时配置旧密钥和新密钥。[使用说明](../../user/libraries/network-config/README.md#ospf-邻居认证)
- **在桌面配置 OSPF**。管理员可按接口启停 OSPFv2 和 OSPFv3，配置区域、代价和仅通告模式，管理认证密钥与换钥，审阅后提交。[OSPF 设置](../../user/programs/network-manager/README.md#ospf-设置)
- **在桌面查看 OSPF 邻居**。获授权的用户可刷新所选域的 OSPFv2 和 OSPFv3 链路，查看邻居地址、同步状态、认证拒绝次数和最近的失败原因。[OSPF 邻居](../../user/programs/network-manager/README.md#ospf-邻居)

### 连接跟踪、NAT 与防火墙

- **管理双栈防火墙规则**。管理员可按入接口及源、目的网段放行或丢弃流量，按顺序设置例外、复用目的地址集和规则组，并查看每条规则的命中次数。
  [规则与计数](../architecture-contract/ip-forwarding.md#预路由规则)、[规则组](../architecture-contract/ip-forwarding.md#调用防火墙子链)
- **按连接状态过滤流量**。管理员可分别过滤新连接、已有连接和关联错误，覆盖双栈 TCP、UDP、ICMP Echo 及其分片。修改规则时可决定是否重新判断已有连接的双向流量。
  [连接状态](../architecture-contract/ip-forwarding.md#转发连接状态)、[策略更新](../architecture-contract/ip-forwarding.md#更新已有连接的策略)
- **通过对外地址发布内网服务**。管理员可将双栈虚拟地址或指定的 TCP、UDP 端口转发给同族后端。内外网客户端均可通过对外地址访问已配置的服务，未转发的接口端口仍可提供本机服务。
  [地址和端口转发](../architecture-contract/ip-forwarding.md#固定目的地址转换)、[内网访问对外地址](../architecture-contract/ip-forwarding.md#组合源和目的转换)
- **让内网设备共用出口**。管理员可为双栈内网流量指定源地址及 TCP、UDP 源端口，或使用出口地址和自动分配的端口。路由器和内网设备可同时访问同一远端服务，回包与关联错误各自送达。
  [固定源地址转换](../architecture-contract/ip-forwarding.md#固定源地址转换)、[自动源地址转换](../architecture-contract/ip-forwarding.md#自动源地址转换)
- **查看和清除转发连接**。管理员可查看双栈 TCP、UDP 和 ICMP Echo 连接的双方地址、状态和剩余空闲时间，订阅变化，或清除指定连接。
  [连接查询](../architecture-contract/ip-forwarding.md#查询转发连接)、[实时跟踪](../architecture-contract/ip-forwarding.md#实时跟踪转发连接)、[连接清除](../architecture-contract/ip-forwarding.md#清除转发连接)
- **在桌面管理防火墙与 NAT**。管理员可编辑匹配条件、动作、地址集和子链，调整规则顺序，并选择新规则如何影响已有连接。[防火墙与 NAT](../../user/programs/network-manager/README.md#防火墙与-nat)
- **在桌面查看防火墙命中统计**。获授权的用户可查看所选域的规则、匹配条件和动作，实时查看命中的报文数和 IP 流量。[防火墙计数](../../user/programs/network-manager/README.md#防火墙计数)
- **在桌面查看转发连接**。获授权的用户可实时查看所选路由域的双栈连接、端点和状态。管理员可清除所选连接。[转发连接](../../user/programs/network-manager/README.md#转发连接)

### 配置与备份

- **通过命令行审阅和提交路由设置**。管理员可导出设置、校验修改、审阅差异和批量提交，断线或重启后仍可查询近期提交结果。[审阅配置](../../user/libraries/network-config/README.md#审阅配置)
- **备份与恢复网络设置**。管理员可在桌面或命令行创建、导出、删除及恢复备份，设置自动备份间隔和保留份数，并查看下次运行时间。
  [桌面](../../user/programs/network-manager/README.md#配置备份)、[命令行](../../user/libraries/network-config/README.md#备份与恢复)
- **升级后继续使用已有网络设置**。升级后可继续使用原有网络设置。管理员也可在桌面或命令行审阅并恢复旧配置文件和较早导出的备份。[备份与恢复](../../user/libraries/network-config/README.md#备份与恢复)
- **在桌面审阅和提交配置草稿**。管理员可导入、编辑和导出草稿，分页查看或查找改动，核对前后差异并提交，随后查看生效状态。[路由配置](../../user/programs/network-manager/README.md#路由配置)
- **本地网络管理**。普通账户可查看接口状态，获授权的观察者可查看路由运行状态，管理员可修改设置。控制面板可调整本机 TCP/IP 属性。[网络管理](../architecture-contract/userspace-abi.md#网络管理入口)
- **远程网络管理**。管理员可在指定地址启用加密管理。获授权的账户可从远端查看运行状态，管理员可审阅、修改及备份配置。[远程管理](../../user/libraries/network-config/README.md#远程管理)

### 网络诊断

- **在桌面查看接口流量**。获授权的用户可实时查看本机接口的收发速率、累计流量与丢包数，并查看所选接口的报文数和错误数。[接口流量](../../user/programs/network-manager/README.md#接口流量)
- **在桌面查看实时流量分布**。获授权的用户可按接口查看主要流量的协议、源和目的、收发速率与字节占比，观察近期变化和采集丢包。[实时流量分析](../../user/programs/network-manager/README.md#实时流量分析)
- **在桌面按接口抓包**。管理员可捕获所选接口的收发报文，停止后查看抓包丢弃数，并用常见分析工具打开保存的文件。[抓包](../../user/programs/network-manager/README.md#抓包)
- **用 tcpdump 实时抓包**。获授权的用户可在命令行按接口查看和筛选报文，也可保存为抓包文件供后续分析。[命令行抓包](../../user/base/README.md#命令行抓包)
- **命令行连通性与逐跳探测**。管理员可用 `ping` 查看目标应答、丢包率和延迟，用 `traceroute` 查看 IPv4 或 IPv6 路径上应答的各跳及往返时间。[使用说明](../../user/base/README.md#命令行网络诊断)
- **桌面网络诊断**。管理员可在控制面板打开网络管理器，探测主机名或双栈地址，查看应答、丢包率、延迟及逐跳结果。可停止探测并保留已收到的结果。[使用说明](../../user/programs/network-manager/README.md)
- **测试到指定对端的带宽**。可按地址和端口测试 TCP 上传或下载，查看逐秒速率和接收端平均速率，亦可在 LiteOS 上启动临时测速对端。[带宽测试](../../user/programs/network-manager/README.md#带宽测试)
