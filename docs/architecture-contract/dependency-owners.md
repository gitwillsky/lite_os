# 领域依赖说明

精确的 kernel DAG 由[全局契约](../architecture-contract.md)维护。此处解释内核机制与服务之间的依赖方向。

## 内核机制

- `arch` 拥有 ISA、执行上下文、trap 编解码、页表编码、ASID、MMIO 与 IOMMU 寄存器机制；只经编译期 façade 选择后端。
- `platform` 拥有 machine/firmware、DTB 中的 CPU/内存/中断控制器/串口事实、早期串口、GIC/IMSIC 与跨 CPU 同步。它不枚举通用总线或装配用户态驱动。
- `execution::entry` 只解码 boot/trap raw ABI；`execution::cpu` 独占 hardware→logical identity 与 CPU 生命周期。
- `execution::sync` 拥有内核 IRQ-safe/可睡眠同步；`execution::deferred` 合并内核延后工作；`execution::timer` 提供单调时钟与本地 timer。它们不拥有服务的工作队列。
- `serial` 独占串口 RX ring；platform 只发布字节，native console 读取它。`log` 独占日志输出与 ring，不作为串口输入的中转 owner。
- `memory` 只拥有 frame、地址类型和堆分配。`vm` 拥有 VMO、VMAR、页表帧、translation commit 与 pager 请求，不理解进程映像、文件、VMA 或 errno。
- `iommu` 独占 domain、IOVA 与固定页生命周期；`arch::dma` 提供具体翻译硬件。泛型 `DmaFrame` 由上层 frame owner 实现，后端不反向依赖 VM、对象或 platform。
- `thread` 独占 native 线程、调度状态、CPU 时间、等待 membership 和上下文切换。`object` 拥有句柄、Job/Process、channel/port/event、pager、resource/BTI 等原生对象。
- `syscall` 解码 native ABI、user-copy 和对象操作；`bootstrap` 从 bootfs 载入唯一 component-manager。`main` 只装配，`crash` 只处理内核 fail-stop 与持久现场。

## 用户态服务

| Owner | 独占事实 | 依赖与边界 |
|---|---|---|
| `system/component-manager` | 服务启动顺序、监督策略、设备汇聚目录、宿主 Job 与管理器交接决策、能力路由 | native-runtime、launch-protocol、service-directory、device-protocol；不执行 Linux init，不解释设备协议内容 |
| `system/driver-manager` | 根 resource、DTB/PCI 枚举与 BAR 分配、IOMMU/BTI、节点匹配、驱动宿主监督、向根设备目录发布 | resource/IOMMU/BTI native 对象；只读设备身份与 PCI 配置，不运行驱动 |
| `system/driver-host` | 一个节点的驱动（VirtIO、xHCI 及其 UAC2 子设备）、IRQ 完成线程、DMA、GPU 宿主的启动画面、本宿主设备目录与会话回收 | 节点 MMIO/IRQ resource 与 BTI；经 device-protocol 服务设备目录 |
| `system/fshost` | 块设备会话、每盘 ext4 实例、原生数据卷挂载、各服务私有目录 | device-protocol、filesystem-protocol；ext4 的唯一启动者，不解释 Linux 挂载或块设备语义 |
| `system/ext4` | ext4 inode、extent、journal、普通文件的内容 VMO（补页、直写同步、回写）、同一文件系统会话的串行化 | device-protocol、pager-pool、service-rpc 弹性消息分派与 FUSE 服务；不拥有调用者凭据、Linux FD 或进程权限 |
| `system/audio-core` | PCM 输出会话、混音引擎、主音量、独占格式切换、USB 声卡热插拔 | device-protocol、audio-protocol；不拥有 ALSA 状态、子流编号或 Linux ioctl |
| `system/pager-pool` | 进程内 native pager、port、key 注册与可增长补页线程池 | native-runtime；补页政策由各内容 owner 的 `PageSource` 决定 |
| `system/netstack` | 网络域、双栈 FIB/地址/端口、转发过滤与连接状态、原生 TCP/UDP/RAW 与链路、AF_PACKET、执行事务 | device-protocol、network-protocol 与 router-protocol 执行描述；不发现网卡、不决定接口配置，不拥有 Linux syscall、信号或阻塞超时 |
| `system/router-config` | 网卡发现与安装授权、配置树与提交决定、原文件撤销、执行重试及 BGP 域寿命 | network-protocol、device-protocol、filesystem-protocol、router-protocol；不拥有协议栈状态 |
| `system/routing-rib` | 候选与解析；授权来源会话、接口/静态意图与统一来源确认 | 候选库复用 prefix-index；原生服务经 network-protocol 委托 NET 执行，不持 FIB 前像 |
| `system/bgp` | 原生 TCP 会话、接收路由与状态；纯库拥有协议裁决、选路、来源及出口链路资格投影 | 库依赖 packet-wire/router-protocol/fallible-tree/alloc；bin 从 NET 投影资格并装配原生服务，不持持久配置；只接管显式委托来源，见[BGP 契约](bgp.md) |
| `system/dhcp-client` | DHCPv4 地址和 DHCPv6 委派租约 | 通过 network-protocol 收发；租约交给 router-config 应用 |
| `system/dhcp-server` | 下游 DHCPv4 地址池、客户端身份和租约截止时间 | 只接受 router-config 的会话请求，通过绑定接口的 packet socket 收发 |
| `system/dns-cache` | 原生 DNS 监听、上游事务及回复缓存 | 核心依赖 packet-wire 和 fallible-tree，执行器只接受路由配置能力，经 network-protocol 收发。见 [DNS 转发契约](dns-forwarding.md) |
| `system/linuxd` | Linux ABI、进程/线程/namespace、安全策略、VFS、tmpfs 与 Linux FUSE 守护进程的页缓存、设备 UAPI（含 ALSA 状态机）、Linux IPC、授权端点适配 | restricted mode 执行应用；只消费路由给它的设备类与协议 |

