# 设备与终端契约

## Owner

- concrete VirtIO adapter 独占 queue、DMA、descriptor、completion 与 reset state；`devices::model` 只发布通用设备类接口。
- VirtIO Console adapter 独占 control RX/TX 与 exact-name 命中的唯一 data RX/TX pair、fixed RX/TX slot、named-port control state、
  byte ring 与 terminal reset；`devices::virtio_port` domain 独占自己创建的 notification Pipe。`drivers` 只发布
  唯一物理 byte-stream seam，devfs 不复制 connected/readable/writable state。`VirtIODevice` 的
  transport enum 是 MMIO 与 PCI 的唯一分派点；两种 transport 不得各自实现 console adapter。
- `VirtQueue` 独占 split-ring cursor、descriptor free list 与单一 pending-used latch；`used()` 只摘取
  ring entry 并返回不可复制的 `UsedDescriptor`，`recycle_used()` 只消费当前 queue 的唯一 token。
  adapter slot/generation/head、device length 与 response 尚未验证前，free list 不得改变。
- `drivers::virtio::queue::DmaBuffer` 是 fixed bytes 与初始化时 kernel-VA→physical segments 的唯一共同
  owner；adapter slot 从映射建立保持到 completion 或 reset，`VirtQueue` 只消费带 lifetime 的
  cached `DmaSlice`，不拥有、翻译或延长 backing 生命周期。
- VirtIO-GPU `GpuCommand` 是 runtime opcode、wire length 与 completion stage 的唯一共同 owner；
  `sequence` 只从已验证 completion 选择一个后继领域 command 或 terminal retirement，随后由
  `submit_command` 单一出口编码并发布。`poll_update` 不得自行组合 prepare/opcode/length/stage。
- VirtIO-GPU cursorq 独占一个 DMA-stable 56-byte request slot、descriptor head 与单调 completion
  sequence；`MOVE_CURSOR`/`UPDATE_CURSOR` 不得进入 controlq 或复用 render fence。纯 move 在 slot
  占用时只覆盖尚未发布的 latest position，并携带 cursorq 当前 resource 可见性，ioctl 不等待 used
  completion；shape update 必须等待 slot readiness，且 DRM cursor2 wait token 在 exact used completion
  前保活 64×64 B8G8R8A8 dumb backing。
- `devices::model::io_completion` 是 block/RNG 共用的唯一 request slot、descriptor identity、completion
  handshake 与 capacity membership owner；typed `IoWaitKey { device, kind }` 保留完整 slot、
  generation 与 ticket，不位打包或复制 adapter 私有 wait ABI。`device` 是每个 adapter 构造时分配的
  实例 identity，而非设备种类；同类多实例按种类区分会让 completion 唤醒另一设备的 waiter。block 的 16 个 fixed slots 独占
  request/data/status DMA，RNG 的 4 个 fixed slots 独占 device-write DMA；scheduler 只通过
  `IoWaitTarget` callback 拥有 `WaitMembership::DriverIo`。
- `devices::console` 独占 console RX ring 与通用串口 `ConsoleDevice`（platform 提供设备名与同步输出原语）；
  `devices::tty` 经它读写 raw 字节。输出原语接收整段字节，platform 必须在与内核日志相同的 console 序列化锁内
  写完整段（aarch64/riscv64 各自的 `CONSOLE`）；逐字节输出会让 tty 数据插进一条内核日志的中间，
  日志行被撕裂，依赖串口提示符的 runtime gate 在多核下概率性失败。单次写入至多 512 字节，所以
  锁内的本地中断屏蔽时间与一条内核日志同量级；原语不得记录日志，否则会重入同一把 `IrqMutex`。
  RISC-V 批量 console 即使先前写失败也必须继续 flush 完整批次与尾部，并返回首个错误；
  flush 复位批量长度，错误短路会令满缓冲无法推进。host 故障注入直接执行同一生产 console owner。
  Benchmark 决策：不新增 benchmark。每次 tty write 只多一次无竞争取锁，被其后逐字节的 UART MMIO
  轮询淹没；riscv64 的 SBI 调用数反而从逐字节降为 `ceil(n/256)`，日志批量成本由既有 `log_cost` 围栏继续约束。
  块设备 seam 属于 `block`，`fs` 不依赖 `drivers`。
- `devices::model::registry` 独占各设备类 adapter Arc 与 `DRIVER_IO` completion 源的发布；只追加，index 是
  稳定 identity，不区分“主设备”。platform 只经 trait 对象注册，消费领域不得持有具体 VirtIO 类型；具体 adapter 位于 `virtio`，块设备注册表归 `block`。
