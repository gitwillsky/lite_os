# OSPF 邻接与区域内选路契约

[命令行使用方式](../../user/libraries/network-config/README.md#ospf-邻居发现)

## 配置与权限

`router-config` 持有持久决定。`router-protocol::ospf` 定义配置、运行绑定与可观察状态。每个链路引用现有三层物理接口配置或 VLAN 网关，保存 Router ID、普通区域、链路代价及启用版本的定时参数。
物理网桥成员不能作为三层链路。物理接口按设备匹配条件绑定，VLAN 按名称绑定。持久配置随路由事务校验、提交与加载。

`component-manager` 只把 `liteos.ospf.Configuration` 路由给 `router-config`。OSPF 实例取得指定域的网络连接和路由来源。
具名域的 `ospf` 只引用本域接口配置。邻居、组播订阅和采用路由归各自域；协议失败独立重试。状态和生效路由可按域查询。[配置与查询](../../user/libraries/network-config/README.md#在具名路由域配置-ospf)
工厂 `OPEN` 请求携带 `u32` 版本、`u64` 字节数、含 `Session` 正文的 VMO 及同域来源和网络 channel，回复一个实例 channel。
实例 `SNAPSHOT` 请求携带 `u32` 版本，回复字节数及一个含 `Snapshot` 正文的 VMO。当前版本为 6。创建方确认实例能响应后才登记寿命。Linux 管理入口通过已有 `netview` 或 `netdev` 授权查询 `OSPF_STATUS`。

## 链路与邻居寿命

每个配置实例的线程拥有链路、邻居及定时状态。每个启用版本共用一个 IP 协议 89 的 socket，按接口加入 AllSPFRouters 组。使用 packet info 绑定发送接口和源地址，并验证报文实际到达的接口与目的地址。
仅在实例运行时读取网络接口和地址，每秒更新一次有效绑定。v2 使用接口主 IPv4 地址，v3 使用可用的 IPv6 链路本地地址。匹配歧义、设备停用和地址失效都会使链路停止收发。
本地绑定改变时清除旧邻居。关闭实例 channel 或失去网络连接时，线程释放 socket 和组播订阅。进程退出后配置服务重放持久设置，邻居由报文重新发现。

点对点链路最多保留一个当前对端。按版本校验区域、定时参数和校验和，v3 另校验实例号。收到有效 Hello 后记录 `Init`，确认双向通信后进入 `ExStart`，超过失效间隔时删除邻居。
普通区域可按版本启用邻居认证。允许基础报文携带格式合法的 LLS 扩展区，跳过扩展内容。[RFC 2328 §10.5](https://www.rfc-editor.org/rfc/rfc2328.html#section-10.5)、[RFC 5613 §2](https://www.rfc-editor.org/rfc/rfc5613.html#section-2)

## 邻居认证

v2 使用 RFC 5709 的认证字段和 HMAC-SHA-256。v3 使用 RFC 7166 的认证尾部、协议专用密钥及绑定 IPv6 源地址的填充。认证覆盖各类报文。v3 摘要包含存在的 LLS 块，v2 跳过独立的 LLS 扩展内容。
协议实例在改变邻接和数据库前验证认证。启用认证的接口丢弃缺少认证数据的报文。状态查询显示拒绝计数及最近原因，密钥留在授权的配置和执行路径内。
[RFC 5709 §3](https://www.rfc-editor.org/rfc/rfc5709.html#section-3)、[RFC 7166 §§4.2–4.6](https://www.rfc-editor.org/rfc/rfc7166.html#section-4.2)

v2 按当前邻居检查非递减序列，邻居失效后清除该序列。v3 按接口、邻居及报文类型检查严格递增序列，邻接超时后仍保留该实例观察过的高水位。
v3 同一实例的全部发送共用序列。各域在配置锁内从同一持久计数器保留高 32 位，配置导入和备份恢复不改变计数器。写入失败时实例等待重试。低 32 位用尽时重新申请发送序列空间。
[RFC 2328 附录 D](https://www.rfc-editor.org/rfc/rfc2328.html#appendix-D)、[RFC 7166 §4.1.1](https://www.rfc-editor.org/rfc/rfc7166.html#section-4.1.1)

## 数据库交换与寿命

点对点邻接按 Router ID 决定 DD 主从角色，按接口 MTU 分页交换摘要。主端定时重发未确认的 DD，从端收到重复 DD 时回送上次答复。交换完成后请求缺失或过期的条目，待请求清空才进入 `Full`。
报文序列或选项不一致时重启交换。对端或本地链路失效时，释放该邻接的请求、重传和摘要状态。

数据库按版本与 LSA 作用域持有一份正文。收到更新先校验 Fletcher，再按序列、校验和与年龄决定替换。可靠洪泛按邻接登记 LSA 标识并按 MTU 分批发送，只有对应实例得到确认时才结束重传。
年龄由接收时刻派生。到期索引引用同一份正文，事件循环等待最早截止并限量处理。MaxAge 条目在邻接结束同步且相关洪泛已确认后释放。
[RFC 2328 §§10、13、14](https://www.rfc-editor.org/rfc/rfc2328.html#section-10)、[RFC 5340 §§4.5、4.6](https://www.rfc-editor.org/rfc/rfc5340.html#section-4.5)

## 接收与采用区域内路由

每个地址族从当前 `Full` 邻接出发，以本端链路代价为第一跳代价。SPF 在各区独立验证反向链路，合并等价下一跳，并将通告前缀关联到可达路由器或中转网段。
v3 点对点反向链路必须匹配两端 Interface ID，网关保留链路范围。路由器片段按 Router ID 合并，忽略禁止参与 IPv6 转发的节点与标为不可用于单播的前缀。
[RFC 2328 §16.1](https://www.rfc-editor.org/rfc/rfc2328.html#section-16.1)、[RFC 5340 §4.8](https://www.rfc-editor.org/rfc/rfc5340.html#section-4.8)

数据库内容改变合并在一次 SPF 中，只有序列或年龄更新的通告不触发重算。邻接失效直接重算，按地址族完整替换候选，空批次撤回所有旧候选。
路由管理距离缺省 110，协议编号 188。RIB 按已有准入政策和设备事实确认并安装。实例关闭即释放来源，RIB 退出则由配置服务重建授权。OSPF 进程不持有 FIB 写入能力。
实机互通仍待完成。[Linux rtnetlink 路由来源编号](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/linux/rtnetlink.h)

## 本机网络通告

配置服务将已提交的转发状态交给协议实例。启用对应地址族转发后，只从本机已绑定且可用的 OSPF 接口生成通告。`passive` 接口可发布直连网络，同时不加入协议组播组，也不启动邻接或 Hello 定时器。
v2 Router-LSA 为 Full 邻居生成点对点链路，为可用接口生成直连网络。v3 分别生成 Router-LSA、Link-LSA 和区域内前缀 LSA。网络通告采用可用的全局范围直连前缀及接口主机地址，链路本地地址供邻接与下一跳解析。
[RFC 2328 §12.4.1.1](https://www.rfc-editor.org/rfc/rfc2328.html#section-12.4.1.1)、[RFC 5340 §4.4.3](https://www.rfc-editor.org/rfc/rfc5340.html#section-4.4.3)

本机正文、序列、校验和与确认都沿既有数据库与洪泛路径。按最早刷新截止等待，拓扑或地址变化重新生成受影响通告，保持同一 LSA 的最小生成间隔。对端发送更新的本机旧副本时，用现有正文和更高序列纠正。
移除的通告以 MaxAge 洪泛。序列到达上限时先做 MaxAge 洪泛，回收后使用初始序列重建。正常停用或重配先释放 RIB 来源，再尽力发送 MaxAge，未收到撤回的对端按邻接寿命重新选路。
[RFC 2328 §12.1.6](https://www.rfc-editor.org/rfc/rfc2328.html#section-12.1.6)、[RFC 2328 §13.4](https://www.rfc-editor.org/rfc/rfc2328.html#section-13.4)

## 普通区域之间的选路

转发开启且同时具备可用的非被动骨干接口和普通区域接口时，本机对该地址族承担区域边界路由器职责。骨干为区域 `0.0.0.0`，通告随所属区域洪泛。v2 使用 type 3 Summary-LSA，v3 使用 Inter-Area-Prefix-LSA。
单区路由器读取本区摘要，多区路由器只读取骨干区摘要。摘要必须来自本区 SPF 可达且设置了 B 位的路由器。区域内路径优先。跨区度量为到边界路由器的代价加通告度量，等价下一跳继承该边界路由器的路径。
[RFC 2328 §16.2](https://www.rfc-editor.org/rfc/rfc2328.html#section-16.2)、[RFC 5340 §4.8.3](https://www.rfc-editor.org/rfc/rfc5340.html#section-4.8.3)

边界路由器从同一次 SPF 结果生成跨区摘要及 RIB 候选。区内选用的路由可发布到其他已连接区域，骨干学到的跨区路由只发布到普通区域。下一跳所属区域不会收到同一路由的回告。
摘要沿同一数据库更新、重传、刷新和撤回。IPv4 为重叠前缀选择可区分的 LS ID，IPv6 按前缀派生区域内标识并处理碰撞。区域范围聚合、虚链路和 AS 外部路由的选路属于待扩展范围。
[RFC 2328 §12.4.3](https://www.rfc-editor.org/rfc/rfc2328.html#section-12.4.3)、[RFC 5340 §4.4.3.4](https://www.rfc-editor.org/rfc/rfc5340.html#section-4.4.3.4)
