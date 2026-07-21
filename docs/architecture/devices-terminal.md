# 设备与终端当前架构

## 当前设计

- `platform` 发现并装配具体 adapter；`drivers` 只公开 block、network、display、input、RTC、RNG 与 interrupt 等通用 seam。
- VirtIO queue 与 DMA payload 由各 adapter 拥有；block/RNG 的 request slot、descriptor identity、
  lost-wake handshake 与 capacity wait 由 `drivers::io_completion` 统一拥有。hardirq 共用
  transport-error latch，只确认 MMIO 并发布 `DriverIo` deferred bit，不进入 ordinary adapter lock，completion 在统一
  user-return/idle safe point 消费；VirtIO-net RX 由单一 slot lifecycle owner 原子
  claim/repost/retire。
- split virtqueue 摘取 used entry 只产生一个 `UsedDescriptor` capability，不立即回收 chain；
  concrete adapter 必须先 claim slot/generation/head 并验证 device-written length/response，随后才
  exactly-once recycle。duplicate、unknown、非法 length 或 sequence completion 永久关闭 queue 并
  reset device。真实 VirtQueue 回归测试固定：单 descriptor completion 在 claim 前 free count 保持
  3/4，合法 recycle 后恢复 4/4，duplicate 保持 4/4、unknown/out-of-range 保持 3/4，均不得二次回收。
- net RX/TX、input、GPU control/damage、block request 与 RNG 固定 buffer 初始化时建立并持有
  `DmaBuffer` 物理 segment mapping；steady-state virtqueue submission 只投影 cached ranges，地址
  空间锁与 page walk 均为 0。adapter drop 写 reset 并等到 status 读回 0 后，再释放这些 mapping。
- QEMU `virt` backend 把 DTB `dma-coherent` 作为必需 machine fact；VirtIO 不维护 non-coherent shadow buffer 或运行时 cache-maintenance fallback。
- GPU runtime completion 由独立 sequence owner 验证 fence/response 与 stage 顺序，阶段分支只选择
  下一条 `GpuCommand`；统一 command seam 负责 wire encoding、长度与 queue publication。
  该层只增加 fixed enum dispatch，不增加 lock、allocation 或 descriptor 数；MMIO completion 主导实际
  latency，因此不新增微基准，改由 architecture cost gate 固定 `poll_update` 的 direct assembly 与
  direct publication 为 0、sequence submission 出口至多 1 个。
- VirtIO-block 使用 16 个固定 DMA request slots，允许乱序 multi-outstanding；同步 caller 在
  task context 睡眠，bootstrap caller 以 trap-PC-resumed external IRQ/WFI 原子等待同一
  completion owner；第 17 个
  caller 进入 FIFO capacity wait，slot release 直接 handoff，不伪造设备故障。
- block completion 消费 used `len`：4 KiB Read 只接受 4097（data+status），Write/Flush 只接受 1
  （status）。短/超长 completion 在接触 status 或返回 read data 前 fail-stop reset，并由 request
  claim owner 的 reject→drain 路径 exactly once 完成和释放所有受影响 slot。
- VirtIO-RNG 使用 4 个固定、提交前不预零的 4 KiB device-write DMA slots；task caller 睡眠，
  创建 init `AT_RANDOM` 的唯一 cold-boot caller WFI。64 KiB `getrandom` 或 entropy-device read 从
  256 个 256-byte batch 降为 16 个 heap-backed 4 KiB batch；固定 64-poll 模型的 MMIO polling/
  spin 从 64/64 降为 0/0，output/DMA 覆盖前预零从 131072/4096 bytes 降为 0/0。
- DRM owner 组合 display operation、GEM/framebuffer、KMS、damage fence、master 与 event；syscall 只编码 Linux DRM UAPI。
- input owner 组合 device state、每-open evdev queue、grab、clock 与 revoke；VirtIO input adapter 只提供 raw event/config。
- PTY registry、pair 与 Terminal session/foreground/winsize 各守自己的 seam；控制面使用标准 PTY、termios、ANSI/ECMA-48。
- 图形 userspace 拆为 `desktop` 与 `terminal` 两个进程：`desktop` 是 DRM master、evdev、合成与窗口管理的
  唯一 owner；`terminal` 是 ANSI parser 与终端 renderer 的唯一 owner，作为桌面客户端运行。两端经
  `display-proto` 协议通信，GEM handle 经 SCM_RIGHTS 共享的同一 OFD 传递，DESTROY 只归桌面。
- 三个应用是标准 Linux/musl `std` binary；文件、Unix socket、process、time 和动态集合直接使用
  `std`。`linux-uapi` 只补足 DRM、evdev、PTY、poll/SCM_RIGHTS 等稳定 `std` 未提供的 Linux UAPI，
  fd/GEM/mapping/child 都由 owned RAII 类型表达。窗口和客户端使用可增长稳定槽位；poll 向量跨事件
  循环复用容量，damage/协议 frame/渲染行仍保持固定 buffer，不在热路径新增分配。
- headless boot 缺少 DRM/input 时，`desktop` 在同一进程内以 5 秒 deadline 退避重试，
  不触发 init respawn 风暴；设备可用时仍只创建唯一 compositor owner。
- terminal font 是 checked A8 atlas；普通构建只消费生成产物，升级由显式 generator 完成。
- 图形资产（壁纸 / bootlogo XRGB、UI 与终端字体 atlas、XP 箭头光标 lc1）不内嵌进
  二进制，随 rootfs 分发到 `/usr/share/liteos/`，由 desktop / terminal / splash 启动时
  读入并 checked 解析；缺失或损坏时 desktop / terminal 启动失败（退避重试），splash
  静默跳过 logo。

## Known limits

- GPU 只开放 VirtIO-GPU 2D resource/scanout/transfer/flush；VirGL、Vulkan、3D context、DRM atomic/auth/lease、完整 evdev output/multitouch 和设备热拔插尚未开放。
- 桌面窗口语义当前覆盖重叠窗口的移动/缩放/最小化/最大化/关闭/置顶/焦点；shell 含任务栏
  （Start/窗口按钮/时钟）、双栏开始菜单（`/etc/startmenu.conf`）、壁纸与关机画面；
  `splash` 在 sysinit 绘制启动画面，桌面首帧后接管。已知 cosmetic 问题：新窗口首帧
  在窗口矩形外偶发残留像素（create/cascade 几何时序），随后续 damage 自愈。
