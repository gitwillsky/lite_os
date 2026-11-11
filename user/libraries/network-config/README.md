# network-config

- **类型**：库
- **作用**：原生 router-config 网络状态与设置的 Linux 客户端，以及 sysfs 链路快照。普通用户查询 `/dev/network-status`；设置与订阅经 `/dev/network-settings`，需要 netdev 组。
- **产物 / 安装位置**：静态链接进使用它的程序（资源管理器、控制面板、设备管理器、resolvd）
- **对应的 Windows 2000 组件**：netcfgx 的 TCP/IP 设置与 netshell 的连接管理
- **依赖**：linux-uapi、router-protocol

控制面板的“Internet 协议 (TCP/IP) 属性”与资源管理器的“启用/禁用”通过它把设置提交给 router-config：router-config 先暂存可逆执行、
持久化后确认；失败撤销，确认未完成会明确返回错误。Linux 侧没有配置文件或需要重载的服务。没有单独设置的以太网接口按 Windows 默认：启用、自动获得 IP 与 DNS；
禁用时 router-config 保留之前的静态地址，重新启用后恢复。

连接列表、状态和属性窗口、任务栏共用 `ethernet_links()` 的显示名。重命名持久化在 router-config，实际操作接口时继续使用
链路快照中的 Linux 接口名。

客户端保留完整原生 DNS 与搜索列表；仅 resolvd 在生成 musl 配置时应用解析器容量。持久配置加载失败的 Rejected 状态不允许局部更新覆盖未知配置。

`configuration::{export,schema,validate,difference,import}` 经同一 netdev 授权端点操作原生配置树：导出不含租约，
校验没有副作用，对比仅供审阅，完整导入带预期快照代际并复用整批提交。Rejected 时可从状态快照/校验取得代际，再整份导入恢复。
代际冲突需要重新查询；修改超时或断线也不能盲目重放。正文通过标准 SCM_RIGHTS fd，控制帧与正文大小解耦；协议、所有权与未完成验收边界见网络契约。

`configuration::{backups,create_backup,export_backup,delete_backup,restore_backup}` 提供备份管理；自动策略属于原生配置树的 backup 字段。
手动副本不受自动轮转删除，恢复仍走带预期代际的完整导入。查询返回真实调度状态/存储元数据；修改结果不确定时先刷新列表。

`ipconfig /export /root/router.json` 导出当前完整配置并显示代际，仅新建权限为 0600 的文件，不覆盖已有文件。
编辑后运行 `ipconfig /import /root/router.json <预期代际>`；代际来自本次导出，发生冲突时先重新导出并合并修改。
导入出错或连接中断不代表未提交，必须先重新查询配置和应用状态，再决定下一步。
该 JSON 是管理工具的导入/导出文件，唯一持久配置仍由原生 router-config 管理。

可在导出的目标接口对象（`interfaces` 中按配置名查找）加入 `ipv6` 字段，其余字段保留：

```json
"ipv6": {
  "addresses": [
    { "address": "2001:db8:1::1", "prefix_length": 64, "on_link": true },
    { "address": "fd00:1::1", "prefix_length": 64, "on_link": false }
  ]
}
```

示例地址按实际网络替换；`on_link` 缺省 true，false 表示不从此地址安装同前缀直连路由。
导入后通过 `ip -6 addr` 查询检测中、可用或冲突状态；同一地址修改前缀不重新检测，新地址必须先通过冲突检测才能收发。
设置空数组或移除 `ipv6` 清除手动地址，自动链路地址保留；IPv4 租约更新不会清除 IPv6。
当前支持地址/邻居查询，本机、直连及静态网关的 UDP 收发、ping 回复和端口不可达回程；回复可按路由选择不同网卡。
IPv6 TCP、跨网卡转发及 RA 已接入。OSPFv3 和整机互通验收仍在路由计划内。
详细边界见[IPv6 地址契约](../../../docs/architecture-contract/ipv6-link.md)。

