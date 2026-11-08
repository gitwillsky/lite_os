# 设备与终端契约

## Owner

| Owner | 唯一状态与边界 |
|---|---|
| `kernel::serial` / `kernel::log` | 串口 RX ring / 日志记录与输出；日志不拥有串口输入 |
| `kernel::object::resource` | 不可变 MMIO/IRQ/system 授权范围与中断投递、屏蔽、ack |
| `kernel::iommu` / `kernel::vm::device_space` | stream domain、IOVA / 设备页表及 retirement |
| `arch::dma` | 静态硬件后端、设备表编码与同步失效 |
| `driver-manager::platform` / `nodes` / `host` | 设备事实解码与节点资源 / 节点顺序与驱动匹配 / 宿主启动、目录汇聚与监督 |
| `driver-host::platform` / `driver-host::drivers` | 节点资源（MMIO 映射、中断、BTI）/ 节点的硬件 adapter、DMA、queue、reset |
| `driver-host::io_completion` / `driver-host::completion` | 请求槽与 capacity handshake / 合并完成通知与服务线程 |
| `device-model` / `device-protocol` | 设备类接口 / 设备目录、设备会话、channel、共享 VMO 的传输契约 |
| `linuxd::devices` | DRM、evdev、ALSA、TTY、USB sysfs、virtio-port 的 Linux 语义 |
| `linuxd::fs::chardev` / `devnode` | cdev minor 区间注册 / devfs inode identity |
| `netcfg` → `netstack` / `fshost` → `ext4` | 网卡发现并安装进协议栈 / 块设备所有与文件系统服务 |
| `user/linux-uapi` 与 compositor | 应用 Linux UAPI 资源封装 / DRM master、evdev、标准 Wayland |

驱动绑定时复用自身的诊断标签，由 driver-host 附加 MMIO 基址或 PCI 配置空间地址并发布进程名。
任务管理器本地化标签，任务身份仍由 KOID 表示。
adapter 的 queue、slot generation 与设备地址只在 driver-host 有一份权威；Linux 文件壳和 RPC client
只持有协议能力，不复制硬件 state。DMABUF/GEM/framebuffer、TTY session 与 evdev per-open queue
分别留在 linuxd 的相应领域。用户态终端只消费标准 PTY，不使用私有显示或 console ABI。

DRM/KMS、framebuffer、VirGL/PRIME 与图形资源退休的唯一细节 owner 见[图形契约](drm.md)。

## 设备目录与会话

1. 每个驱动宿主在自己的设备目录登记设备（类、静态描述与开会话函数）；driver-manager 向 component-manager 持有的汇聚目录登记条目（对应 Fuchsia devfs）：转发条目的 `OPEN` 原样交给所在宿主，独占与会话回收由宿主判定；宿主终止时撤下
   它的全部条目（`remove`），宿主重启后设备以新 ID 重新登记。目录 ID 单调不复用；使用者 `LIST` 枚举、`WATCH`
   取得变化 Event（先清除再读取，读取之后的变化不会丢失）、`OPEN` 建立新会话、`SCOPE` 派生只含指定类的目录。
   初始枚举按节点顺序并入（首个 GPU、PCI console/block、按 DTB 顺序的 VirtIO-MMIO、xHCI），使用者据登记顺序命名
   设备。component-manager 按类派生给各服务，宿主的监督查询和会话打开使用独立连接。
2. 会话是设备类协议的一组句柄（请求 channel、活动 Event，PCM 另有事件 channel）。会话状态与结束标记由服务闭包
   整体持有，随全部服务线程退出而析构：先回收会话状态，再置位结束标记。各类回收：网卡取消未提交的发送预留；
   PCM 停止并释放流、撤销事件端点；显示关闭 scanout、释放 2D 缓冲、销毁本会话的 virgl context 与 resource、
   隐藏光标。缺失回收时使用者崩溃会让发送槽、流或 GPU 对象永久残留。显示回收后 GPU 宿主作为回退客户端
   （Fuchsia virtcon）重新提交开机画面；下一次 `OPEN` 先推进它的事务至完成、disable 并释放其缓冲，再交出会话。
   回退事务在独占边界上同步完成，此时活动 Event 只有宿主等待；超时只放弃画面，不阻止交接。
