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
- D8 完成：fshost 持有 ext4 映像与实例 Job，每块盘一个实例（同盘再挂载交给同一实例，实例内按 UUID 串行化会话），
  数据卷独占一个实例且终止即 fshost 失败停机；linuxd 经卷挂载协议按盘序号挂载。
- D5 方案（Starnix RemoteFs + fxfs）：ext4 为每个文件维持 pager VMO，是文件大小与内容的唯一 owner；READ 经
  VMO（缺页由 ext4 补页），WRITE 直写存储并同步 VMO（块分配与 ENOSPC 同步报告），截断/分配调整 VMO；
  mmap 写脏的页由 ext4 在 fsync、syncfs、周期回写与节点释放时写回。linuxd 对 ext4 服务挂载的文件不建
  页缓存：read/write 经 FUSE READ/WRITE，mmap 映射挂载控制 `GET_BACKING_MEMORY(nodeid)` 交出的 VMO。
  `/dev/fuse` 的 Linux 守护进程仍用 linuxd 页缓存。pager 分派线程池抽为两者共用的 crate。
- D5 完成（两架构 clippy 通过）：`pager-pool` crate；ext4 `content`（每文件 pager VMO、直写同步、截断/分配
  调整、FORGET/拆除/5 秒周期回写）；挂载控制 `BACKING_MEMORY`；ext4 不再协商 `WRITEBACK_CACHE`；linuxd
  `DataBacking::Remote`、`Inode::backing_memory` 与 `page_cache::remote_file`。
- D6/D7 待定：linuxd 的 DRM KMS 状态机（活动扫描输出、翻转完成、fence 时间线、主控关闭回退）与 ALSA 混音引擎
  （8 子流、ALSA 状态与软件参数）都与 Linux UAPI 状态同锁耦合；下放需新的显示提交协议与渲染协议（共享环形缓冲
  VMO、应用/硬件指针经共享内存），涉及实时路径，Starnix 不实现 ALSA，没有可直接沿用的先例。

## 阶段与验证点

1. D1 改名与目录 → 两架构 clippy、architecture-check 通过。
2. D2–D4 驱动框架 → 设备目录与会话行为不变；人为终止可重启驱动的宿主后设备重新出现。
3. D5 文件协议与页缓存迁移 → 文件读写、mmap 共享/私有、回写与 fsync 语义不变。
4. D6–D7 显示/GPU 与音频 → 桌面、GPU、音频门禁不变。
5. 文档与契约迁移，完整 `make verify`。
