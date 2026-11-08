# 设备与终端契约

## Owner

| Owner | 唯一状态与边界 |
|---|---|
| `kernel::serial` / `kernel::log` | 串口 RX ring / 日志记录与输出；日志不拥有串口输入 |
| `kernel::object::resource` | 不可变 MMIO/IRQ/system 授权范围与中断投递、亲和、屏蔽、ack |
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
   取得变化 Event（先清除再读取，读取之后的变化不会丢失）、`OPEN(id, options)` 建立新会话、`SCOPE` 派生只含指定类的目录。
   options 为带长度的设备类私有版本化参数，汇聚目录原样转发，不解释硬件策略；网络类 v9 使用 pair 数(u16) 与 TX payload 容量(u32)，均小端，其他现有类只接受空 options。
   初始枚举按节点顺序并入（首个 GPU、PCI console/block/net、按 DTB 顺序的 VirtIO-MMIO、xHCI），使用者据登记顺序命名
   设备。component-manager 按类派生给各服务，宿主的监督查询和会话打开使用独立连接。
2. 会话是设备类协议的一组句柄（请求 channel、活动 Event，PCM 另有事件 channel）。会话状态与结束标记由服务闭包
   整体持有，随全部服务线程退出而析构：先回收会话状态，再置位结束标记。各类回收：网卡排空已接受 TX 并同步 stop DMA，再释放本会话的队列租约；
   PCM 停止并释放流、撤销事件端点；显示关闭 scanout、释放 2D 缓冲、销毁本会话的 virgl context 与 resource、
   隐藏光标。缺失回收时使用者崩溃会让发送槽、流或 GPU 对象永久残留。显示回收后 GPU 宿主作为回退客户端
   （Fuchsia virtcon）重新提交开机画面；下一次 `OPEN` 先推进它的事务至完成、disable 并释放其缓冲，再交出会话。
   回退事务在独占边界上同步完成，此时活动 Event 只有宿主等待；超时只放弃画面，不阻止交接。
3. 块设备、网卡、显示、输入、PCM、port 是独占类：旧会话的使用者仍持有请求 channel 时 `OPEN` 返回
   `ERR_ALREADY_BOUND`；已离开时等旧会话回收完毕再建立新会话。entropy、USB 连接与启动进度可共享。
4. 稳定硬件实例的活动 Event 在登记时绑定；网络类的队列按会话创建，绑定属于本次硬件租约，退出时一并退休。
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

- 原生 `INTERRUPT_SET_AFFINITY` 只接受当前中断句柄的 WRITE 权限及一个在线逻辑 CPU，不能指定其他中断源或以硬件标识代替逻辑 CPU。
  `object::resource::Interrupt` 校验 CPU 生命周期，platform façade 分别交给 GIC/PLIC；不引入 Linux 应用接口或通用网络概念。
  改路由保留原事件和屏蔽/ack 状态：GIC 先停止已启用 SPI、更新 IROUTER 后恢复原使能；PLIC 在覆盖 claim→complete 的同一控制器锁内改 context enable，并保持 priority。
  错误在改变硬件路由前校验；PLIC 元数据 OOM 不发表目标。沿用控制器现有锁，不新增 IRQ 热路径锁/分配；此配置冷路径不另设微基准，运行时路由核对留到路由计划统一验收。
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
- block/RNG/sound 中断处理共用 `VirtIOCompletionIrq`；transport error 锁存后仍须请求完成分派，
  使 completion owner reset 并唤醒全部 outstanding request，不能吞掉已确认的唯一设备边沿。
- net 使用同一 IRQ 错误锁存：正常完成/配置 IRQ 直接通知队列，status/ack 错误请求完成线程消费，
  由它沿设备失败屏障 reset 并通知全部队列。IRQ 不取 pair/control 锁，也不等待 DMA 撤离；没有新网络错误专用线程或正常路径转发。
- `driver-manager::nodes` 决定节点顺序与驱动匹配，`driver-host::drivers::virtio::binding` 只装配本节点匹配的驱动；driver、DRM、input、filesystem 与 syscall 不得依赖 QEMU machine types。
- VirtIO PCI discovery 必须从 vendor capabilities 得到 modern common/notify/ISR/device windows，按
  `cap.offset + queue_notify_off * notify_off_multiplier` 计算 doorbell，禁用 MSI-X 并只消费 DTB
  `interrupt-map` 给出的 INTx。BAR、capability chain、queue size 或 route 非法时不得发布半初始化 adapter。
