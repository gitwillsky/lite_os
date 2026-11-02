# 计划：服务拓扑对齐 Fuchsia，linuxd 可插拔

Status: Active

执行期间存在；完成后把持久事实迁入架构/契约文档并删除本文件。

## 目标

linuxd 与 Starnix 一样只是消费者：不运行 linuxd 时其余系统服务照常工作，linuxd 与 netstack 均可独立重启。
内核不变。依赖方向改为：driver-host 发布设备 → 原生服务各自取用 → linuxd 只消费路由给它的 capability。

## 决策

| # | 决策 | Fuchsia 对应 |
|---|---|---|
| D1 | 设备目录改为 driver-host 持续服务的目录协议：`Watch` 列出现有并推送新增设备（稳定 ID、类、描述），`Open(id)` 建立新会话，`Scope(classes)` 派生只含指定类的目录。会话可重开；独占类同时只有一个会话 | devfs / 服务目录 |
| D2 | root-server 持有完整目录并按类路由：netstack←Network；fshost←Block；linuxd←Display/Input/Pcm/Port/Entropy/Usb/Boot | component_manager 能力路由 |
| D3 | root-server 提供服务目录：消费者按名称 `Connect` 得到当前实例的新连接。被依赖的服务重启后，消费者在 `PEER_CLOSED` 后重连，不需要知道重启 | `/svc` 协议路由 |
| D4 | netstack 自己从 Network 目录接入网卡；重启后重新打开会话。linuxd 只通过服务目录取得 socket 提供者 | netstack + netcfg Installer |
| D5 | 新增原生 fshost：拥有 Block 目录，经 ext4d 挂载根文件系统，向 linuxd 提供根目录、挂载与块会话的代理；Linux 现有块设备 UAPI 与 `mount -t ext4` 语义经它实现 | fshost |
| D6 | 接口配置与 DHCP 移到原生 netcfg 角色；配置文件格式沿用 interfaces(5)，经 fshost 读取；控制面板经授权的服务协议请求重新配置 | netcfg + dhcp-client |
| D7 | root-server 监督策略：netstack、linuxd 退出后重启，10 秒内 5 次则判定崩溃循环并关机；driver-host、fshost 退出仍关机 | component restart policy |

## 步骤 → 验证点

1. 设备目录协议（D1、D2）：device-protocol 目录协议与各类会话可重开；driver-host 服务目录；root-server 路由。
   → 两架构 clippy；目录协议单元测试（Scope 过滤、独占会话、会话结束回收）。
2. 服务目录与 netstack 解耦（D3、D4、D7 的 netstack 部分）：root-server 服务目录与重启监督；netstack 自行接入网卡；
   linuxd 改为经服务目录连接、断开后重建各网络域并让旧 socket 按 Linux 语义失败。
   → clippy；linuxd 网络单元测试；guest 网络门禁。
3. fshost（D5）：新服务；根挂载、Linux `mount`/块设备 UAPI 经 fshost；ext4d 只被 fshost 调用。
   → clippy；ext4/块设备门禁（`LITEOS_BLOCK_*`、`LITEOS_MOUNT_*`）。
4. linuxd 可插拔（D2、D7 的 linuxd 部分）：linuxd 只持有路由给它的 capability；退出后 root-server 重启它，
   其他服务不受影响；启动进度由各自 owner 推进。
   → clippy；guest 启动与桌面门禁。
5. netcfg（D6）：原生 DHCP 与静态配置；控制面板改用授权协议；删除 network-service 脚本与 udhcpc 路径。
   → clippy；网络与桌面门禁。
6. 文档、契约、ABI 矩阵、architecture-check 与完整 `make verify`。

## 进度