`native-runtime` 封装原生对象和服务线程；`service-sync` 提供服务可睡眠同步；`service-rpc` 提供服务 RPC。
`service-rpc` 的弹性池把控制、可选共享数据面 Event、停止通知统一汇入同一 port；具体共享区格式、credits、DMA/flush 与完成仍属于设备协议。
纯控制积压直接读 channel，不逐条往返 port；有活动源时按 port 来源顺序处理，避免控制流饿死数据。
活动接口接受非空 Event 集合及接收源索引的非阻塞数据处理器，至少两个工作者；控制来源在 handler 返回后才重武装，最多一个耗时控制。
clear→arm→drain 保留 Event 在注册列表中的索引，不能在通用池中丢掉来源而迫使设备扫描所有队列；同一队列的多个来源由该领域归并。
请求与设备完成直接接入同一 reactor，不新增通知桥接线程。
每个 worker 创建前以独立停止句柄预留一份异步 packet；停止 Event 无分配广播，空闲收缩关闭该句柄自动退订。
source/rearm 失败先停止领取、唤醒全部读者，在途 handler 全部退出后才释放会话闭包；不靠定时轮询检查停止。
仅多余空闲读者使用退出截止，最后一个空闲读者永久等真实通知；忙线程不算读者，不能在仍有阻塞工作时收缩掉唯一接收者。
该 port 与停止 Event 是每个会话一次性准备的控制资源，没有另一组转发线程或内核工作队列。
`device-model` 定义设备类词汇与 backing；各 `*-protocol` crate 拥有服务消息和客户端/服务端适配。
它们是系统服务接口；Linux 应用的内核入口只使用 Linux UAPI。服务不得直接链接另一个服务的 binary 源码。
块存储的 BlockDevice 是同步消费视图；BlockProvider/BlockIoQueue 是实际驱动及范围装配的非阻塞提交/完成能力。
设备协议只向该 provider 发布设备，不能以默认阻塞实现伪造异步队列；缓冲保活、准入取消和退出排空由实际队列拥有。
`task-observer-protocol` 是授予 adm 组系统工具的只读任务观测消息与端点 owner；仅定义数据，不依赖 native ABI、Linuxd 或 UI。
原生权限、授权与观测契约见[执行契约](execution.md#原生任务观测)。
`router-protocol` 是原生配置类型/语义校验/JSON Schema 元数据、网络设置与 DHCP 租约编码、原生接口执行描述及 `/dev/network-settings` 帧格式的 owner；同样只定义数据，
router-config、netstack、dhcp-client、dhcp-server、dns-cache、linuxd 适配层与 Linux 侧的 `network-config` 共用各自所需类型。
BGP 联盟身份、成员集合与 PeerKind 由 router-protocol 拥有，运行时消费冻结关系，不从报文推断信任边界；见[联盟契约](bgp-confederation.md)。
AS 路径表达式是模型拥有的已验证不可变值，纯编译依赖固定 regex-automata，不持会话、执行器或匹配缓存；序列化仍只有源字符串，见[筛选契约](router-configuration.md#bgp-as-路径筛选)。
`router-config::policy` 只拥有 bootfs 默认策略与历史文件导入，不再定义第二套持久设置类型；Linux DTO 是当前执行/桌面投影，不是原生配置身份。
原生列表不采用 libc 的容量；resolvd 独自负责 musl 的 DNS/搜索行投影限制。
接口事实与地址列表分开：固定 InterfaceSnapshot 服务收发路径，namespace::addresses 拥有冷查询的 AddressSnapshot；IPv6 地址资格仍由 netstack::inet::ipv6::address 拥有。
静态地址意图由 router-protocol::configuration 提供，interface 的借用视图仅用于配置预备；NET 拥有实际地址/共享组及本机/直连索引，事务不复制 DAD/监听协议状态。
network-protocol::routes 统一拥有两族的类型化路由/下一跳字段与编解码；sealed RouteAddress 只定义标准地址类型的静态宽度/族/字段编码，不拥有选路、权限或设备状态。
IPv4 执行及 Linux 投影显式使用 IPv4 类型；IPv6 gateway/RIB 执行仍须接通，不能把 codec 的表达能力当作实际功能。
原生执行事务仅由 network-protocol 的安装授权派生，不能从 Linux 网络域 channel 获取；所有事务状态与逆向资源只属于 netstack。
router-config 的 transaction 拥有持久决定与未确认工作，staging 持有一份整批执行能力和 DHCP 保留/替换操作；
MTU 期望值只属于原生配置树，编译时与 IPv4 投影成对传递，不进入另一份接口缓存；netstack 事务只保存原 MTU 前像，设备/协议能力由同一接口发布入口更新；
administration 只解码授权管理请求与生成审阅结果，所有修改仍交给唯一配置提交。原文件补偿沿 filesystem-protocol 的同一 StorageService 能力执行；
Snapshot 的提交状态只从实际工作投影。Storage 远程 client 与 fshost provider 使用同一 read/write/create/remove/sync/list 契约，不另造配置存储后端。
network-config 的 configuration 只负责 Linux 客户端，linuxd 只翻译端点与传输；只读重连重放不能扩展为未知结果修改的自动重试。
router-config 的 backup 拥有自动调度/归档规则，filesystem-protocol 只提供文件事实与原子创建，不解释备份名。
配置树以不可变 Arc 发布，导出/备份只保活一致快照；新 Arc/通知在提交副作用前准备，持久决定后只替换权威指针。
自动备份在同服务的独立后台线程做 I/O，不占用设备/租约 Port，不持配置锁等待存储；其短状态锁不反向取得配置锁。
配置管理的 Request/Reply 元数据属于 router-protocol::administration；Linuxd 的 network_settings::configuration 只捕获 fd/转交 VMO，内容生命周期仍属文件/内存 owner。
RegularFile 拥有文件到独立 VMO 的捕获，驱动更新与配置传输共用；endpoint 只沿标准 UnixRights graph 传递/退休文件能力，不引入另一个服务或数据缓存。
`network-protocol::namespace` 持有网络域创建/操作的 trait 与请求分派，根 re-export 保持公共接口不变；
`installer` 派生设备/执行事务能力，`configuration` 持有事务控制及复合批次消息，`bgp` 持有配置工厂与域状态能力；
通用结果和地址字节机制仍由 crate 根持有。routes::snapshot 拥有冻结接口名/共享组/行的正文、引用校验及借用视图；
namespace 只通过 VMO 传输该正文，不复制另一套路由模型。Linux rtnetlink 拥有 UAPI 权重/长度边界及每组不可变派生布局，/proc 拥有首路径投影。
service-rpc::Writer 的 from_bytes/into_bytes 仅转移编码存储所有权，前缀 framing 由领域 codec 保证，不解释内容或新增缓存。

## 服务拓扑与能力路由

线程模型：等待多个来源（子进程终止、目录变化、路由连接）的监督者与路由器在单线程 `native_runtime::executor`
上以 async 任务推进（component-manager、driver-manager），或以单个 port 事件循环推进（router-config）；任务内只做短的
同步 RPC，不在任务内等待设备 I/O。设备、文件系统与协议服务的阻塞工作保持每连接（会话）一个服务线程，
音频混音等实时工作保留独立线程；router-config 自动备份另有一个后台线程执行存储 I/O，持不可变配置快照，不占用设备/租约事件循环。
缺失该划分时一个慢来源会阻塞同一监督者的其他来源，或把阻塞 I/O 带进 executor。

1. 每个原生服务启动时得到 outgoing 服务目录的服务端（`service_directory`），按协议名接受连接；component-manager 持有各
   outgoing 的客户端，并为每个消费者服务一条路由目录：只转发该消费者声明使用的协议（component-manager 的 `ROUTES` 与各
   `*_USES` 表，对应 Fuchsia 的 `expose`/`use`），未声明的协议返回 `ERR_ACCESS_DENIED`。
2. 路由给 linuxd 的协议：`liteos.network.Control`、`liteos.storage.Blocks`、`liteos.filesystem.Ext4`、
   `liteos.network.Settings`、`liteos.audio.Core`、`liteos.driver.Update`；给 router-config：`liteos.network.Control`、`liteos.routing.Configuration`、
   `liteos.network.Dhcp`、`liteos.network.DhcpServer`、`liteos.network.DnsCache`、`liteos.bgp.Configuration`；给 routing-rib：`liteos.network.Control`、`liteos.network.Installer`；
   给 dhcp-client、dhcp-server、dns-cache 与 bgp：`liteos.network.Control`。
   `liteos.storage.Admin` 只供 component-manager 派生私有目录；配置来源能力不能直接提交 FIB、下一跳更新或任意来源身份。
3. 设备目录按类派生：Network → router-config，Block → fshost，Pcm → audio-core，其余 → linuxd；派生目录不能扩大范围。
4. 监督：netstack、routing-rib、bgp、dhcp-client、dhcp-server、dns-cache、router-config、audio-core、linuxd 终止后重启（linuxd 先终止整个 Linux Job 并等它结束），
   10 秒内 5 次判定为崩溃循环并关机（`native_runtime::task::CrashLoop`）；driver-manager、fshost 是关键组件，
   当前实例意外终止即关机。被依赖的服务重启期间，component-manager 让新的连接请求等待新实例就绪；消费者在 `PEER_CLOSED` 后重连。
   driver-manager 以同一判定监督各驱动宿主，fshost 监督 ext4 实例（见 6、7）。
5. 重启后的状态恢复由各使用者负责：NET 退出时 RIB 同时丢弃旧接口来源，router-config 重新安装网卡并重新配置；单独 RIB 退出只重放持久决定，不重复安装存活网卡。linuxd 为存活网络域重建协议实例，旧 socket
   以 `ENETDOWN` 失败；默认网络随 netstack 存续，linuxd 重启不影响接口配置。audio-core 重启后 linuxd 重连新实例：
   旧声卡的渲染流随旧实例关闭而断开，新实例的输出作为新卡登记（与声卡重新插入一致）。
   NET 的安装登记只重交实际接口的寿命能力；router-config 单独重启不会再次打开已安装硬件。登记不拥有接口地址、链路状态或 FIB 副本。
6. 驱动框架（Fuchsia DFv2）：driver-manager 拥有根 resource、DTB/PCI 平台事实与节点拓扑，为每个节点在独立 Job 中
   启动一个 `driver-host`，只交付该节点的 MMIO/IRQ resource 与 BTI。根服务持有宿主 Job、汇聚目录和管理器交接决策。
   管理器持有节点与宿主监督状态，通过独立的发布 capability 登记转发条目。重启、换版与会话边界见
   [设备目录与换版](devices-terminal.md#设备目录与会话)。
7. fshost 为每块盘启动一个 ext4 实例；同盘再次挂载交给同一实例，实例按超级块 UUID 串行化会话。原生数据卷独占
   一个实例，其终止使 fshost 失败停机。

FUSE 线格式属于 ext4 的服务适配边界，不把 Linux 凭据策略带进 ext4 引擎。ext4 实例拥有普通文件的内容
（每文件一个 pager VMO），linuxd 经挂载控制取得它做 mmap；PCM 设备与混音属于 audio-core，linuxd 只翻译 ALSA。

`libraries/packet-wire` 只拥有无状态报文字节算法与借用视图，不依赖 allocator、服务或原生对象。
其 batch 阶段只填写调用者提供的视图槽；netstack::inet::device 拥有帧领取期间的短寿命 IngressFacts，
Ethernet 与 loopback 的路由共用该投影，不能把未校验 checksum 的格式事实当成协议接收决定。
当前格式与错误边界见[报文格式契约](ipc-network.md#报文格式)；DHCP 客户端不再依赖 smoltcp。packet-wire::bgp 拥有消息/OPEN/UPDATE/NLRI 的无状态边界，不拥有会话或 RIB，见[BGP 契约](bgp.md)。
`libraries/tcp` 拥有独立于设备/服务的连接、流缓冲与无状态 MAC；依赖 packet-wire、managed、alloc 及固定 MD5/subtle/zeroize，不持有 NET、接口、凭据或配置；密钥策略归 NET，见 [TCP 认证](tcp-authentication.md)。
调用者提供同次冻结的路径、时钟与初始序号；输出完成后才推进流序号。IPv4/IPv6 已接入产品 NET，端口与接口范围由同一 NET owner 约束；边界见 [TCP 契约](ipv4-transports.md#tcp-连接机制)。
DHCPv6 与 ND 共用非压缩 DNS 名称格式机制；icmp 唯一拥有两族共享的错误引用/扩展结构，icmpv4/v6 各自拥有类型、长度单位与校验和边界；租约、PD/RA、错误投递与 resolver 状态不进入字节库。
Netstack 的设备 adapter 消费校验和机制构造伪头 seed，driver-host 在独占 RX claim 内补全部分校验和；
硬件能力与缓冲寿命仍归设备 owner，路由/传输状态仍归 netstack。
libraries/prefix-index 唯一拥有 Poptrie 机制，依赖只到 alloc；FIB/RIB 分别拥有索引实例和所指候选数组，不共享可变前缀状态。
system/routing-rib 同时提供候选库与原生服务：Source 拥有不可变来源头部（身份/授权代价比较范围/元数据）、声明及共享规范字典引用，Catalogue 只保活来源并索引八字节行引用。
候选机制依赖 prefix-index、router-protocol 的无运行时词汇及 fallible-tree；服务装配额外依赖 native-runtime、launch-protocol、service-directory 和 service-rpc。
router-config::leases 拥有 DHCP 会话与租约事件的地址发布；默认路由属性借唯一持久配置投影，不保存会话创建时的政策副本。
router-protocol::configuration 同时拥有静态 IPv4 持久模型及来源投影；实际 DeviceMatch→ifindex 绑定由配置服务借当前接口提供，不持久化运行时身份。
sources::Routes 以（身份、族）协调声明，Facts 只冻结本轮接口/地址；配置覆盖不猜地址出处。
router-protocol::routing 唯一拥有双栈来源正文及借用验证；routing-rib::declaration 接管已验证字典、规范路径/权重，再交给原 Source 捕获，不复制另一套 codec 或逐路由成员。
resolution::forwarding 额外依赖 network-protocol 和 native-abi 的边界状态，负责结果到原生执行描述的冷投影；network-protocol 拥有唯一流式字段 codec 和 VMO 传输，RIB 不复制 framing 或执行状态。
router-protocol::policy 拥有有序准入模型/元数据与固定字段 codec；routing-rib::policy 只拥有编译动作/分桶索引和本轮属性投影，不依赖接口/会话 owner。
sources 持原始声明、确认后的编译程序与同代解析快照，传入可信来源标记；身份/元数据/路径定义按来源不可变共享，拒绝声明不被销毁，变换不扩大来源执行授权。
resolution 独占冻结选定行、递归工作栈/本批 memo、重选暂退/依赖位图与共享结果，接口可用性只借用调用者冻结事实。
sources::Snapshot 持有同批冻结接口投影；resolution::installation 只在该图上检查候选覆盖后的可达路径，wire 与事务检查扩展分别归 router-protocol::tracking::installation、network-protocol::routing::installation。
来源事务的 evaluate 复用原 candidate 锁/能力，不持第二份执行状态；普通来源观察仍不等于安装准入，见[安装后检查](router-configuration.md#来源事务的安装后可达检查)。
接口与独立来源共用 transaction 的预备/激活/确认/撤销，来源可观察同一候选图；授权和退出撤回归 source，已关闭来源不能被派生事务晚到确认复活。
接口/载波变化由 NET Event 直达 RIB。RIB 主线程只拥有未完成重算工作，配置服务不再镜像路由重试状态。
观察源只在执行域 NIC 发布前装配；逻辑变更在 NET 内通知，硬件回调只捕获 Event，不反向保活域；来源关闭标记登记并 signal，原 Port 确认后撤回。
router-protocol::tracking 拥有查询/证明/立即出口及 codec；resolution::tracking 无分配借用同一选择图，sources::Snapshot 只保活该图，network-protocol 拥有只读能力/VMO 传输，原生 tracking actor 独占登记与确认后 Event 通知。
跟踪登记不镜像结果或执行状态，不参与报文路径；默认实例两族共享同代已确认图，双栈 BGP 来源已接通；VRF/OSPF 联动仍待实现，见[跟踪契约](router-configuration.md#双栈下一跳跟踪)。
暂态可用性不跨求解缓存，没有执行侧 FIB 副本，见[RIB 候选目录](ipc-network.md#rib-候选目录)与[配置来源事务](router-configuration.md#原生-rib-配置来源)。
route::fib 在 RoutingTable 内共同拥有唯一活动连续记录、prefix-index::Poptrie 实例和 route::nexthop 路径表；前缀区间直接引用同一数组，路由属性不复制进索引。
内联下一跳按完整路径值共享，具名对象按非零配置身份独立，记录只保存所属根内槽号；身份/路径索引、引用数、LINK/HOST 范围约束计数和空槽复用同属一个 owner。
具名零引用定义保留配置寿命，接口配置候选继承定义；route::group 拥有规范成员排序与带权 L3 选择，具名独立更新只交换槽内成员数组，既不复制设备在线状态也不逐前缀同步。
冷准入/读取投影不是常驻状态；Draft 直接构造最终数组，不另存百万条完整解析副本。
整表候选与原根、独立下一跳更新的单槽前像各由同一配置事务持有；发布/撤销按对应粒度交换，设备身份与写租约证明无需正常整表过滤，退休根在 NET guard 外释放。
IPv6 Ipv6Routes 拥有同族前缀/索引，复用类型化 nexthop 字典与组选择；地址候选/逆向根仅冷共享不可变字典，资格始终取实际接口/地址 owner。
RibAddress 静态统一递归键、策略及执行字段映射；Forwarding 仅借用冻结解析并按来源/共享组捕获执行元数据；两族直接编码同一配置，不建立逐前缀副本或独立发布 owner。
邻居失败通过至多八项的轮内完成事实交回 NET，再由 UDP owner 按当前根清理等待者，不增加失败缓存或每流网关状态。
不把前缀索引或路由策略下沉通用 kernel。
NetworkStack 独占实际双栈转发策略，原配置事务交换前像；inet::forward 独占本域转发待发、分片游标和错误限速；只消费唯一 FIB/邻居机制，不保存另一份路由或 ARP/ND，见[转发契约](ip-forwarding.md)。
network-protocol::routes 拥有原路由字段和完整集合 framing，configuration::batch 拥有接口/FIB 的可选组合；netstack::inet::route::admission 是单条与复合批次的唯一字段/设备准入，
route::forwarding 拥有受管来源的整体替换规则。接口和 FIB 集合共用唯一配置写租约、PreparedRouteChange 与 ConfigurationSession 生命周期，
不因新增控制入口建立第二个 FIB owner；routing-rib 负责选路，执行服务不能重复实现递归路由或默认路由意图。

`libraries/pci-config` 仅定义 PCI 标准配置快照、capability/MSI/MSI-X 格式与表项布局，无硬件或服务依赖。
总线枚举、用户驱动的未绑定表项初始化、内核原生 Interrupt 的源控制共用这一个格式 owner；
寄存器访问分别留在持有对应 authority 的执行域，不把 PCI 策略带入通用中断控制器。
Driver-manager 独占枚举时据此撤下固件消息中断并验证 BAR 区间；driver-host 独占节点后用同一 codec 发现 VirtIO 能力。
快照只在冷发现事务中使用，不作为寄存器缓存；中断分配与投递仍归平台/Interrupt 对象。

## 验证职责

结构检查只约束真实 owner、依赖、scoped interface、unsafe/全局状态、ABI 和静态硬件边界。
不再以旧函数名、已删除的 swap 字段或“内核驱动 bootstrap WFI”证明服务行为，也不把源码分支乘固定次数当作实测成本。
资源寿命、并发和协议由实际生产 owner fixture 验证；IPC、restricted mode 与设备硬件由冷启动 guest 验证。
覆盖路由见[构建与验证](../development/build-and-verify.md)。
