# Display、input 与 terminal architecture contract

> 权威入口：[architecture-contract.md](../architecture-contract.md)
>
> 机器读取的依赖矩阵、状态 owner、持久 `FallibleMap` 清单和 source-size review
> 继续保留在入口文件；本文承载本领域的详细 interface/capability 证明。

## 1. Device backing 与 display adapter

- `memory::DeviceBacking` 是 device-shared 非连续页的唯一 owner/interface；`try_allocate` 以精确 page count 和 allocation class 构造，最大 256 个 extent、单 extent 最大 64 页，按可满足剩余数量的最大 buddy order 下降尝试，任何中途失败都只靠已构造 `FrameTracker` 的 RAII 回滚。`page/pages` 只供 device VMA index translation，`extent/extent_count` 只供 VirtIO SG attach；禁止向 DRM、VMA 或 driver 暴露内部 Vec、复制 PPN 列表，或建立第二份 backing。
- `drivers::display` 是 DRM 可见的唯一显示 seam；VirtIO resource ID、command header、descriptor head 与 MMIO device 不得穿过该 seam。启动期同步 bootstrap 只允许发生在 IRQ/scheduler publication 前；运行期 config event 与 controlq completion 只发布合并 display softirq。
- GET_DISPLAY_INFO 只更新 connector preferred mode 并发布 hotplug，不触发 modeset；scanout、damage 与 disable 各自产生唯一 fence，分别异步推进完整 resource transaction、TRANSFER+FLUSH 与 SET(resource_id=0)+UNREF。scanout 捕获提交 mode 与同一 `DeviceBacking` Arc，旧 backing 只在 UNREF completion 后释放；禁止 syscall/DRM 自旋、CPU 全帧 memcpy、永久 fallback 或在拖动期间内核自动分配 framebuffer。

## 2. DRM 与 kobject hotplug

- `drm::DrmFile` 是 devfs/OFD 与 DRM domain 的唯一 backend seam；syscall 只编解码固定 Linux UAPI、copy user arrays、等待 `DrmWait`/event，不读取 display adapter。每个 OFD 的 `FallibleMap` 是唯一 GEM handle namespace；device VMA、framebuffer 与 adapter resource 以 Arc 保活同一 SG backing。
- device state 唯一拥有 master、active object 与 pending fence。`device::{submit_scanout,submit_damage,submit_disable}` 只允许 parent DRM lifecycle 调用，分别是 backing switch、bounded clip flush 与 resource_id=0 disable 的唯一 publication transaction。任一时刻只有一个 pending operation；RMFB active object 与 OFD close 必须先完成 disable，再零分配删除 object，不存在 fallback framebuffer。
- DIRTYFB 单次最多导入 32 个 clip 并等待 TRANSFER+FLUSH；page flip 只表示 framebuffer switch。每个 OFD 固定 4 KiB event ring 是唯一 event-space owner，completion 无分配发布完整 event。atomic、auth/lease、vblank wait 未完成时不得伪造相应 capability。
- `socket::kobject::KobjectRegistry/KobjectSocket` 唯一拥有 group-1 listener membership、sequence、固定消息 queue 与 latest-event coalescing。registry 只持 Weak，endpoint close 禁止反向获取 registry lock；dead Weak 必须由 new/publish 的 allocation-free `retain` 回收。DRM 只调用 `socket::publish_drm_hotplug`；它不持有 listener、queue 或 notification state。queue 满时只能替换 coalesced latest slot，readiness 只在 empty→non-empty 时 signal；事件路径禁止分配和重复 edge。
- `socket::observation` 是 sealed `SocketBackend` 到 local/peer address、poll state、readiness generation 与 wait source 的唯一只读投影；这些 scoped methods 虽位于 child module，调用者仍只看到 parent façade。该 module 不拥有 endpoint 状态、不分配、不缓存 readiness，也不得把 concrete backend variant 穿过 fs/syscall seam。

## 3. Input、PTY 与 terminal userspace

- `drivers::input` 是 input domain 可见的唯一 raw-event seam；VirtIO selector/eventq/DMA slot 不得穿过。`input::InputFile` 是 evdev 唯一 backend，device/client state 与每-OFD queue/clock/grab ownership 不得复制。
- PTY byte Pipe 与 notification Pipe 必须分离；前者拥有 output capacity，后者只承载 state edge。Terminal、Process controlling handle 与 process graph SID/PGID 各自保持单 owner，master close consequence 只经 task seam 发布。
- `user/` 顶层只允许固定 BusyBox identity/init/network policy、C musl probes、单 ELF `liteos-stress` 与唯一 `liteos-terminal/` crate。terminal 必须保持无 dependency 的 `no_std` staticlib、标准 `src/lib.rs` 布局和精确六源文件集合，由 nightly Cargo 以 Linux-musl target 重建 PIC `core/compiler_builtins`，再经既有 musl CRT/libc driver 链接为动态 PIE。
- root workspace 必须显式 exclude terminal crate。该 crate 不得拥有 linker script、私有 syscall/runtime、init 或第二 rootfs build track；旧 C terminal、rootfs font 文件与 `build-user`/旧 init artifact 均禁止恢复。init 是 session restart policy 的唯一 owner。
- terminal reactor 必须保持 single-thread poll；PTY 每 turn 最多 64 KiB、keyboard output 只经固定 4 KiB ring+`POLLOUT` backpressure、render 最多 60 fps、idle 无限等待。字体 metrics 只允许由 checked atlas v2 header 构造并在 session 内保持不变，任何 display mode/窗口尺寸都不得选择或合成另一套字号。resize 只由 netlink 触发并采用 50 ms latest-mode quiet period、query-build-query 与 pre-commit failure atomicity；成功 SETCRTC 后才提交 model、回收旧 buffer 与设置含 pixel dimensions 的 PTY winsize。DIRTYFB 最多 32 clips；禁止 periodic connector poll、拖动中 modeset、shadow framebuffer、无界 input queue 或用 page flip 提交同一 framebuffer damage。