- `drm::DrmDevice`/`DrmFile` 独占 display/KMS/GEM/framebuffer/master/event state；`input::EvdevDevice`/`InputFile` 独占 input/client state。
- `fs::chardev` 独占字符设备 driver 注册表，`fs::devnode` 独占 devfs 节点与块设备注册表：driver 按单 major 内的 minor 区间登记（Linux `cdev_add`），
  devfs 节点按路径登记（devtmpfs）；devpts 等动态节点只登记 driver 区间。设备子系统以
  `DeviceFile` 拥有记录边界、阻塞、poll 唤醒源、ioctl UAPI 与 mmap 裁决，fs/devfs/syscall 不认识
  具体设备种类。
- `devices::tty` 独占系统 console Terminal 单例、TTY/pts 设备文件与 termios/session ioctl；`fs::mem` 独占
  `null`/`zero`/`random`/`urandom`/`kmsg`。`tty::pty` 独占 PTY registry/pair；Terminal 独占
  session/foreground/termios/winsize 与创建时绑定的实际设备号。用户态终端只消费标准 PTY，
  图形窗口由共享 UI 运行库拥有。
- userspace `linux-uapi` 独占 DRM/evdev/PTY/process/poll ancillary 的 raw musl FFI；`OwnedFd`、
  `DumbBuffer`、`InputDevice`、`PtySession` 与 `SessionChild` 独占对应资源 cleanup。应用不得复制
  layout、constant、extern block 或裸 owner。

- 实际 OFD 最后 fput 通过 `DeviceFile::release` 消费 PTY/FIFO admission；backend Arc 观察者
  只保活已退休 allocation，不推迟 peer EOF/HUP、PTY hangup 或索引复用。master 与独立 slave
  的失败构造和最终 Drop 消费同一 close 路径，不另设回滚 owner。
- `PtyRegistry.slots` 是唯一 weak AVL index；Terminal/pair/node allocation 和 devpts capacity
  准备在 registry spin guard 外，发布重验可复用索引，loser/旧 weak storage 锁外释放。
  Pair 的实际 `master_open/slave_opens` 决定可复用性；不得以 Weak 是否还能 upgrade 代替 admission。

## Interface

- 设备类 deferred vector 由消费领域（drm、input、virtio_port）以 `deferred::register` 分配并经
  device seam 的 `bind_completion_work` 绑定给 adapter；adapter 只经 `raise_completion` 发布，绑定时补发
  一次以消费绑定前已到达的 completion。核心向量（timer、console、network、driver I/O）是固定常量。
- VirtIO hardirq handler 只读写 interrupt status/ack 并发布合并 deferred bit，不得取得
  queue/control/event lock、遍历 `KERNEL_SPACE` 或回收 descriptor。net、GPU、input 的 ordinary
  adapter lock 只允许 task context 与统一 user-return/idle deferred safe point 进入；禁止为个别
  adapter 增设 IRQ-lock 兼容路径。
- VirtIO Console hardirq 只发布 `devices::virtio_port` 绑定的 deferred vector；deferred pass 必须先 claim/validate/recycle completion，
  再 repost RX descriptor，并按每次 repost 通知对应 queue。用“used ring 仍有数据”代替 repost
  notification 会在恰好 drain 完一批后永久停止 host→guest stream，禁止这种近似。
- `PORT_OPEN` 与 `PORT_NAME` 顺序没有领域保证；adapter 必须分别保存 host-open 与 exact-name match，
  `PORT_NAME` payload 必须先消费 QEMU 发布的单个尾随 NUL 再做 exact match；漏掉该 wire terminator
  会使标准 `com.redhat.spice.0` 永远无法命中。
  exact-name match 后，adapter 必须以标准 multiport queue mapping 选择 data pair：port 0 为 0/1，
  port N 为 `2N+2`/`2N+3`，并只配置一次该 pair。UTM 可以在 SPICE port 前枚举
  `org.qemu.guest_agent.0`；固定使用 4/5 会把 SPICE payload 投递到 QEMU Guest Agent，表现为
  channel connected 但 capabilities 与 monitor config 永远缺失。
  name match 后 exactly once 回复 guest `PORT_OPEN=1`，并只在 guest acknowledgement 已发布且
  host-open 同时成立时发布 connected。缺少 guest open 会让 QEMU 保留 channel 却不投递 byte
  stream；`PORT_REMOVE` 或 transport failure 必须使 read/write/poll 同时观察 terminal disconnect。
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
  delivery 启用前发布，必须经同一 `VirtIOCompletionIrq` owner 先 ack 已断言的 device line 再 reclaim，
  使下一次慢 completion 能产生可唤醒 WFI 的新中断。enable-to-WFI 窗口
  由上述 trap-PC resume 规则关闭；这不是 MMIO polling 或 spin fallback。