- PCI queue 初始化在独占 transport 配置阶段读取并校验每队列通知偏移，按设备声明的 num_queues 准备存储。
  configure_queue 的 Release 发表与通知的 Acquire 读取配对；未配置队列拒绝通知，偏移 0 仍合法。
  notify_queue 不再改写共享 queue_select，因此不同队列可以并发敲 doorbell；特性与队列配置仍必须串行，不能与运行时通知并发重配。
  不新增热路径锁/分配；每个设备队列增加一个机器字，省掉每次通知的 queue_select 写、queue_notify_off 读与乘法。
  Benchmark 决策：寄存器访问形态可静态审查，最终多队列吞吐/时延作来宾诊断，不能由该改动推算整机收益。
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
- `compositor` 独占 DRM master 与 evdev 读取；其首帧必须与driver-host 的 `boot_screen` 的最后一帧逐像素一致（`verify_display.py` 以 `LBS2` 参考解码器裁决），光标素材是 `LCS1` 数据文件，不内置常量。
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
  归还 allocator；device 不完成 reset 时只能保活 owner 并 fail-stop 等待，reset 寄存器访问失败同样不能吞掉后继续释放。
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
  必须先 claim head→slot mapping 并验证 returned length，才允许 VirtQueue recycle；claim 借用实际池并冻结已验证长度，不能跨队列消费。
  合法 completion 只交出槽号与长度，客户端结束 CPU 借用后才 release/repost；无法 repost 时进入 terminal reset。head→slot mapping 只在
  available ring publication 前建立，重复/未知 head、畸形 length 不得窃取或回收仍 posted 的 slot。
  TX 同样必须先 claim `transmit_by_head` 与 `InFlight` identity，且 device-written length 必须为 0；
  任一不一致永久锁存 failed 并 reset，后续 reserve/poll/receive 不得再进入 queue。
- VirtIO-net 的 MMIO/PCI 只在发现/传输层分支，网络队列、DMA 和服务发布使用同一实现；PCI 当前仍走 INTx。
  全部队列与 DMA owner 必须先放入具有 reset Drop 的实例，再设置 DRIVER_OK 和首次通知；此后任一失败先 reset 再释放缓冲。
- `virtio::queue::dma::DmaPool` 只在构造时线性切分共享固定存储，不能重复取得同一槽；DmaBuffer/DmaRegion 不可复制，区间借用仅覆盖自身槽位。
  池按明确的 stride 切分；定长 take_fixed 校验尺寸且不增加定长 buffer 字段，运行时 take 交出带长度的 DmaRegion，两者共享同一游标与投影算法。
  Arc 只保活共享 VMO/映射/pin，不参与包的 DMA 所有权转换；原 head/slot/generation 规则仍决定何时 CPU 可以访问，最后槽位释放前不能撤销存储。
  单缓冲也使用同一 owner，DeviceWriteBuffer 的未初始化输出契约保持独立。池中范围查询二分定位首个 cached segment，只遍历覆盖本次范围的片段。
  网卡每 pair 的 RX/TX 分别密集分配一个池，TX 槽按 64 byte 对齐；当前 32+32 槽的数据页分配从 256 KiB 降至 112 KiB，不含 ring/元数据。
  Benchmark 决策：新增 Arc 只在构造/析构增减引用，稳定收发仅借用；额外 owner 间接访问与稠密元数据的综合成本留到目标机吞吐/时延诊断，
  不用 host 虚拟地址模拟设备/IOMMU 时延。现有 DMA fixture 覆盖线性槽、共享保活及跨非连续页的地址投影，不能代替实际 DMA 验收。
- 网络队列由 `(device, queue pair, slot)` 标识；pair 数实例内固定、非零，槽号不能脱离队列寻址。
  device-protocol 网络描述版本为 9，携带硬件最大 pair 数，不接受旧布局。客户端明确请求非零且不超过上限的 pair 数，
  驱动必须精确满足或失败，不能按 CPU 数替用户决定。目录独占 OPEN 的设备选项直接交给 NetworkController::open；
  MEMORY 授权 RX/TX 池与布局，
  QUEUE 一次授权四环 VMO、ready Event 与 progress Event，重复领取返回 ALREADY_BOUND。
  驱动名由设备提供，远端拥有可释放字符串，trait 按 self 借用，不把重连元数据泄漏成永久 static。
