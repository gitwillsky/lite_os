# 内核可插拔性重构计划

Status: Active

消除架构审计发现的封闭枚举、单实例主设备、写死挂载与不一致的能力注入。完成后把持久事实迁入
领域文档并删除本计划。

## 审计发现

- F1 设备模型是封闭枚举：`fs::DeviceKind`、字符设备后端、devfs 节点名、`sys_ioctl`、
  `cpu::DeferredWork` 与 task 延迟分派按设备种类写死，新增设备需改六处。
- F2 驱动注册层不统一：块设备单槽、输入注册表、其余单例；音频/RNG/virtio-port 注册绑死
  具体 VirtIO 类型；`dispatch_io_completion_work` 写死完成队列设备。
- F3 没有 `mount(2)`/`umount2(2)`：内核写死挂载 `/dev`、`/dev/pts`、`/proc`、`/sys`。
- F4 没有内核命令行：`/bin/init`、根设备与 console 写死。
- F5 能力注入签名不一致：可阻塞 pipe 依赖 task，下层只能由 composition root 注入。
- F6 `PlatformConsole` 适配器位于 `main.rs`。
- F7 初始化顺序只由注释约束。

## 步骤 → 验证点

1. F5：pipe 阻塞下沉到 `sync` 的 task wait adapter，删除 pipe/notification 工厂注入 →
   双架构 clippy、kernel-unit 与 boot gate。
2. F1：fs 拥有字符设备注册表（major/minor、devfs 名称、`open`）；打开的设备文件经
   `DeviceFile` trait 提供 read/write/readiness/ioctl/mmap；devfs 枚举注册表；设备 ioctl
   UAPI 随设备子系统迁出 syscall；删除 `DeviceKind` 与后端枚举 → fs/syscall 不再依赖设备子系统。
3. F1：延迟工作改为固定容量的 handler 注册，删除 `DeferredWork` 设备变体 → task/cpu 不再依赖
   设备子系统。
4. F2：驱动注册统一为按 trait 的多实例注册表，完成队列由注册设备自报 → 删除具体 VirtIO 类型注册。
5. F3：`mount`/`umount2` 与按名称的文件系统类型；`/proc`、`/sys`、`/dev/pts` 由 init 挂载 →
   BusyBox gate。
6. F4：解析 DTB `/chosen/bootargs` 的 `init=`、`root=`、`console=` → boot gate。
7. F6/F7：console 适配器归位；初始化顺序由类型或围栏约束 → `make verify`。

## 进度

- 步骤 1–3 落地：字符设备注册表（driver 区间 + devfs 节点）、`DeviceFile` 与 `UserOutput`/`UserInput`
  游标、mem/TTY/evdev/DRM/ALSA/virtio-port 设备文件、`JobControl` hook、`syscall_abi::{errno, signal}`、
  设备类 deferred vector 注册。持久事实已写入 `architecture/devices-terminal.md`、设备与终端契约、依赖表
  与 `syscall-support/filesystem-io.md`。
- 步骤 4 落地：`drivers::registry` 统一设备类注册表、`PortDevice`/`EntropySource` seam、`CompletionSource`
  自报与按实例分配的 `IoDevice`。
- 下一步：步骤 5（F3）。