- block/RNG hardirq 共用 `VirtIOCompletionIrq`：status/ack 成功时精确确认 bits，读取或 ack 失败时
  发布 transport-error latch，并无条件发布一次 `DriverIo` deferred work；safe point 消费 error 后
  reset/fail 全部 request。吞掉 MMIO error 会让已 claim 的唯一 IRQ edge 后 waiter 永久睡眠。
- `main` 决定装配顺序，`drivers::virtio::binding` 拥有具体 VirtIO 绑定表；driver、DRM、input、filesystem 与 syscall 不得依赖 QEMU machine types。
- VirtIO PCI discovery 必须从 vendor capabilities 得到 modern common/notify/ISR/device windows，按
  `cap.offset + queue_notify_off * notify_off_multiplier` 计算 doorbell，禁用 MSI-X 并只消费 DTB
  `interrupt-map` 给出的 INTx。BAR、capability chain、queue size 或 route 非法时不得发布半初始化 adapter。
- QEMU `virt` 必须在任何 VirtIO queue publication 前证明 root `dma-coherent`；缺失时 fail-stop，禁止增加 bounce buffer、每次提交 cache flush 或“先运行再探测”的兼容路径。
- DRM/evdev/ALSA/TTY ioctl UAPI 由各设备子系统经 `DeviceFile::ioctl` 编解码；syscall 只提供
  `UserMemory`、`O_NONBLOCK` 与调用者特权位。devfs 只按注册表发布 node identity，不拥有 device state。
- 设备读写经 `UserOutput`/`UserInput` 游标（Linux `iov_iter` 子集）直接交付：破坏性出队（evdev/DRM
  event、PTY master、virtio port）前必须先 `reserve` 整批目标，出队后的 copy 才不会因 fault 丢数据；
  写入先 `copy` 再按设备实际接受量 `consume`。syscall 以游标累计进度为结果，已有进度时优先返回进度。
  设备只在尚未交付任何字节时阻塞。
- `/dev/tty` 是调用者 controlling terminal 的别名：open 按该 Terminal 的设备号重开底层 console 或
  pts，读写与唤醒源与直接打开一致；caller session 没有 controlling TTY 时返回 `ENXIO`。
- `/dev/virtio-ports/com.redhat.spice.0` 是 mode `0600` 的 character byte stream：read/write 支持
  blocking 与 `O_NONBLOCK`，poll/epoll 只投影 adapter level readiness 与 disconnect。它不是私有
  clipboard syscall；SPICE agent framing 属于用户态，当前没有 consumer。后续 agent 必须在单一 owner
  解析 capability/消息，并把 resize 只提交到标准 DRM/KMS seam。
- display completion、input packet 与 PTY byte readiness 统一投递 semantic event；hardirq 不执行 renderer、filesystem 或 task logic。
- terminal userspace 只能使用标准 PTY、termios、signal、ANSI/ECMA-48；禁止私有 console syscall/protocol。用户态显示协议不进入内核 ABI。
- 动态 client/window/child/config 集合必须使用 fallible `Vec`/`String` reserve 后发布，不得恢复任意
  8-slot 上限；damage、wire frame、input batch 与 renderer row 是协议或热路径 bounded buffer，继续
  使用固定数组。poll descriptor/mapping vector 必须跨轮次复用容量，事件稳态不得新增分配。
- `SessionChild` 只接受固定 I/O profile 的 `SessionCommand`，并强制通过单线程
  `/bin/session-launch` 建立 parent-death、session/process-group 与同步 exec-error 契约；
  多线程客户端不得运行 `pre_exec`。图形监督进程的 stderr 进入 `/dev/console`；PTY 模式由 launcher
  在 setsid 后执行 TIOCSCTTY，目标 stdin/stdout/stderr 均指向 slave，独立 CLOEXEC fd 只承担 exec-status。
- `terminal-model` 独占 VT parser/screen、选择与有效颜色投影，`keyboard` 独占 CSI/SS3 与已提交 Unicode 的输入编码。
  `/bin/terminal` 使用共享 SDK Canvas 与标准 PTY，没有私有 terminal helper 协议。
