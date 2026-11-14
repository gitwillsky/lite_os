# TCP 认证与密钥契约

本文件拥有 TCP 认证的密钥配置、选择、报文认证与生命周期。连接、拥塞与重传见 [TCP 机制](ipv4-transports.md#tcp-连接机制)，BGP 会话见 [BGP 契约](bgp.md)。
已接通原生和 Linux 双栈 TCP-MD5、TCP-AO，以及 BGP 邻居的两种认证配置。VRF/L3 master 作用域及系统互通仍待完成。

## TCP-MD5 所有权与授权

- netstack 的 TCP endpoint 拥有可选的不可变密钥集合；未配置时没有集合分配。项包含对端前缀、可选实际逻辑接口和唯一零化秘密对象的 Arc。
  配置替换先完成所有 fallible 分配，再发布新集合；失败保留旧集合。空密钥删除精确 selector，无目标返回 NoEntry/ENOENT。
- 一个实际连接只保存已选择的密钥关联，不复制密钥字节。逻辑接口限定优先于通配，其后取最长前缀，同等条件取最近新增项；身份仍保留原地址、长度与作用域。
  关联缺少原接口资格时不能因重新选路降为无签名；只有配置项被显式删除或替换才改变保护要求。普通连接的空关联不建立另一份邻居/路由表。
- 已完成握手的 pending child 冻结同一密钥集合版本，accept 移交该版本和实际连接；父监听器改密钥不会改写已建立 child。
  未验证 SYN 的候选关联仅在当前栈帧存在，不得污染空闲监听槽。关闭/退出按原 endpoint/handle 生命周期释放引用，在途回复只延长被选密钥的寿命。
- libraries/tcp::Md5Key 只拥有字节和无状态 MAC，最后引用退出时用固定 zeroize 擦除；调试格式隐藏秘密。签名比较使用固定 subtle 的常数时间比较。
  Arc 的原子计数只证明不可变密钥的共享寿命，不参与协议状态裁决；NET guard 仍是配置/匹配/状态改变的唯一串行化 owner，没有新增锁、全局秘密或线程。
- Linux 使用者经其已拥有的标准 TCP socket 修改密钥；原生 BGP 只使用已授予网络域/socket 的控制能力。没有新 syscall/ioctl、公开私有设备或默认额外应用授权。

## TCP-MD5 输入与输出

1. IncomingSegment 一次捕获唯一格式库的已检查视图，内部预览只用于匹配实际 tuple/设备，不把格式成功当作认证成功。
   选定 endpoint/child 密钥后才 authenticate；要求 MD5 而缺签名、收到不期望的 MD5 或 MAC 错误均静默丢弃，不更新 TCP 序号、窗口、活动时钟、发送等待或监听状态。
2. MAC 按 RFC 2385 对实际 IPv4/IPv6 伪头、原始固定 TCP 头（checksum 按零）、完整 payload 和秘密依次计算。
   伪头长度包含选项；MAC 不包含选项本身，保留原 Data Offset、flags、Urgent Pointer 等固定字段。硬件 checksum 资格不能代替认证。
3. SYN、SYN/ACK、普通/即时 ACK、数据、重传、FIN 和实际关闭 reset 均用当前选定密钥。无匹配连接的签名输入不产生无签名 reset。
   即时回复冻结密钥引用，在同一有界 IP 回复队列编码成完整字节后释放；排队后不因配置改变重签或保留已关闭 endpoint。
4. 输出在最终窗口保留 MD5 选项位置，经格式 owner 写固定头后计算 MAC，再完成 checksum；无正文复制、无临时报文副本。
   真实硬件卸载仍可承担普通 checksum，MAC 计算不省略。受保护 Segment 缺少匹配 key 的 emitter 必须失败，不能漏签后提交设备。
5. 普通发送、TCP_MAXSEG 查询、Nagle、拥塞控制与 SACK 恢复使用扣除实际选项占用后的 SMSS；SACK 块数按最终 header capacity 限制，与 MD5/TS 共存不超出 40-byte 选项区。
   发送成功记账、RX loan 归还、背压和单轮预算沿用 TCP 路径。

## 系统服务传输

network-protocol::inet 拥有 INET 控制分派，options 拥有 TcpKeySelector 与密钥请求。新增 MD5_KEY(20) 使用独立版本 1：
u32 版本、现有 InetAddress 编码（port/flowinfo/zone 必须为零）、u8 前缀、u32 可选逻辑接口（零为无）和 bytes 秘密。
地址宽度、mapped 别名、正文尾部和 80-byte 当前互通上限均在副作用前检查；回复空间先预留。请求不携额外对象能力。
RemoteInetSocket 与实际服务使用同一协议；空字节是显式删除，不是关闭认证的隐式缺省。秘密不进入 native 观察结果。

## Linux UAPI

- AF_INET/AF_INET6 TCP 提供 TCP_MD5SIG(14) 和 TCP_MD5SIG_EXT(32) 的前缀规则；只捕获一次 216-byte tcp_md5sig，尾部忽略。
  sockaddr family 必须对应 socket，IPv6 mapped 对端转为实际 IPv4；MD5 不使用 sockaddr 的 port/flowinfo/zone。普通选项使用完整主机前缀，EXT 的 PREFIX flag 使用显式长度。
- keylen 为零删除，非零最多 80 字节；错误地址/长度返回 EINVAL，坏 user-copy 返回 EFAULT，资源失败返回 ENOMEM。命令副本的秘密区随 Zeroizing 退出擦除。
  密钥不能通过 getsockopt 读回；未声明的查询仍返回 ENOPROTOOPT。
- EXT 的 IFINDEX 指 Linux L3 master，绝不是物理 NIC。当前没有 VRF/L3 master，带此 flag 返回 EOPNOTSUPP；完整作用域随 VRF 功能补齐，不伪装为已支持。
  原生系统服务的逻辑接口 selector 是显式服务能力，不改变 Linux IFINDEX 的含义。

## BGP 配置与操作

唯一配置树的 bgp.neighbors.<name>.md5_password 是可选的 1..=80 字节 UTF-8 字符串；例如在既有邻居对象加入：

```json
{"md5_password":"replace-with-shared-secret"}
```

`tcp_ao` 与 `md5_password` 互斥。缺省为普通 TCP。启用认证时，主动连接在 connect 前安装精确对端规则，链路对端带实际接口；共享双栈监听器在 listen 前装配全部邻居规则。密钥不匹配时握手失败。
仅修改已启用 TCP-AO 邻居的密钥或首选 ID 时，配置服务保留原实例及路由来源，BGP 更新监听器和当前连接，并在使用半连接继承的密钥前核对已接收连接的配置。
失败时端点保留原策略，配置服务沿原 Applying 重试向前应用。设置或取消认证模式，以及修改 MD5 秘密，沿既有配置事务重建实例。

`TcpSecret` 的 Debug 隐藏内容，所有权退出时擦除。管理员配置导出、差异及备份包含秘密，由 [router-api](userspace-abi.md#网络管理入口) 限制管理员访问。
BGP 状态与 ipconfig 报告认证模式及可取得的当前发送、首选接收 ID。`tcp_ao` 的配置示例和分阶段轮换见[客户端文档](../../user/libraries/network-config/README.md#bgp-邻居认证)。

## 验证与成本

固定依据见 [TCP 认证基线](../standards-baseline/network.md#原生-tcp-机制)：RFC 2385、RFC 6691、固定 Linux 的 TCP MD5 输入/输出和密钥匹配语义。
独立 hashlib 向量检查双族伪头、原始固定字段、选项/checksum 排除及 payload 篡改；真实 NET/NIC 夹具检查坏签名静默、主动/被动握手、分配失败不改旧钥及父换钥后 child 仍按原钥收发。
这些检查不证明真实 IPv6 NIC 签名互通、来宾 user-copy/跨服务调度、FRR/BIRD 或完整系统性能；仍需统一验收。

## TCP-AO 报文认证

1. `AoMasterKey` 准备 HMAC-SHA-1 或 AES-128-CMAC 主密钥状态。方向密钥派生以实际地址、端口和双方 ISN 为上下文。无 ACK 的 SYN 使用零目的 ISN，SYN-ACK 及后续报文使用双方 ISN。
   AES 主密钥非 16 字节时执行 RFC 4615 提取。方向密钥临时字节与初始化后的密钥状态随所有权退出清理。
2. 报文认证按 SNE、完整 IP 伪头、TCP 固定头与选项、正文计算。checksum 与 MAC 字段按零处理。选择排除其他 TCP 选项时，仍覆盖 TCP-AO 自身的头和两个密钥 ID。
   伪头由 `packet-wire` 同时提供给普通 checksum、MD5 与 AO。输入使用常数时间比较，校验成功后才返回可交给 TCP 状态机的段。
3. `IncomingSegment` 只暴露端口、序号和 KeyID 候选供选择密钥。使用匹配的 AO 方向密钥校验成功后，才返回可供 TCP 处理的段。设备发送和排队回复共用签名编码入口。
   `Socket` 的认证模式驱动真实 MSS、SACK 空间与输出段长度。先填签名再完成 checksum，硬件 checksum 卸载沿用原契约。
4. `SequenceExtension` 以初始序号为基准，把每个线路序号映射到最近的 64 位位置。查找是只读操作，乱序与重传可以引用前一代。
   调用方在 TCP 接受输入后，用实际 SND.UNA 和 RCV.NXT 推进检查点。恰好相距半个序号空间或超出 64 位范围时拒绝映射。[RFC 9187](https://www.rfc-editor.org/rfc/rfc9187.html)
   NET 在真实连接内持有两个序号检查点，已认证的 SYN 被 TCP 接纳后才固定对端 ISN。路由重选仍检查密钥绑定的实际逻辑接口。

成本决策。HMAC 直接持有初始化状态，AES 只在构建主密钥与方向密钥时按实际后端分配扩展密钥。失败返回资源错误，调用方可在发布密钥前处理。
连接按需缓存已用的方向密钥。缓存命中后复用预初始化算法，无报文临时副本或每段分配。无认证 socket 的 AO 引用为空，普通报文无散列开销。整机验收时测量握手分配与两种算法的发送成本。

宿主验证使用 RFC 9235 的 IPv4 SYN、SYN-ACK 与 IPv6 数据向量覆盖双算法、ISN 与选项覆盖差异。另检查错误 SNE、篡改密钥 ID、无签名发出失败及实际 checksum。
真实 TCP 状态机夹具验证跨回绕收发和前一代重传。原生 NET/NIC 夹具验证 IPv4 主动和被动握手、坏签名丢弃、监听器删除密钥后半连接继续握手、双方换钥信号和签名 reset。IPv6 NIC 与外部实现互通仍待系统验收。

## 原生 TCP-AO 配置

- 通过原生 TCP socket 的 `set_ao_key` 为对端前缀配置 SendID、RecvID、算法与选项覆盖，可绑定实际逻辑接口。重叠选择器下，发送和接收 ID 分别唯一。添加与 MD5 重叠的 AO 密钥返回 `KeyRejected`。
- 空秘密删除精确选择器及 ID。主动连接优先使用指定的匹配密钥，未指定时使用首个匹配密钥。被动连接按经认证的初始 KeyID 选择。
  半连接及子连接保留接纳时的配置，监听器修改影响新连接。已建立的 AO 连接可增删自身对端的密钥，增删和指定替换 ID 原子提交。
  删除当前发送密钥或首选接收密钥时必须同时指定替换 ID，否则返回 `Busy`。失败保留原密钥和已派生的方向密钥。
- 对端可通过经认证的 RNextKeyID 请求发送换钥。未知 ID 保留当前发送密钥，尚未删除的旧接收密钥继续验证乱序和重传报文。
  终止时在待发 reset 已签名或丢弃后释放方向密钥。
- 原生 `AO_KEY(22)` 版本 2 在选择器、算法、两个 ID、覆盖选项和秘密后附加可选的两个新 ID。`SET_OPTION` kind 28 只更新选择。
  `AO_STATUS(23)` 版本 1 返回当前选择与通过、失败、无匹配密钥、缺失签名和忽略的 ICMP 五类计数，不返回秘密。
- 原生 `AO_REPLACE_KEYS(24)` 版本 1 传一个精确对端选择器、选择字段和 1 至 256 个密钥。整组和新 ID 在一个 socket 内原子发表，其他对端的规则保留。
  被动连接只继承实际匹配的密钥。单对端快照可直接复用，多对端监听器在验签通过后只为匹配项分配引用，避免各子连接重复保留整张监听密钥表。
- 认证计数随所属端点持有。同步态的 AO 连接忽略 RFC 5925 §7.8 指定的硬错误，已匹配的 IPv4 分片错误也不更新路径 MTU。IPv6 Packet Too Big 沿用普通路径反馈。

## Linux TCP-AO 配置

`TCP_AO_ADD_KEY`、`TCP_AO_DEL_KEY` 和 `TCP_AO_INFO` 使用 [Linux v6.12 UAPI](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/tcp.h) 的结构布局。
应用可为对端安装 HMAC-SHA-1-96 或 AES-128-CMAC-96 密钥，连接建立后增删密钥、轮换 ID 或读取统计。`TCP_AO_INFO` 读取按请求长度返回前缀并保留 `optlen`。
Linux 的 VRF master 作用域及其余不支持的选项见 [socket ABI 矩阵](../syscall-support/socket.md#已知缺口)。

现有 IPv4 NIC 夹具覆盖带数据的换钥、原子替换失败后旧策略可用、多个对端的密钥隔离、退役后拒绝旧签名，以及计数和硬 ICMP 抑制。Linux 应用与 BGP 邻居的来宾运行及外部互通列入系统验收。

benchmark 决策：无密钥的路径只多一个空策略/格式分支，不做 MAC、密钥分配或原子克隆；格式捕获仍只做一次。
受保护连接按配置键数线性匹配（不是逐路由/逐字节索引），关联不变只借秘密，只有真实关联改变或在途回复延寿才克隆 Arc。
MD5 是保护流量固有的 O(头+payload+key) 工作；存储仅实际 key、冷配置集合及每 handle 的小关联，无每包分配或常驻整包副本。
最终同等连接/报文负载对比签名与无签名吞吐、延迟和峰值内存，不新增脱离完整路径的窄墙钟门禁。