IPv6 默认网关配置在导出文档的 `routes` 数组中；`interface` 使用 `interfaces` 的配置名，示例值按实际网络替换：

```json
"routes": [{
  "destination": "::", "prefix_length": 0,
  "action": { "kind": "forward", "members": [{
    "target": { "kind": "direct", "interface": "uplink", "gateway": "fe80::1" }
  }] }
}]
```

同一数组可混合两族，网关须与目标同族，前缀分别最长 32/128 位。
全局/ULA 网关须由显式直连前缀证明，或明确设置 `on_link`；链路网关须指定接口，不为无 zone 的递归地址猜出口。
保存、导入、备份和恢复沿同一配置树；IPv4 来源/租约更新保持 IPv6。通过 `ip -6 route` 查询已安装路由，未解析声明仍保留在配置中。


### 按源网段和入接口选择出口

在导出的根配置中加入 `policy_routing`。下面让从 `lan` 进入的 `192.168.50.0/24` 流量使用表 100 的默认网关。
`lan` 和 `wan2` 是 `interfaces` 中的配置名，示例网关应替换成 `wan2` 上实际可达的地址。

```json
"policy_routing": {
  "tables": [{
    "id": 100,
    "routes": [{
      "destination": "0.0.0.0", "prefix_length": 0,
      "action": {"kind": "forward", "members": [{
        "target": {"kind": "direct", "interface": "wan2", "gateway": "192.0.2.1"}
      }]}
    }]
  }],
  "rules": [
    {"priority": 100, "source": "192.168.50.0", "prefix_length": 24, "ingress": "lan", "table": 100}
  ]
}
```

使用 `ipconfig /import` 提交。可在同一表加入 IPv6 路由，并用不同优先级配置 IPv6 源前缀。
要让本机源地址 `192.168.50.1` 也使用表 100，可加一条本机规则。

```json
{"priority": 101, "source": "192.168.50.1", "prefix_length": 32, "origin": "local", "table": 100}
```

用 `ip route get 203.0.113.9 from 192.168.50.1` 检查本机路径，规则列表用 `iif lo` 显示本机规则。
要按 SO_MARK 值 42 将本机应用流量送入表 100，可用下列规则替换上面的本机源地址规则。

```json
{"priority": 102, "source": "0.0.0.0", "prefix_length": 0, "origin": "local", "mark": 42, "table": 100}
```

