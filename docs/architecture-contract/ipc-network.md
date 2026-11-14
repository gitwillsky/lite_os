# IPC 与网络契约

Linux syscall、阻塞超时、namespace 授权、AF_UNIX/NETLINK 属于 `system/linuxd`；主机网络、FIB、端口与 AF_PACKET 队列属于 `system/netstack`。下述协议 owner 通过 `network-protocol` 交互，不共享内核状态。


## Owner

- `ipc::Pipe` 独占 byte ring、endpoint count、atomicity 与 readiness generation。Fresh/Live/Retired 与 endpoint counts 在同 state 锁线性化；端点锁外预分配，typed open
  一次冻结 prior counts/counters、判 nonblock writer peer requirement，并登记完整方向集合。
  每个 PipeEnd 的 registered AtomicBool 是唯一一次 open capability：未发布 unique Arc 在
  admission guard 内置 true；close/shutdown/fallback Drop 以 AcqRel swap 消费为 false，再在
  Pipe guard 内归还 count。Acquire read/write 拒绝已关闭端点，观察者 Arc 不延长打开阶段。
  最后 close 同锁退休、锁外销毁 ring；增加的 atomic load 纳入既有 Pipe CPU benchmark，阈值不变。
- named/anonymous pipe 共用 FifoFile 实际 I/O 与 poll；只 named 执行 peer rendezvous，只“named+nonblock+read+prior 无writer”保存 HUP suppression baseline，其余读端按真实 writer EOF/HUP。FifoFile allocation 发生在 endpoint admission 前；OOM 不留下半次 RW open。
- 全部 Pipe 共用 task 在 `task::initialize` 安装的唯一 `PipeScheduler`（唤醒与阻塞）；任何领域都
  直接调用 `Pipe::pair`/`Pipe::notification_pair` 创建 endpoint，并只经 `Pipe::wait` 阻塞，禁止恢复
  composition root 注入的 pipe 工厂或领域直接调用 task 的 pipe wait。安装前不存在任何 task，因此
  不存在可唤醒的 waiter 或 poller。
- `ipc::ReceiveBuffer` 独占 kernel receive staging 的 initialized prefix；heap storage 只保留 capacity，backend 只能通过 append 扩展可读取前缀。
- `file::Epoll` 独占 interest、incremental ready membership、ET/ONESHOT 与 nesting state；
  持久 source index 把 Pipe/console edge 精确路由到 interest，OFD reverse index 独占
  final-close detach membership；sharded WaitRegistry 只独占实际 task wait membership。
- AF_UNIX socket、rights graph、NetworkStack、AF_PACKET registry 与 kobject registry
  分别独占各自 namespace、queue 和 protocol state。`NetworkStackOwner` 的
  `TaskMutex<NetworkStackState>` 是 NET protocol state 的唯一 owner；普通 task 竞争时睡眠，
  deferred poll 只 `try_lock`。state 内的 payload-loan count 只证明库 socket 与原生 UDP 队列是否完整，
  不复制 protocol state。
