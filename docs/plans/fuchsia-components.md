# 计划：组件划分与命名对齐 Fuchsia

Status: Active

执行期间存在；完成后把持久事实迁入架构/契约文档并删除本文件。

## 目标

在已对齐的服务拓扑（能力路由、服务目录、linuxd 可插拔）之上，把剩余的进程划分、所有权与命名对齐 Fuchsia：
驱动按节点隔离并可按声明重启；文件内容的页缓存由文件系统服务拥有；设备侧机制（显示合成提交、GPU 资源、混音）
归设备服务，linuxd 只做 Linux UAPI 翻译。内核不变。

## 决策（2026-11-03 用户确认“全部按 Fuchsia 做”）

| # | 决策 | Fuchsia 对应 |
|---|---|---|
| D1 | 进程改名：`root-server`→`component-manager`，`ext4d`→`ext4`；`linuxd` 保留；`create_fs.py` 移入 `scripts/` | component_manager；fxfs/minfs 以文件系统命名 |
| D2 | 驱动框架拆为 `driver-manager` 与 `driver-host`：manager 拥有根 resource、DTB/PCI 平台事实、节点拓扑、驱动匹配与设备目录；每个绑定的驱动在自己的 `driver-host` 进程中运行，只拿到所属节点的 MMIO/IRQ resource 与 BTI | DFv2 driver_manager / driver_host |
| D3 | 驱动声明 `host_restart_on_crash`：宿主崩溃后 manager 重新创建宿主并重绑节点；未声明的节点从拓扑移除（目录发出移除，会话 `PEER_CLOSED`）。父驱动的子节点（如 xHCI 下的 USB 功能）与父驱动同宿主（colocate） | `host_restart_on_crash` / `colocate` |
| D4 | driver-manager 与 fshost、ext4 是关键组件：退出即关机 | critical component（重启系统） |
| D5 | 页缓存归文件系统：`ext4` 为打开的文件创建 pager VMO 并补页/回写；linuxd 经原生文件协议取得文件 VMO（`GetBackingMemory`）做 read/write/mmap，不再自建持久文件页缓存。`/dev/fuse` 仍服务 Linux FUSE 守护进程 | fuchsia.io + fxfs pager；Starnix RemoteFs |
| D8 | fshost 为每个挂载的卷启动一个 `ext4` 实例（component-manager 只交给 fshost 启动所需的 job 与映像）；实例崩溃只影响其卷 | fshost 启动 fxfs/minfs 实例 |
| D6 | 显示与 GPU 资源归设备侧：扫描输出、页翻转提交与完成、GPU 资源寿命由显示/GPU 服务拥有；linuxd 的 DRM 只翻译 Linux UAPI（ioctl、sync_file、事件读取） | display coordinator、Magma；Starnix 只翻译 |
| D7 | 混音归音频服务：多路 PCM 混音与设备流由原生音频服务拥有；linuxd 的 ALSA 只翻译 PCM/control UAPI | audio_core；Starnix 只翻译 |

## 进度

- 阶段 1 完成：改名与 `scripts/create_fs.py`。
- 阶段 2 完成：`driver-manager`（平台事实、PCI 枚举、节点匹配、宿主监督、汇聚目录）与每节点一个
  `driver-host`；目录协议增加转发条目与移除；内核 PMT 隔离与 `BTI_RELEASE_QUARANTINE`（Zircon 语义）；
  `CrashLoop` 移入 `native_runtime::task`。`restart_on_crash` 目前只声明给熵源：网卡、输入、声音与
  console 端口的使用者尚无设备移除语义（netstack 不撤下接口、evdev/ALSA 节点不撤下），按默认语义移除节点。

## 阶段与验证点

1. D1 改名与目录 → 两架构 clippy、architecture-check 通过。
2. D2–D4 驱动框架 → 设备目录与会话行为不变；人为终止可重启驱动的宿主后设备重新出现。
3. D5 文件协议与页缓存迁移 → 文件读写、mmap 共享/私有、回写与 fsync 语义不变。
4. D6–D7 显示/GPU 与音频 → 桌面、GPU、音频门禁不变。
5. 文档与契约迁移，完整 `make verify`。
