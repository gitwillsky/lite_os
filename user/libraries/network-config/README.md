# network-config

- **类型**：库
- **作用**：经本地 WebSocket 管理网关访问原生网络状态与设置，并提供 sysfs 链路快照。
- **产物 / 安装位置**：静态链接进使用它的程序（资源管理器、控制面板、设备管理器、resolvd）
- **对应的 Windows 2000 组件**：netcfgx 的 TCP/IP 设置与 netshell 的连接管理
- **依赖**：linux-uapi、router-protocol

控制面板的“Internet 协议 (TCP/IP) 属性”与资源管理器的“启用/禁用”通过它把设置提交给 router-config：router-config 先暂存可逆执行、
持久化后确认；失败撤销，确认未完成会明确返回错误。Linux 侧没有配置文件或需要重载的服务。没有单独设置的以太网接口按 Windows 默认：启用、自动获得 IP 与 DNS；
禁用时 router-config 保留之前的静态地址，重新启用后恢复。

连接列表、状态和属性窗口、任务栏共用 `ethernet_links()` 的显示名。重命名持久化在 router-config，实际操作接口时继续使用
链路快照中的 Linux 接口名。

客户端保留完整原生 DNS 与搜索列表；仅 resolvd 在生成 musl 配置时应用解析器容量。持久配置加载失败的 Rejected 状态不允许局部更新覆盖未知配置。

