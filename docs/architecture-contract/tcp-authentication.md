# TCP 认证与密钥契约

本文件拥有 TCP MD5 的密钥配置、选择、报文认证与生命周期；连接/拥塞/重传见 [TCP 机制](ipv4-transports.md#tcp-连接机制)，BGP 会话见 [BGP 契约](bgp.md)。
当前已接通原生双栈路径、Linux TCP_MD5SIG 及 BGP 邻居配置；宿主验证不代替来宾、FRR/BIRD 或真实网络验收。TCP-AO 与 VRF/L3 master 的 MD5 作用域仍未完成。

## 所有权与授权

- netstack 的 TCP endpoint 拥有可选的不可变密钥集合；未配置时没有集合分配。项包含对端前缀、可选实际逻辑接口和唯一零化秘密对象的 Arc。
  配置替换先完成所有 fallible 分配，再发布新集合；失败保留旧集合。空密钥删除精确 selector，无目标返回 NoEntry/ENOENT。
- 一个实际连接只保存已选择的密钥关联，不复制密钥字节。逻辑接口限定优先于通配，其后取最长前缀，同等条件取最近新增项；身份仍保留原地址、长度与作用域。
  关联缺少原接口资格时不能因重新选路降为无签名；只有配置项被显式删除或替换才改变保护要求。普通连接的空关联不建立另一份邻居/路由表。
- 已完成握手的 pending child 冻结同一密钥集合版本，accept 移交该版本和实际连接；父监听器改密钥不会改写已建立 child。
  未验证 SYN 的候选关联仅在当前栈帧存在，不得污染空闲监听槽。关闭/退出按原 endpoint/handle 生命周期释放引用，在途回复只延长被选密钥的寿命。
- libraries/tcp::Md5Key 只拥有字节和无状态 MAC，最后引用退出时用固定 zeroize 擦除；调试格式隐藏秘密。签名比较使用固定 subtle 的常数时间比较。
  Arc 的原子计数只证明不可变密钥的共享寿命，不参与协议状态裁决；NET guard 仍是配置/匹配/状态改变的唯一串行化 owner，没有新增锁、全局秘密或线程。
- Linux 使用者经其已拥有的标准 TCP socket 修改密钥；原生 BGP 只使用已授予网络域/socket 的控制能力。没有新 syscall/ioctl、公开私有设备或默认额外应用授权。

## 输入与输出

1. IncomingSegment 一次捕获唯一格式库的已检查视图，内部预览只用于匹配实际 tuple/设备，不把格式成功当作认证成功。
   选定 endpoint/child 密钥后才 authenticate；要求 MD5 而缺签名、收到不期望的 MD5 或 MAC 错误均静默丢弃，不更新 TCP 序号、窗口、活动时钟、发送等待或监听状态。
2. MAC 按 RFC 2385 对实际 IPv4/IPv6 伪头、原始固定 TCP 头（checksum 按零）、完整 payload 和秘密依次计算。
   伪头长度包含选项；MAC 不包含选项本身，保留原 Data Offset、flags、Urgent Pointer 等固定字段。硬件 checksum 资格不能代替认证。
3. SYN、SYN/ACK、普通/即时 ACK、数据、重传、FIN 和实际关闭 reset 均用当前选定密钥。无匹配连接的签名输入不产生无签名 reset。
   即时回复冻结密钥引用，在同一有界 IP 回复队列编码成完整字节后释放；排队后不因配置改变重签或保留已关闭 endpoint。
4. 输出在最终窗口保留 MD5 选项位置，经格式 owner 写固定头后计算 MAC，再完成 checksum；无正文复制、无临时报文副本。
   真实硬件卸载仍可承担普通 checksum，MAC 计算不省略。受保护 Segment 缺少匹配 key 的 emitter 必须失败，不能漏签后提交设备。
5. 普通发送、TCP_MAXSEG 查询、Nagle、拥塞控制与 SACK 恢复使用扣除实际选项占用后的 SMSS；SACK 块数按最终 header capacity 限制，与 MD5/TS 共存不超出 40-byte 选项区。
   原有发送成功记账、RX loan 归还、背压和单轮预算保持。TCP-AO 没有配置/密钥 owner 时仍明确拒绝，不把未验 MAC 当成受保护输入。

## 系统服务传输

network-protocol::inet 拥有 INET 控制分派，options 拥有 TcpMd5Selector 与密钥请求。新增 MD5_KEY(20) 使用独立版本 1：
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

缺省为普通 TCP。设置后主动连接在 connect 前安装精确对端规则，链路对端带实际接口；共享双栈 listener 在 listen 前装配全部邻居规则。
两端秘密不一致时 TCP 无法建立，不降级或绕过认证。修改沿既有配置事务重建 BGP 实例，不宣称无中断换钥或 TCP-AO 式滚动轮换。
Md5Password 的 Debug 隐藏内容，所有权退出时擦除；管理员配置导出、差异及备份仍包含其实际配置，沿原 root:netdev 授权设置入口与服务私有存储。
普通只读网络状态不开放配置管理；BGP 状态与 ipconfig 仅报告 tcp_md5 是否启用，不携秘密或签名。

## 验证与成本

固定依据见 [TCP 认证基线](../standards-baseline/network.md#原生-tcp-机制)：RFC 2385、RFC 6691、固定 Linux 的 TCP MD5 输入/输出和密钥匹配语义。
独立 hashlib 向量检查双族伪头、原始固定字段、选项/checksum 排除及 payload 篡改；真实 NET/NIC 夹具检查坏签名静默、主动/被动握手、分配失败不改旧钥及父换钥后 child 仍按原钥收发。
这些检查不证明真实 IPv6 NIC 签名互通、来宾 user-copy/跨服务调度、FRR/BIRD 或完整系统性能；仍需统一验收。

benchmark 决策：无密钥的路径只多一个空策略/格式分支，不做 MAC、密钥分配或原子克隆；格式捕获仍只做一次。
受保护连接按配置键数线性匹配（不是逐路由/逐字节索引），关联不变只借秘密，只有真实关联改变或在途回复延寿才克隆 Arc。
MD5 是保护流量固有的 O(头+payload+key) 工作；存储仅实际 key、冷配置集合及每 handle 的小关联，无每包分配或常驻整包副本。
最终同等连接/报文负载对比签名与无签名吞吐、延迟和峰值内存，不新增脱离完整路径的窄墙钟门禁。