3. 块设备、网卡、显示、输入、PCM、port 是独占类：旧会话的使用者仍持有请求 channel 时 `OPEN` 返回
   `ERR_ALREADY_BOUND`；已离开时等旧会话回收完毕再建立新会话。entropy、USB 连接与启动进度可共享。
4. 活动 Event 在登记时绑定驱动一次，每个会话得到它的副本；会话结束不解除驱动侧绑定。
   EventHub 的 ActivityBinding 是可克隆 RAII 登记：最后一个副本退出或显式 retire 同时摘除活动与关联 peer-close source，
   回调/句柄锁外退休。登记只弱引用 source/hub，关闭监听只弱引用活动；Retired 拒绝迟到的 bind/on_closed。
   peer-close 的复用槽 packet 必须复查真实 PEER_CLOSED 及关联身份；迟到 packet 不得断开新会话。
   退休不等待已经领取的回调结束；回调须沿既有幂等通知路径重新检查实际 owner 状态。
5. 块设备会话（`LBLOCK04`）参照 Fuchsia block FIFO：控制 channel 只执行 INFO、ATTACH、DETACH、CONNECT、QUEUE。
   detach_buffer 消费唯一引用并同步等待现有 DETACH 回复；服务端在 PMT 析构后回复，失联禁止移交页。普通 Drop 仍为非阻塞释放。
   QUEUE 一次性交出提交/完成共享 VMO、请求 Event、完成 Event；重复领取返回 ALREADY_BOUND，不能产生两个不协调的 producer。
   数据与 flush 只走共享环；旧 TRANSFER/TRANSFER_BATCH RPC 已删除，未引入应用 ABI 或新增内核调用。
   两端必须同时更新协议版本，不保留旧 codec 或在线混用路径。
6. `block::queue` 只拥有共享记录/索引，`block::server` 拥有登记和 DMA/flush 屏障，`block::client_queue` 拥有调用者 credits、
   request identity 与完成分派；硬件 slot/descriptor credits 仍只属于驱动的 RequestOwner。
   VMO 固定大小，容量为 `max(queue_depth, 64)` 向上取 2 的幂（上限 65536）；头部 256 字节，之后是 capacity 个 48 字节请求、
   capacity 个 16 字节完成，按页取整。头部 SQ head/tail 在 0/64、CQ head/tail 在 128/192（u32），closed/abort 在 8/12。
   请求为六个小端 u64：request ID、operation（0 读/1 写/2 flush，bit63 为 GROUP_LAST）、vmoid、设备首块、块数、VMO 字节偏移；
   完成为 request ID 与低 32 位保存的 Status（高位零）。所有字段用对应宽度的原子访问，错误对端也不能制造非原子数据竞争。
   各端分别用本地短锁串行 producer/consumer，索引按 u32 环绕、差值不超过 capacity；写完记录 Release head，读完记录 Release tail，
   另一端 Acquire 后读取/复用。观察 has_requests 也持 consumer 锁，避免把跨两个快照的 head/tail 误判为损坏。
7. 客户端在连接时准备 pending/空闲表，提交直接把固定记录写进环，无描述符 Vec、Writer 或 channel_call 回复缓冲。
   request ID 的完整序号加槽号由同一短锁分配，序号溢出在发表前拒绝；旧完成不能命中新一代槽。
   credits 直到所属调用消费全部完成才归还，因此 CQ 容量总能覆盖所有在途请求；调用者可一次发表至协商容量，
   不按服务端 64 项的领取窗口限制提交。常见容量 128 以内的 ticket 暂存放栈上，更大批次只在发表前准备一次有界暂存。
   多调用者不跨 DMA 持锁。
   当前调用者之一推进 CQ/port，其他调用者等本地 futex epoch；谓词在分派权交接的同一锁内重查。
   归还本地 credits 也通知完成 Event，不能让代表其他提交者等空位的分派者睡在没有新 DMA 的 port 上；无常驻转发线程。