- `NetworkStack.udp_ports/tcp_ports` 分别独占 UDP/TCP local tuple membership；
  `PortLease` 是 endpoint 释放的唯一 capability。per-port/exact-address 索引与 ephemeral
  bitmap 必须同一 transaction 更新，禁止恢复 endpoint 扫描或平行占用表；双族地址空间与生命周期见[端口契约](socket-transport.md#本地端口与地址空间)。
- AF_UNIX rights graph 的 node state 独占 incoming、outgoing 与两者之和的 incident reference
  count；attach/detach 在同一 graph lock transaction 更新三者。detach 只检查 batch 的唯一
  source endpoints 与 target，访问上界为 `unique_sources + 1`；reference 归零才从 topology
  index 精确摘除，禁止全图 retain。

- `SO_ACCEPTCONN` 只由 `sys_getsockopt` 编解码；Socket::is_listening 投影实际 backend 状态。
  AF_UNIX 在已有 endpoint state guard 内读取 Listening，双栈 TCP 在已有 NetworkStack guard 内读取 TcpMode，UDP/RAW/packet/netlink 返回 false。
  新 scoped getter 的 production caller 只有该 syscall 投影及下一层 façade；不增加依赖、缓存、锁对象或可修改状态。

## Socket 服务传输

控制地址、共享队列、双族创建、生命周期与顺序边界由 [Socket 传输契约](socket-transport.md)维护；IPv6 UDP 的回环/同链路范围与完整双栈缺口见[IPv6 契约](ipv6-link.md)。

## 报文格式

`packet-wire` 是报文字节机制的唯一新实现 owner；已进入 DHCP 客户端、AF_PACKET 与 RX 批分类，netstack 状态仍待重写；新增 BGP 格式边界及未实现行为见[BGP 契约](bgp.md)。
格式库只使用 core，无堆分配、锁、全局状态、设备权限或 socket 语义；固定规范与独立对照来源见[基线](../standards-baseline/network.md#报文字节格式)。

1. IPv4 parse 验证 version/IHL/total length、选项 TLV、header checksum 与分片长度，裁去链路 padding；
   parse_header 只检查结构，供批内路由/RST 事实使用，不能作为接收决定或省略协议引擎的头 checksum。
   transport 只能消费完整重组后的 payload。header emitter 先验证长度、选项与分片字段，再原位写 header 和 checksum，保留既有 payload。
   TTL、路由、IP 选项的策略及分片重组状态均不属于 codec；当前 DHCP 客户端明确拒绝尚未重组的分片。
2. UDP view 验证 length 与实际伪头 checksum，允许 IPv4 省略 checksum，IPv6 必须非零；两端地址族须一致。
   emitter 原位构造 header，checksum 的零结果以 0xffff 发送，不复制 payload。IPv6 UDP 超过 65535 时发送零 Length；
   接收零 Length 从完整 IP upper-layer 长度推导（包括 RFC 2675 的无 Jumbo option 情况），IPv4 零 Length 仍非法。
   调用者必须先完成 IP 长度验证、分片重组及 Routing header 最终目的地址解析，不以未验证尾部构造伪头。
3. DHCPv4 在构造 view 时验证固定头、cookie 和启用区域的完整 TLV/END；option overload 仅由主 options 的 52 决定。
   逻辑选项严格按主 options→file→sname 拼接，同一字段可跨任意字节边界；没有首段优先或逐段按地址宽度截断。
   固定字段 getter 校验拼接后的精确宽度，地址列表校验非空及 4 字节整倍数；未知选项保留原始借用。
   builder 保留 END 空间，长选项按 255-byte 片段编码；容量失败不改变已有前缀。最终 overload 验证失败时丢弃未发表报文。
4. DHCP 客户端保留唯一租约状态机，生产编解码全部使用 packet-wire；校验 op/hardware type、事务与客户端地址后才采纳回复。
   DNS 个数沿已有设置契约限制；选项 15 拼接后仍由既有 dns_suffix 校验，不引入另一套名字规则。
   发送只用实际请求字段的编译期容量上界，不再分配堆帧；接收 view 借用 packet socket 给出的字节，生命周期不越过该帧。
5. Ethernet 基础视图只验证 14-byte MAC header，AF_PACKET 保留外层 Length/Type、标签与 padding。直接承载的 IPv4/IPv6 结构事实随原帧借用交付。
   网络视图另行解开标准 C/S tags，按内层 802.3 长度裁剪 padding，拒绝截断或保留的 Length/Type 区间；LLC 数据仍作为 LLC 上送。
   VID 0、保留 VID 与 PCP/DEI 的原值保留，接收、分类、PVID 与端口策略属于 VLAN/bridge，不由 codec 猜测。
   ARP 按 hlen/plen 校验四个地址范围并裁去 padding；硬件类型、协议与 opcode 保持原值，邻居学习/解析/超时仍归协议引擎。
6. PacketBuffer 独占实际存储的有效区间；prepend/append/consume/truncate 只改变边界，不分配、移动或擦除 payload。
   暴露的新头/尾必须由调用者填好再发表。Ethernet header 的准备验证与容量检查均在窗口变更前，之后写入不能失败。
   AF_PACKET TX 仍只复制一次调用者 payload 到最终 DMA 位置，再消费预留 MAC 头部；loopback、raw 与 datagram 沿同一格式 owner。
7. IPv6 view 一次验证固定头、长度及可见扩展链，再通过借用迭代器访问 Hop/Destination options、Routing、Fragment 与 AH。
   首个 HBH 的局部解析事实只在同一不可变 buffer 上复用，避免 Jumbo 长度发现重复扫 TLV，不新增全局 cache。
   仅 HBH 必须紧跟固定头；其他扩展头接受重复及非推荐次序，ESP/未知协议保留为 opaque terminal。
   No Next Header 不把其后字节投给 transport；选项保留 skip/discard/ICMP 动作及 mutable 位，由协议 owner 执行并限速。
   非首分片停止解码其数据；首分片验证可见扩展链，atomic fragment 可继续正常解析，reserved 位按标准忽略。
   需重组的数据不作为完整 upper-layer 交付；错误保留位置，缺失后继扩展头单独标记不完整首分片，不能靠排队等待补全。
   完整首分片的上层头校验、重组/重叠拒绝、ICMP 产生、选项动作、Routing type 执行与 AH 认证仍归最终引擎，未由 codec 冒充实现。
8. IPv6 Jumbo Payload 仅允许首 HBH 中一个正确长度/对齐且大于 65535 的选项，固定头长度必须为零，禁止并存 Fragment。
   输出固定头在写入前核对实际长度及扩展链；options emitter 完成 x*n+y 对齐、Pad1/PadN 与八字节收尾，Fragment emitter 清零 reserved。
   Routing/AH 的 emitter 只编码格式，AH 长度按 32-bit word 计并补齐 IPv6 八字节对齐；不声明 IPsec 或未知路由类型已经可用。
9. TCP 的 Prefix 只验证固定字段与 Data Offset 范围，供观察事实与首分片头范围检查；不验证 TLV、checksum 或连接状态。
   Packet::parse_header 再验证完整选项，Packet::parse 才验证完整段与 IPv4/IPv6 伪头 checksum；不得用部分输入证明已重组/已认证。
   RST tuple 观察及 TX partial-checksum 的头长读取均复用 Prefix，保持旧准入语义，不额外扫描 TLV 或计算一次 checksum。
   类型化选项保留 MSS、WS 原值、全部 SACK blocks（最多四块）、TS、MD5/AO 字节与 offset；未知选项按长度跳过。
   接收 WS 大于 14 的缩放裁决属于连接 owner；输出 WS 超过 14 则拒绝。EOL 后的接收 padding 不解释，输出必须为零。
   MD5/AO 不得同段或重复；AO 仅按格式允许至少四字节，MAC 算法长度、密钥、SNE/派生、验证与轮换仍属待实现认证 owner。
   选项 builder 每次追加先预留四字节对齐空间，失败不写入已有前缀；固定头 emitter 原位保留 options/payload，最后写完整 checksum。
   TCP checksum 的合法零值保留，不能复用 UDP 的“零表示省略”处理。序号比较、SACK 窗口、协商及拥塞均不由 codec 决定。
10. ICMPv4/v6 验证共同头、已知固定字段和 checksum；v4 无伪头，v6 用真实源/最终目的与完整长度；仅头方法不证明完整性。
    Echo/错误参数/v4 Timestamp 类型化，未知 Type/Code 保留；icmp::ErrorBody 分开读取引用与 RFC 4884 扩展，扩展失败不撤销基础信封。
    仅 v4 类型 3/11/12、v6 类型 1/3 解释长度，分别以 4/8 bytes 计；零表示整个引用，无猜测旧格式；非零至少 128，引用含 padding 且可截断。
    icmp::Extensions 校验版本 2、保留位、非零 checksum 与非空四字节对象链；零 checksum 表示省略，未知 Class/C-Type/重复项保留，迭代不重复扫描。
    基础 emit 只写共同头；emit_error 先检查完整尺寸/对象/参数，再在最终缓冲写引用/零 padding/扩展与 checksum，失败不改输出；外层 MTU、错误投递/限速/PMTU 仍归引擎。
11. ND 在 ICMPv6 之上检查 RS/RA/NS/NA/Redirect 的固定范围和 Code=0，不重复 checksum 扫描。
    选项 framing 一次验证非零八字节长度与完整边界；未知项保留。Prefix、MTU 等已知内容由单项 decode 检查，
    失败不自动抛弃其他 framing 完整的选项，最终忽略/拒绝行为由对应协议规则决定。
    Link-layer Address 的硬件宽度属于链路，接收借用可能包含 padding；Redirected Header 也允许原包引用截断。
    输出预检全部选项后清零固定保留字段并构造一次 checksum；RA/Prefix flags 原值由启用扩展的 owner 提供，不能用固定掩码删除扩展位。
    hop limit 255、地址合法性、DAD/邻居学习、选项适用消息、RA/SLAAC/生命周期与 DNS/路由扩展策略均未由格式层实现。
12. IGMPv1/v2/v3 与 MLDv1/v2 共用 source-filter 来源/记录的无状态格式 owner，地址宽度由所属消息决定。
    Query 依据精确旧代长度或新版最小长度区分；来源数、记录数与辅助区逐项验证后才暴露借用迭代器，不因未知记录类型越过其范围。
    IGMP view 保留同一不可变输入的已验证消息事实，getter 不重扫整份报告；不建立可变状态或全局 cache。
    额外接收尾部及辅助数据参与 checksum，但不混入来源或最后一条记录；基础输出拒绝它们，所有失败先于改写头。
    RFC 9776/9777 的 Query/Report 扩展 flags 保留；MLD 接收忽略 Code，不能复用 ND 的非零 Code 拒绝规则。
    浮点时间编码按标准解码，单位和版本由协议调用者选择；成员关系、兼容计时器、TTL/hop limit、Router Alert 与来源/组地址策略仍归引擎。
13. DHCPv6 解析普通/中继固定头与本层 16-bit TLV framing；选项内容错误和下一层 framing 单独由 decode 返回。
    原始 code、重复实例、未知选项和本层 offset 均保留；不拼接重复值，也不把 IA_NA/IA_PD 的不同 IA 当成 singleton。
    singleton helper 只对指定 code 检测重复；中继 innermost 使用循环逐层读取唯一 Relay Message，每层长度严格缩小，
    不使用递归栈或堆树。helper 不证明 hop-count、方向、角色、来源或认证有效，不能代替 relay 协议准入。
    DUID 只检查 3..130-byte 总宽度，支持未知 Type，不从中提取 MAC；Interface-ID 也保持 opaque。
    IA、地址、前缀、Class、Vendor、认证原字段、状态/计时选项按各自宽度读取；容器只验证下一层 framing，
    VendorInformation 内部的 code 必须由厂商命名空间解释，不能当作标准 DHCP 选项。
    decoder 保留生命周期、T1/T2 与前缀字节，地址配置 owner 再按消息方向与客户端/服务端角色验证，不隐式接受租约。
    typed emitter 预检所有字段/容量后写完整值；raw option builder 仅提供 TLV 编码，不能把成功当成已知内容/位置有效。
    信封在已就位 options 前原位填写，失败不改头；无 DHCPv4 END/PAD 语义，UDP/IPv6 负责完整 checksum。
    已废弃 IA_TA/Server Unicast 在接收时保留供协议忽略，typed output 不再产生；RKAP/MAC/重放、租约与 PD 状态机尚未实现。
14. 非压缩 DNS 名称只有一个 owner，DHCPv6 Domain Search 与 ND DNSSL 共享它；名称不隐式转 UTF-8、改大小写或执行 IDNA。
    label 长度最大 63，含 root 的编码名称最大 255；压缩/扩展 label 与截断 root 拒绝。DHCPv6 不加 padding，
    DNSSL 只在一个完整名称之后识别最多七字节的零 padding，不能把 terminator 剥掉或凭空增添搜索域。
    RDNSS 借用至少一个完整 IPv6 地址；RIO 单独核对 1/2/3-unit 长度与前缀长度组合，接收保留位忽略，preference 10 使单项失败。
    RIO 输出选最短合法长度并清零非前缀位；DNS/路由/生命周期的采纳、过期、撤销与配置发布仍由协议 owner 执行。

验证边界：既有 netstack-unit host 夹具新增格式对照，用固定 smoltcp 的 encoder/decoder 独立验证互通，
并覆盖 overload/跨段长选项、UDP 地址族/奇数长度/checksum、分片边界、全部截断前缀与字节变异。
L2 用例独立核对 QinQ wire 字节、802.3 padding、原位 payload 指针、失败窗口不变及 ARP 可变宽地址范围；
现有实际 AF_PACKET owner 用例保留原始头、同域投递、回环、发送者排除与 OOM/无分配扇出契约。
IPv6 使用独立 wire/checksum 向量及扩展头/分片/Jumbo 的截断与变异输入；尚未完成 IPv6 的成熟实现及 Linux 抓包互通对照。
TCP 使用固定 smoltcp 双向编解码对照、独立零 checksum 向量、四块 SACK、认证格式冲突与头/选项截断变异；
实际 TCP 生命周期夹具使用原生连接机制；格式对照和生命周期用例均不能代替 MD5/AO 认证验收。
ICMP/ND 用固定 smoltcp IPv4 oracle、独立 checksum 向量及截断/变异，覆盖引用截断/扩展边界、零/负零校验、未知对象及坏扩展隔离、
ND 零长度与单项内容错误、输出失败不变；组播用例覆盖旧代 IGMP oracle、新代固定 wire/checksum 向量、
来源/记录/辅助区边界、额外尾部、扩展 flags、版本长度及变异输入。尚无 IPv6/MLD 的成熟 oracle 或真实抓包结论。
DHCPv6 使用固定 Kea IA/Prefix 字段向量、24-bit ID/不透明标识/嵌套 scope/256 层迭代中继输入、字段宽度与截断/变异；
名称配置用例覆盖 63/255 边界、二进制 label、指针拒绝、DHCPv6 与 DNSSL 的不同 padding、RDNSS/RIO 范围和未知保留位。
这些是格式及输入边界证据，不是运行 Kea、Linux 抓包对照或 DHCPv6-PD/RA 来宾验收。
来宾 DHCP 验收及完整模糊测试仍见路由计划。ARP 执行归[原生 IPv4 邻接](ipv4-transports.md#ipv4-邻接与-arp)，VLAN 二层转发见[网桥契约](switching.md)。
Benchmark 决策：低频 DHCP 配置不新增 host 时延阈值。基础 MAC 视图只替换原固定宽度解码，TX 保留一次 payload copy，
无新增锁、分配或中间 frame；既有 AF_PACKET 分配账本继续覆盖该路径。VLAN 网络视图可批内解析。
批内头部 CPU 成本沿现有 TCP lifecycle benchmark 覆盖，不把 host 对照外推为数据面吞吐。
TCP IPv4 收发已由 packet-wire 编解码，真实 RX checksum proof 可省略求和；完整段/选项与最终批处理图的成本仍需沿生产 benchmark 评价。
ICMP/ND/组播及错误扩展仅提供格式机制，无新增产品收发路径、分配、锁或全局状态；接入最终引擎时测量对象扫描、批解析和 checksum 成本，
不为尚未接入的控制报文添加脱离产品路径的吞吐阈值。
DHCPv6/名称配置同样无锁、全局或堆树；本批没有接入新的设备数据面，也不预留 MTU 大小的长期缓冲或按嵌套深度分配栈。
DHCPv6-PD 与 RA 的真实链路成本及互通随完整配置流程验证，批解析 benchmark 仍在最终引擎的生产路径完成。

## 收包推进

设备共享环按有界批次交付只读帧，所有权/错误归还由[设备契约](devices-terminal.md)维护；协议状态仍由唯一 NetworkStack 持有。
每个实际 TCP 帧的 ingress 结束后立即观察匹配端点，之后才消费下一帧；不能把维护移到整批末尾而丢失同批 ACK→RST 等中间转换。
匹配与维护只借用实际端点及其连接，不借用仍有 RX token 存续的设备表；没有复制 TCP 状态、第二份句柄索引、延迟事件队列或第二个 owner。

IP-medium loopback 继续使用自己的队列与单个 receive scratch。可存续的 RX token 由同 owner 的 receive_claimed 身份独占：
NET guard 串行化领取，借用未结束前不发出第二个 token；consume 的局部 scratch/queue 借用先退出，随后 token Drop 才撤销 claim。
不消费而 Drop 只撤销 claim、保留队列头，不能把批 guard 退出当成已发出 token 的字节借用结束。
这是单个 RX scratch 的实际所有权状态；缺失时批 API 可为同一队列头生成多个 token，导致重复出队或覆盖仍在解析的输入。

Ethernet RX 在设备批交接后调用 packet-wire::batch：先检查全批 MAC/VLAN/Length-Type 边界，再转换 ARP/IPv4/IPv6 类型化头；IPv6 此层只确认固定头和完整长度，转发/终点各拥有其扩展头语义。
输出只借原帧，逐槽记录错误，不因一帧无效丢弃整批或重排；未知协议/LLC 保留精确 payload，IPv6 保留错误字段位置。
每批把当前主机引擎需要的地址事实投影到调用者的固定栈窗口，读取阶段不再分别重解这些头；所有借用在帧移动/归还前结束。
RST 终止原因只来自原生 TCP 的完整校验和实际 Closed transition，不在设备 adapter 另存事件。原始帧仍送 AF_PACKET 和协议引擎，不因预解析错误而跳过抓包。
VLAN 网桥成员在主机解析前转交[二层 owner](switching.md)。主机引擎的 VLAN 接收仍未接入。IP-medium loopback 在独占 claim 时只解析一次队列头，
复用同一 IP 头结构投影供路由消费。链路准入仍归 Delivery，不能因格式解析失败跳过原来的协议接收路径。
这完成了现有 RX 路由的批内头部分类；transport 校验/dispatch 仍逐帧，R16 完整批处理图、卸载集成和最终原生主机引擎仍未完成。

## Interface

- `router-config` 是唯一的接口配置服务，`router-protocol` 拥有设置投影及 DHCP 租约编码；启动装配、bootfs 和所有消费者只使用这组入口。
  私有数据卷仍使用既有稳定身份 `netcfg`，与服务名无关；这不是可调用的旧服务或兼容端点，重命名不能另建空数据卷。
  配置使用 `router-protocol::configuration::Configuration` 的 JSON 版本 1，历史二进制 1–5 只读迁入相同模型。
  私有对象 `interfaces` 同时保存配置和[提交编号](router-configuration.md#原生配置审阅与整份替换)。类型声明生成固定 Draft 2020-12 字段元数据；
  Serde 严格解码与配置语义校验拒绝未知字段、重复对象键、重复设备/显示名和非法地址/域名，不把错误当空配置。
  当前配置树只覆盖接口/IPv4 和 DNS 核心，不等于完整路由 schema、跨执行服务事务或 WebSocket API；旧设置端点待 API 阶段整体替换。
  当前接口/DNS 提交由配置 owner 协调可逆执行、持久决定与确认；失败及未确认状态不返回普通成功，具体边界见下文。
- 执行安装能力只授权给 RIB；router-config 经专用配置来源能力委托设备目录、ID 和静态描述，两者均不预先打开硬件会话。
  netstack 的 worker owner 选择非零 pair 数，经目录 OPEN 的设备类 options 交给 driver controller；硬件超限请求失败，不静默截断。
  当前主机协议引擎单 worker 明确请求一对，最终每核/RSS 数据面仍属路由计划。目录转发不复制队列策略或配置状态。


- DHCP 租约事件带服务器标识、发出请求的单调时刻及租期。router-config 只在地址配置成功后发布租约，失效或切换配置时一并清除。
  只读快照用当前 UTC 与单调时钟的偏移投影取得和过期时刻；配置文件不包含租约，系统时钟校正不改变续租或失效期限。
- DHCPv4 选项 15 提供租约后缀，router-config 以手动连接后缀覆盖它。没有生效地址时不发布连接后缀。
  显式全局搜索列表覆盖自动搜索。自动模式依次尝试主 DNS 后缀、各接口后缀、主后缀的父级，去重且父级至少保留两个标签。
  router-config 持有系统主后缀及父后缀开关，并在同一次持久化提交中保存一并修改的网络设置。重复提交或只改 DNS/搜索设置时保留现有租约、连接时长与路由。
  生效 DNS 从手动配置与成功租约投影，不保存需同步的副本；清除手动覆盖直接恢复租约的服务器列表，不重新获取租约。
  resolvd 与 `ipconfig /all` 使用同一有效搜索顺序。
  原生配置、DHCP 租约和快照保留完整 DNS 列表及搜索顺序；单域名最多 253 字节，使用 ASCII 标签，国际化域名使用 A-label。
  只有 resolvd 的 musl 投影取前 3 个服务器和总长不超过 247 字节的完整搜索域前缀（含分隔空格），不截断域名、不改写原生配置。
- 通告 DNS 由 netstack 接收接口持有。网络域 ordinal 16 查询版本 1 冻结正文，返回逻辑长度和一个 VMO；ordinal 17 交付配置服务观察 Event，配置服务重连时替换旧登记。
  正文由 `router-protocol::dns` 定义，至多 32 个带 scope 的 IPv6 服务器和 32 个域名。观察者清除通知并重新装订，再发布设置代际；设置查询读取当前事实，不维护长期 DNS 副本。
  已配置或 DHCP 的服务器先于通告服务器。显式搜索列表仍覆盖自动列表，自动搜索在接口后缀之后加入通告域名。resolvd 以 `地址%ifindex` 投影链路本地服务器。
- 用户租约操作走当前接口的 DHCP 会话。续租在原地址上发出 DHCPREQUEST，只有新租约应用后才回复成功；释放尽力发送
  DHCPRELEASE，随后撤销本地配置并暂停该会话的自动获取。完成句柄跟随租约事件回到 router-config，避免把排队当成应用成功。
  此状态随会话结束而消失，操作不写入持久设置。替换接口或配置会关闭旧会话及其在途操作。
- RIB 保留各接口默认路由声明，以 ifindex 定义稳定优先级，只把已选项交给 NET；接口计划不再让 NET 生成第二份默认路由。
- 原生接口策略按具名对象保存，配置名不继承 Linux IFNAMSIZ；设备匹配可用名字或 MAC，与临时 ifindex 和显示名分开。
  当前桌面对已知设备的显式修改绑定 MAC；历史名字绑定继续读取，修改时迁移。仅改显示名保留默认 IP 策略的继承，不能冻结一份默认副本。
  名字/MAC 两个对象若匹配同一实际设备，应用必须报错，不能任意选一个。
  局域网连接的自定义显示名和 IP 配置共同保存在原生设置文件。名称读取随公开状态快照发布，
  写入沿用网络设置端点的 `netdev` 权限，更新仅发布新的设置代际，接口身份、地址和 DHCP 会话继续存续。
- 物理载波与速率只由设备共享快照提供，管理开关由 netstack 持有；无 LINK RPC 或第二份在线缓存，读取失败不能当作有载波。
  自动 IPv6 链路地址由 NET 持有并只读投影，详见[地址、邻居与同链路网络契约](ipv6-link.md)。
  驱动 v13 的链路通知经 NET 通知 RIB。接口快照 v4 从同一共享字获取载波、速率和载波代际，NET 在锁外读取物理统计。
  安装设备 v2 返回接口序号、寿命 channel 及该网卡独立的链路 Event。重新取得安装结果复用同一观察能力，租约客户端不消费 RIB 的域级 Event。
  软件计数只由 NET 捕获，随后与硬件累计量饱和相加；两者不是同一瞬间的跨 owner 原子采样，不保存手工同步的在线状态缓存。

- 地址投影区分不存在的 source 与未命名 UNIX 地址：recvmsg/recvfrom EOF 的 name 长度为零；
  UNIX getsockname/getpeername/accept 则由实际 backend 投影 family-only 地址，通用编码器不猜 domain。

- notification pipe 只承载 edge；epoll_ctl ADD 在发布 interest 前预分配
  ready/source/reverse 节点，wake 只回收并重用节点。epoll_wait 只消费
  ready index 并等待单个 epoll notification，禁止每次 wake clone/poll 全部 interests
  或重建 source keys；所有 poll/epoll/blocking caller 仍必须在 wake 后复查
  backend level readiness。source notifier 持 epoll owner 时只能无等待地观察 backend；
  IPv4 owner 竞争或 payload loan 使状态暂不完整时，先保守发布 ready membership，再由
  task-context delivery 精确 level recheck，禁止在 deferred context 注册 `TaskMutex` waiter
  或吞掉唯一 edge。
- eventfd 每次成功 write/read 都必须分别发布 `EPOLLIN`/`EPOLLOUT` edge，而不只在
  counter 的 0/满边界通知。Mio 的 epoll waker 不读取 eventfd counter；若重复 write
  不推进 readiness generation，Tokio task 可已入队但所有 worker 仍睡眠。
- syscall socket 层只处理 sockaddr/iovec/msghdr/cmsg/option codec、user-copy 与 errno；不得匹配或泄漏 concrete protocol adapter。
- protocol message limit 与 stream/atomic classification 由 `socket::message_limits` 唯一提供。
- pipe 与所有 socket backend 只向 `ipc::ReceiveBuffer` 追加实际取得的 bytes；64KiB heap staging 只 reserve、不预清零，stream control barrier 通过 bounded append 保持，syscall 只 scatter initialized prefix。不得取得未初始化 capacity 的 Rust slice，
  也不保留 slice/zeroed 双轨。
- 原生 TCP/UDP/RAW、IPv4 链路机制、VirtIO-net 与 Linux socket ABI 通过 network-device 和 socket façade 分隔，任何一层不得复制另一层状态。
- 每个 `InetSocket` 独占自己的 operation membership；send/receive 通过
  `NetworkStackOwner::with_payload_loan` 在两个短 `TaskMutex` transaction 中用同类型 closed
  placeholder 保持真实端点稳定，把其传输对象借到 owner 外完成 payload
  copy 后原位归还。不同 endpoint 只共享 O(1) loan count，不共享互斥 guard；deferred poll 只在
  loan count 为零时取得一次必要的 exclusive owner，否则 O(1) 回投。禁止恢复全局 data-plane
  mutex、reader/writer spin gate、staging 协议副本或旧的锁内 fallback。
- AF_PACKET RX tap 对一个 Ethernet frame 只构造一个不可变 `SharedPacket` owner；匹配 endpoint
  queue 只克隆 Arc membership。每个 endpoint 的 64-slot capacity 在 constructor 锁外一次准备，
  payload Vec/Arc 在 packet registry spin guard 外准备，重新取得 guard 后只重验绑定/容量并发布。
  队列满独立丢包；payload OOM 丢弃该 frame；RX publication 和 receive 均不增配 queue。
  receive 取得实际 Arc 后锁外复制和退休，禁止恢复逐 endpoint payload 分配与复制。
- local-port 冲突由同一双族、地址与接口 namespace 判断；bind、listener、connect 精确化与 child 移交
  的权威范围见[端口契约](socket-transport.md#本地端口与地址空间)。不同设备不得因只比较端口而误冲突；
  accepted 与 active connect 持实际 local address 的精确租约，不长期借 listener/bind wildcard。

- 实际 Socket facade fput 统一撤销 AF_PACKET、KOBJECT_UEVENT 与 Inet 的注册；backend Arc
  observer 不延长绑定或接收队列。packet/netlink backend Drop 不反入 registry，避免 broadcaster
  在 guard 内丢弃最后观察 Arc 时自锁。Kobject bind/receive/poll 在同一 registry admission 上判断
  closed；端口立即可由新 OFD 使用，旧 observer 不得重新绑定或继续等待空队列。

### DHCPv6 前缀委派会话

- 现有 DHCP 服务接收上联接口名、持久 DUID 字节、IAID、是否接受重配置以及该接口的链路 Event，调用者持有返回的租约 channel。执行服务绑定该接口的 IPv6 UDP 546，以链路范围组播访问服务端 547。
- DHCP 客户端按 RFC 9915 持有发现、申请、续租与重新绑定事务。消息按事务 ID、服务器标识、客户端标识和 IAID 准入，已委派前缀分别到期。续租省略的前缀保留原寿命，显式零有效期撤回。
- 租约 channel 传递版本化完整快照，每项含前缀和单调首选/有效截止时间，空快照表示失去委派。服务或上联网络执行域关闭后，消费者须撤销其持有的委派。
- 一次 IA 最多 32 项，确保含最大 DUID 的更新可在 IPv6 最小链路 MTU 内发送。租约交换只影响控制面，复用现有 UDP 收发与一次性等待，暂不增加独立性能阈值。
- 持久 `prefix_delegation` 指定路由上联及 DUID-LL、IAID。物理下游和 VLAN 网关用各自的固定编号从委派派生不同的 /64，保留租约截止时间。
  重叠父前缀取更具体的授权。编号超出委派范围时等待可用分配。[RFC 9915 §18.2.10.1](https://www.rfc-editor.org/rfc/rfc9915.html#section-18.2.10.1)
- 下游分配与接口同批变更，持久确认后再切换上联。会话关闭撤回授权。临时错误交原 Port 重试，无待处理工作时不进行周期查询。
- 上联失去载波时暂停交换，原租约照常到期。重新接入且仍有有效前缀时发出有界 Rebind，使用 Confirm 的计时。合并的断开和恢复通过快照代际识别。
  重新确认超时后继续按原有截止时间使用有效租约。没有确认下游寿命已结束前不得释放上联委派。真实互通仍待验收。
- `prefix_delegation.accept_reconfigure` 默认关闭。启用后仅从匹配客户端、当前申请事务和服务器的已采用 Reply 接收 RKAP 密钥。密钥只保存在会话内，换服务器或失去委派后擦除。
  Reconfigure 必须以单播送到绑定接口并匹配双方 DUID。按 RFC 9915 校验递增重放值和完整报文的 HMAC-MD5，校验成功才推进计数和限速。
  合法触发进入原 DHCP 事务，支持 Renew、Rebind 和 Information-request。信息请求更新 DHCP 刷新和重传参数，不延长租约；与续租冲突时优先续租。
  密钥由初始 DHCP Reply 交付，使用时依赖可信上联。认证只处理低频 DHCP 控制报文，分段计算摘要，无正文复制、数据转发开销或独立性能阈值。

### DHCPv4 下游地址分发

- 受监督的 dhcp-server 仅由 router-config 授权连接。启动请求指定实际接口与地址池，接口必须已启用并持有池的 IPv4 地址及掩码。每接口同时最多一个会话，关闭能力或 NET 退出时停止收发。
- 直连 Ethernet 使用绑定接口的 packet socket 和 packet-wire 验证 IP、UDP 和 DHCP。直连请求的 IP 源及以太网源须与 DHCP 地址字段一致，按 RFC 2131 选择广播或单播回复。
- 池持有地址、客户端标识及截止时间索引。已发 Offer 占用地址，收到有效选择请求才提交租约；续租重用原节点，释放和到期归还地址，冲突申报使地址隔离 10 分钟。
- 新分配默认发 RFC 5227 格式的直连 ARP 探测。发送成功后观察一秒才报价。看到地址声明或竞争探测时，隔离地址并尝试下一候选；发送失败释放预留，等待客户端重试。
  每接口最多保留 64 个并发检查。相同客户端的重传复用检查，暂停时取消。已提交租约的续租不走探测。
- 查询通过会话 channel 按地址分页，只列当前已提交租约。无租约工作时没有周期唤醒。池稀疏存储，按实际占用分配；新租约预留全部索引，轮转查找连续占用区间，续租不分配。
- RFC 6842 客户端标识随回复回送。选项与回复大小沿标准最小 IPv4 数据报界限，DNS 按顺序装入可用容量。
- 冲突检查复用报文套接字，仅启用检查的下游订阅 ARP。整机验收时测量高 ARP 负载下的分发延迟和 CPU。
- 物理下游和 VLAN 网关各自的 `dhcp_v4_server` 保存池首尾、租期及分发选项。服务器地址和掩码取该接口的静态 IPv4 设置，接口不可用时保留池意图。可在受管隔离链路按接口关闭探测。
- 变更前以寿命能力暂停原会话，接口和持久决定确认后才应用新池。撤销确认后恢复旧会话，结果不明时持有暂停并重试。同地址与掩码下仍在范围内的租约保留。
  已移出的地址按旧截止时间占用，停用时关闭数据 socket，只保留未过期占用。配置服务重启或接口移除会结束会话，实际链路上的冲突检测及客户端互通仍待整机验收。
- 管理员查询通过既有授权设置端点，按实际接口名及地址游标分页，返回实际租约及读取时刻。配置代际变化会中止本次查询；租约按页观察。

### DNS 转发

DNS 监听、上游事务和缓存的边界见[转发契约](dns-forwarding.md)。

## RIB 候选目录

system/routing-rib 提供候选/解析库、原生执行投影及已接入启动装配的授权来源服务；求解模块不调用原生运行时。Source 拥有不可变元数据、声明及共享下一跳字典引用，
接管时验证非零来源身份、规范前缀和字典引用，删除未引用属性/下一跳并按提供者完整 Ord 值去重、重映射。
原始路由顺序保持；来源身份由上层授予，不由路由正文授予权限。属性、下一跳和可信元数据在捕获后不得通过内部可变性改变。
每条前缀只保存两个 u32 字典引用，不持逐路由 Arc 或成员副本；字典以每来源 Arc 供策略投影共享，来源更新目前是完整替换，不宣称增量更新成本。

Catalogue 对每个来源保活一个 Arc，只为路由保存八字节来源/行引用，并拥有自己的共享 Poptrie 实例。
候选顺序为最长前缀、全局 preference、授权比较范围、范围内 metric、稳定来源身份、声明行序；不同范围的代价不直接比较。
等偏好跨范围以范围身份稳定裁决，同范围比较代价；等价候选行尚未自动合并，输入组可递归展开，不能把此顺序当成 BGP/OSPF 协议内选路算法。
候选迭代、借用投影和查询均不分配、不增加引用计数；调用者在同一冻结事实中判断下一跳可用性，不缓存在线状态。

prepare_replace/prepare_remove 返回新不可变目录，原快照保持原来源；撤销不存在来源、重放同一来源 Arc 直接复用原代际。
空声明撤销来源，重复来源拒绝；任何验证/分配失败都不发布候选。目录代际由上层发布，并在发布锁外退休旧代际。
没有新增 unsafe、global、锁或 Atomic；Arc 只在管理快照和候选准备时克隆，属性字典压缩的映射仅在捕获期间存在。
router-protocol::routing 拥有来源正文版本 2，复用已有 Encoder/Decoder，不依赖 native runtime。
头部为五个 little-endian u32：版本、地址族（4/6）、属性数、下一跳数、路由数；正文依次为三张字典/记录表。
属性仅含 preference/metric，各八字节；路由为网络字节序地址、u8 前缀长度、两个 u32 引用，IPv4/IPv6 分别 13/25 bytes。
下一跳头为 u8 动作（Forward/Discard/Unreachable/Prohibit）与 u32 成员数；每成员为 u32 权重、u8 类型、u32 接口、u8 flags 和 16-byte 地址（IPv4 后补零）。
flags 第 2 位标记 IPv4 前缀使用 IPv6 网关或递归下一跳。第 0/1 位表示直接成员的网关存在和 on-link，递归时接口和这两位为零。未使用字段须为零，终止动作无成员，Forward 非空且权重为正。
无 zone 的递归 IPv6 link-local 拒绝，带接口的 Direct 可以表达；编解码不将这些声明当作设备可用、邻接成立或递归已解析。

Batch::parse 完整验证版本/数量/成员/规范前缀/字典引用及包尾，不分配解析数组；后续查询只读组头和按需借用成员。
Batch::encode 借用属性并重放冻结下一跳/路由迭代，完整预检后一次预留最终正文；重放数量/尺寸或字段失配拒绝，OOM 不交出部分消息。空路由是完整撤销，不表示“保持”。
Source::capture_batch 按确定地址族捕获；NextHop 成员按完整目标排序、按 GCD 精确约分，重复目标拒绝，随后沿原 Source::capture 去重字典。
相同目标/权重比例的组共享一份声明，来源身份仍由调用者授予，正文不能选择来源/路由实例/权限。所有临时分配失败由候选 owner 自动退休。
resolution::Resolved 保活同一 Catalogue，并按每个结构前缀保存一个 u32 已选择行号；未选择用 u32::MAX，不复制另一份前缀索引。
Poptrie::prefixes/matching_indices 只提供同一不可变索引内的结构身份，重建后不得沿用旧旁路元数据；原 FIB 查询仍复用同一 rank 路径。
resolve 的 eligible 闭包冻结候选，usable 闭包使用同一接口事实验证立即出口；求解期间事实不变、无设备 I/O，不保存可写在线状态。
递归查找只考察这份已选择集合的最长前缀；已选择的不可解析/终止项不能偷偷绕到父前缀，更不能借将被替换的旧 FIB 作证明。
每代先选 IPv6，IPv4 编译借用同代冻结 IPv6 图解析跨族依赖。每次展开跨族成员增加一次冷路径前缀查找，不增加转发表、常驻缓存或逐包工作。

下一跳按来源字典建立图，显式 Vec 工作栈逐成员 DFS，不把链长变成 Rust 调用栈深度；工作栈在各根之间复用，结束即释放。
遇到当前祖先即剔除该分支并记录 cycle；无可用成员的 Forward 没有解析结果，终止动作不能作为递归出口。
无环子图可在本次冻结编译内 memo；出现祖先剪枝的正/负结果依赖上下文，不能复用给其他根，否则会隐藏可达出口。
直接相连叶没有网关时采用本层递归地址，已有立即网关保持；确定网关后重新验证可用出口，并按剩余成员重新分配父路径权重。
展开使用精确有理数和 GCD/LCM，将相同立即出口相加、约为最小正整数；u128 暂态运算、u32 成员数/最终权重越界均拒绝整批，不截断、饱和或近似。

最终相同动作/成员组去重，前缀只经来源字典引用解析结果；Arc 仅用于目录与不同结果组，查询不克隆，临时 AVL 去重索引在编译结束时退休。
上下文中间组仅在编译期间存在，最终按根结果引用压缩字典并重映射，无引用组释放；失败丢弃全部临时状态，旧目录/读者保持原状。
resolve 保留不可解析项及 cycle/unreachable 原因用于诊断；select_resolved 复用同一编译器重选，只返回全部已选项可解析的快照，均不是已安装 FIB。
重选先按失败字典对象批量暂退别名，再尝试恢复候选；声明不删除，暂退位图只活于本次冻结求解，新事实必须从原目录重新求解。
稳定子集建立后，从所有声明的递归目标向上标记可能改变 LPM 的前缀，遇到已有有效覆盖即停止；恢复只增加覆盖/升级候选，被遮住的祖先不会改变依赖。
非敏感前缀按共享字典可用性批量恢复；敏感前缀逐个晋升并完整复核已有选择，同前缀/同对象的失败试探在选择不变期间只做一次。
暂退阶段掩码严格增长，恢复阶段只接受保持全部选择有效的优先级提升，因此有限收敛；没有固定重试次数或丢弃候选的绕过路径。
eligible/usable 只读取冻结策略及立即接口/邻接事实，不能依赖求解中变化的选择；工作位图/字典可用性及试探身份在本次编译结束时释放，无持久可达性缓存。
当前已接通授权双栈来源、静态意图/准入策略及接口/载波事件重选；同代解析图支持[双栈跟踪通知](router-configuration.md#双栈下一跳跟踪)。
双栈 TCP 承载的 BGP 和 IPv6 单播转发已接通。VRF/OSPF 与敏感前缀、有环子图的规模验收仍待完成。

resolution::forwarding 投影两族 Resolved：任一已选择字典对象没有解析结果即拒绝整批，不静默漏掉前缀。
来源 protocol 标记由调用者按可信来源身份一次捕获，缺失即失败；执行表由调用者给定且非零，不能由输入声明伪造权限。
每个共享组只转换一次原生成员并推导 scope，终止动作不创建下一跳定义；每条前缀通过冻结行/来源/结果索引按需投影，不收集第二份完整 DTO 数组。
IPv6 主选图稳定后，在同一编译器中解析优先级较低的合格静态候选。只保留具有可用立即路径或终止动作的行，路径组继续去重。
IPv6 主备按原优先顺序下发。两族网关失联或恢复均触发重算。由选路或安装检查引用的失败邻接随来源事务登记复探，即使该族暂时没有可转发路由。
Forward/Discard/Unreachable/Prohibit 分别投影为 Unicast/Blackhole/Unreachable/Prohibit；终止动作没有出口/网关。
当前投影保留来源 metric，prefsrc 为空、传输 metrics 为默认；来源正文尚不携带这些属性，不等于完整路由配置 schema。
组身份只在本批内有效，以两族完整替换发布，不承诺跨编译代际身份或独立更新关联。
接口计划不得再声明 gateway，默认路由必须来自候选；接口与路由事实仍共用原复合事务，禁止在投影里另建默认路由 owner。
router-config 已改为经 RIB 提交接口意图，沿 NET 原事务发布；来源确认、服务退出与成本边界见[配置来源契约](router-configuration.md#原生-rib-配置来源)。原生 RPC/VMO 尚未做来宾验收。
当前正文只携带通用 preference/metric，不冒充 BGP 属性、协议内选路或路由策略协议已经完成。

## FIB 前缀索引

netstack::inet::route::RoutingTable 是实际路由唯一 owner；内部 route::fib 共同拥有连续记录、共享下一跳与 libraries/prefix-index::Poptrie 实例。
共享库只拥有 32/128-bit 前缀索引机制，不依赖服务、原生对象或 Linux；FIB 边界负责将索引错误映射为 SocketError。
记录按 network/prefix、table、metric、publication 顺序排序，每个前缀直接给出同一数组的 start/count 区间；
查找沿父前缀链退回，再在对应区间裁决实际 alias/设备/scope，不逐前缀探测 bucket，也不二次查询全表树。
LOCAL→MAIN 共用一份候选链；没有逐路由堆节点、设备状态副本、地址结果 cache 或第二条 selector。

Poptrie 使用 6-bit stride、vector/leafvec + population count 和连续数组；叶值是前缀身份，run 只压缩相同身份，不能合并不同前缀而丢失 fallback。
32/128-bit 地址共用编译/查找机制，生产同时服务 IPv4 与 IPv6。两族选路优先级和路径资格由各自执行域定义。
编译递归深度最多为地址位数除以六向上取整，每层固定 64 个分支，不随报文长度/攻击者嵌套分配栈或堆。

区间起点与引用数属于同一复合状态，按实际记录维护，编译与新增检查 u32 容量边界。
零引用的结构前缀不参与匹配，可以无分配恢复同一前缀；单条变更同步调整后继区间，批量过滤沿有序记录线性重计。
删除/热拔出不为缩表申请内存，容量至多保留此前编译规模；新前缀/配置冷编译只采纳实际路由，无前缀时释放记录/索引数组。
具名定义有独立的配置寿命；路由与定义都为空时才释放整个 FIB 的全部存储。
索引不另存路由属性/设备寿命，没有全局或额外锁。

单路由新增先准备索引与记录容量，全部成功才发布；替换在原 alias 位置完成，保留顺序，精确 no-op 不分配。
PREPARE 构建完整候选根：未受影响的标量记录复制到候选，原根仍是唯一活动事实；不复制 socket、设备、能力或动态状态。
候选和前像只在事务期间并存，需要为完整数组和索引付出峰值内存及冷编译成本，不是两套活动路由。
ACTIVATE/ABORT 交换整个根；预备时记录的设备身份结合配置写租约和 ifindex 不复用，证明设备未退出时无需遍历路由。
如有设备退出，先按当前实际设备无分配过滤；准备后/发布后的热拔出均不能在撤销时复活接口。
正常发布/撤销不移动逐路由节点、不重计整表，不分配；退休根由配置会话在 NET guard 外释放。
当前单条新前缀仍全索引冷编译，完整候选仍在 NET guard 内准备，读者也仍持该 guard；
具名下一跳和带权组的独立更新见下节；增量前缀更新和 RCU 发布由路由计划继续完成。

### 共享下一跳存储

`route::nexthop` 拥有每个活动 FIB 内的下一跳槽、身份/路径索引、引用数、范围约束计数和空槽链；自动地址、配置、用户与受管来源共用。
内联路径按实际出口、网关与 on-link 声明去重；具名对象按非零 u64 配置身份唯一，即使当前路径相同也不合并其独立身份。
两种声明都编译成相同的四字节可空槽引用，常驻前缀不复制网关与已解析出口，没有第二条 selector。
槽号加一编码为 NonZeroU32，零只表示拒绝路由等无下一跳形态；容量与引用数在发布前检查，不能溢出后引用另一槽。
下一跳独占路径和 on-link 声明，选择仍读取实际设备在线状态；无下一跳形态保留自身的请求标志。
读取或冷准入的 Record 是短寿命投影，不是第二份常驻路由；省略 oif 的内联输入按实际出口比较，具名引用则保留对象身份。

1. 新增先准备记录、索引、下一跳节点与槽容量，随后一起发布；准备失败不增加引用或占用空槽。
   精确重放不分配，同下一跳的属性替换保持引用数不变，其他替换先取得新引用再释放原引用。
2. 引用数精确等于所属根的路由引用数，link-scope/host-scope 计数分别等于 scope >= LINK/HOST 的引用数；增加、删除及原地范围修改共同维护这些计数。
   最后一个匿名引用删除路径索引并归还槽；具名对象在零引用时仍保留配置身份，只由完整受管集合替换删除。
   匿名路径优先复用空槽，不按历史变更次数累积存储；接口配置重建继承具名组，退休失效成员后重建引用，不静默变成匿名路径。
   多成员组没有 HOST 引用；软 DOWN 移除多成员组内的路径，单成员描述可保留 HOST 语义，但非 HOST 前缀退出，后续 UP 不复活原前缀。
   地址退出或热拔出强制移除对应成员；尚有其他成员的组保留前缀，空组退休前缀，只保留待修正的配置身份。
3. Draft 直接生成最终属性数组和去重后的路径表，不再同时暂存一份完整逐条解析数组；索引编译成功后才成为可发布 Fib。
   候选根和原根各自拥有自己的引用计数；整表发布/撤销交换根，不能跨根持有或复用槽号。

所有变更由同一 NET 独占 guard 串行化；预备 token 在下一次路径变更前消费或丢弃。热查找只直接索引槽，不查询冷路径树、分配或增加锁/Atomic。
遗漏释放会泄漏路径，提前释放或跨根使用槽号会造成错选出口，因此槽与引用的变更必须由同一个 Fib 完成。

具名独立更新沿同一 ConfigurationSession 预备/激活/确认/撤销：预备验证实际设备/邻接及引用约束，保留一个槽的原值；
激活/撤销只交换成员数组所有权，不复制或重编译任何前缀，工作量与引用路由条数无关。范围计数阻止 LINK 引用被改成经网关转发、HOST 引用被改成多成员。
写租约保证槽身份存活；热拔出仍可清理路由，撤销后按实际设备过滤并报告 NoDevice，不复活失效出口，随后释放租约。
确认只在 NET guard 外退休前像数组；内联与具名操作没有另一套事务状态或锁。

带权组由 route::group 编译：路径按出口/网关/on-link 规范排序，重复路径、零权重和空安装组拒绝。
IPv4/IPv6 复用静态类型化路径字典与 hash-threshold 选择器；IPv4 使用源/目的 two-word hash，IPv6 使用完整源/目的八个 u32 的 Jenkins jhash2。
选择只读取实际设备事实，不读取仅首片存在的端口；IPv6 候选/逆向根共享不可变字典，不在逐包路径增加 Arc 引用或维护在线缓存。
成员按出口连续分组，每次区间遍历只对一个连续出口组裁决一次，两个权重遍历由同一 NET guard 保证事实稳定；不保存可用性副本或逐流结果 cache。
单成员不计算哈希；多成员首次源地址未知时先冻结源，再在同一前缀组中重选，不做第二次 LPM、不按新出口反复改源。
UDP/TCP/raw 准入、实际 TX 队首、TCP reset 与 IP reply 均传递实际源地址；UDP/raw 的已排队 logical oif 约束保持。

准入时将零值 prefsrc 规范为未指定；候选发布前按最终 FIB 与接口事实校验显式 prefsrc；只有失效引用才申请一位/条的标记，先完成判定再退休，必要时在冷阶段准备压缩数组。
MTU 失去 IPv4 能力也先预备地址/绑定/源引用的退出，全部成功才一起改变设备与 FIB，OOM 不留下半次配置。
原生组选择、发送及完整只读成员快照已接通；这仍不能视为完整 ECMP 产品能力。
双栈单播实际转发与等待/错误边界见[转发契约](ip-forwarding.md)；VRF/RCU 与完整转发范围仍未完成。

### 路由只读快照

network-protocol::routes::snapshot 拥有独立版本 4 的冻结正文：20-byte 头（版本、地址族、接口数、组数、行数）。
RouteAddress 由标准 IPv4/IPv6 类型静态提供族标签 4/6 和字段宽度 W=4/16；Rust 具体类型必须与 wire 标签一致，不接受布局降级或截断。
每接口 24 bytes（正且严格递增的 ifindex、16-byte 名字及长度），每组复用下一跳 framing，每行 2W+30 bytes（IPv4 38、IPv6 62）。
行含 2W+25-byte 前缀属性、从零开始的 u32 组索引（u32::MAX 表示无组）及严格 bool；有组时 bool 必须为 false，标志只由成员拥有。
组身份为快照内顺序号加一，不暴露 FIB 槽号、配置身份或 Linux nhid；只捕获有路由引用的组，零引用配置定义不进入观测字典。
同一 NET guard 编码捕获接口名、共享组和全部行；每组只保存一份成员，无逐路由成员副本。
完整验证版本、长度、次序、接口/组交叉引用、行字段和包尾后只保留连续字节与 O(组数) 偏移索引。
行和成员按需借用；group 查询不再次验证全组，后续 FIB/名字变动不改变旧快照。
独占正文编码后立即释放 NET guard，随后完整验证、组偏移分配及失败退休均在 guard 外；线性捕获/编码仍持锁，是当前冷路径限制。

域 channel 的 ROUTE_SNAPSHOT 请求为 u32 版本 1、u8 地址族（0/4/6），无句柄；拒绝未知版本/族、旧空请求及尾部。
成功回复为结果、u32 数量、逐正文 u64 长度及对应只读 VMO；族 4/6 恰好一份，族 0 恰好两份且按 IPv4/IPv6 排列，同一 NET guard 捕获，不能跨配置代际。
服务端预留固定回复和句柄容器，任一 VMO 失败不返回部分结果；客户端检查准确数量/句柄/长度后捕获并按预期族完整解析，失败退休已捕获能力和正文。
netstack 与 linuxd 必须配套交付该回复布局，不保留旧内联回复或降级分支。
Writer::into_bytes 与 RoutingSnapshot::into_bytes 只移交完成编码的唯一存储，后者同时退休派生组偏移；VMO 构造和捕获仍各有一次正文复制及峰值存储。
ROUTE_LOOKUP(18) 使用 u32 版本 3、u8 地址族、同族目的和源地址、u8 方向（0 输出、1 输入）、u32 接口（0 仅允许无绑定输出）、u32 标记，无句柄。回复为结果、u32 表与出口、u8 路由类型及源和网关地址槽。
RULE_SNAPSHOT(19) 请求为 u32 版本 1 和 u8 地址族（0/4/6），无句柄；回复为结果、u64 长度和一个只读 VMO。正文为 u32 版本 2，随后每行 40 字节原生规则与 16 字节 NUL 补零入接口名。

Linux rtnetlink 的类型化 RouteDump 复用两族同一 codec，只持冻结正文及 O(组数) 派生布局；AF_INET/AF_INET6 只捕获对应族，AF_UNSPEC 在同一 NET guard 捕获两族，任一失败不发表部分 dump。
单成员用 RTA_OIF，多成员用 RTA_MULTIPATH。网关同族用 RTA_GATEWAY，异族用 RTA_VIA（u16 AF_INET6 加 IPv6 地址）。含网关的 rtnexthop 长度为 16/28/32 bytes。
先按公因数约分；仍无法放入 rtnh_hops 的比例返回 EOVERFLOW，超过 u16 属性长度返回 EMSGSIZE，不裁剪、近似或改变 FIB。
这些是 Linux 观测边界，原生 u32 权重及完整成员不受该 UAPI 容量约束。/proc/net/route 按固定 Linux 行为仅展示首成员。
接收按窗口批量编码，首消息大于窗口时仍完整生成并按 MSG_TRUNC 交付；其他消息须整条放下。
chunk 与目标 ReceiveBuffer 在 state guard 外准备，重取锁后核对当前回复需求；容量不足重做准备，分配失败不推进 dump 游标或摘除控制消息。
消息编码只使用已预留容量，peek 不推进；旧快照/控制正文在 guard 外退休，没有新增锁、全局 cache 或每包操作。

### 原生受管 FIB 集合

network-protocol::routes 拥有完整集合 framing，forwarding 子域定义前缀属性、内联路径及具名引用；原单路由字段 codec 继续由同一模块拥有。
当前正文版本为 7：u32 版本、前缀族、定义数、路由数。每个定义为非零 u64 身份、u32 成员字节长度，每成员 26 bytes，字节长度须为非零倍数。
成员为 u32 出口、u8 网关族（0/4/6）及 16 字节网关（IPv4 后补零）、严格 bool on-link、正 u32 权重；IPv6 前缀拒绝 IPv4 网关。组与成员只借用完整捕获正文。
路由 tag=0 为 2W+48-byte 内联路由，tag=1 为非零 u64 身份加 2W+25-byte 前缀属性；含 tag 的总长分别为 IPv4 57/42、IPv6 81/66 bytes。
数值字段为小端；IPv4 地址以一个大端数值的 u32 编码，IPv6 地址按高/低两个大端数值的 u64 编码；可选地址缺席时 presence 与所有地址位均为零。
读取完整检查计数/长度、版本、身份、枚举、严格 bool/Option 与包尾后才借用有界迭代；None 的值必须为零，不分配解析数组。
只接受当前版本和对应静态地址族，不保留旧版本降级分支。格式通过不表示设备/网关已准入；缺失/重复具名身份与重复前缀在执行预备时拒绝。
空集合清除该受管来源及其具名定义；同一 table/network/prefix 只能有一个已选路条目，不能以多个单路径 alias 冒充 ECMP。

完整集合作为[复合执行批次](router-configuration.md#复合网络执行批次)的可选 FIB 正文，经同一 Installer PREPARE_CONFIGURATION(2) 与只读 VMO 传输；
可以与新接口地址/MTU 同批预备，不受 channel 正文容量限制，没有旧独立集合 opcode。
权限仍只来自根服务路由的安装能力，Linux namespace channel 不提供该入口，没有新增应用 syscall/ioctl。
ConfigurationBatch::encode 只编码接口变化；encode_forwarding 从可重放的精确长度迭代器直接写完整 FIB，复用同一字段 codec，不引入第二条 framing 或执行入口。
NetworkInstaller::prepare_configuration 只接管已经编译的 Vec 正文，构造只读 VMO 后释放原存储；编码归调用侧现有配置 owner，服务端仍完整校验。
返回原 PreparedConfiguration；沿既有 ACTIVATE/CONFIRM/ABORT 执行，关闭未确认能力自动撤销。
回复容器与固定确认容量在副作用前准备，不能在发布后因为追加回复发生分配失败。
同一 Installer 的 PREPARE_NEXT_HOP(4) 也使用 u64 正文长度和一个 VMO；正文为当前 u32 版本、u32 地址族加一个完整组定义，拒绝多余句柄/尾部，返回同一 PreparedConfiguration。
它只能更新已有对象，不隐式创建；权限和错误路径与完整集合一致，没有新增私有 syscall/ioctl 或 Linux next-hop UAPI。

RoutingTable::admit_with 是单条和复合批次共用的字段/设备准入；复合查询排除将撤销的 Forwarding 来源与旧接口状态，不能借它们证明网关/首选源有效。
RouteRequest/RouteAttributes/NextHopMember 以地址类型参数表达同族字段，RouteKind/RouteMetrics 为共享语义；Ipv4RouteRequest 是 AF_INET 边界的具体化，不另持字段或实现。
Installer 的同一配置能力已接通两族执行；RIB 的 Forwarding 借用视图可将两族冻结解析结果交给同一个编码器，不拼接两次执行事务或收集逐前缀 DTO。
持久静态来源已接通双栈；独立协议来源协调与具名单槽更新仍限 IPv4；不能以 IPv6 执行或编解码推导完整双栈产品可用，范围见[IPv6 契约](ipv6-link.md#受管-ipv6-路由)。
benchmark 决策：静态泛化不引入动态分派、热路径分支或 IPv4 常驻记录膨胀；IPv4 批次/快照只增加四字节族头，行与成员宽度保持。
编码/查询的冷路径成本沿统一双栈验收测量；格式回归覆盖两族高/低位、严格版本/族、隐藏可选值及截断，不能代替 RPC、配置和实际收发验收。
输入是已选路集合；递归编译属于 routing-rib，执行侧不新增 RIB；接口默认路由已使用该服务发布链路。显式 on-link 下一跳仍按原准入检查实际设备。
原生集合仅替换 Forwarding 来源及其具名定义，地址/租约/其他实际路由仍在同一 FIB 中；记录不复制进另一条 selector 或旁路。
PREPARE 一次准备新接口事实、完整候选 FIB、设备依赖和已知设备身份；不循环提交接口/单路由，不编译第二份完整中间根。
发布和撤销复用接口配置的整根交换机制，配置租约只排斥写者，预备不改变活动路由。
激活前检查实际设备依赖，热拔出后撤销先恢复存活设备，再报告 NoDevice；确认后的设备退出不撤销已决定的配置。
Blackhole/Prohibit 在同一准入中不允许出口/网关，分别保留本机发送 Invalid/PermissionDenied，Unreachable 保留 HostUnreachable；明确 oif 仍按原 route_output fallback。
新动作追加到原 wire 枚举，FIB/下一跳正文升为 4、快照升为 2，netstack/linuxd/调用者须配套交付；不接受旧正文。
当前通用协议来源、双栈/ECMP/VRF、完整配置树和 UI 仍在计划内；接口来源的 host 编译链路不代表完整产品路由或真实 RPC/VMO 验收。

## 原生配置

配置树、MTU/地址/FIB 的同批执行、持久决定及 DHCP 寿命归[路由配置契约](router-configuration.md)；
本页只保留网络服务、报文和 FIB 的 owner，不复制配置事务或管理协议。

## Failure and cleanup

- MSG_COPY 在 ID/permission lookup 前预留并从用户 buf 起点读取 min(bufsz,msgmax) 的实际
  storage，保留至 completion；实际 queue selection 仍只读不可变 Message Arc。NOERROR
  跳过 E2BIG 后仍由 copy capacity 检查返回 EINVAL，不截断副本，不改计数/时间/历史 PID。
  普通 receive 不受随后降低 msgmax 影响；copy fault 前的普通 dequeue 按 Linux 消费消息。
  send/receive 等待中断输出 ERESTARTNOHAND typed policy，捕获 handler 即返回 EINTR。

FD receive admission 的 bookkeeping/backing OOM 按固定 scm_detach_fds 语义截断 controls；已成功 copyout 的 FD prefix 立即安装到实际文件表，MSG_CTRUNC 标记剩余未发表 capabilities，在锁外释放。不会因后续 FD 扩容失败把已消费的 payload
变为 ENOMEM；当前 scalar copy fault 只撤销当前单 FD reservation。

connect 前的 stream SHUT_WR 只终止自身后续 writer admission，不向尚不存在的新 peer 重放 EOF；DirectionState.write_disabled 属于真实 writer permission，而 DirectionState.writer_closed 拥有 peer 可观察 EOF。显式 connected shutdown 消费真实 writer
capability；两个事实不能用同一关闭动作合并，否则 connect 前关闭写侧会错误地令 accepted peer 立即 EOF。该控制状态在真实 direction guard 下读取，未增加第二套 transport 或兼容分支。

新增热路径 Mutex/capacity 比较的 benchmark 沿用现有 hot 200ns 门禁；packet 的 native 逐消息 node/payload allocation 作为 cold roundtrip 独立报告，不降低阈值。host 回归已补 >11 SeqPacket、sender watermark/refund、100000-byte deferred staging、
peek/OOM 不退款、不互连第十二条拒绝和 self-send；native guest 与实际 packet benchmark 纳入集中门禁。

## 具体 owner 契约

- [路由配置与执行事务契约](router-configuration.md)
- [IPv4 与 TCP/UDP 传输契约](ipv4-transports.md)
- [Packet 与 Netlink 契约](packet-netlink.md)
- [UNIX socket 与辅助数据契约](unix-sockets.md)
- [System V IPC 对象与资源契约](systemv-ipc.md)
- [Pipe 与 readiness 契约](pipe-readiness.md)