`router-api` 随系统启动。普通用户可运行 `ipconfig /all`，`netview` 成员可查看路由服务运行状态，root 或 `netdev` 成员可管理配置。
已有镜像按需用系统组工具创建 `netview` 组并分配成员。权限和传输见[本地管理入口](../../../docs/architecture-contract/userspace-abi.md#网络管理入口)。

### 审阅配置

```sh
ipconfig /schema router-schema.json
ipconfig /export router.json
# 编辑 router.json 后检查有效性和差异
ipconfig /validate router.json
ipconfig /diff router.json router-diff.json
ipconfig /import router.json "$reviewed_revision"
```

将 `reviewed_revision` 设置为审阅差异时显示的完整代际。配置服务重启或代际冲突后，重新导出并合并修改。
校验和对比只读取当前状态，导出仅包含设置。导出的文件仅在目标不存在时创建，权限最多为 `0600`。
差异会标出 BGP 和 OSPF 密钥的新增、删除与替换，密钥正文显示为「已隐藏」。

发送导入前会显示提交编号。若连接中断或报错，运行 `ipconfig /commit-status <提交编号>` 查询该次导入是否已保存，重启后也可查询。
系统保留最近 128 次已保存的导入编号。若结果无法确认，先重新导出并核对当前配置。
使用 `ipconfig /watch-config` 查看当前代际和应用状态并接收更新，按 Ctrl+C 结束。
若无法加载已保存的配置，可通过 `/watch-config` 或校验取得代际，再整份导入已确认的配置恢复。
[路由配置契约](../../../docs/architecture-contract/router-configuration.md#原生配置审阅与整份替换)

### 为转发指定专用 CPU

在导出的根配置中加入 `"forwarding_cpus": [1, 2]`，填写按升序排列的目标在线逻辑 CPU 编号，审阅后导入。使用空列表恢复共享调度。系统须留至少一颗共享 CPU。

目标不可用或调度隔离发生冲突时，提交会返回错误。用 `ipconfig /watch-config` 查看配置的应用状态。已保存的设置会在系统启动时重新应用。

### 建立独立静态路由域

导出当前设置，在 `routing_domains` 中按名称建立独立网络。把例子中的 MAC 和地址换成实际值。同一块网卡只在一个域中配置。将网卡移出默认域时，同时从顶层 `interfaces` 和引用该接口的设置中移除它。其余已配置网卡也用 MAC 绑定，运行时接口名可能重新编号。

```json
"routing_domains": {
  "lab": {
    "interfaces": {
      "uplink": {
        "device": { "kind": "mac", "value": [2, 0, 0, 0, 0, 2] },
        "ipv4": {
          "assignment": "static", "address": "192.0.2.2", "prefix_length": 24,
          "gateway": "192.0.2.1", "dns": [], "dns_suffix": ""
        },
        "ipv6": { "addresses": [
          { "address": "2001:db8:1::2", "prefix_length": 64, "on_link": true }
        ] }
      }
    },
    "ipv4_forwarding": true,
    "ipv6_forwarding": true
  }
}
```

域内 `routes` 沿用[静态路由格式](../../../docs/architecture-contract/router-configuration.md#静态路由配置)，出口填写本域的接口配置名。多个域可以使用重叠的 IPv4 和 IPv6 地址。也可为同一域分配多个接口，在它们之间转发。
按[审阅配置](#审阅配置)导入后，用 `ipconfig /watch-config` 查看应用状态。跨域改绑期间旧链路会中断，新配置随系统启动加载。删除域或设备分配后，网卡回到默认域并采用其设置。
具名域支持静态地址与路由、转发开关和 IPv6 通告。域内 DHCP 和 DNS 仍待接入。

### 在指定路由域启动应用

在本机以 root 运行以下命令。`lab` 必须是已应用的具名路由域，后面的程序和参数原样传入。

```sh
ipconfig /domain lab /exec ip address show
ipconfig /domain lab /exec ip route show
ipconfig /domain lab /exec ping -c 3 192.0.2.1
```

程序经 Linux `setns` 加入目标网络，子进程继承该域。之后打开的 socket 和 `/proc/net` 读取目标域，继承的旧 socket 保留原域。命令只切换网络命名空间，保留原来的用户、文件系统和 sysfs 挂载。
权限或应用失败时，命令返回错误。删除再创建同名域后，新的程序使用新实例，旧进程仍留在旧域。[Linux `setns(2)`](https://man7.org/linux/man-pages/man2/setns.2.html)

### 按路由域设置选路和防火墙

在 `routing_domains` 的目标域中编辑 `policy_routing`、`route_policy` 和 `firewall`，字段格式与默认域相同。备选表编号、接口名称、子链和地址集都在本域中查找。同样的表号与地址可在不同域各自使用。
可按入接口、源网段和标记选路，按连接状态过滤流量并配置源或目的 NAT。[选路示例](#按源网段和入接口选择出口)、[防火墙示例](#过滤入站转发流量)、[NAT 示例](#将虚拟地址转给后端)

导入后，用 `ipconfig /firewall lab` 查看 `lab` 域每条规则的匹配条件、动作、命中报文和字节数。命令省略域名时查询默认域。查询权限与默认域相同。

### 备份与恢复

- `ipconfig /backups` 查看备份编号、类型、大小、时间及定时任务状态。
- `ipconfig /backup` 创建手动备份并显示编号。
- `ipconfig /backup-export <编号> backup.json` 导出备份到一个新的私有文件，可用 `/diff` 审阅。
- `ipconfig /backup-restore <编号> <预期代际>` 恢复备份，使用审阅时的完整代际。它同样提供可查询的提交编号。
- `ipconfig /backup-delete <编号>` 删除指定备份。创建或删除结果不明确时，先刷新备份列表。

升级后会自动读取旧版保存的网络设置。若手上有旧版网络设置二进制文件（版本 1–5）或较早导出的版本 1 JSON 备份，可把文件交给 `/validate` 和 `/diff` 审阅，再以审阅时的代际执行 `/import`。

要开启定时备份，在完整配置中设置 `"backup":{"interval_seconds":86400,"retain":10}` 后提交。手动备份由管理员显式删除。[配置备份契约](../../../docs/architecture-contract/router-configuration.md#配置备份)

### 远程管理

在 LiteOS 上将监听地址和服务器证书写入 root 拥有的 `/etc/router-api.conf`。私钥权限设为 `0600`，证书的 SAN 包含客户端连接的主机名或 IP。示例地址须换成实际管理接口地址。

```json
{
  "listen": "192.0.2.10:8443",
  "certificate": "/etc/ssl/certs/router.pem",
  "private_key": "/etc/ssl/private/router.key"
}
```

重启 `router-api` 或系统后生效。为远程账号设置密码，并将其加入 `netdev` 或 `netview`。客户端持有签发服务器证书的 CA 文件。

```sh
read -rs password
printf '%s\n' "$password" | ipconfig /remote wss://router.example:8443/router alice router-ca.pem /all
unset password
```

将示例末尾的 `/all` 换成所需的审阅、提交、订阅或备份命令即可远程操作。若修改中途断线，重新连接并使用 `/commit-status` 查询原提交编号。

### IPv6 地址与网关

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
IPv6 TCP、跨网卡转发及 RA 已接入。OSPFv3 邻居查询见[下文](#ospf-邻居发现)，路由学习和整机互通验收仍在路由计划内。
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

### 跨网段转发组播

在导出的根配置中加入 `multicast_routes`，用配置名引用物理接口或 VLAN 网关。以下将同一 IPv4 组播流送到两个下游，并将 IPv6 组播流送到其中一个。

```json
"multicast_routes": [
  {"group": "239.10.0.1", "source": "192.0.2.10", "ingress": "uplink", "outputs": ["lan10", "lan20"]},
  {"group": "ff3e::8000:1234", "source": "2001:db8:10::10", "ingress": "uplink", "outputs": ["lan10"]}
]
```

开启对应地址族的转发并导入。上联须将组播送到指定入接口，下游应用在各自网段加入该组。普通组可省略 `source` 以匹配任意来源，源特定组播必须填写来源。删除规则并重新导入即可停止相应的跨网段转发。
[静态组播转发](../../../docs/architecture-contract/ip-forwarding.md#静态组播转发)

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

可选的 `source` 用来区分同一接口上的不同源网段。下面的规则只丢弃来自 `2001:db8:10::/64` 且发往 `2001:db8:20::/64` 的转发流量。

```json
{
  "source": {"network": "2001:db8:10::", "length": 64},
  "destination": "2001:db8:20::",
  "prefix_length": 64,
  "action": {"kind": "drop"}
}
```

IPv4 使用同一格式。直接指定的源和目的网段须同族，也可以将源网段与 `destination_set` 一起使用。省略 `source` 时匹配任意来源。

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

引用集合的规则可以同时限定入接口、源网段和连接状态。修改成员后用 `ipconfig /import` 提交，所有引用规则一起生效，原命中次数继续累计。
一个报文命中集合的多个重叠成员时，每条引用规则仍只执行一次。空集合匹配不到报文。`ipconfig /firewall` 以 `@restricted` 显示引用。

### 分组和调用防火墙规则

在配置的 `firewall` 对象内用 `chains` 定义子链，在入口链或其他子链中用 `jump` 调用。示例中，发往指定 IPv4 网段的报文先进入 `lan-policy`。
命中丢弃规则时结束预路由，其余报文返回入口链并继续下一条规则。

```json
"prerouting": [
  {"ingress": "lan", "destination": "203.0.113.0", "prefix_length": 24,
   "action": {"kind": "jump", "chain": "lan-policy"}},
  {"action": {"kind": "accept"}}
],
"chains": {
  "lan-policy": [
    {"destination": "203.0.113.9", "prefix_length": 32, "action": {"kind": "drop"}},
    {"action": {"kind": "return"}}
  ]
}
```

省略目的条件会匹配两族任意目的地址。读到子链末尾也会返回。子链中的放行或丢弃会结束本次预路由，返回前设置的标记会保留。
循环、未知子链或超过 16 层的调用会被拒绝。用 `ipconfig /firewall` 查看规则所属链、链内序号、跳转目标和命中数。

### 按连接状态过滤

在 prerouting 规则中填写 connection_state，取值为 new、established 或 related。以下示例允许 lan 发起 IPv4 TCP、UDP 和 ICMP Echo 流量，
接收匹配的回包及关联错误。其他发往该网段的流量由末条规则丢弃。

```json
{"destination": "192.168.50.0", "prefix_length": 24, "connection_state": "established", "action": {"kind": "accept"}}
{"destination": "192.168.50.0", "prefix_length": 24, "connection_state": "related", "action": {"kind": "accept"}}
{"ingress": "lan", "destination": "0.0.0.0", "prefix_length": 0, "connection_state": "new", "action": {"kind": "accept"}}
{"destination": "192.168.50.0", "prefix_length": 24, "action": {"kind": "drop"}}
```

IPv6 使用相应的 IPv6 网段。TCP 从经过路由器的初始 SYN 开始跟踪。related 可放行路径 MTU 等引用有效流量的 ICMP 错误。
启用连接状态规则的地址族会在分片收齐后按完整报文判断。可用省略连接状态条件的规则匹配其他协议。

### 将虚拟地址转给后端

后端回程须经过同一台 LiteOS。使用虚拟地址时，先在上游配置经过 LiteOS 的路由。在 `firewall.prerouting` 中添加下列规则，再用 `ipconfig /import` 提交。

```json
{
  "destination": "203.0.113.20",
  "prefix_length": 32,
  "action": {"kind": "destination_nat", "address": "192.0.2.20"}
}
```

客户端连接 `203.0.113.20`，后端收到发往 `192.0.2.20` 的流量，客户端仍看到虚拟地址发出的回包。IPv6 使用同族地址及 128 位主机前缀。
上面的规则适用于 TCP、UDP 和 ICMP Echo，并保持端口及标识符。要按服务改写端口，可为同一虚拟地址添加规则，例如将 TCP 443 转给后端 TCP 8443。

```json
{
  "destination": "203.0.113.20",
  "prefix_length": 32,
  "transport": {"protocol": "tcp", "destination_port": 443},
  "action": {"kind": "destination_nat", "address": "192.0.2.20", "port": 8443}
}
```

规则按顺序执行。若同时使用地址级转换规则，把具体服务的规则放在前面。UDP 将协议写为 `udp`。客户端回包仍来自其原本连接的地址和端口。
也可以将 `destination` 填成 LiteOS 的接口地址，例如 WAN 地址。仅匹配指定 TCP 或 UDP 端口时，该地址上的其他本机服务仍可访问。
可用 `ipconfig /firewall` 查询命中情况，用 `ipconfig /connections` 查看连接。
规则更新应用于新连接。要提前释放已有映射，可通过 `ipconfig /clear-connection` 清除对应连接。

### 固定源地址转换

在 `firewall.prerouting` 中添加 `source_nat` 规则，再用 `ipconfig /import` 提交。下例将来自 `192.0.2.0/24` 的转发流量改用 `203.0.113.20` 作为源地址。

```json
{
  "source": {"network": "192.0.2.0", "length": 24},
  "action": {"kind": "source_nat", "address": "203.0.113.20"}
}
```

源地址可填 LiteOS 的出口接口地址。IPv6 采用相同格式并填写 IPv6 地址。要指定固定源端口，在规则中声明 TCP 或 UDP 的 `transport` 条件，并在动作中填写 `port`。
省略端口时沿用原端口。回包恢复原内网地址和端口。若另一连接已占用同一个线上连接元组，新映射会被拒绝。

### 自动源地址转换

出口地址会变化时，可在 `firewall.prerouting` 中使用 `masquerade`，再用 `ipconfig /import` 提交。例如，以下规则让来自 `192.0.2.0/24` 的转发连接自动使用所选出口的地址。

```json
{
  "source": {"network": "192.0.2.0", "length": 24},
  "action": {"kind": "masquerade"}
}
```

IPv6 同样适用，源网段填写 IPv6 前缀。TCP、UDP 优先保留原源端口，与其他转发连接冲突时使用可用端口。出口地址变化后，新连接自动使用新地址。旧地址失效后，可清除原映射并重新连接。
用 `ipconfig /firewall` 查看规则命中，用 `ipconfig /connections` 查看相关连接。

### 内网访问对外地址

需要让内网设备通过对外地址访问同一内网的后端时，在目的转换规则里加上源转换。下例把 `203.0.113.20:443` 转给内网的 `192.0.2.20:8443`，后端回包经 LiteOS 送回原客户端。

```json
{
  "source": {"network": "192.0.2.0", "length": 24},
  "destination": "203.0.113.20",
  "prefix_length": 32,
  "transport": {"protocol": "tcp", "destination_port": 443},
  "action": {
    "kind": "destination_nat",
    "address": "192.0.2.20",
    "port": 8443,
    "source": {"kind": "masquerade"}
  }
}
```

把规则加入 `firewall.prerouting`，放在适用同一服务的一般端口转发规则前面，再用 `ipconfig /import` 提交。
若需要固定使用路由器内网地址，把动作内的 `source` 改为 `{"kind":"fixed","address":"192.0.2.1"}`。IPv6 填写同族地址和前缀。

### 更新已有连接的策略

在导出的配置中，将 `firewall.existing_connections` 设为 `"preserve"`，再用 `ipconfig /import` 提交。已有跟踪记录的连接继续沿用各自的规则和地址集，新连接使用本次配置。
如果要让新规则也检查已有连接，把该值改为 `"reevaluate"`。省略该字段时同样会重新检查。规则可按连接状态、入接口或地址分别限制两个方向。

修改 NAT 目标后，新连接使用新目标。要让指定连接换用新目标，先用 `ipconfig /connections` 找到该连接，再用 `ipconfig /clear-connection` 清除并重新连接。

### 查看转发连接

用 root、`netdev` 或 `netview` 账号执行 `ipconfig /connections`。每页显示当前跟踪的 TCP、UDP 和 ICMP Echo 连接、发起方和目的端点、连接状态及剩余空闲时间。
如有下一页，把输出末尾的游标追加到原命令。连接到期后不再显示。转发服务重启后，重新运行不带游标的命令。
启用[连接状态过滤](#按连接状态过滤)、地址转换或保留已有连接的策略后，列表开始记录相应的转发连接。

清除一条连接时，将列表该行首列的完整标识传给 `ipconfig /clear-connection`。它会清除该连接的跟踪状态，并取消还在等待转发的关联报文。
连接已到期或服务已重启时，请重新查询。此后新到的流量按现行防火墙规则处理。

用 `ipconfig /watch-connections` 持续观察。命令先列出当前连接，随后显示新增、状态更新和移除。若积压超过可补读的历史，命令会重新显示当前列表。
事件行的空闲时间取自变化发生时。按 Ctrl+C 退出。

具名域可在查询、观察和清除前加 `/domain 路由域`，例如 `ipconfig /domain lab /watch-connections`。清除需用管理账户，命令中的路由域应与产生连接标识的查询一致。

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
接口字段可填写物理接口的配置名或 VLAN 网关名。上游为有序列表，端口使用标准 DNS 端口。

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
若启用[网桥与 VLAN](#网桥与-vlan)示例中的 `lan10`，可在 `listen` 中指定 `{"interface":"lan10","address":"192.168.10.1"}` 和 `{"interface":"lan10","address":"fd10::1"}`，并将对应地址填入 VLAN 的 DHCPv4 和 IPv6 通告 DNS 列表。
监听在网关地址可用后启动。链路本地上游失联时，查询可使用列表中其他可用的上游。
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

可在导出的配置根对象中加入 `prefix_delegation`。上联使用 `interfaces` 中的路由接口名称，下游使用物理接口的配置名或 VLAN 网关名。

```json
"prefix_delegation": {
  "upstream": "uplink",
  "duid_mac": [2, 0, 0, 0, 0, 1],
  "iaid": 0,
  "accept_reconfigure": false,
  "downstream": { "office": 0, "lan10": 1 }
}
```

首次配置时将 `duid_mac` 填为本设备持有的一个唯一单播 MAC，并与 `iaid` 一起保持稳定。后续网卡换名或更换硬件时，可单独修改设备匹配条件。
上联需有 IPv6 默认路径。可配置静态网关，或设置上联 `ipv6.accept_ra` 为 `always` 接收路由通告。系统须开启 IPv6 转发。
物理下游须启用 `ipv6.advertise`，VLAN 下游须启用网关对象内的 `advertise`。VLAN 可只通告委派前缀，静态 IPv6 地址可以省略。
下游编号在各接口之间唯一。收到 /56 时可使用 0–255；收到 /64 时只支持编号 0。超出上联实际分配范围的编号等待可用委派。
保存后用 `ip -6 route` 查看实际分配到下游的 /64，客户端通过路由通告自动配置地址。更换分配、租约到期或删除配置时，旧路由与通告撤回。
可信上联可启用 `accept_reconfigure`，允许服务器经 RFC 9915 认证触发重新申请或信息刷新。切换该选项会重新建立上联会话。
配置可导出、备份和恢复，运行时租约由当前上联重新取得。见[委派会话契约](../../../docs/architecture-contract/ipc-network.md#dhcpv6-前缀委派会话)。


### 在具名路由域配置 OSPF

将 `ospf` 放入 `routing_domains` 中的目标域，格式沿用[邻居发现](#ospf-邻居发现)和[邻居认证](#ospf-邻居认证)。`links` 的键使用该域的 `interfaces` 配置名。同一域可运行 OSPFv2 和 OSPFv3，学习及通告的路由留在该域。

```sh
ipconfig /domain lab /ospf
ipconfig /domain lab /routes
```

第一条显示 `lab` 域的链路、邻接和认证结果。第二条分页显示当前生效的双栈路径，OSPF 学到并采用的路由在来源列标为 `OSPF`。分页期间可能发生重新选路。省略 `/domain lab` 查询默认域。

### OSPF 邻居发现

导出当前路由配置，在根对象添加 `ospf`，再按[审阅配置](#审阅配置)提交。以下示例在已有的 `wan` 接口配置上启用两个版本。

```json
{
  "ospf": {
    "router_id": "192.0.2.1",
    "links": {
      "wan": {
        "area": "0.0.0.0",
        "cost": 10,
        "v2": { "hello": 10, "dead": 40 },
        "v3": { "instance": 0, "timers": { "hello": 10, "dead": 40 } }
      }
    }
  }
}
```

链路两端使用不同的 Router ID、相同的区域和定时参数。对端设为普通区域中的点对点链路，v3 使用同一实例号。只启用一个版本时省略另一个版本。
v2 要求接口有 IPv4 地址，v3 使用 IPv6 链路本地地址。`cost` 是本端链路代价，省略时为 1。将 `ospf` 设为 `null` 可停用所有链路。

`ipconfig /ospf` 显示链路版本、区域、本地地址和邻居状态。`Init` 表示已收到对端 Hello。`ExStart`、`Exchange` 和 `Loading` 分别表示正在协商、交换摘要和补齐通告。`Full` 表示初始数据库同步完成。
邻居失联超过 `dead` 秒后从列表消失。[邻居认证](#ospf-邻居认证)

邻接同步完成后，可在桌面网络管理器的路由表或 `ip route`、`ip -6 route` 查看采用的区域内路由、出口和下一跳。OSPF 的路由来源编号为 188。
转发经过本机的流量需要启用对应地址族的转发。[跨区域选路](#跨-ospf-区域选路)

### 在 OSPF 区域内通告本机网络

启用对应地址族的转发后，LiteOS 会通告已启用 OSPF 的接口上的可用 IPv4 和 IPv6 直连网络。若局域网没有需要建立邻接的路由器，可在同一区域给该接口设置 `passive`，通过其他接口的邻接通告它的网段。
以下是 `ospf.links` 中一个已有 `lan` 接口配置的示例。`v2` 和 `v3` 指定需要通告的地址族。

```json
{
  "lan": {
    "area": "0.0.0.0",
    "passive": true,
    "v2": { "hello": 10, "dead": 40 },
    "v3": { "timers": { "hello": 10, "dead": 40 } }
  }
}
```

`ipconfig /ospf` 将这类接口显示为「仅通告」。在同区域的对端路由表查看该接口网段。移除地址、停用链路或关闭对应地址族的转发后，对端会更新路由。[OSPF 契约](../../../docs/architecture-contract/ospf.md#本机网络通告)

### 跨 OSPF 区域选路

要让 LiteOS 连接多个普通区域，为骨干侧接口设置 `"area": "0.0.0.0"`，为其他区域的接口设置相应区域 ID，例如 `"area": "0.0.0.1"`。开启对应地址族的转发，并确认各接口上的邻接达到 `Full`。
其他普通区域通过骨干互通。边界路由器自动通告能到达的网段。在 `ip route`、`ip -6 route` 或桌面路由表查看跨区网段的出口。断开骨干链路后，经过该链路的跨区路径会撤回或切换。[选路契约](../../../docs/architecture-contract/ospf.md#普通区域之间的选路)

### OSPF 邻居认证

在链路两端配置 HMAC-SHA-256、相同的密钥 ID 与共享密钥。以下字段可加入 `ospf.links` 中已启用相应版本的点对点接口对象。提交前将示例值替换为该链路的实际密钥。

```json
{
  "v2_authentication": { "send_id": 7, "keys": { "7": "REPLACE_WITH_THE_IPV4_SHARED_SECRET" } },
  "v3_authentication": { "send_id": 12, "keys": { "12": "REPLACE_WITH_THE_IPV6_SHARED_SECRET" } }
}
```

v2 ID 的范围为 0–255，v3 为 0–65535。发送采用 `send_id`，接收接受 `keys` 中的各个 ID。换钥时可先在两端添加新 ID，再逐端切换发送 ID，最后删除旧密钥。提交链路认证设置会重建 OSPF 邻接。
`ipconfig /ospf` 显示启用状态、累计拒绝数和最近原因。常见原因包括缺少认证数据、密钥 ID 不匹配、签名错误和旧序列。[认证契约](../../../docs/architecture-contract/ospf.md#邻居认证)

### 在具名路由域配置 BGP

将 `bgp` 对象放在 `routing_domains` 中的目标域内，邻居、收发策略和认证沿用下面的 [BGP 配置](#bgp-配置)。IPv6 链路本地邻居的 `interface` 填该域 `interfaces` 中的配置名。网卡缺席时该邻居等待接口，同一域的其他邻居可继续建立会话。
各域可以使用相同的对端地址，各自学习、选用和发布路由。删除该域的 `bgp` 对象可停止本域会话并撤回这些会话提供的路由。

```sh
ipconfig /domain lab /bgp
ipconfig /domain lab /bgp 198.51.100.0/24
ipconfig /domain lab /bgp-refresh upstream ipv4
```

会话状态和路由查询沿用观察权限，刷新需要管理权限。配置审阅中的 TCP MD5 和 TCP-AO 密钥正文均会遮蔽。

### BGP 配置

可在导出的根对象中加入以下 `bgp` 字段（其余配置保留）：

```json
"bgp": {
  "local_as": 4200000001,
  "identifier": "192.0.2.1",
  "networks": [{ "network": "203.0.113.0", "length": 24 }],
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
      },
      "export": {
        "default": "deny",
        "rules": [
          {
            "prefix": { "network": "203.0.113.0", "length": 24 },
            "action": { "kind": "permit", "med": { "kind": "set", "value": 20 } }
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
自动选择下一跳时，通常使用 TCP 本机地址。IPv6 链路本地连接可自动选择合格的本机全局地址。需要指定下一跳时可在出口规则设置 `next_hop`，可用地址族取决于实际协商能力。
IPv6 出口按实际链路携带或剥除辅助链路地址；需要辅助地址但资格不足时撤回，恢复后重新发布。范围不明确会拒绝该目标，不用另一接口或本机链路地址冒充第三方；见[下一跳契约](../../../docs/architecture-contract/bgp-next-hop.md)。
`passive` 表示只接受入站连接。单独停用一个邻居时，在它的配置中写入 `"shutdown": true`，再审阅并导入。该邻居的会话关闭，系统重新选择受影响的路由，其余会话继续运行。`ipconfig /bgp` 显示「已停用」。将 `shutdown` 改回 `false` 或移除此字段即可重新启用。
将根 `bgp` 设为 null 可停止整个域。其他邻居参数变化通常重建该域会话，已启用 TCP-AO 的邻居可在线换钥。

### BGP 多路径

在目标邻居对象中设置 `"receive_add_path": true` 或 `"send_add_path": true`，再导入配置。收发方向可分别启用。对端须在双方启用的 IPv4 或 IPv6 单播族开启互补方向。缺省按单路径交换。
发送方向对每条可用路径执行该邻居的出口策略。每个网段仍只有一个 BGP 最佳路径参与系统转发选路。
`ipconfig /bgp` 显示收发协商结果。`ipconfig /bgp 203.0.113.0/24` 可查看接收路径及判定，也可查看已发布的本端编号。撤回只影响对应编号。[出口契约](../../../docs/architecture-contract/bgp.md#出口策略与实际发送)

### BGP 邻居认证

在已授权导出的配置中，为目标邻居添加 `tcp_ao`，再用当前代际导入。该邻居只能选择一种认证方式。使用 TCP-AO 时移除 `md5_password`。

```json
{
  "tcp_ao": {
    "send_id": 11,
    "receive_id": 21,
    "keys": [
      {"send_id": 11, "receive_id": 21, "algorithm": "aes128_cmac", "secret": "replace-with-shared-secret"}
    ]
  }
}
```

对端使用相同的算法和秘密，交换发送与接收 ID。另可选 `hmac_sha1`。每个邻居内的发送 ID 和接收 ID 分别唯一。`exclude_options` 缺省为 `false`，两端应保持一致。

1. 在两端各加入使用新 ID 的密钥，保留旧密钥和当前首选 ID。
2. 把两端的首选接收与发送 ID 改为新密钥。`ipconfig /bgp` 显示实际发送与首选接收 ID，可在两端查看切换结果。
3. 确认两端都用新密钥持续收发后，再删除旧密钥。配置导出及备份包含秘密，须按授权配置文件管理。
`import` 缺省或为 null 时保留接收记录，但不采用该邻居的路由。上例只允许指定 /24，其他目标拒绝。
规则按顺序首个匹配生效，可组合前缀及长度范围、AS 包含条件和普通/扩展/大团体条件；同条规则的所有条件都须满足。
允许动作可覆盖选路优先级及全局下一跳。IPv4 路由也可指定 IPv6 下一跳。`import.preference` 越大越优，允许为零；它与邻居的管理距离 `preference` 含义不同。
内部邻居默认保留收到的 LOCAL_PREF，规则可显式覆盖。原始接收属性不被改写。
`resolve_via_default` 明确允许通过默认路由解析下一跳，缺省为 false；允许规则本身不构成可达性证明。

`ipconfig /bgp` 查询实际连接状态、最后错误、各邻居原始接收数量、入口策略是否配置及待应用状态；该查询需要现有 netdev 授权端点。
“已确认选择”是已确认的 BGP 路由选择数量，仍可能被全局路由策略或其他来源优先，不能当作最终转发条目数。
IPv4/IPv6 邻居支持两族路由学习、发布和撤回，以及路由反射和 AS 联盟。来宾运行和外部协议互通仍待验收。[BGP 契约](../../../docs/architecture-contract/bgp.md)

每个邻居独立配置 `import` 和 `export`。上例只接收 `198.51.100.0/24`，只允许向该邻居发布 `203.0.113.0/24`，并把 MED 设为 20。`networks` 中的网段须有本机路由表里的精确路由才能主动发布。省略 `export` 时不发布。
出口规则沿用入口的匹配字段和动作；保留团体、AS 去环和 iBGP 分割水平不能被允许规则绕过。
`advertisement_seconds` 设置同目标宣告/撤回的最短间隔，默认外部邻居 30 秒、内部邻居 5 秒，零关闭限速。
`ipconfig /bgp` 的发布量只统计完整送入当前 TCP 的宣告，不表示对端确认；超出消息上限的目标显示最近拒绝原因。

要启用路由反射，在同 AS 邻居上设置 `"route_reflector_client":true`，并配置明确的入口和出口策略。其他邻居默认是非客户端。
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
成功表示请求已排队。双方支持增强刷新时，收到结束标记后撤回对端遗漏的路由。

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

BGP 的 `"redistribute":{"connected":true,"protocols":[4,188]}` 发布实际直连网段及指定来源的当前已选双栈路由。未配置或设为 `null` 时停止发布这些来源。
协议值取提供者元数据，示例中的 4 为静态来源，188 为 OSPF 来源。空选择和 BGP 来源 186 会被拒绝，以避免循环重发布。
只发布可用或显式丢弃的路由；来源、度量或可用性变化后更新或撤回，仍须邻居出口政策允许。本地 MED 采用来源实际度量。
`ipconfig /bgp` 区分学习选择、本机网段与其他来源候选；逐网段查询显示来源和度量，实际逐邻居发布结果仍以完整 TCP 发送记录为准。

入口和出口规则均可配置 `"as_path_regex":"_65001_"`，匹配经过完整 AS 65001 的路径；`"^65001_"` 匹配以它开始，`"_65001$"` 匹配以它结束，`"^$"` 匹配空路径。
该条件与网段、AS 包含及团体条件同时满足才命中；入口检查原始有效路径，出口检查已采用的路径，均先匹配再执行本条动作。
支持 ASCII 分组、分支、字符类和重复；不支持环视、反向引用或 Unicode，非法/过于复杂的表达式在导入时拒绝。完整边界见[路径筛选契约](../../../docs/architecture-contract/router-configuration.md#bgp-as-路径筛选)。

对端不识别 OPEN 能力参数时，允许 IPv4 的邻居会按标准改用无可选参数的 OPEN 重试；不会把扩展消息、刷新或四字节路径当成已协商能力。
必需能力不相容或再次被拒时，`ipconfig /bgp` 显示协议错误和“协商已暂停”。修正配置后修改本机 BGP 配置，或关闭再启用 BGP；重新导入完全相同配置不会重建会话。

### 网桥与 VLAN

用 `ipconfig /export` 导出完整配置，在 `interfaces` 中给网桥成员建立接口配置名，再添加 `bridges`。保存后用 `ipconfig /import 文件 预期代际` 提交。物理成员的接口配置只指定设备、管理状态或 MTU，移除成员上的 IP、默认路由和 DHCP 配置。

例如将接入口 `desk` 接入 VLAN 10，将 `uplink` 用作带标签的上联。两个名称都须引用 `interfaces` 中现有对象。

```json
{
  "bridges": {
    "office": {
      "ports": {
        "desk": {"edge": true, "pvid": 10, "vlans": {"10": "untagged"}},
        "uplink": {"vlans": {"10": "tagged", "20": "tagged"}}
      }
    }
  }
}
```

`pvid` 为无标签数据帧指定 VLAN，省略时拒绝无标签数据入口。`isolated: true` 可让隔离端口只与普通端口通信。

网桥默认启用 RSTP。所有 VLAN 共用一棵生成树。将只连接终端的端口设为 `edge: true` 可立即转发，收到 BPDU 后会取消边缘状态。连接共享介质时设置 `point_to_point: false`。
可通过较低的桥 `priority`（0 至 61440，步长 4096）优先选根，通过较低的端口 `path_cost` 优先使用该链路。仅在确认拓扑无环时使用 `rstp: false`。

在上述 `office` 网桥中加入以下 `gateways` 配置，可为两个 VLAN 分配双栈网关，并让 VLAN 10 的设备自动获得网络设置。保持相应的 `ipv4_forwarding` 或 `ipv6_forwarding` 开关启用即可跨 VLAN 转发。

```json
"gateways": {
  "10": {
    "name": "lan10",
    "address": "192.168.10.1",
    "prefix_length": 24,
    "ipv6": [{"address": "fd10::1", "prefix_length": 64}],
    "dhcp_v4_server": {
      "first": "192.168.10.100", "last": "192.168.10.199",
      "router": "192.168.10.1", "dns": ["192.168.10.53"], "lease_seconds": 3600
    },
    "advertise": {
      "mode": "enabled", "max_interval_seconds": 60,
      "router_lifetime_seconds": 180, "dns": ["fd10::53"]
    }
  },
  "20": {
    "name": "lan20",
    "address": "192.168.20.1",
    "prefix_length": 24,
    "ipv6": [{"address": "fd20::1", "prefix_length": 64}]
  }
}
```

`name` 是系统接口名，可用于查看地址、邻居和抓包。上述示例假设 VLAN 10 可访问配置中的 DNS。用 `ipconfig /leases lan10` 查看 IPv4 租约。
启用 IPv6 转发后，VLAN 10 也可自动获得静态前缀、默认网关及 DNS。VLAN 20 使用客户端静态配置。[网桥契约](../../../docs/architecture-contract/switching.md#vlan-网关)

在静态路由的 `target.interface` 中使用 `lan20`，可将它作为下一跳所在的接口。选表规则和防火墙的 `ingress` 可填写 `lan10`，只匹配该 VLAN 收到的流量。
在同一次 `ipconfig /import` 中提交网关及引用它的路由和规则即可。删除或重命名网关时，在同一份配置中调整其引用。