8. 服务复用 `service-rpc::serve_events`，在同一 port 接收控制、提交与驱动完成 Event；clear → arm → drain 后才等待。
   活动服务一次只处理一个控制操作，返回后才重新武装控制来源；数据 handler 非阻塞，最多两个工作者，空闲时收缩为一个。
   注册、pin 等耗时控制不能占满数据接收者；若无法为控制准备接收者，停止并排空会话，不执行可能阻塞的控制。
   `BlockProvider` 是实际驱动/范围装配的提交能力，创建 `BlockIoQueue`；远端文件系统 client 仍使用同步 BlockDevice 视图。
   两者共用同一 RequestOwner/descriptor/状态解码与退休机制，没有旧的数据 RPC 或把同步设备调用包装为 async 的替代路径。
9. `BlockIoQueue::try_submit` 返回 Queued/Completed/Busy，错误没有接收命令。Queued 在完成被 reap 消费前保活缓冲 Arc；
   SQ 每次领取至多 64 项，只是本地待提交窗口，不限制在途深度。一次 dispatch 最多推进 256 项，预算耗尽重发提交通知；
   不再用阻塞批次或工作线程数量限制硬件队列。每条实际完成单独返回结果，不把一条 I/O 的失败投影给同批成功项。
   Busy 队首保有已经解析的登记及命令，不能在 DETACH/复用 vmoid 后改绑到别的缓冲。
   flush 封闭后续 SQ，先收割本队列此前全部请求，再非阻塞提交 flush，收到其完成后才继续。
10. 驱动每个提交者有预留容量的完成 FIFO 与 physical-slot→caller-ID/缓冲 owner，完成只追加到目标 FIFO；
    reactor 直接消费已完成 token，不扫描所有在途请求。物理 slot、descriptor credits 与 generation 仍只由 RequestOwner 拥有。
    完成分派在硬件/slot 锁外按目的队列分组，每个目的队列一轮通知；不逐请求 signal，不广播给空闲会话。
    线程等待者只在握手武装后 wake；reactor 完成始终入自身 FIFO，两种消费者共享同一物理请求生命周期。
11. 非阻塞准入复用 RequestOwner 的 FIFO。每队列最多一个队首等待登记，节点锁外准备；Event target 获得 grant 或错误时无须睡眠也会通知。
    换命令/退出先撤销未授予的节点；已经 handoff 的 permit 必须取回并归还，取消队首后继续推进新 FIFO 头。
    failure drain 可能在摘下节点后、锁外发布错误；取消不得把此窗口误认为丢失了 grant，也不得重复释放。
    descriptor publication 失败且尚无完成方时，先把 WaitCompletion 恢复到完成状态再归还 slot，block/RNG 遵守同一规则。
12. 队列损坏/source/rearm 失败只停止领取，不等于 DMA 取消。最后 Session Drop 先撤销准入并排空已接收请求，
    再释放登记、发布 closed 和完成通知。队列析构在自己的完成 FIFO 锁内复查空队列并登记停驻者，完成方锁外 unpark；
    不依赖已经退出的服务 port，也不在退出时分配等待登记，不把丢弃 pending SQ 当作已停止硬件。
    设备宿主退出仍由实际 channel PEER_CLOSED 及既有 PMT quarantine 契约处理。
    目录观测句柄由 SessionEnd 共享 owner 持有，状态回收后主动摘除再发布 ended，死亡服务不能被观测引用伪装为在线。
    OPEN 安装观测前服务已退出时，Finished 状态禁止重新安装死亡端点。未登记 CPU 缓冲仍经 BlockBufferPool 适配；直接供页仍是登记 VMO DMA。

## 运行时换版

1. 受控替换按 MMIO 基址或 PCI 配置空间地址选择节点。新 ELF 先装载到尚未运行的进程；旧宿主 Job 终止后，
   新宿主才执行。复用节点 BTI，新宿主复位设备、释放隔离页并完成枚举后，管理器发布新条目。
   启动失败时，同一流程装载上一版。旧会话随宿主关闭，消费者按设备重新出现处理。