- PTY 会话继承已认证账户的环境与凭据；显式提供 `TERM=linux`、`SHELL=/bin/sh`，优先使用当前
  HOME、USER、PATH，仅缺失时采用系统默认值。工作目录是账户 HOME，不能强制切回 root。
  `SessionCommand` 拥有环境覆盖项与工作目录，spawn 失败时不留下半个会话。
- 新建 console/PTY 的 Linux sane termios 必须发布 `VSTART=^Q`、`VSTOP=^S` 与 `VSUSP=^Z`；
  control chars 留为 NUL 会让进入 raw mode 的交互式 CLI 观察到非标准 TTY 初态。
- Console write 是同步且非阻塞的 output drain seam；Terminal transaction 的唯一锁序是
  `output→input→state`。普通 output 与 input echo 在 output transaction 内只用 state lock
  快照 termios，调用 Console 时不得持有 state；Pipe/Console readiness publication 会同步重入
  `input_ready`，持有 state 会形成 epoll 自锁。TCSETSW 取得 output transaction 后应用设置；
  TCSETSF 再取得 input transaction，在不持 state 时丢弃 raw adapter input，随后清除 cooked
  queue、partial line 与 EOF 并应用设置。未来 adapter 若在 `Console::write` 内阻塞等待将破坏
  同步 drain 契约。
- 普通 console formatting 在唯一 IRQ-safe owner 内使用 256-byte BSS batch，并通过所选 platform
  的同步 console seam drain（RISC-V SBI DBCN、AArch64 PL011）；panic 保留无锁单字节 fail-stop 通道。全局 severity 由 logging Atomic owner
  在 format arguments 构造前判断，被过滤日志不得取得 logger/console lock。

### UART 基线

- init 只监督网络服务、`display-service`（有 `/dev/dri/card0` 时 exec `/bin/compositor`，否则安静待命）与 UART shell，不建立 seat socket、显示器文本 VT 或旧图形会话。
- `compositor` 独占 DRM master 与 evdev 读取；其首帧必须与内核 `boot::boot_screen` 的最后一帧逐像素一致（`verify_display.py` 以 `LBS1` 参考解码器裁决），光标素材是 `LCS1` 数据文件，不内置常量。
- 指针焦点跟随光标下的窗口，键盘焦点跟随栈顶（活动）窗口；带描述符的消息（`wl_keyboard.keymap`）按段发送，fd 总先于引用它的消息到达。
- `input::Decoder` 先累积原始相对位移并映射、钳制绝对坐标；会话仅原地合并同批相邻的语义指针位置，
  保留最新位置。按钮、按键和滚轮均为顺序屏障，不跨越屏障合并，不增加队列或分配。
- Wayland 服务端以 `pread` 而非 mmap 读取客户端 shm（客户端截断文件只会得到协议错误，不会 SIGBUS）；
  按 64 KiB 暂存预算批量读取完整行，单行超过预算时只暂存一行，不要求末行尾部 padding。
  每个连接一个协议状态机，窗口栈以稳定的连接 id 为键。
- CPU/GPU 合成只改变服务端内部实现，不改变标准 Wayland commit、buffer、damage 和 frame callback 契约。
  `wl_buffer.release` 只在服务端不再访问客户端缓冲时发送；GPU 直接引用客户端缓冲时须遵守相应标准同步契约。
  全帧重绘可以保守覆盖 damage；局部更新必须正确处理 surface/buffer 坐标、scale 与每块输出缓冲的历史内容。
  GPU 客户端缓冲共享须经标准 linux-dmabuf 与 DRM UAPI 接入，不增加应用私有渲染协议。
- 窗口请求：move/resize 只接受由仍按住的按钮触发、serial 匹配且窗口不是最大化/全屏的请求；抓取期间客户端看不到指针事件，
  窗口消失即结束抓取。最大化/全屏保存还原几何，取消时 configure 带还原尺寸。
- 最小化的窗口不绘制、不命中、不是活动窗口。Alt+Tab 打开按最近使用排序的图标网格面板（最多 7 列，Alt+Shift+Tab 反向），松开 Alt 才激活选中窗口（也恢复最小化）；面板的字体与图标缺失时只切换不显示。
  Alt+Esc 把活动窗口放到最底层；窗口（系统）菜单由 compositor 绘制与裁决（`xdg_toplevel.show_window_menu`、Alt+Space），打开期间独占指针与键盘，“移动/大小”进入由点击或 Enter 结束、Esc 取消、方向键微调的键盘抓取，
  二者吞掉按键。
