# Display、input 与 terminal 当前架构

> 权威入口：[architecture.md](../architecture.md)
>
> 本文只记录当前实现事实；稳定 owner、依赖与 capability 约束见
> [display contract](../architecture-contract/display-terminal.md)，Linux ABI 状态见
> [syscall-support.md](../syscall-support.md)。

## 1. VirtIO-GPU 与 DRM/KMS

- VirtIO-GPU 2D adapter 查询第一个 enabled scanout，并在 scheduler/IRQ publication 前用 `DeviceBacking` 完成唯一同步 boot scanout。`DeviceBacking` 精确拥有 page count，以不超过 256 KiB 的 buddy extents 组成最多 256 项 SG 表；GEM、VMA、framebuffer 与 GPU resource 都只 clone 同一个 Arc。首个 userspace SETCRTC 替换 boot resource，之后不保留 fallback。
- 运行期 config/vring interrupt 只发布合并 display softirq；deferred context 每次有界推进一个 completion。GET_DISPLAY_INFO 立即更新 connector preferred mode，但与 active CRTC mode 相互独立且绝不自动 modeset；变化经 `AF_NETLINK/NETLINK_KOBJECT_UEVENT` group 1 发布标准 NUL-separated DRM hotplug。每个 listener 使用固定 16×256-byte queue，满时只合并最新 event，Pipe 只发布 empty→non-empty 边沿；关闭留下的 dead Weak 由下次创建/广播无分配清扫，因此 resize storm 不产生重复唤醒、分配或 registry-lock 自死锁。
- 两个固定 resource ID 支持三类异步 operation：新 scanout 的 CREATE→ATTACH(SG)→TRANSFER→SET_SCANOUT→FLUSH→旧 UNREF、DIRTYFB 的 bounded TRANSFER→FLUSH，以及 disable 的 SET_SCANOUT(resource_id=0)→UNREF。每次 submission 捕获 backing/mode/rectangles，最终 fence 才发布 active state 与 Pipe edge；旧 backing 只在 UNREF completion 后释放。
- devfs 发布标准 primary node `/dev/dri/card0`（226:0）。query 支持 VERSION、GET_CAP、GETRESOURCES、GETCRTC、GETENCODER 与 GETCONNECTOR；CRTC/encoder/virtual-connector identity 固定，connector 返回当前 preferred mode，active CRTC 返回最后成功 SETCRTC 捕获的 mode。DRM owner 只消费通用 display seam，syscall 只消费 `DrmWait`，没有 adapter 泄漏或 `drm ↔ task` 反向依赖。
- 每个 card OFD 独立拥有 GEM handle map。CREATE_DUMB 创建 linear XRGB8888/bpp=32、page-aligned SG backing；MAP_DUMB 返回 file-private fake offset，`mmap(MAP_SHARED)` 建立按 page index 投影的 device VMA。handle DESTROY/close 后，既有 VMA、framebuffer 或 GPU resource 继续保活同一 Arc；最后 owner 才逐 extent 回收。device VMA 不进入 page cache、writeback 或 anonymous reclaim，也禁止 executable permission 与 MADV_DONTNEED。
- device-wide framebuffer map 支持 legacy ADDFB/GETFB/RMFB 与 XRGB8888 single-plane ADDFB2；primary node 第一个 open 自动成为 master，SET/DROP_MASTER 遵循 current/was-master 与 effective-root 边界。SETCRTC/PAGE_FLIP/DIRTYFB 只允许 current master，任一时刻只允许一个 pending display operation；active RMFB、disable 与 OFD close 必须先等待 resource_id=0 transaction，随后零分配删除 object。
- DIRTYFB 接受 Linux `drm_mode_fb_dirty_cmd`，annotations 只按标准 mask 接受，单次最多 copyin 32 个 `drm_clip_rect`，空 clip 表示 full framebuffer；返回前等待 TRANSFER+FLUSH fence。PAGE_FLIP 只表示 framebuffer switch，支持 flags=0 与 `DRM_MODE_PAGE_FLIP_EVENT`；completion 在每-OFD 固定 4 KiB ring 中无分配发布完整 event。target/async flags、auth/lease、vblank wait 与 atomic KMS 尚未发布。

## 2. VirtIO-input、evdev 与 Unix98 PTY