- 四个 SPSC 环依次为 Receive、Fill、Transmit、Completion；本端每队列锁串行化 producer/consumer。
  容量为 `max(RX 槽数, TX 槽数, 32)` 向上取 2 的幂，上限 65536。头部 576 字节；每环 head/tail 分居 64-byte cache line，
  closed/abort/kick 在偏移 512/516/520；记录为五个小端原子 u64：slot、非零 ticket、length/status、checksum、segmentation。
  Fill 后三字、CQ 后两字及 RX 第五字为零，RX 第四字为 checksum-valid 的 0/1。
  单批至多 32 条，记录字 Relaxed 访问、整批 head Release/Acquire 交付，tail Release/Acquire 归还元数据空间；
  wrapping head-tail 超过容量即协议错误。记录使用原子字，畸形对端不能通过改写记录制造 Rust 非原子数据竞争。
  RX ticket 每队列递增，Fill 必须匹配仍持有的槽；TX 由客户端生成递增 ticket，服务端按槽保留上次 ticket，拒绝重复、过期和越界提交。
  未接受的 TX 后缀保留在固定窗口；硬件只接受前缀，不把队列暂满当故障。CQ 只在真实 used/recycle 后发表；客户端核对 Submitted ticket 后才复用。
- 硬件只实现 `NetworkProvider` 的共享槽批量接口；`NetworkDevice` 向主机栈交出有寿命的共享帧借用，不在任何一端保留复制帧入口。
  正常数据面无逐包 channel RPC：TX/FILL 发表后仅 kick 的 0→1 转换 signal ready；服务端排空前 AcqRel 清 kick，
  并发发表要么被本轮消费，要么重新唤醒。ready/硬件使用服务 reactor 的 SIGNALED，progress 使用 EventHub 的 USER_0，不能混用。
  服务复用 service-rpc 弹性线程消费控制消息、ready 与硬件通知，不为每条队列增加桥接线程；每轮有界推进，有 backlog 再调度。
  客户端回收 CQ 也 kick，释放 CQ 背压；reserve/available 提前回收时把容量边沿留给下一 poll，不能吞掉等待发送者的通知。
  EventHub 在验证关联 peer 真正关闭后保留 disconnected 事实，队列 poll 读取该 owner，不逐包向内核查询 peer。
- 客户端一次校验 VMO 大小、槽数、stride、header、TX 上限并映射；收发直接投影，不逐帧 VMO read/write。
  RX 的 Receive Acquire 取得 Borrowed ticket，`NetworkReceiveFrame` 借用 device 保活映射，显式 release 或 Drop 后才 Fill Release；不在返回帧前归还槽。
  TX 的 `NetworkTransmit` 在 Reserved ticket 中直接构造 payload；消费 token 的 submit 结束全部 CPU 引用，再 Release 发表，matching CQ 前不可重写。
  两类 token 不可复制，不逐包分配或增加 Arc 引用；安全访问只在 token 寿命内创建临时 &[u8]/&mut[u8]，不在交出所有权后保留 Rust 内容引用。
  token 的 unsafe 构造者证明范围、初始化、非别名与 owner 寿命；底层 release/submit/cancel 也是 unsafe 接口，防止安全代码绕过 token 提前复用。
  客户端本地短锁只保护票据和 ring，不跨协议回调或 payload 编码。TX 通知失败锁存 abort，已发表的槽不能按可恢复错误重新借出。
  池 VMO 大小固定，仅授予 READ/WRITE/MAP/INSPECT/DUPLICATE/TRANSFER；设备 header 不属于客户端 payload；端点只授权指定系统组件。
  句柄/映射不证明字节借用权；原始映射依赖双方槽协议，DmaBuffer::share_vmo 的 unsafe 契约覆盖对端 CPU，Arc 只证明存活。
  现有主机栈已直接消费 RX 共享帧，并在预留 TX 槽中编码；AF_PACKET 直接写 header/payload，移除原临时 Vec 与 NIC 适配复制。
  Codec 所需零初始化保留在实际 TX 范围；TX 拥塞时仅用丢弃缓冲推进协议，不发布替代 DMA 或阻塞 RX/ACK。
  Socket/capture 自有 payload 语义仍需相应存储；最终每核路由引擎、跨网卡转发借用与新引擎的大包生成尚未完成，不能称完整零拷贝路由数据面。
  所有权循环参考固定 Linux revision 的 [AF_XDP UMEM](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/Documentation/networking/af_xdp.rst)，不导入 Linux UAPI 到内核。