- `wayland::Client` 独占角色与协议状态：cursor、xdg 与 layer 不能混用；layer 状态只在 commit 生效，空 buffer 取消映射并清除旧 configure/ack，重新映射必须重新握手。
- `Session` 独占外壳策略：Background/Bottom → 应用窗口 → Top/Overlay 的绘制与命中顺序；
  `shell_layout` 只做锚点、边距和工作区的纯几何计算。正独占区（含对应边距）只从已映射图层投影，最大化窗口随其变化重排；全屏始终使用完整输出。
  Top/Overlay 的 Exclusive 图层优先获得键盘并接收窗口快捷键；OnDemand 图层点击获得焦点，点击应用或切换窗口后释放。
- 会话管理能力来自 compositor 创建的 socketpair，不来自 uid、环境变量或公开监听 socket。`linux-uapi::process::spawn_with_wayland_socket`
  通过标准 `Command` stdin 装配传递连接（`WAYLAND_SOCKET=0`），不安装 pre_exec；普通监听连接即使同为 root 也不能看到或绑定外壳全局对象。
  `/bin/logon` 存在时才启动外壳；其退出被 wait 回收后 compositor 退出，由 init 的唯一 display-service respawn 重建会话，避免留下无认证界面。
- SAS 只从 evdev 的 Ctrl+Alt+Delete 识别；`Session` 保存初始 logon ClientId，将标准键盘焦点和完整序列投递给它持续映射的背景。
  握手期间 secure_pending 阻断旧窗口指针输入与快捷键，直到可信 Overlay 映射；缺少此屏障会泄露安全切换期间的输入。
  可信 Overlay 在同层渲染、命中与独占键盘排序中最高；该优先级不能作用于 Background，否则登录背景会遮住用户桌面。
  Ctrl+Esc 与 Super+R 在没有独占安全层时路由到桌面背景；桌面获得焦点时 Alt+F4 交给外壳。
  新窗口/退出改变活动窗口以及 foreign 操作时撤销旧 OnDemand 焦点；SAS 握手的可信目标不能被这些请求撤销。
- security-context-v1 只向初始 Session 连接公布。`wayland::security::Context` 独占 Pending metadata 与 fd，commit 后将 listener 唯一所有权移交 Server，
  协议对象销毁或创建者断连都不撤销已提交 listener；只有 close_fd 的 HUP/ERROR 停止接受新连接，同轮 HUP 优先于 accept。
  Server 给 `org.liteos.session` / `org.liteos.desktop` 且 instance 非空的 context 分配 Shell 能力：Layer/Foreign 可用，security-context 不可见，禁止嵌套委派。
  engine/app/instance 只在已经受信任的 Session 来源内收窄能力，不用于认证 uid，也不声称提供 namespace sandbox。
  监听 fd 经 linux-uapi 检查 AF_UNIX、SO_TYPE 和 SO_ACCEPTCONN；socket 刷新永久失败时丢弃待发 bytes/fd 并移除连接，避免断连在 poll 集合中忙循环。
- `wayland::foreign` 独占每个 manager 的句柄及最近发送描述，`Session` 从窗口栈投影标题、app_id 与模式状态，
  不复制一份可修改的窗口状态。每次绑定独立宣告窗口；stop 只销毁 manager，已有句柄继续更新直到窗口 closed 或客户端 destroy；
  已销毁的句柄保留“已宣告”记录到窗口消失，防止同一窗口反复重新宣告。句柄操作由稳定的 `(ClientId, surface)` 路由，窗口消失后忽略。
- `wl_pointer.set_cursor` 只采用当前指针焦点客户端的最新 enter serial。光标 surface commit 更新硬件图像，null surface 或无 buffer 隐藏，
  换焦点恢复默认箭头；缩放抓取使用对应方向箭头，结束或目标消失时恢复。
- `keyboard`、`terminal-model`、`terminal-font`、`canvas`、`classic-theme`、`classic-ui`、`typeface`、`wayland-scanner` 和 `wayland-wire` 是独立库；不得恢复退休的私有显示、音频或 seat 协议。

## Failure and cleanup

- PTY master→slave line discipline 与 UART console 共用 256-byte input batch；UART 批末 raw backlog
  必须重新发布 deferred work，不能依赖用户可见 readiness 继续 drain。
