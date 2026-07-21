# 设备与终端契约

## Owner

- concrete VirtIO adapter 独占 queue、DMA、descriptor、completion 与 reset state；`drivers` 只发布通用 device seam。
- `VirtQueue` 独占 split-ring cursor、descriptor free list 与单一 pending-used latch；`used()` 只摘取
  ring entry 并返回不可复制的 `UsedDescriptor`，`recycle_used()` 只消费当前 queue 的唯一 token。
  adapter slot/generation/head、device length 与 response 尚未验证前，free list 不得改变。
- `virtio_queue::DmaBuffer` 是 fixed bytes 与初始化时 kernel-VA→physical segments 的唯一共同
  owner；adapter slot 从映射建立保持到 completion 或 reset，`VirtQueue` 只消费带 lifetime 的
  cached `DmaSlice`，不拥有、翻译或延长 backing 生命周期。
- VirtIO-GPU `GpuCommand` 是 runtime opcode、wire length 与 completion stage 的唯一共同 owner；
  `sequence` 只从已验证 completion 选择一个后继领域 command 或 terminal retirement，随后由
  `submit_command` 单一出口编码并发布。`poll_update` 不得自行组合 prepare/opcode/length/stage。
- `drivers::io_completion` 是 block/RNG 共用的唯一 request slot、descriptor identity、completion
  handshake 与 capacity membership owner；typed `IoWaitKey { device, kind }` 保留完整 slot、
  generation 与 ticket，不位打包或复制 adapter 私有 wait ABI。block 的 16 个 fixed slots 独占
  request/data/status DMA，RNG 的 4 个 fixed slots 独占 device-write DMA；scheduler 只通过
  `IoWaitTarget` callback 拥有 `WaitMembership::DriverIo`。
- `drm::DrmDevice`/`DrmFile` 独占 display/KMS/GEM/framebuffer/master/event state；`input::EvdevDevice`/`InputFile` 独占 input/client state。
- `fs::pty` 独占 PTY registry/pair；Terminal 独占 session/foreground/termios/winsize；`terminal`（桌面客户端）独占 ANSI parser 与 renderer state。

## Interface

- VirtIO hardirq handler 只读写 interrupt status/ack 并发布合并 deferred bit，不得取得
  queue/control/event lock、遍历 `KERNEL_SPACE` 或回收 descriptor。net、GPU、input 的 ordinary
  adapter lock 只允许 task context 与统一 user-return/idle deferred safe point 进入；禁止为个别
  adapter 增设 IRQ-lock 兼容路径。
- 同步 block 与 entropy request 在 task context 通过 `IoCompletion` handshake 发布
  `WaitMembership::DriverIo` 后睡眠；只有启动期 block I/O 与创建 init `AT_RANDOM` 尚无 current
  task，经 architecture assembly seam 临时补开 SEIE/SSIE/SIE 并执行带固定 resume
  label 的 WFI。hardirq 已发布的 SSIP 作为已确认 device edge 的耐久 wake token；
  external/software trap 若命中 enable-to-WFI 窗口，kernel trap entry 必须把 `sepc`
  从 WFI 精确推进到 resume label，避免唯一 device edge 已确认后重新睡眠；这个
  PC identity 是唯一 race owner，不得增加平行 flag/poll path。精确恢复原状态后调用
  同一 completion consumer。两条路径都不得轮询 MMIO/used ring 或持有 queue lock 等待。
  scheduler processor topology 必须先于 wait-target factory 建立；随后加载 `/bin/init` 的 block I/O
  才能安全观察“无 current task”并选择 bootstrap WFI。反序会让 `current_task()` 永久等待尚未建立
  的 topology。bootstrap wait 每次睡眠前必须检查一次 used ring；若 completion 已在 S-mode external
  delivery 启用前发布，必须经同一 `VirtIoCompletionIrq` owner 先 ack 已断言的 device line 再 reclaim，
  使下一次慢 completion 能产生可唤醒 WFI 的新中断。enable-to-WFI 窗口
  由上述 trap-PC resume 规则关闭；这不是 MMIO polling 或 spin fallback。
- block/RNG hardirq 共用 `VirtIoCompletionIrq`：status/ack 成功时精确确认 bits，读取或 ack 失败时
  发布 transport-error latch，并无条件发布一次 `DriverIo` deferred work；safe point 消费 error 后
  reset/fail 全部 request。吞掉 MMIO error 会让已 claim 的唯一 IRQ edge 后 waiter 永久睡眠。