- modern MMIO v2 adapter 读取 name/serial/device ID、property/event bitmap 与 absolute-axis limits，并为每个设备发布稳定 physical path。`EV_SYN` 是 Linux input core 固有能力，即使 VirtIO config 不重复声明也会进入 event-type bitmap；未知 type/code 在进入 evdev state 前丢弃。
- eventq 固定最多 64 descriptors，并预留 `queue_size/2` 个永久 8-byte DMA slot，以证明任一跨页 buffer 都有两个 descriptor。descriptor head 到 slot 使用 O(1) index；hardirq 只 ack vring/config status 并发布 input softirq，deferred context 每轮每设备最多消费 64 个 event、立即 repost 同一 slot 并在批末只 notify 一次，持续指针流不会分配或忙等。
- input core 为每个 adapter 唯一维护 live key/absolute state、client weak registry 与 exclusive grab；每个 `/dev/input/eventN` OFD 独立维护 64-entry ring 和 REALTIME/MONOTONIC/BOOTTIME clock。只有到 `SYN_REPORT` 为止的完整 packet 可读，空 report 被丢弃；overflow、clock change 或 state ioctl copyout failure 以 `SYN_DROPPED` 要求 userspace 重取状态。
- devfs 发布 Linux input major 13、minor 64+N。RV64 `struct input_event` 固定 24 bytes；read/readv 支持 blocking、`O_NONBLOCK`、整数 event 边界与 partial copy，pselect/ppoll/epoll 共用 device Pipe notification 和 level recheck。ioctl 支持 VERSION/ID/NAME/PHYS/UNIQ/PROP/BIT/KEY/ABS、CLOCKID 与 GRAB；write/statusq、LED/sound/force-feedback injection、event mask/revoke、multitouch slot state、runtime config change 与 hot-unplug 尚未开放。
- `/dev/ptmx` 每次 open 分配一个锁定的 Unix98 pair；`TIOCGPTN/TIOCSPTLCK` 是 slave index 与 unlock 的唯一 ABI。独立 devpts filesystem 挂载在 `/dev/pts`，lookup/getdents 只投影仍有 live master 的动态节点；master 最后关闭后节点立即消失，最后一个 endpoint 释放后 index 才可复用。
- pair 的单一 lifecycle lock 同时拥有 lock、master-open 与 slave-open count。master→slave raw input 进入同一个 Terminal line discipline；slave→master output 使用独立 64 KiB byte Pipe，小块原子写与真实 write-capacity wait 保证 CRLF 转换不会部分提交，阻塞/nonblocking write 不会返回伪零进度。两端另有不进入字节流的一字节 readiness Pipe，slave `POLLOUT` 直接复查 byte Pipe 容量，因此 input/output、hangup、poll/epoll generation 不会靠伪字节同步。slave 最后关闭使 master read 返回 `EIO/POLLHUP`；master 最后关闭使 slave read 返回 EOF、write `EIO/POLLHUP`，并经 composition-root 注入的 task seam 对 foreground group 发布 SIGHUP/SIGCONT。
- Process 以单锁持有当前 controlling-Terminal Arc；成功 `TIOCSCTTY` 原子替换，fork 继承，`/dev/tty` 始终投影该 handle。Terminal 自己仍唯一拥有 controlling SID、foreground PGID、termios 与 cooked queue，process graph 仍唯一拥有 SID/PGID membership，未复制 job-control 状态。

## 3. Rust display terminal

- `/bin/liteos-terminal` 是唯一 display-session/terminal owner。它是 dependency-free `no_std` Rust staticlib，经 Linux-musl target 的 PIC `core/compiler_builtins` 与既有 musl CRT/libc 链接为动态 PIE，不含私有 syscall、allocator runtime、linker script 或第二 rootfs build track。启动时订阅 netlink hotplug、查询 DRM topology、扫描 keyboard evdev、nonblocking 回放 `/dev/kmsg`，在首次 SETCRTC 前完成整帧渲染，随后通过 Unix98 PTY 启动 login-style ash；init 是唯一 restart policy owner。
- 字体由显式 `make regen-font` 从固定 JetBrains Mono NL Medium/Bold 源生成并校验为 checked-in A8 atlas；普通构建只嵌入 16×32、464 个严格升序 codepoint 的两-face atlas，rootfs 不安装 TTF/PSF/第二份 atlas。atlas v2 header 是 cell metrics 的唯一事实源，terminal session 内 metrics 不变；display resize 只增加或减少网格行列，禁止把 framebuffer 分辨率解释为 DPI 或隐式缩放字号。每个 cell 固定 16 bytes，保存 Unicode codepoint、XRGB foreground/background 与 bold/dim/underline/inverse/hidden；parser 支持增量 UTF-8 replacement、常用 CSI、16/256/truecolor SGR、deferred autowrap 与 `?1049` alternate screen。soft-wrap 标记复用 cell 保留位：primary resize 按可见逻辑行 reflow，alternate resize 清屏并由 SIGWINCH 驱动应用重绘，无 scrollback 或隐藏 shadow grid。
- 单线程 reactor 只 poll PTY、evdev、nonblocking netlink 与两个 monotonic deadline；空闲时无限阻塞，不轮询 connector。PTY 每轮最多消费 64 KiB，keyboard→PTY 使用固定 4 KiB ring 与 `POLLOUT` backpressure，`SYN_DROPPED` 会清除不可信 modifier snapshot，不静默丢弃一次已导入的 key sequence。首次内容变更立即调度，之后最多 60 fps；renderer 只重绘 row dirty span 和 cursor，并把最多 32 个 clip 交给 blocking DIRTYFB，不用 page flip 冒充 damage。TIOCSWINSZ 同时提交 rows/columns 与真实 pixel width/height。
- hotplug 以 latest-mode-wins 重置 50 ms quiet deadline，随后执行 GETCONNECTOR→构造候选 model/framebuffer→GETCONNECTOR；mode 未变才 SETCRTC，成功后释放旧 framebuffer并 TIOCSWINSZ/SIGWINCH。单 framebuffer 大小上限为 `min(MemTotal/8, 32 MiB)`；超限或 pre-commit OOM 保留完整旧状态且同 mode 只警告一次，post-commit PTY 同步失败则退出让 init 干净重建。

当前 terminal 是紧凑 terminal emulator，不是 compositor。下一个显示竖切是明确的 DRM-master/session handoff 与可独立退出的 graphics client，再由首个真实 2D/3D consumer 决定 atomic KMS、render node、buffer sharing 或 input seat 的最小扩展。