2. VirtIO 熵源、声卡、输入、控制台端口、网卡及 xHCI 支持换版。xHCI 停止并复位主控后才释放旧 DMA 隔离页，
   然后重新枚举端口。控制器就绪时可以完成换版，USB 设备随后按热插拔流程出现。旧 USB 引用和 PCM 流随宿主关闭，
   新的 USB 音频输出由 audio-core 接入，应用需重新打开播放流。块设备、GPU 在停机前拒绝换版，挂载与显示会话
   尚缺少各自的恢复契约。节点撤下后仍保留 BTI 隔离。具备恢复契约的节点触发崩溃上限或回退失败后，可按同一地址
   提交修复映像。设备复位并重新发布后重新计数，修复失败则继续保留隔离、等待下一次请求。
3. 管理器切换由 component-manager 提交。旧管理器逐节点冻结监督，复制节点资源、宿主 Job/进程/目录、
   已发布 ID、崩溃窗口与当前映像；新管理器收齐状态并报告就绪后才取得监督权。撤销时旧管理器继续运行。
   宿主、发布目录、原有目录连接和设备会话均跨切换保留。当前管理器意外退出仍是关键服务故障。
4. ELF 的独立 PT_NOTE 标识映像角色与兼容代际；角色错误或版本不同的映像在切换前拒绝。生效范围限当前启动。
   管理器切换保留当前系统已使用的节点与分配结果，平台枚举策略变更在下次冷启动生效。
   驱动按节点重绑借鉴 [Fuchsia DFv2 的宿主重启机制](https://fuchsia.dev/reference/fidl/fuchsia.driver.development#Manager.RestartDriverHosts)。

## DMA 与 IOMMU 寿命

1. BTI 绑定设备 stream，PMT 保活固定页和设备映射；固定期间不得 decommit、替换或 COW 冻结该页。
   恒等 BTI 只提供物理设备地址和 pin，不提供设备隔离。
   component-manager 保留初始根 authority，并只向 driver-manager 委派根副本；driver-manager 为每个节点切出覆盖其
   窗口的 MMIO resource、其向量的 IRQ resource 并创建 BTI，驱动宿主只拿到这些；linuxd 仅获得 system 子范围，
   覆盖 debuglog、串口、UTC、电源、内存观测与调度；audio-core 仅拿独立的调度子范围。system resource 不能创建 MMIO/IRQ/IOMMU、不能扩大父范围或恢复根。
   Resource authority 属于不可变 kind/range，创建子 resource 另要求句柄 WRITE；普通应用不接触 native resource。
   driver-manager 按 DTB 路由分别持有恒等/硬件 IOMMU；MMIO 设备不借 PCI stream 0。
   PMT 只有经 `PMT_UNPIN` 才释放；持有者终止等未解除就关闭的 PMT 连同设备映射进入所属 BTI 的隔离区（Zircon
   quarantine），直到 `BTI_RELEASE_QUARANTINE` 或 BTI 销毁。driver-manager 持有每个节点 BTI 的原件，重启的宿主在驱动
   复位设备之后释放隔离区；缺失时恒等 BTI 上的设备可向已复用的页 DMA。
   PCI iommu-map 的全部区间经 phandle、重叠与溢出校验，应用 iommu-map-mask 后选择 stream；
   声明映射但 requester 未覆盖时 fail-stop，不隐式绕过。当前只支持一个平台 IOMMU。
   VirtIO transport 对平台提供的 ACCESS_PLATFORM 统一协商，全部 adapter 继续使用同一 BTI DMA 接口。
2. domain 索引与 ARM context storage 必须先预留，再发布硬件根指针。发布后不得因可恢复分配失败
   析构根表；命令队列失效失败只能保活 owner 并 fail-stop。domain tag 单调分配，耗尽返回错误且不回绕。
3. 新映射先完成 coherent 页表写入屏障，再交付 IOVA。撤销及部分映射回滚先移除 PTE，再完成设备
   TLB 失效，最后释放页表页、IOVA 与 VMO pin。CPU translation fence 不能代替设备失效。
4. IOMMU owner 退休先使 ARM GBPA abort / CR0 disable 完成，或使 RISC-V DDTP Off / command queue
   停止完成，再释放硬件表、队列和 domain 根表。构造中启用硬件后的失败遵循同一顺序；禁止退到 bypass/Bare。
5. `DmaFrame` 保证 table/queue 物理连续、清零、驻留到 owner drop；后端只使用 physical page number
   和 arch direct-map，不反向依赖通用内存分配实现。

当前 ARM 使用线性 stream table，最多 4096 streams；RISC-V 使用两级设备目录，basic/extended
context 分别覆盖 16/15 位设备号。SMMUv3 事件队列（寄存器窗口含页 1 时）与 RISC-V 故障队列不接中断，
由 domain owner 在 map/unmap 与 BTI 查询（`info::BTI`）时在 domain 锁内排空：故障按 stream 计数，日志在
第 2^n 次时输出，溢出与未绑定 stream 的故障单独计数；只做诊断，不改变映射。Benchmark 决策：排空给每次
map/unmap 增加一至两次寄存器读、无分配，相对同路径的同步失效命令可忽略，不另设基准。driver-manager 在宿主终止时记录
其节点 BTI 的故障数。host OOM 回归执行实际 domain owner，验证发布前预留与 tag 耗尽。boot gate 分别开启
SMMUv3/RISC-V 系统 IOMMU，以 PCI 根盘和 MMIO net/RNG 验证混合 DMA 的真实读写、同步与网络；不宣称设备
热移除、故障注入或物理机验收。

## Interface

- driver-host 的中断线程只确认设备并请求完成分派；电平中断在设备侧清除后才 ack。
  `completion::raise` 是可合并通知，完成线程清除 pending 后分派全部已注册 source；
  新完成必须重新置位，不能依赖下一次无关中断。普通 adapter lock 不跨等待持有。
- VirtIO Console completion 必须先 claim/validate/recycle，再 repost RX 并通知对应 queue；
  used ring 有数据不能代替 repost notification。
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
- block 与 entropy request 使用 `IoCompletion` 的统一先复查、登记、睡眠握手，
  经服务线程描述符 park/unpark；不轮询 MMIO/used ring，也不在 queue lock 内等待。
  `driver-host::io_completion` 的 typed device/slot/generation/ticket 是唯一请求身份，不交给内核解释。
- block/RNG 中断处理共用 `VirtIOCompletionIrq`；transport error 锁存后仍须请求完成分派，
  使 completion owner reset 并唤醒全部 outstanding request，不能吞掉已确认的唯一设备边沿。
- `driver-manager::nodes` 决定节点顺序与驱动匹配，`driver-host::drivers::virtio::binding` 只装配本节点匹配的驱动；driver、DRM、input、filesystem 与 syscall 不得依赖 QEMU machine types。
- VirtIO PCI discovery 必须从 vendor capabilities 得到 modern common/notify/ISR/device windows，按
  `cap.offset + queue_notify_off * notify_off_multiplier` 计算 doorbell，禁用 MSI-X 并只消费 DTB
  `interrupt-map` 给出的 INTx。BAR、capability chain、queue size 或 route 非法时不得发布半初始化 adapter。
- QEMU `virt` 必须在任何 VirtIO queue publication 前证明 root `dma-coherent`；缺失时 fail-stop，禁止增加 bounce buffer、每次提交 cache flush 或“先运行再探测”的兼容路径。
- DRM/evdev/ALSA/TTY ioctl UAPI 由各设备子系统经 linuxd 的 `DeviceFile::ioctl` 编解码；syscall 只提供
  `UserMemory`、`O_NONBLOCK` 与调用者特权位。devfs 只按注册表发布 node identity，不拥有 device state。
- 设备读写经 `UserOutput`/`UserInput` 游标（Linux `iov_iter` 子集）直接交付：破坏性出队（evdev/DRM
  event、PTY master、virtio port）前必须先 `reserve` 整批目标，出队后的 copy 才不会因 fault 丢数据；
  写入先 `copy` 再按设备实际接受量 `consume`。syscall 以游标累计进度为结果，已有进度时优先返回进度。
  设备只在尚未交付任何字节时阻塞。
- `/dev/tty` 是调用者 controlling terminal 的别名：open 按该 Terminal 的设备号重开底层 console 或
  pts，读写与唤醒源与直接打开一致；caller session 没有 controlling TTY 时返回 `ENXIO`。
- `/dev/virtio-ports/com.redhat.spice.0` 是 mode `0600` 的 character byte stream：read/write 支持
  blocking 与 `O_NONBLOCK`，poll/epoll 只投影 adapter level readiness 与 disconnect。它不是私有
  clipboard syscall；SPICE agent framing 属于用户态，agent 必须在单一 owner
  解析 capability/消息，并把 resize 只提交到标准 DRM/KMS seam。
- display/input completion 经设备协议投递，PTY byte readiness 在 linuxd 的 Pipe owner 内投递；内核 IRQ 路径不执行这些领域逻辑。
- RFB 由 compositor 持有唯一的前台 scanout 与 seat，不创建第二套场景。只接受 root 私有目录下的 Unix socket，
  OpenSSH 负责身份验证与转发。正常渲染不复制帧；只在有效的远程请求到达时读取上一已完成 front target。
  网络读写非阻塞，一个连接最多保留一份待发送更新，连接结束时撤销其按键和按钮。远程修饰键和按键不参与本地安全注意序列。
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

- PL011 的 RX FIFO 电平由 DATA 读取降低到阈值以下时撤销；有限预算的 hardirq 不显式清除 RX，
  保证宿主持续回填、单轮预算耗尽时电平仍触发后续处理。只在读取前确认 receive timeout，避免
  清掉读空之后的新通知。不得用输入分段、延时或提高预算掩盖中断丢失。

- init 只监督网络服务、`display-service`（有 `/dev/dri/card0` 时 exec `/bin/compositor`，否则安静待命）与 UART shell，不建立 seat socket、显示器文本 VT 或旧图形会话。
- `compositor` 独占 DRM master 与 evdev 读取；其首帧必须与driver-host 的 `boot_screen` 的最后一帧逐像素一致（`verify_display.py` 以 `LBS1` 参考解码器裁决），光标素材是 `LCS1` 数据文件，不内置常量。
- 指针焦点跟随光标下的窗口，键盘焦点跟随栈顶（活动）窗口；带描述符的消息（`wl_keyboard.keymap`）按段发送，fd 总先于引用它的消息到达。
- `input::Decoder` 先累积原始相对位移并映射、钳制绝对坐标；会话仅原地合并同批相邻的语义指针位置，
  保留最新位置。按钮、按键和滚轮均为顺序屏障，不跨越屏障合并，不增加队列或分配。
- Wayland 服务端以 `pread` 而非 mmap 读取客户端 shm（客户端截断文件只会得到协议错误，不会 SIGBUS）；
  按 64 KiB 暂存预算批量读取完整行，单行超过预算时只暂存一行，不要求末行尾部 padding。
  每个连接一个协议状态机，窗口栈以稳定的连接 id 为键。
- CPU/GPU 合成只改变服务端内部实现，不改变标准 Wayland commit、buffer、damage 和 frame callback 契约。
  软件或 VirGL scanout 首次接管后，都在新画面的 FLUSH completion 后退休 boot resource，
  等 RESOURCE_UNREF 确认才在 control lock 外归还旧 backing。退休操作携带原来的 OperationCompleted/
  RenderCompleted 结果及 fence，不能丢失或改变完成类型。只有首次交接多一条 UNREF，不增加日常帧的命令或缓存。
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
  唯一 capacity wait node，OOM 不发布 membership 并返回 `OutOfMemory`；请求服务线程睡眠，
  slot 在完成结果/读数据消费后 release 并直接 FIFO handoff，随后锁外精确唤醒。同步调用不可由
  signal 中途取消，request DMA owner 必须保留到 completion 或 device reset；reset 原子发布 failed、
  完成固定 outstanding 并清除 head mapping，capacity waiters 以固定 batch 续投 deferred work，最终
  全部取得 device error；generation 使 late/duplicate completion 不能命中新 request。
  notify/IRQ/reclaim 不提前释放仍由 caller 消费的 slot。
- `RequestOwner` 在同一 queue guard 内同时预留 slot 与请求实际需要的 descriptor credits，
  直到 caller 消费完成或撤销未发布请求后才一起归还。slot record 是 generation/credit 的唯一 owner，
  空闲槽用预分配数组内的索引链管理，预留和释放为 O(1)，stale generation 或重复释放 fail-stop。
  credits 不足与 slot 不足都走同一 FIFO capacity wait；不能取得 slot 后再把 descriptor 竞争误报为设备失败。
  prepared waiter 保留原请求 footprint，重入时按 ticket 判断能否先于已登记请求准入；FIFO 头不满足时保留登记。
  释放后逐个授予所有可满足的 FIFO 请求，唤醒发生在 queue guard 外，随后重新检查 adapter 的 failed 状态。
  没有新增锁/原子或 uncontended 分配；credits 在 descriptor 已回收而 caller 尚未消费结果时仍由该 slot 保留。
- VirtIO-block 的全部 header/status 共用一张固定 DeviceBacking。每槽独占 32-byte 区间，header 为前 16 byte，
  status 为随后 1 byte；CPU 写入只覆盖当前未发布槽的原始字节，不创建覆盖其他 DMA 活动槽的 Rust 引用。
  table 在 DRIVER_OK 前分配、固定、映射并触及页表，提交与 completion 只投影已缓存的设备地址；
  不在 queue 锁内为 metadata 创建映射/固定记录。status 仅在同一 slot/generation 的 used 长度与回收已验证后读取。
  表与 adapter 同寿命，Drop 仍先确认 reset 再释放 DMA owner。
  `DmaSlice::backing` 接受非空字节区间及 device-writable 方向；边界在 publication 前检查，复用原 extent 裁剪逻辑。
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
- DRM close/RMFB/disable、evdev revoke、PTY master close 与 session exit 必须沿唯一 owner seam 清理并在锁外发布 consequence。

- 设备 OpenRequest 保留原始 status flags；PTS 的 O_NOCTTY 禁止自动接管。文件壳和 slave
  lifetime pin 在接管之前完成分配；open 失败不会留下控制权。PTY master 的控制身份查询
  使用 slave Terminal 的同一实际 control owner，但 TIOCNOTTY 不适用于 master。

## USB host 与 UAC2 playback

1. `drivers::usb::xhci::Host` 独占控制器 DMA 与 slot generation。中断服务线程只 ack/请求完成分派；State 短锁 只覆盖有界 ring 操作，task-only operations mutex 串行化 command、EP0、时钟与 alternate setting。枚举线程由 `driver-host::main` 创建，驱动不反向依赖 task。
   端口的 Empty/Seen/Addressed 状态同时覆盖未绑定的 HID 等设备，避免 reset 事件自激重枚举。
2. producer 最后 Release 发布 TRB cycle，doorbell 前经过 `mmio` 的 device-ordering 屏障；consumer Acquire 读取 event cycle，核对地址/slot/DCI 后回收。event ring 每次最多处理 128 项，仍有工作则再次投递。
   每个 slot generation 在控制器生命周期内不复用；旧 PCM adapter 即使遇到相同 slot/port，也不能操作新连接。
3. Disable Slot completion 是 context、ring 与 payload 的释放屏障；Stop Endpoint completion 后才能清 ring、Set TR Dequeue 或重配。command/control 超时后停止控制器并保留注册 Arc 的 DMA，不以超时冒充硬件已停止。
   Ring Underrun/Overrun 是无 TD 完成的等时空队列通知，pointer 可以是下一 dequeue 地址，不得据此回收 TRB 或断开声卡；已停止的流不发布 XRUN 通知。设备传输错误断开对应卡，未知 completion/控制器错误隔离整台 host。
4. UAC2 从 Type-I PCM alternate、USB streaming terminal、当前 clock selector 与 clock source GET_RANGE 求交能力。异步 OUT 必须配显式 feedback；high-speed feedback 为 16.16 samples/microframe。
   空、短、零值或越界反馈不更新速率，但完成的反馈 TD 仍须回收并续投；首次有效反馈前使用协商采样率，之后保留上次有效值，不能读残留 DMA 字节或因单包无效隔离控制器。
   反馈只决定完整 PCM frame 的 packet 分布，不做插值、重采样或数字增益；24-in-32 的 USB 左对齐与 ALSA 低位对齐之间仅移动有效位。原生 DSD 不作为 PCM 暴露。
   连续流的 USB packet 可以跨 ALSA period；下一 period 尚未提交时保留尾部与分数相位，不得在 period 边界人为缩短包并消耗一个完整 service interval。
5. 当前覆盖 PCI xHCI root-port 上的 USB2 high-speed、单 AudioControl function、双声道 UAC2 Type-I PCM；枚举可选择不同 configuration。UAC1、USB hub/TT、FS/LS/SS 音频、capture 和 native DSD 未实现，遇到时不发布猜测的声卡。
   PCM 能力上限包含 768 kHz，但每一格式仍须满足设备实际 clock range 与 service payload。
6. 热路径使用预分配的 4 个 PCM periods、128 个 OUT packet slots 和 16 个 feedback slots；没有每 packet 分配、USB control 或睡眠。IOC 至少每 8 个 OUT packets 或 period 边界一次。
   benchmark 决策：分包、整数反馈累加与 staging copy 是新增成本，需用目标 USB 连续播放观察 underrun/CPU；未执行实机门禁时不声明延迟或零丢样已验证。
7. `device_model::UsbDevice` 投影每次连接的身份、在线状态和停止操作；`devices::usb` 向 `fs::UsbSysfsSource` 注入注册表。连接状态只有 xHCI slot/generation 一个 owner，注册表只追加并保留旧 identity；sysfs 不查询硬件寄存器。
   当前只发布成功绑定的 UAC2 连接，未覆盖未支持类别的通用 USB 枚举。`/sys/bus/usb/devices/<bus>-<port>/` 提供 `idVendor`、`idProduct`、`product`、`busnum`、`devpath` 与 root-only `remove`，其余 sysfs 节点保持只读。
   sysfs 挂载为 rw，属性 owner 单独裁决写入，不能把整个挂载设成 ro 使 remove 永远不可用。
8. `remove` 的 OFD 绑定实际连接 adapter，不能按端口重查新连接。停止由 task-only control 事务串行化：禁止 refill、通知音频断开、等待 Disable Slot completion，再清除 slot/DCBAA 和释放 DMA；失败不得提示可安全拔出。
   移除后的端口保持 Seen，直到真实断开/重插才重新枚举。用户态设备列表保留打开时的 remove 句柄并检查 inode identity，避免旧窗口或异步确认误停新设备。
   桌面采样线程由对象停止通道与 join 回收，不执行移除；设备窗口确认后在后台调用标准属性。

## 日志传输

native debuglog 是唯一有界记录环，拥有 sequence、timestamp、severity、opaque annotation 与原始消息。
服务只能附带自己的 annotation，内核不解释 Linux priority；读取和环覆盖按整条记录传递该标签，
内核自身记录及普通 native-runtime 日志使用 0，复用槽位必须清除旧标签。
linuxd 拥有 `/dev/kmsg` facility/level 的 schema 与转换；四档显示 severity 不能替代八档原始 level。
沿用同一环、锁和等待队列，只增加每记录固定整数拷贝，不新增分配；实际 guest kmsg 读写和环复用 host fixture 验证信息保真。