- platform 是 concrete adapter 的唯一装配者；driver、DRM、input、filesystem 与 syscall 不得依赖 QEMU machine types。
- QEMU `virt` 必须在任何 VirtIO queue publication 前证明 root `dma-coherent`；缺失时 fail-stop，禁止增加 bounce buffer、每次提交 cache flush 或“先运行再探测”的兼容路径。
- DRM/evdev syscall 只编码固定 Linux UAPI。devfs 只发布 object identity，不拥有 device state。
- display completion、input packet 与 PTY byte readiness 统一投递 semantic event；hardirq 不执行 renderer、filesystem 或 task logic。
- terminal userspace 只能使用标准 PTY、termios、signal、ANSI/ECMA-48；禁止私有 console syscall/protocol。桌面客户端协议（`display-proto`）是用户态进程间 seam，不进入内核 ABI。
- `desktop` 是 graphical session 的唯一 owner：经 SET_MASTER 取得 DRM master，独占 evdev
  输入与 scanout；客户端经 SCM_RIGHTS 共享同一 OFD，CREATE_DUMB handle 的 DESTROY 只归桌面。
  headless boot 中 DRM/input 不可用时 `desktop` 保持同一进程并以 5 秒 poll deadline 重试，只报告一次。
  禁止退出后依赖 init `respawn` 紧循环重复 exec，也禁止复制第二套 headless compositor state。
  图形资产（壁纸 / 字体 atlas / 光标）只从 rootfs `/usr/share/liteos/` 单轨加载，禁止二进制
  内嵌与 fs 之外的第二条资产路径；缺失或校验失败即启动失败，与无 GPU 同属启动失败路径。
- `splash` 是 sysinit 的临时屏幕 owner：首个 open `/dev/dri/card0` 取得 master 完成启动画面
  modeset 后立即 DROP_MASTER（DIRTYFB 不需要 master，进度条动画不受影响），fork 后父进程退出
  使 sysinit 完成；子进程写 `/run/splash.pid`，`desktop` 首帧提交后经该 pid SIGTERM 接管并摘除
  pid 文件。splash 失败必须静默退出（不打印、不读 console input），系统无 splash 必须能继续启动。
- `terminal` 独占 ANSI parser 与 renderer state；它不再持有 DRM master 或 evdev，像素经 dumb buffer + damage 提交给 `desktop` 合成。
- Console write 是同步且非阻塞的 output drain seam；Terminal state lock 必须覆盖普通 output 与 input
  echo 的完整 Console write，TCSETSW 取得该锁后才应用设置。TCSETSF 还必须在 Terminal→Console
  唯一 lock order 下同时丢弃 raw adapter input、cooked queue、partial line 与 EOF；未来 adapter 若在
  `Console::write` 内阻塞等待将破坏该临界区契约。
- 普通 console formatting 在唯一 IRQ-safe owner 内使用 256-byte BSS batch，并通过所选 platform
  的同步 console seam drain（RISC-V SBI DBCN、AArch64 PL011）；panic 保留无锁单字节 fail-stop 通道。全局 severity 由 logging Atomic owner
  在 format arguments 构造前判断，被过滤日志不得取得 logger/console lock。

## Failure and cleanup

- PTY master→slave line discipline 与 UART console 共用 256-byte input batch；UART 批末 raw backlog
  必须重新发布 deferred work，不能依赖用户可见 readiness 继续 drain。
- PTY master syscall write 的 user-copy chunk 同样限制为 256 bytes，并在返回前同步 drain 完整
  chunk；因此用户可见 `POLLIN` 只投影 cooked input/canonical EOF，未成行 raw bytes 只供内部
  `wait_ready(raw || cooked)` 封闭进度竞态。其他 character backend 保持 512-byte chunk；
  architecture fence 直接解析 `CharacterDevice::poll_events` 与 sequential-write production dispatch，
  禁止 user-visible poll 改用 raw readiness，或 PTY master 退回 512-byte character chunk。
- PTY registry 通过 composition root 保存不可变 input-signal callback；PTY master drain 生成的 ISIG bitset 必须由 task owner 路由到当时的 foreground process group，filesystem 不得反向依赖 task graph。
- DMA/storage 与完整物理 segment mapping 必须在 publication 前预留；跨页 buffer 按缓存 segment
  精确消耗 descriptor capacity，mapping 失败不得发布部分 chain。queue ownership、fence 或 mapping
  损坏时 fail-stop，不得退回运行期 translation。
- used ID 越界、pending token 被跳过、token/queue identity 不符或 descriptor chain 损坏必须锁存
  terminal queue failure；concrete adapter 对 duplicate/unknown head、非法 returned length、GPU
  response/fence/sequence mismatch 也必须在不回收该 chain 的情况下关闭 adapter 并 reset device。
  禁止返回普通 `Device` error 后继续使用原 queue，也禁止 reset 前局部修补 free list。