- PTY master syscall write 的 user-copy chunk 同样限制为 256 bytes，并在返回前同步 drain 完整
  chunk；因此用户可见 `POLLIN` 只投影 cooked input/canonical EOF，未成行 raw bytes 只供内部
  `wait_ready(raw || cooked)` 封闭进度竞态。其他 terminal 写入保持 512-byte chunk；architecture
  fence 直接解析 `tty/mod.rs` 的 `TerminalFile::poll` 与 `PtyMasterFile`/`TerminalFile::write`，禁止
  user-visible poll 改用 raw readiness，或 PTY master 退回 512-byte chunk。
- session、process group 与 signal 只经 task 在 `task::initialize` 安装的 `tty::JobControl` 访问；PTY master
  drain 与 console deferred drain 生成的 ISIG bitset、master close 的 SIGHUP/SIGCONT 与 TIOCSWINSZ 的
  SIGWINCH 都由 fs TTY 经它路由到当时的 foreground process group，filesystem 不得反向依赖 task graph。
- DMA/storage 与完整物理 segment mapping 必须在 publication 前预留；跨页 buffer 按缓存 segment
  精确消耗 descriptor capacity，mapping 失败不得发布部分 chain。queue ownership、fence 或 mapping
  损坏时 fail-stop，不得退回运行期 translation。
- used ID 越界、pending token 被跳过、token/queue identity 不符或 descriptor chain 损坏必须锁存
  terminal queue failure；concrete adapter 对 duplicate/unknown head、非法 returned length、GPU
  response/fence/sequence mismatch 也必须在不回收该 chain 的情况下关闭 adapter 并 reset device。
  禁止返回普通 `Device` error 后继续使用原 queue，也禁止 reset 前局部修补 free list。
- VirtIO Console 的 duplicate/unknown head、非法 device-written length、RX ring overflow、repost/
  notify failure 必须进入同一 terminal failure，exactly once reset device 并唤醒全部 port waiter；
  reset 后不得继续使用原 queue。
- GPU successor order 必须在 request 编码和 descriptor 摘取前验证；2D scanout 的
  `UNREF→CREATE→ATTACH→TRANSFER→SET_SCANOUT→FLUSH`、2D damage 的 transfer/flush 与 disable 的
  递增 slot UNREF chain 不得跳步、倒序或跨 operation。已完成 VirGL render 的 alternate target 必须把
  `SET_SCANOUT→RESOURCE_FLUSH` 作为一个 ordered batch 发布：前者只绑定 host scanout texture，后者才触发
  display update；operation fence 只能在 flush completion 发布。两条 command 不得经 completion round-trip
  串行提交，否则一次 page flip 会多付一次 host event-loop/display-sync 延迟。失败保持当前 operation owner，
  publication 前由原 rollback seam 恢复。
  completion-confirmed active CRTC mode 由 adapter 的单一 scanout state 拥有，不得从 2D residency
  推导；VirGL framebuffer 在 resident slots 为空时仍必须能沿同一 resource-id-zero transaction disable。
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
- DRM connector preferred mode 与 completion-confirmed active CRTC mode 是两个 generation；
  VirtIO-GPU display-info 更新不得使旧 active mode 的合法 PAGE_FLIP/DIRTYFB 失效。同步
  SETCRTC/DIRTYFB/RMFB 遇到内部 display-info 或另一 display operation 时必须按 device readiness
  generation 睡眠并重提；无 mode 变化的 display-info completion 也必须发布 readiness，禁止向
  userspace 泄漏 adapter `WouldBlock` 或发生 lost wake。
- `DRM_IOCTL_MODE_CURSOR2` 是硬件光标的唯一 UAPI：shape update 必须先完成标准
  `RESOURCE_CREATE_2D`/`RESOURCE_ATTACH_BACKING`/`TRANSFER_TO_HOST_2D` transaction，再提交
  cursorq 并等待 exact completion；不得把 VirGL 3D texture 直接交给 cursorq。纯 move 只向 queue 1 发布或覆盖 latest
  position，立即返回。两者都不得修改 active framebuffer、生成 page-flip event 或排到 VirGL controlq
  后面。
- DRM close/RMFB/disable、evdev revoke、PTY master close 与 session exit 必须沿唯一 owner seam 清理并在锁外发布 consequence。

- 设备 OpenRequest 保留原始 status flags；PTS 的 O_NOCTTY 禁止自动接管。文件壳和 slave
  lifetime pin 在接管之前完成分配；open 失败不会留下控制权。PTY master 的控制身份查询
  使用 slave Terminal 的同一实际 control owner，但 TIOCNOTTY 不适用于 master。