- 步骤 1 代码完成：目录协议（`LIST`/`WATCH`/`OPEN`/`SCOPE`）、9 类设备会话化及会话回收（网卡发送预留、PCM 流与事件端点、
  显示 scanout/缓冲/virgl 对象/光标）、driver-host 服务目录、linuxd 按目录打开。目录逻辑依赖原生 channel，宿主单元测试
  无法覆盖，留给 guest 门禁。
- 步骤 2 代码完成：`service-directory` 协议；root-server 路由目录、按类派生设备目录、netstack 监督重启；netstack 自行打开网卡、
  经 outgoing 接受连接；linuxd 经服务目录连接并在断开后重建全部网络域（旧 socket 以 `ENETDOWN` 失败）。
  重启后的接口配置由步骤 5 的 netcfg 恢复。
- 步骤 3 代码完成：新增 fshost（独占块设备会话与 ext4d 控制端；`liteos.storage.Blocks` 交出全盘范围连接，
  `liteos.filesystem.Ext4` 原样转发挂载）；linuxd 的设备范围不再含 Block，ext4d 只被 fshost 调用。
  ext4d 按超级块 UUID 串行化同一文件系统的挂载：使用者重启后的新挂载等待旧挂载 shutdown 完成。
- 步骤 4 代码完成：root-server 监督 linuxd（主成员终止 → 终止整个 Linux Job → 等 Job 结束 → 以新的设备范围与路由目录
  重启；与 netstack 共用崩溃循环判定）。netstack 自己拥有默认网络（全部网卡、回环启用、随 netstack 存续），
  linuxd 的初始网络域只是连上它，linuxd 重启不丢失接口配置。
- 步骤 5 改为完全按 Fuchsia 实现（2026-11-03 确认），D6 修订如下。

## 步骤 5 细化（D6 修订）

| # | 决策 | Fuchsia 对应 |
|---|---|---|
| D6.1 | 原生数据卷：独立磁盘镜像（ext4，卷标 `liteos-data`），fshost 按卷标识别、经 ext4d 挂载，不列入交给 Linux 的块设备清单 | fshost 按分区类型识别 data 分区 |
| D6.2 | 存储能力：fshost 为每个原生服务在数据卷上派生私有目录，root-server 在启动服务时把目录连接作为角色句柄交付；协议为整文件原子读写 | `storage: "persistent"` 路由到 `/data` |
| D6.3 | netstack 只提供协议：`liteos.network.Control`（网络域与 socket）与 `liteos.network.Installer`（安装网卡）；默认网络启动时只有回环 | fuchsia.posix.socket / interfaces.admin.Installer |
| D6.4 | netcfg：从 Network 类目录发现网卡并经 Installer 安装；按 bootfs 只读默认策略（全部以太网口 DHCP）与数据目录中的用户设置配置接口；netstack 重启后重新安装并重配 | netcfg + `/config/data` + `/data` |
| D6.5 | dhcp-client：独立服务，经 netstack 的 packet socket 完成 DHCPv4，向 netcfg 报告租约（地址、掩码、网关、DNS） | dhcp-client 组件 |
| D6.6 | 每个消费者只拿到路由给它的协议：linuxd←Control/Blocks/Ext4/Settings；netcfg←Control/Installer/Dhcp | 按组件的能力路由 |
| D6.7 | 设置：netcfg 提供 `liteos.network.Settings`；linuxd 以 `/dev/network-settings`（root:netdev 0660）适配给控制面板；DNS 由 linuxd 写入 Linux 的 `/etc/resolv.conf` | settings 组件 / Starnix 适配 |
| D6.8 | 删除 network-service、`/etc/network/interfaces`、udhcpc 路径与 inittab 条目 | — |

子步骤：5.1 netstack 协议拆分与安装接口 → 5.2 root-server 通用监督与按消费者路由 → 5.3 netcfg → 5.4 dhcp-client →
5.5 fshost 数据卷与存储能力 → 5.6 linuxd 设置适配与 DNS → 5.7 控制面板与用户态清理 → 5.8 镜像、bootfs 与 QEMU 参数。