- 会话最后一个 handler/activity 退出后，析构有界排空已经接受的 TX，归还所有 CPU RX；未接受后缀不再发布。
  超时、协议损坏或硬件失败时同步 stop 全设备，不能把取消/超时当作 DMA 已结束。只有排空或确认 reset 后才发表 closed 并唤醒客户端。
  正常关闭也同步停止本次 RX DMA，随后才能 closed；未归还的 RX 仍由硬件 owner 保活至 stop。
  停止只废弃本次配置，controller 在旧租约彻底退休后允许重新配置另一数量的队列，本期不承诺故障后恢复运行时状态。
  Benchmark 决策：每 pair 四环和固定 ticket 表换取无逐帧分配/无逐包 RPC，默认 32 容量的四环连同头部共两页；
  host codec 用例覆盖容量、回绕、错误位置和 kick 重新武装；实际 device-model 的提交/取消用例与现有引擎 wire 用例覆盖帧寿命及直接写入。
  Benchmark 决策：借用帧替代每包 Arc/CPU 帧复制，减少 RX 栈缓冲及 AF_PACKET 临时分配；保留 codec 零初始化，不以 host 假 DMA 代替真实设备吞吐。
  实际 DMA、跨核唤醒和批处理吞吐在最终来宾验收测量。
- VirtIO-net 的 MQ/RSS 都依赖 CTRL_VQ；只在控制队列可用且至少支持其一时协商它。max_virtqueue_pairs 从同代配置读取并校验 1..=0x8000。
  VirtIONetworkDevice 只拥有 transport、能力与冷配置租约；ActiveNetwork 拥有本次配置的 queues/control/pools/failed。
  发现设备时不创建数据队列或置 DRIVER_OK；OPEN 按请求重新协商 feature、准备精确数量的队列与 reset owner，再启用 DMA。
  初始化失败尚未发表租约时 Drop 只 reset，不能重入已经持有的配置锁；租约 Weak 即使失效也不是可重配的证明，必须等旧 owner reset、退休全部 queue/pin/IOVA 并显式撤销。
  已 stop 的租约若仍被完成线程临时保活，OPEN 等待 controller 的 retired Event；最后一个 owner 在清空租约的同一锁内 signal，新租约发表前清除。
  所有 Weak upgrade 得到的强引用在配置锁外 drop，避免最后引用析构反入该锁；没有定时轮询或逐包开销。
  IRQ 不强持 ActiveNetwork，回调由 controller 的租约保护并在锁外调用；错误完成线程先无锁观察错误，仅错误时 try_lock 消费本代锁存并临时 upgrade 租约，竞争时重调度，不能阻塞其他设备的完成；数据收发不取配置锁。
  IRQ 的 ACK/错误锁存与冷重配串行，重开先清除旧锁存；控制 used/ACK 不依赖 IRQ 服务线程，不会因该串行化互等。
  各 pair 独立准备 ring、RX/TX pool 和锁；控制队列位于 `2 * max_virtqueue_pairs`，不以活动数猜测控制队列编号。
  DRIVER_OK 后，MQ 使用 VQ_PAIRS_SET；仅 RSS 的硬件用标准 RSS_CONFIG（hash=0、未分类队列 0、max_tx_vq=请求数）初始化。
  只有唯一 head、设备写入长度与 ACK 全部正确后才发布 adapter；失败/超时先 reset 再释放 DMA。
  控制命令为冷初始化，有界睡眠等待，不占数据队列锁、不忙等。共享 MMIO/INTx 中断通知全部 pair，尚未声称实现每队列 MSI-X。