## GPU resource identity

- adapter 独占 host resource namespace 的保留前缀；启动、alternate scanout、光标与崩溃画面的角色由同一枚举分配。
  GraphicsDevice::first_dynamic_resource_id 将边界交给 DRM，后者的动态 allocator 必须从该值开始，不能复制常量。
  KMS framebuffer ID、GEM handle、context ID 属于各自命名空间，不得由数值相等推断与 host resource ID 相同。
- response mismatch 在 adapter fail-stop 前记录 opcode、context、首参数、响应码、长度和 exact fence；错误记录不放宽响应验证或继续复用失败队列。

## VirGL、PRIME 与隐式同步

`drm::graphics` 拥有资源及提交事务；`graphics::attachment` 拥有 file/context attachment，
`graphics::retirement` 分别退休 attachment、context 与设备级资源。`drm::prime` 拥有 dma-buf backend；
`device` 只在唯一 completion owner 下推进标准 VirtIO 命令。

- VirglBuffer 与导出 DrmFile 无关；dma-buf、KMS、GEM VMA、attachment 和提交 pins 分别保活它，禁止 dma-buf 反向持有 OFD/context。
  VirglLifecycle 的 mutex 只保护最后 fence 和导出 backend 的 Weak；后者保证重复导出同一 dma-buf identity，且不形成自环。
  DeviceMappingSource 独立持有实际 OFD 与映射对象；DeviceArea 的 split/fork 克隆同一 lease，TLB 退休完成后才释放。
  GEM 与 dumb backing 以 `DeviceMemoryKind::GemShmem` 分配：对照 Linux drm_gem_shmem，页计入 meminfo `Shmem`；
  `drm_gem_shmem_object_mmap` 的 `VM_PFNMAP` 映射不计进程 RSS 与 `Mapped`。
  GEM VMA 与 PRIME 共用 DmaBuf 资源 owner，最后映射释放后主动推进退休，不依赖下一次无关 GPU 提交。
  只有组合独立设备对象与 OFD 的 mmap 准备阶段分配一次 lease；OOM 返回 ENOMEM 且不修改页表，unmap/fork 不新增 lease 分配。
  GEM_CLOSE 只撤销 handle/context attachment，不能提前 UNREF 仍有 VMA 的资源；仅保留物理 backing 不满足此契约。
- 每个 VirglAttachment 在 host attach 前预分配退休节点；接受命令立即记录 exact fence。
  close/失败/中断先移除 file-local handle，最后 pin 退出后等待 fence，发送 CTX_DETACH_RESOURCE；
  context 只等待自己的 detach，不等待其他进程仍持有的资源。全局退休观察者是最后一个 Arc 且 fence 完成后才 RESOURCE_UNREF。
- context_creation task mutex 串行化 context 创建、PRIME import 与 GEM_CLOSE 的 handle 发布，等待 host 时不持有 spin lock。
  重复/self-import 返回原 handle，不增 GEM handle 引用计数；未发布 handle 的 copyout/分配/等待失败统一回滚。
- 每资源 reservation task mutex 在 exec/transfer 的提交与 fence 记录间独占；多资源按设备 resource ID 排序获取，
  可中断获取返回 EINTR。对照 Linux virtio-gpu，exec/transfer 不在 CPU 上等待旧 fence：主机按控制队列顺序执行，
  先提交的生产者命令必然先于后提交的消费者命令生效；需要 CPU 可见结果的一方经 VIRTGPU_WAIT 或 DMA_BUF BEGIN_SYNC 等待。
  CPU 等待会把每次提交量化到主机 fence 通知周期（macOS QEMU 实测约 10 ms），合成每帧都被拖慢一个周期。
  VirglAccess 去重并保活真实 attachment，最后 fence 只取 max；独立资源不共享这把锁。
- EXECBUFFER 命令流上限 `VIRGL_COMMAND_MAX` 等于 Mesa virgl 命令缓冲最大值（64 Ki + 1024 dwords）。
  ioctl 在任何提交锁之前把命令流分段拷入 `DeviceMemoryKind::Driver` 的 DeviceBacking，`Submit3d` 以 Arc 持有它；
  adapter 只在槽位写 32 字节请求头，命令流按 backing extent 作为第二个 descriptor 发布，槽位持有 backing 到 used ring 归还，
  对应 Linux 每次提交复制命令缓冲并单独挂入 sg。extent 超过 `VIRGL_COMMAND_EXTENTS_MAX`（16）返回 ENOMEM，
  否则这条 chain 永远装不进 64 项队列而无限重试。槽位请求缓冲只按 ATTACH_BACKING 的 256 个 extent 条目预留。