- GPU successor order 必须在 request 编码和 descriptor 摘取前验证；scanout 的
  `UNREF→CREATE→ATTACH→TRANSFER→SET_SCANOUT→FLUSH` 与 disable 的递增 slot UNREF chain 不得
  跳步、倒序或跨 operation。失败保持当前 operation owner，publication 前由原 rollback seam 恢复。
- concrete VirtIO adapter 的 `Drop` 必须先写 device status 0，并等待读回 0 证明 reset 完成、同步
  撤销设备对所有 descriptor 的 ownership，再释放 queue、fixed slot 与 cached mapping；初始化进入
  `DRIVER_OK` 后的任意失败也必须由同一 owner drop 路径 reset。缺少读回或该顺序会把仍可 DMA 的页
  归还 allocator；device 不完成 reset 时只能保活 owner 并 fail-stop 等待。
- block request slots 与 head index 在 device ready 前一次预留，uncontended 提交不分配；slot
  exhaustion 是 capacity backpressure，不是 device error。contended caller 在 queue lock 外准备
  唯一 capacity wait node，OOM 不发布 membership 并返回 `OutOfMemory`；task 睡眠、bootstrap WFI，
  slot 在完成结果/读数据消费后 release 并直接 FIFO handoff，随后锁外精确唤醒。同步调用不可由
  signal 中途取消，request DMA owner 必须保留到 completion 或 device reset；reset 原子发布 failed、
  完成固定 outstanding 并清除 head mapping，capacity waiters 以固定 batch 续投 deferred work，最终
  全部取得 device error；generation 使 late/duplicate completion 不能命中新 request。
  notify/IRQ/reclaim 不提前释放仍由 caller 消费的 slot。
- VirtIO-block slot 必须保存原 `RequestOperation`，因为 used `len` 只证明 device-writable prefix。
  Read 的 writable chain 是 4096-byte data 后接 1-byte status，完成长度必须精确为 4097；Write 与
  Flush 只有 status，必须精确为 1。长度未覆盖 status/data 或超过 writable capacity 时，不得读取
  status、不得复制 data；completion claim 必须先 reject 回 owner，再走唯一 device reset drain，
  exactly once 发布失败、唤醒并释放 slot。只有长度验证通过且 status 成功才可复制完整读块。
- block/RNG used token 在 queue 外先产生必须 exactly-once accept/reject 的 `CompletionClaim`；slot generation 与
  result 是 driver-owned invariant，任何不一致必须 fail-stop，不能伪装成可恢复 device error。RNG
  对 device-controlled returned length 验证后才 accept，非法 length 必须 reject 回 outstanding index，
  只有 identity/length 均合法才把 queue token 交给 `recycle_used`，再 accept owner claim；非法
  completion 由 terminal failure transaction 发布 error、complete、wake 并 reset。静默丢 claim 会使 request
  脱离 failure drain，因此 `CompletionClaim` 是 `must_use` token。
- RNG DMA 使用 `DeviceWriteBuffer<4096>`：allocation 保持 `MaybeUninit<u8>`，只有同 generation
  descriptor 已从 used ring 摘取且 `0 < returned <= requested` 后才投影 initialized prefix；缺失
  identity/length proof 会读取未初始化内存。`getrandom` 与 `/dev/random`/`/dev/urandom` 使用唯一
  heap-backed `EntropyBatch<4096>`，成功填充后才 copyout；不得在 kernel stack 放 4 KiB staging，
  不得预零随后被 device 覆盖的 output/DMA bytes。
- VirtIO-net RX slot owner 必须使 `posted + driver-owned + retired` 恒等于初始化容量；completion
  必须先 claim head→slot mapping 并验证 returned length，才允许 VirtQueue recycle；正常 completion
  与 copy capacity 出口在同一 seam repost，无法 repost 时进入 terminal reset。head→slot mapping 只在
  available ring publication 前建立，重复/未知 head、畸形 length 不得窃取或回收仍 posted 的 slot。
  TX 同样必须先 claim `transmit_by_head` 与 `InFlight` identity，且 device-written length 必须为 0；
  任一不一致永久锁存 failed 并 reset，后续 reserve/poll/receive 不得再进入 queue。
- DRM CREATE_DUMB/ADDFB 必须先预留 backing、object node 与 identity；完整 ioctl copyout 成功后才
  无分配发布 handle/ID；identity reserve 同时预留 rollback node，copyout failure 必须按任意并发
  顺序无分配回收全部未发布 identity，已经 publication 的 identity 永不复用。
- DRM close/RMFB/disable、evdev revoke、PTY master close 与 session exit 必须沿唯一 owner seam 清理并在锁外发布 consequence。