- RSS 按固定 [VirtIO 1.4 CS01 网络规范](https://github.com/oasis-tcs/virtio-spec/blob/917e900e0246b7fe21cdde795b0e566dd4f57d8d/device-types/net/description.tex)实现可配置 Toeplitz 分流。
  只协商 RSS，不协商 HASH_REPORT，收发 header 保持 12 字节。能力来自同代配置的 key/table/hash-types；key 上限至少 40、table 上限至少 128，
  领域 hash bits 只涵盖 IPv4/TCPv4/UDPv4、IPv6/TCPv6/UDPv6 与 IPv6 EX 三类；不公布未实现的隧道散列。
  v9 静态描述在 maximum-pairs 后编码 TX flags(u8，checksum/TCPv4/TCPv6/ECN 为 bits 0..3)、普通帧上限(u32)、TX 容量上限(u32)，
  之后为 rss-present(bool)、可选 key-max(u8)/table-max(u16)/hash-types(u32)、MAC/name。未知位、依赖和容量矛盾均拒绝。
  RSS_CONFIG 冷请求携带 length(u64) 和只读 VMO，VMO 格式是 hash-types(u32)、unclassified(u16)、key-length(u16)、table-count(u32)、
  table-count 个 u16 pair 下标、原始 key 字节。所有整数小端；服务先有界复制快照，再按同一领域校验检查权能/长度/队列，避免 TOCTOU 及 64 KiB channel 限制。
  表长为 2 的幂且不超过设备上限，所有下标在本次会话内；启用散列时 key 非空且不超过设备上限；40-byte key 是执行者策略，不作为拒绝合法短 key 的协议门禁。禁止硬编码设备不支持的 hash bits。
  硬件控制 payload 使用标准 hash-types/mask/unclassified/table/max_tx_vq/key-length/key 顺序，pair 下标就是 rss_rq_id（实际 virtqueue index >> 1），max_tx_vq 是数量。
  控制 DMA 按真实上限预分配连续 backing/pin（通常一页），只有输入与 ACK 两个 descriptor；不为普通 NIC 固定预留大 RSS 缓冲。
  串行控制锁拥有整个 command→ACK 生命周期；未确认、拒绝或畸形完成均 reset，不能假定失败时硬件保留旧表。
  Netstack 在多 worker 时选择均衡表和随机 40-byte key，单 worker 保留标准初始化、不额外计算散列。RSS 切换可能移动既有流，worker 切换职责仍属于协议引擎。
  Benchmark 决策：编码/VMO/连续控制页只在冷配置分配，不改变逐帧缓冲与复制路径；主机 codec 覆盖超 64 KiB 表及非法队列/hash，真实分流和吞吐在最终来宾验收。
- TX partial checksum 仅在设备提供 `VIRTIO_NET_F_CSUM` 时协商并发布。TX 第四字为零表示逐字节发送；
  非零字的 bit 48 是 presence，低 48 位依次为 start/offset/header-length 三个 u16，其余位必须为零。
  提交前校验 `start + offset + 2 <= header-length <= length`，不从包内容猜测是否请求卸载。
  VirtIO header 置 NEEDS_CSUM，写入 hdr_len/csum_start/csum_offset，gso_type 保持 NONE；发送者预填伪头 seed。
  当前主机栈仅对实际 TCP/UDP/DHCP emitter 的明确 PacketMeta 请求启用，RAW 与 AF_PACKET 不请求、不重写其 transport 字节；
  无能力或分片回退时由同一 emitter 计算完整软件校验和。IP/ICMP 仍软件校验；现有主机 emitter 不生成 TSO 大包。
  `packet-wire::checksum` 拥有 RFC 1071 的无分配、分段奇偶保留的 64-bit one's-complement 累加；adapter 只组合伪头，driver-host 的 RX NEEDS_CSUM 使用同一累加器补全。
  驱动物理租约/失败归 net.rs，pair 队列/DMA 收发归 net/queue_pair.rs，控制命令归 net/control.rs。
  Benchmark 决策：每条环记录增加 8 字节，换取不逐包发送控制 RPC；伪头处理不扫描 payload，真实卸载吞吐留至统一硬件验收。
  出口 tap 位于硬件之前，卸载包的 checksum 尚为 seed；AF_PACKET 经现有 socket 环传递状态，Linuxd 用标准 AUXDATA 标明 CSUMNOTREADY，边界见 socket ABI 矩阵。
- TX 分段协商 HOST_TSO4/6，仅在 CSUM 存在时启用；HOST_ECN 依赖 TSO，提供准确头长时协商 GUEST_HDRLEN。
  TX 第五字为零表示无分段；否则低 16 位为 MSS、bits 16..17 为 IPv4(1)/IPv6(2)、bit 18 为 ECN，其余位拒绝。
  分段必须同时提交 checksum、完整 TCP 头与非零 MSS，单段全部头加 MSS 不超过普通帧上限；无分段时仍按普通 MTU 拒绝大包。
  net/offload.rs 的 VirtIO header 按地址族置 TCPV4(1)/TCPV6(4)，ECN 为 0x80，写 gso_size；其余元数据归零，payload 不复制或改写。
  OPEN 明确请求 payload 容量，范围为普通帧上限至硬件公布上限；每槽 stride 为 (12 + 请求容量) 向上对齐 64。
  当前 VirtIO 普通帧上限 1514，具备 TSO 时容量上限 65549；旧主机引擎只请求 1514，不为闲置能力预留大槽。
  默认 pair 数据页仍为 112 KiB，32 个最大 TX 槽按需增加到 2052 KiB TX 页；sg descriptor 容量不足只接受批前缀，保留其余帧至完成。
  Benchmark 决策：五字记录默认四环仍为两页；逐包不分配，大槽与引用只在 OPEN 创建。实际分段正确性及吞吐在硬件验收。
- RX 协商 `VIRTIO_NET_F_GUEST_CSUM`，不协商接收 GSO。驱动在 ReceiveClaim 的唯一 CPU 槽内检查已初始化的 12-byte header；
  DATA_VALID 直接标记 transport 已验证，NEEDS_CSUM 先校验 start/offset 边界、按已有 seed 原地补全，再发表 checksum-valid；
  两标志都无则必须软件验证。未知 flags 按规范忽略，未协商的 GSO 或非法范围终止设备，不把畸形 DMA 数据交给客户端。
  不依赖 RX hdr_len，不在 NEEDS_CSUM 缺失时读取无意义的 checksum offsets。只访问 used_length，交付前不创建客户端内容引用。
  checksum-valid 随 v9 RX 第四字和 NetworkReceiveFrame 寿命传递；token 的 unsafe 构造者必须证明 true 的事实。
  主机引擎只在该次 ingress 的作用域跳过 TCP/UDP RX 校验，保留 TX 设置及 IP/ICMP 校验，结束时恢复；不能由全局设备能力跳过所有包。
  AF_PACKET 入队冻结 Verified，Linuxd AUXDATA 投影 CSUM_VALID；软件补全发生在 tap 之前，不会把未完成 checksum 当作已验证内容。
  Benchmark 决策：DATA_VALID 不扫描 payload，NEEDS_CSUM 仅在 driver 计算一次，四环记录不再扩容；本次未测量真实设备卸载吞吐。
- 设备失败原子状态为 Live(0)/StopRequested(1)/ResetOwner(2)/Quiescent(3)；先 0→1 拒绝新 DMA，再非阻塞锁过所有 pair/control，
  仅 1→2 的唯一 owner reset，最后 Release 发表 3。完成线程遇到忙队列/控制事务就重调度其他完成源，锁存保留到 DMA 已停止；
  同步 stop 才在无锁冷路径 sleep 等待。并发 stop 必须等到 Quiescent，不能看到已请求撤离就提前归还 DMA。
  descriptor publication 与 doorbell 都在所属队列锁内，延迟通知不能跨过故障屏障；doorbell 失败同样 stop，reset 失败 fail-stop 保活所有存储。
  统计只在冷查询时汇总各队列权威计数。
- 当前主机栈以轮转起点消费所有 RX pair，回复预留回到原 pair；显式外发/AF_PACKET 使用 queue 0，后续每核 worker 不受此适配层限制。
  TX 完成返回实际 completed 数，主机栈跨队列/网卡扣同一轮预算，零预算仍可观察 backlog；畸形完成数不得扩大预算。
  Benchmark 决策：每 pair 独立锁与预分配池换取并行，驱动队列热路径不分配；活动数量增加时存储按请求 pair 数线性增长。
  冷配置锁隔离重开/初始化，当前共享 INTx 的回调短锁/Arc 成本在最终目标机诊断，不用 host 模拟设备时延；
  整机吞吐/内存及共享 IRQ 扇出成本在最终来宾诊断。当前主机 TSO 大包生成和 MSI-X 仍待路由阶段补齐，不能据此宣称数据面完成。
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