- dma-buf 支持标准 PRIME FD、DMA_BUF_IOCTL_SYNC、poll 与共享 backing mmap（当前 offset=0）。
  BEGIN_SYNC 等待同一 reservation/fence；END_SYNC 使用当前一致性 DMA backing。poll generation 来自资源 fence，不能被其他资源 completion 制造 ET 边沿。
  DeviceFile 可提供弱 OFD identity；syscall 负责 reserve FD、复用实际 OFD、copyout 后发布，CLOEXEC 属于 FD。
  候选 OFD 在锁外创建；缓存只持 Weak，关闭最后 FD 不形成 OFD/backend 环。
- renderD128 不取得 DRM master，禁止 KMS 与 SET/DROP_MASTER；primary 继续独占扫描输出。
  fs::GraphicsSysfsSource 由 DRM 注入只读实际 MMIO identity，sysfs 仅投影 card/render、设备号与 subsystem/uevent，fs 不依赖 driver。
- capset 按实际版本分别读取，不把最高版本 payload 当作 v0。virtio_gpu DRM version=0.0，不宣称未实现的 sync_file FD。
  非 blob legacy transfer 的 stride/layer_stride 必须为零；resource_create 的 size/stride 分别为 Linux u32 字段。
- DRM `DeviceFile::write_file_information` 在现有 GEM namespace 锁内只捕获数值、锁外格式化标准
  `drm-driver`、`drm-client-id`、`drm-total-memory`、`drm-shared-memory`、`drm-resident-memory`。client-id 来自唯一设备的
  monotonic OFD identity，dup/fork 共用；handle 关闭后不再归属该客户端，残留 VMA/KMS/退休 pins 仍按原寿命保活。
  当前 backing 全部页对齐且常驻系统内存，不冒充宿主专用显存。每客户端各 buffer 只计一次，跨客户端共享不可直接求和；
  不以 Arc 引用计数推断 GEM 共享，也不把 transport fence 等待时间发布为 GPU execution time。
  共享按 Linux `drm_gem_object_is_shared_for_memory_stats`（`handle_count > 1 || obj->dma_buf`）：VirGL 资源的
  `VirglLifecycle::shared` 在首次 PRIME 导出时置位，与 Linux 保留 `obj->dma_buf` 至最后一个 handle 关闭的寿命等价；
  dumb buffer 不可导出，恒为私有。fdinfo 在 OFD 锁内读取 lifecycle 锁，锁序与 `virgl_wait` 相同。
  统计沿用现有 owner，无新增状态、锁或分配；每次读取按客户端 GEM handle 数线性遍历，属于显式监控查询冷路径。
- completion 将退休节点移出锁域再析构；回收不分配。已发布 resource ID 不复用，未发布 ID 等 UNREF completion 后回收。
- Benchmark 决策：命令流改为每次提交分配 backing（与 Linux memdup 同量级），换掉 32 个 64 KiB 预分配槽位与锁内整段拷贝；
  去掉 exec/transfer 的 CPU fence 等待后，合成帧与 SHM 提交不再各自付出一个主机 fence 通知周期。
  新增 reservation 成本属于共享资源同步的必要契约；沿用 GPU 门禁检查真实清屏读回、PRIME 寿命、上游 Wayland EGL 消费与 page-flip 节拍，
  不以 host 墙钟推断 GPU 性能。具体结果只由实际运行记录声明。

当前只在同一设备共享 VirGL GEM；不宣称跨设备 PRIME、blob、sync_file/syncobj、Vulkan 或全部 OpenGL resource transfer 语义。

primary OFD 关闭的 reset_after_owner 由同一 completion owner 保留到 disable 被 adapter 接受；
WouldBlock 不清除 intent、不 panic，新 master 在此期间不能提交新 scanout。
render/adapter/display completion 都先推进 reset，再推进 detach/UNREF，避免迟到的 disable 覆盖新所有者或扫描已释放资源。
首个合法 EXECBUFFER 与 RESOURCE_CREATE 一样可惰性发布 context；已发布 context 在 OFD 存活期间不可撤销，热路径不重复取得创建 task mutex。
未置位的 fence/ring 字段及零数量 syncobj 的 stride/pointer 按固定 Linux 语义忽略，不要求客户端写入私有 sentinel。