使用 iproute2 的 `ip route get 203.0.113.9 mark 42` 检查带标记的路径。
没有规则选出路径的流量仍走主表。清空 `policy_routing` 可撤回备选表和规则。
[策略选路契约](../../../docs/architecture-contract/ip-forwarding.md#策略选路)

### 入站流量按标记转发

在导出的配置根级添加规则。此例为从配置接口 lan 进入且发往 203.0.113.0/24 的流量设置标记 42。

```json
"firewall": {
  "prerouting": [
    {
      "ingress": "lan",
      "destination": "203.0.113.0",
      "prefix_length": 24,
      "action": {"kind": "set_mark", "value": 42}
    }
  ]
}
```

再向 policy_routing.rules 添加转发规则，匹配 mark 为 42 并选择目标表。用 `ipconfig /import` 一起提交。
IPv6 可使用同样的规则格式。赋标会继续匹配，放行或丢弃会停止匹配。转发使用首个放行规则之前最后设置的标记。清空 prerouting 列表可撤回入站规则。

### 过滤入站转发流量

将 `{"kind": "accept"}` 或 `{"kind": "drop"}` 用作 action。以下两条规则放行 lan 到 203.0.113.9 的流量，并丢弃 lan 发往同一网段其他地址的转发流量。

```json
{"ingress": "lan", "destination": "203.0.113.9", "prefix_length": 32, "action": {"kind": "accept"}}
{"ingress": "lan", "destination": "203.0.113.0", "prefix_length": 24, "action": {"kind": "drop"}}
```

按示例顺序放进 prerouting 数组。需要为放行流量设置标记时，把赋标规则放在放行规则前面。未命中放行或丢弃时继续选路。

用 `ipconfig /firewall` 查看每条预路由规则的报文数和 IP 字节数。赋标和随后执行的放行或丢弃各自计数。设备缺席的规则仍保留在配置中的位置。
替换生效规则链后从零计数。列表读取期间若规则链换版，重新执行该命令即可查看新链。

### 复用目的地址集

在配置的 `firewall` 对象内声明地址集，多个预路由规则可以引用同一名称。

```json
"address_sets": {
  "restricted": [
    {"network": "203.0.113.0", "length": 24},
    {"network": "2001:db8:29::", "length": 64}
  ]
},
"prerouting": [
  {"destination_set": "restricted", "action": {"kind": "drop"}}
]
```

引用集合的规则可以同时限定入接口和连接状态。修改成员后用 `ipconfig /import` 提交，所有引用规则一起生效，原命中次数继续累计。
一个报文命中集合的多个重叠成员时，每条引用规则仍只执行一次。空集合匹配不到报文。`ipconfig /firewall` 以 `@restricted` 显示引用。

### 按连接状态过滤

在 prerouting 规则中填写 connection_state，取值为 new、established 或 related。以下示例允许 lan 发起 IPv4 TCP、UDP 和 ICMP Echo 流量，
接收匹配的回包及关联错误。其他发往该网段的流量由末条规则丢弃。

```json
{"destination": "192.168.50.0", "prefix_length": 24, "connection_state": "established", "action": {"kind": "accept"}}
{"destination": "192.168.50.0", "prefix_length": 24, "connection_state": "related", "action": {"kind": "accept"}}
{"ingress": "lan", "destination": "0.0.0.0", "prefix_length": 0, "connection_state": "new", "action": {"kind": "accept"}}
{"destination": "192.168.50.0", "prefix_length": 24, "action": {"kind": "drop"}}
```

IPv6 使用相应的 IPv6 网段。TCP 从经过路由器的初始 SYN 开始跟踪。related 可放行路径 MTU 等引用有效流量的 ICMP 错误。省略连接状态条件的规则仍可匹配其他协议或分片。

### 查看转发连接

用有权修改网络设置的账号执行 `ipconfig /connections`。每页显示当前跟踪的 TCP、UDP 和 ICMP Echo 连接、发起方和目的端点、连接状态及剩余空闲时间。
如有下一页，执行输出最后一行的命令。连接到期后不再显示。转发服务重启后，重新运行不带游标的命令。
在对应地址族启用[连接状态过滤](#按连接状态过滤)后，列表才会记录该地址族的新连接。

清除一条连接时，将列表该行首列的完整标识传给 `ipconfig /clear-connection`。它会清除该连接的跟踪状态，并取消还在等待转发的关联报文。
连接已到期或服务已重启时，请重新查询。此后新到的流量按现行防火墙规则处理。

用 `ipconfig /watch-connections` 持续观察。命令先列出当前连接，随后显示新增、状态更新和移除。若积压超过可补读的历史，命令会重新显示当前列表。
事件行的空闲时间取自变化发生时。按 Ctrl+C 退出。

### DHCPv4 地址池

在导出的下游接口对象内配置静态 IPv4 和地址池，服务器地址及掩码沿用该接口的静态配置。

```json
"ipv4": {
  "assignment": "static",
  "address": "192.168.50.1",
  "prefix_length": 24,
  "gateway": "0.0.0.0",
  "dns": [],
  "dns_suffix": ""
},
"dhcp_v4_server": {
  "first": "192.168.50.100",
  "last": "192.168.50.199",
  "router": "192.168.50.1",
  "dns": ["192.168.50.1"],
  "lease_seconds": 3600
}
```

示例假定下游使用单独的局域网。DNS 填客户端实际能访问的解析器。修改后使用原有 `ipconfig /import` 保存，使用 `ipconfig /leases eth1` 查看对应实际接口的地址、客户端 MAC 和剩余租期。
服务器默认在提供新地址前检查冲突。在明确由管理员独占管理的链路上，可在地址池中设置 `"probe_addresses": false` 关闭检查。
删除地址池或禁用接口会停止分发。再次启用前，当前会话仍保留未过期的地址占用。配置备份保存地址池设置，运行时租约留在服务中。

### DNS 转发和缓存

在原有 `dns` 对象中加入 `forwarding`，监听地址须已列在对应接口的静态 IPv4 或 IPv6 设置中。
接口字段使用配置对象名。上游为有序列表，端口使用标准 DNS 端口。

```json
"forwarding": {
  "listen": [
    {"interface": "eth1", "address": "192.168.50.1"}
  ],
  "upstream": [
    {"address": "1.1.1.1"},
    {"address": "2606:4700:4700::1111"}
  ]
}
```

通过同一 `ipconfig /import` 保存并应用。把下游 `dhcp_v4_server.dns` 设为 `["192.168.50.1"]`，
客户端取得或续订租约后通过该地址解析域名。省略 `forwarding` 或设为 `null` 停用服务。
IPv6 链路本地上游还须设置 `interface`，如 `{"address":"fe80::1","interface":"wan"}`。
纯 IPv6 下游可在对应接口的 `ipv6.advertise` 中发布 DNS。例如转发器监听 `fd00:50::1` 时，可使用下面的通告设置：

```json
"advertise": {
  "mode": "enabled",
  "max_interval_seconds": 30,
  "router_lifetime_seconds": 90,
  "dns": ["fd00:50::1"]
}
```

下游需启用 IPv6 转发及通告。客户端通过通告自动取得解析器，寿命为最大通告间隔的三倍。
清空 `dns` 列表或停用通告会撤回之前发布的 DNS 地址。系统应用的解析设置由接口 DNS 和 `resolvd` 管理。

### DHCPv6 前缀委派

可在导出的配置根对象中加入 `prefix_delegation`，使用 `interfaces` 中的配置对象名称。

```json
"prefix_delegation": {
  "upstream": "uplink",
  "duid_mac": [2, 0, 0, 0, 0, 1],
  "iaid": 0,
  "accept_reconfigure": false,
  "downstream": { "office": 0, "guests": 1 }
}
```

首次配置时将 `duid_mac` 填为本设备持有的一个唯一单播 MAC，并与 `iaid` 一起保持稳定。后续网卡换名或更换硬件时，可单独修改设备匹配条件。
上联需有 IPv6 默认路径。可配置静态网关，或设置上联 `ipv6.accept_ra` 为 `always` 接收路由通告。下游须启用 `ipv6.advertise`，系统须开启 IPv6 转发。
下游编号在各接口之间唯一。收到 /56 时可使用 0–255；收到 /64 时只支持编号 0。超出上联实际分配范围的编号等待可用委派。
保存后用 `ip -6 route` 查看实际分配到下游的 /64，客户端通过路由通告自动配置地址。更换分配、租约到期或删除配置时，旧路由与通告撤回。
可信上联可启用 `accept_reconfigure`，允许服务器经 RFC 9915 认证触发重新申请或信息刷新。切换该选项会重新建立上联会话。
配置可导出、备份和恢复，运行时租约由当前上联重新取得。见[委派会话契约](../../../docs/architecture-contract/ipc-network.md#dhcpv6-前缀委派会话)。


可在导出的根对象中加入以下 `bgp` 字段（其余配置保留）：

```json
"bgp": {
  "local_as": 4200000001,
  "identifier": "192.0.2.1",
  "neighbors": {
    "uplink": {
      "address": "192.0.2.2",
      "asn": 4200000002,
      "preference": 20,
      "hold_seconds": 90,
      "retry_seconds": 5,
      "passive": false,
      "resolve_via_default": false,
      "import": {
        "default": "deny",
        "preference": 100,
        "rules": [
          {
            "prefix": { "network": "198.51.100.0", "length": 24 },
            "action": { "kind": "permit" }
          }
        ]
      }
    }
  }
}
```

地址/AS 按实际网络替换；address 接受 IPv4/IPv6，名称唯一，同一地址及接口不得重复。link-local IPv6 必须加 interface（如 eth0），其他地址不设置接口范围；不使用 mapped 别名。
hold 可为 0（不启用保活计时）或至少 3 秒，retry 非零。ipv4_unicast 缺省 true、ipv6_unicast 缺省 false，独立于 TCP 地址，至少开启一族。
`ipconfig /bgp` 显示接口和实际协商的两族能力。IPv6 邻居可使用例如 `"address":"2001:db8::2","ipv6_unicast":true`；
链路邻居使用 `"address":"fe80::2","interface":"eth0","ipv4_unicast":false,"ipv6_unicast":true`，持久配置不保存 ifindex。
出口默认使用目标同族的 TCP 非链路本机地址；异族或链路本地连接需要出口规则提供目标族非链路 next_hop。未配置时该目标不宣告并可查询错误，不用 Identifier 代替地址。
IPv6 出口按实际链路携带或剥除辅助链路地址；需要辅助地址但资格不足时撤回，恢复后重新发布。范围不明确会拒绝该目标，不用另一接口或本机链路地址冒充第三方；见[下一跳契约](../../../docs/architecture-contract/bgp-next-hop.md)。
`passive` 表示只接受入站连接；将根 `bgp` 设为 null 可停止所有邻居。修改 BGP 配置会重新建立该域全部会话，其他网络设置变化不重建健康 BGP 会话。
`import` 缺省或为 null 时保留接收记录，但不采用该邻居的路由。上例只允许指定 /24，其他目标拒绝。
规则按顺序首个匹配生效，可组合前缀及长度范围、AS 包含条件和普通/扩展/大团体条件；同条规则的所有条件都须满足。
允许动作可覆盖选路优先级及目标同族的全局下一跳。`import.preference` 越大越优，允许为零；它与邻居的管理距离 `preference` 含义不同。
内部邻居默认保留收到的 LOCAL_PREF，规则可显式覆盖。原始接收属性不被改写。
`resolve_via_default` 明确允许通过默认路由解析下一跳，缺省为 false；允许规则本身不构成可达性证明。

`ipconfig /bgp` 查询实际连接状态、最后错误、各邻居原始接收数量、入口策略是否配置及待应用状态；该查询需要现有 netdev 授权端点。
“已确认选择”是已确认的 BGP 路由选择数量，仍可能被全局路由策略或其他来源优先，不能当作最终转发条目数。
IPv4/IPv6 邻居、两族路由学习及发布/撤回已有实现；IPv4 路由反射和 AS 联盟已接通。完整属性政策、双栈反射/联盟及系统互通仍须完成及验收。
这些入口需要配套的原生服务和用户程序；代码的初始化握手不代表已通过来宾运行或协议互通验收，完整边界见[BGP 契约](../../../docs/architecture-contract/bgp.md)。

邻居的 `export` 与 `import` 分开授权，未配置时不发布；例如 `"export":{"default":"permit","rules":[]}` 显式允许发布已采用的路由。
出口规则沿用入口的匹配字段和动作；保留团体、AS 去环和 iBGP 分割水平不能被允许规则绕过。
`advertisement_seconds` 设置同目标宣告/撤回的最短间隔，默认外部邻居 30 秒、内部邻居 5 秒，零关闭限速。
`ipconfig /bgp` 的发布量只统计完整送入当前 TCP 的宣告，不表示对端确认；超出消息上限的目标显示最近拒绝原因。

要启用 IPv4 路由反射，在同 AS 邻居上设置 `"route_reflector_client":true`，并配置明确的入口/出口策略；其他邻居默认是非客户端。
缺省簇身份采用本地 Identifier；冗余反射器可在 `bgp` 对象中配置相同的 `cluster_id`。`ipconfig /bgp` 显示有效簇和客户端角色。
反射模式下的 iBGP 策略可筛选及编辑团体，不能覆盖选路优先级、下一跳或 ORIGIN/MED，也不能编辑 AS 路径；非法配置会定位到具体规则，避免破坏反射去环。
非客户端之间保持普通 iBGP 分割水平，客户端路由可反射给其他客户端及非客户端；完整边界见[路由反射契约](../../../docs/architecture-contract/bgp.md#路由反射)。

规则条件可用 `"origin":"igp"`、`"med":{"start":0,"end":10}`、`"local_preference":{"start":100,"end":200}` 及 `"as_path_length":{"start":0,"end":3}`，范围含两端，起点大于终点拒绝。
入口检查原始属性与初始本地优先值；出口检查已确认路由及本地优先值。没有 MED 不匹配任何 MED 范围，包括 0；AS 跳数把普通集合算一跳、联盟段算零跳。
这些条件位于规则上；属性改写位于 `action` 内，先筛选再改写。允许动作可用 `"origin":"igp"`、`"egp"` 或 `"incomplete"` 改写 ORIGIN，参与实际选路或进入出口报文。
MED 用 `"med":{"kind":"set","value":10}` 设置，`"med":{"kind":"remove"}` 删除；显式零与没有此属性不同。
外部出口默认不转发学习的上游 MED，本地发布的 MED 保留，显式出口 Set 是本地重新声明。原始接收保留，清除策略后重新选路；AS 路径正则筛选用法见下文。

允许动作可用 `"communities":{"replace":[65537],"remove":[],"add":[65538]}` 编辑普通团体。
`extended_communities` 和 `large_communities` 使用同一结构，成员分别为 8/12 个字节。缺省 replace 保留原集合，空 replace 清空；先删除再添加，重复及顺序在解码时规范化。
入口新标签参与后续出口匹配；出口新加的 NO_EXPORT 等标签交给接收邻居限制后续传播，已选路由上已有的限制仍会阻止本次发布。

允许动作可用 `"as_path_prepend":[65001,65001,70000]` 加长 AS 路径；保留顺序和重复，禁止 AS 0，空列表保持原值。
`"as_path_exclude":[65001,70000]` 删除原路径中这些 AS 的全部出现；原始接收保持可查，删除本地 AS 不会绕过协议去环。
`"as_path_replace":{"from":65001,"to":90000}` 替换全部匹配 AS；省略 from 表示替换全部。目标必须显式且非零，顺序为删除、替换、前置，前置值不受此前替换影响。
集合按新成员去重，序列保留重复次数；修改前后都检查 AS 环路，清除动作后重新使用原路径。
入口编辑参与选路与去环，出口编辑只影响发布；向外部邻居发送时，实际本地 AS 仍排在策略列表之前。反射模式的 iBGP 不允许路径编辑。

`ipconfig /bgp 198.51.100.0/24` 查看该规范网段来自各邻居的原始接收值、已确认 BGP 选择及未采用原因。
未确认的变化显示“待确认”；已确认选择单独展示后政策路径与下一跳，可能被其他路由来源压过，实际转发仍以路由表为准。
查询精确匹配网段，含主机位（如 `198.51.100.1/24`）会拒绝；支持 IPv4/IPv6 网段，来宾运行与互通尚待统一验收。

邻居可以按已协商的地址族通过标准 Route Refresh 请求重发当前出口路由，无需因此重建会话；重发仍受出口策略、发送间隔和背压约束。
使用 `ipconfig /bgp-refresh 上联 ipv4` 或 `ipconfig /bgp-refresh 上联 ipv6` 请求该配置邻居重发路由。需要 netdev 授权、已建立的会话及对端通告的刷新能力。
成功表示请求已排队。双方支持增强刷新时，收到结束标记后撤回对端遗漏的路由。修改本机 BGP 配置仍会重建会话。

完整配置中的 `"ipv4_forwarding": false` / `"ipv6_forwarding": false` 分别关闭所属族的单播转发，`true` 开启；两者缺省开启，沿既有导出、审阅和带代际的整份导入修改。
开关不影响本机通信或路由学习；单独切换无需重编译路由表。关闭确认后只丢弃所属族的未发待办；未确认失败恢复原策略和待办，另一族不受影响。IPv6 的 ND Router 位随实际开关生效。

IPv4 TCP 邻居可设置 `"ipv6_unicast":true` 协商 IPv6 路由；不改变邻居的 IPv4 传输地址，`ipconfig /bgp` 会显示已协商的 IPv6 能力。
入口策略可匹配 IPv6 网段；IPv4 TCP 发布 IPv6 时，出口许可规则必须显式给出非链路 IPv6 下一跳，例如：

```json
{"prefix":{"network":"2001:db8:900::","length":64},
 "action":{"kind":"permit","next_hop":"2001:db8:1::abcd"}}
```

该地址应是对端可解析的真实 IPv6 下一跳；不会把 IPv4 地址或 BGP Identifier 当成 IPv6 地址。无可用下一跳时显示无法发布并撤回旧宣告。
`ipconfig /bgp 2001:db8:900::/64` 查询采用结果和逐邻居发布状态，`ip -6 route` 查看实际安装结果；保存、导入和备份仍用同一配置树。
本机 IPv6 /128 可加入 networks，按地址使用资格发布/撤回；双栈 TCP 邻居已接入配置、启停与状态查询；原生服务、真实双网卡和成熟路由器互通待统一验收。

BGP 的 `"networks":[{"network":"198.51.100.0","length":24}]` 声明本机网段；只有统一路由表存在该精确可用路由才宣告，父网段/子网段不能代替。
路由失效后停止本机宣告，有有效已学路径时恢复该路径，否则撤回。显式丢弃的汇总路由也可作为依据；网段声明自身不会创建转发路由。
邻居仍须配置允许的出口策略；`ipconfig /bgp` 显示可宣告网段数，`ipconfig /bgp 198.51.100.0/24` 区分本机宣告、已学路径、暂无路由依据，并列出各邻居是否已完整发送宣告（不表示远端确认）。

BGP 的 `"redistribute":{"connected":true,"protocols":[4,89]}` 发布实际直连网段及指定来源的当前已选双栈路由；未配置/null 不发布这些来源。
协议值取提供者元数据，示例中的 4 为静态来源，89 只有对应来源已提供路由才有作用，不代表 OSPF 已交付。空选择和 BGP 来源 186 拒绝，避免自身循环重发布。
只发布可用或显式丢弃的路由；来源、度量或可用性变化后更新或撤回，仍须邻居出口政策允许。本地 MED 采用来源实际度量。
`ipconfig /bgp` 区分学习选择、本机网段与其他来源候选；逐网段查询显示来源和度量，实际逐邻居发布结果仍以完整 TCP 发送记录为准。

入口和出口规则均可配置 `"as_path_regex":"_65001_"`，匹配经过完整 AS 65001 的路径；`"^65001_"` 匹配以它开始，`"_65001$"` 匹配以它结束，`"^$"` 匹配空路径。
该条件与网段、AS 包含及团体条件同时满足才命中；入口检查原始有效路径，出口检查已采用的路径，均先匹配再执行本条动作。
支持 ASCII 分组、分支、字符类和重复；不支持环视、反向引用或 Unicode，非法/过于复杂的表达式在导入时拒绝。完整边界见[路径筛选契约](../../../docs/architecture-contract/router-configuration.md#bgp-as-路径筛选)。

对端不识别 OPEN 能力参数时，允许 IPv4 的邻居会按标准改用无可选参数的 OPEN 重试；不会把扩展消息、刷新或四字节路径当成已协商能力。
必需能力不相容或再次被拒时，`ipconfig /bgp` 显示协议错误和“协商已暂停”。修正配置后修改本机 BGP 配置，或关闭再启用 BGP；重新导入完全相同配置不会重建会话。
