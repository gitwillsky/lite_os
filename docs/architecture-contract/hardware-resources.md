# 硬件资源契约

原生硬件权限与对象由 `kernel::object::resource` 拥有；架构编码归 `arch`，机器装配与中断控制器归 `platform`。
驱动初始化、设备侧消息使能和停止源属于 driver-manager/driver-host；应用不接触这些原生对象。

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
SMMUv3/RISC-V 系统 IOMMU，以 PCI 根盘/网卡和 MMIO RNG 验证混合 DMA 的真实读写、同步与网络；不宣称设备
热移除、故障注入或物理机验收。

## 消息中断

1. `resource::MSI` 授权 32-bit 消息设备 ID 的不可变子范围；`MSI_ALLOCATE(resource, bti, device, count, out)`
   要求 resource WRITE 与 BTI READ|WRITE，非零 count。同设备同时只允许一个组；不支持返回 NOT_SUPPORTED。
   root 装配必须保证 MSI authority 与 BTI 对应同一设备，消息 DeviceID 不能从 IOMMU stream ID 猜测。
   driver-manager 按 `msi-map`/mask 解析 RID→DeviceID，核对唯一 ITS phandle、msi-controller 与一 cell 格式；
   与 IOMMU map 共用 RequesterMap 的区间/溢出/重叠/mask 校验，两个域各持独立数据，声明未覆盖 requester 时拒绝。
   宿主启动与管理器 checkpoint 都按 IRQ、MSI、MMIO 的同一顺序携带 authority；Node 固定格式保持 124 bytes，生命周期 ABI 为 5（含网络 v13 与 native MSI-X 源控制），
   旧映像不得与新启动/交接布局混用，须成套更新启动件。
2. `ObjectType::Msi`、`info::MSI` 的 `MsiInfo` 只含 count u32（4 bytes）；地址/消息值由内核拥有，不向驱动复制路由状态。
   `MSI_CREATE_INTERRUPT(msi, index, table_vmo, table_offset, out)` 要求组 WRITE 与 VMO READ|WRITE|MAP。
   VMO 必须是物理 Device 类型；内核按标准表项大小从 index 投影范围，校验对齐、越界和溢出。
   每槽只允许一个子 Interrupt；先独占登记再接触寄存器，失败归还登记，重复创建不能改写活源。
   IOMMU 映射接收现有 CachePolicy；PMT 从 PinnedRange 的最终 VMO owner 投影属性（包括 slice），
   doorbell 显式为 Device，不能把 MMIO 误映射为普通可缓存内存；不新增缓存属性状态。
   组内 `issued` 锁独占槽登记；子对象强引用组，最后一个句柄与子对象消失后才允许 ID/ITT/IOVA 复用。
3. `MessageInterruptGroup` 静态选择目标 backend。AArch64 ITS 按 compatible 从 DTB 识别，支持单个 coherent
   GICv3 ITS 的物理 LPI；标准 BASER 直接/间接表、MAPD/MAPC/MAPTI、MOVI、INV、CLEAR、DISCARD 与 SYNC。
   GIC claim 只把 1020..1023 视为特殊值，LPI 不误当 spurious。原生对象按固定 Fuchsia dispatcher 模型拥有已绑定 MSI-X 表项：
   创建先 mask/安装消息值、绑定成功后才 unmask，IRQ 在源表项 mask，port 已补回预留后 ack 才 unmask。
   每次写入用同 BAR readback 排空 posted writes；不逐 IRQ 修改 ITS property 或向所有 collection 发 INV/SYNC。
   Source IRQ 锁串行 mask、时间戳发表、初始使能、消息重定位与退休；持锁不调用平台。锁序为控制器 IRQ → Source，
   退休先在 Source 锁内 mask/清目标/置 Retired，释放锁后才 unbind；旧 deferred 引用析构不再访问被新会话复用的表项。
   binding 失败同样退休源并移除待分派记录。VMO 只投影已授权的非 RAM Device 窗口，内核固定 direct-map PTE
   与静态 BAR 同机器寿命，重开不累积 VMA/PTE；不声明 BAR 重分配或物理热移除能力。
   关闭先屏蔽、CLEAR pending、同步并 unbind，再归还槽；整组 DISCARD 与 MAPD invalid、覆盖各 collection 的 SYNC 完成后才释放 ITT。
   MOVI 的同步覆盖旧/新 collection；不保存另一份 CPU 路由状态。调用者必须先停止设备消息源。
   消息目标由 index 的准备令牌投影，不假定同组共用物理地址或 data 连续；原生亲和调用走 Registration 的唯一事务，
   不再由 wired IRQ 入口替消息中断改路由。MessageInterruptRoute 先取得冷配置锁并预备目标，取消不改旧路由；
   Msi 按物理 doorbell 页惰性建立 Device BTI 映射，所有 Vec 预留、IOMMU 分配及可失败锁等待均在源修改前。
   映射 cache 只保存实际 IOVA owner，同组迁回已有目标复用；组先撤销路由，再析构全部映射，不按每向量重复映射同一页。
   锁序为 platform 冷配置 → Msi doorbell → IOMMU，初始绑定释放 doorbell 与平台冷锁后才进入 GIC；IRQ 从不获取这些冷锁。
   Source 在 mask/readback 后改写消息值，进入 Updating 并在 IRQ 锁外提交路由；期间 IRQ/ack 只修改恢复意图，
   提交结束按最新意图恢复 Armed/Masked。平台冷令牌保留到恢复完成，防止下一配置抢入；
   调用强引用使本事务期间不可能退休 Interrupt。准备失败保留旧目标和确认状态；提交后超时只能 fail-stop。
   子 Interrupt 创建同样先准备 boot CPU 目标、映射并安装 masked 源，再提交并在冷锁外绑定；
   即使同组 index 曾配置过其他 CPU，重建子对象也恢复工厂的初始目标，不继承已退休对象的 affinity。
4. platform 通过 `MessageInterruptAllocator` 接收上层 `DmaFrame` 的清零连续驻留存储；type erasure 只用于
   存储寿命/物理地址，不做架构分派。分配策略仍在 object/memory；platform 不依赖通用 memory 或 timer。
   BASER/command/pending 表分配与必要元数据准备先于硬件使能；表的 shareability/cacheability 必须读回匹配，
   不提供 non-coherent shadow fallback。发布后的超时 fail-stop，不能在硬件仍持指针时释放存储。
5. ITS 的唯一 static Controller 延迟到第一次请求才创建硬件表。`state` TaskMutex 串行化分配/退休/改路由；
   `properties`/`commands` Once 仅发布一次并保活 DMA 页，避免中断访问已释放表。
   `Properties.routes` 是 first-LPI→device/event 的唯一索引；IRQ 锁内只 lookup/预备 node 的 commit/remove，绝不分配。
   锁序为 GIC IRQ → routes → commands，冷路径 state → routes/commands；不得持 GIC IRQ 锁取得可睡眠 state。
   LPI 属性的 AtomicU8 由 GIC 锁串行化 CPU 修改，硬件只读；完成屏障后单 event INV，并同步各 collection。
   缺少 INV/SYNC 会继续使用旧 enable/路由；不以 INVALL 清空其他队列缓存，也不依赖可选 DirectLPIS。
6. 组先撤销路由，再由独占 Doorbell 映射 guard 撤销 IOMMU 翻译并完成失效，最后归还 IOVA；子中断先 unbind。
   VirtIO PCI 网卡先 function-mask、禁消息和 INTx、读回完成，再 reset；reset 完成后才清空 native 子中断和 DMA。
   新宿主在任何新 DMA/消息分配前先屏蔽遗留源并 reset；未解除的 payload pin 继续沿 BTI quarantine 规则退休。
   不宣称物理设备热移除或共享 IOMMU stream 的隔离组验收。
   共享 pending 队列按存活对象预留容量；并发创建必须在发布锁内重验容量。关闭移除旧代排队项，并与 deferred
   port deliver 串行化，防止已关闭对象继续投递或耗用新对象的预留槽。
   每个绑定先 reserve，deferred deliver 只消费预留，不能在中断路径扩容并吞掉 OOM 唤醒。
   port 状态为 Reserved/Delivered/Rearming；ack 唯一领取 Rearming，在对象锁外补回队列空间才恢复 Reserved/硬件使能，
   OOM 退回 Delivered，重复/提前 ack 返回 BAD_STATE；bind 前或 Delivered/Rearming 期间的 edge 通过 triggered 补偿重排队。
   关闭归还未消费预留；不允许在最后一个调用引用释放前销毁 Rearming 对象，锁序 slot→port，反向不得取锁。

## RISC-V AIA

唯一 RISC-V QEMU 产品后端使用 APLIC/IMSIC（Smaia/Ssaia），不保留 PLIC 兼容路径。
CPU identity、interrupts-extended 中的文件序号、物理文件地址与 APLIC 的 CPU 目标索引 分别解析；
S-mode 控制器由 cause 9 与 msi-parent 的 phandle 关系确定，不从 硬件 CPU ID 或节点名推导地址。
IMSIC windows、guest/CPU/group 位宽、每个启用 CPU 的文件、APLIC source 范围及 UART parent 必须校验完整。

1. 自带 M-mode SBI 固件拥有根 APLIC。它按 DTB 解析 M/S IMSIC 的地址几何、子 domain 和 delegation，
   在发布 SBI 前设置根 mmsiaddrcfg/smsiaddrcfg、读回校验后锁定，并逐源交给 S-mode 子 domain。
   当前 QEMU 装配要求所有外部源委托给唯一 supervisor domain，拒绝不完整或相互冲突的描述。
   每 CPU 在 DTB 声明 Smstateen 时开放 mstateen0 的 AIA/IMSIC/CSRIND 位；无此扩展不访问该 CSR。
   S-mode kernel 只映射与操作 S-APLIC 和 S-IMSIC，不越权配置 M-mode 根。
2. 消息组分配稳定的软件 vector 租约；子 Interrupt 才准备目标 CPU 的物理 EIID。
   每 CPU 的槽表/dirty bitmap 在启动时预分配，EIID 0 无效，1 专供 GENMSI fence，2 起供真实设备。
   CPU 槽耗尽返回 NO_RESOURCES，元数据分配失败返回 NO_MEMORY；准备失败不改活路由。
   CPU 本地 CSR 编码只在 arch；platform 独占 vector→handler/target、EIID→vector 与物理 MMIO 文件。
3. 冷路径 TaskMutex 串行化分配、迁移与退休，随后短持 routes IRQ 锁提交已准备的 node；不得反向取冷锁，
   IRQ 锁内不分配、不跨 CPU 等待。Source IRQ 锁在平台提交前释放；控制器 IRQ 可以获取 Source，反向不成立。
   request Release 发布 dirty 路由变更，本 CPU 通过 SSIP 或关中断的协作等待更新 EIE/EIP，completion Release/Acquire 确认。
   等待同时推进本地 AIA 与注入的 barrier 确认，避免跨种类 rendezvous 相互等待；没有新增运行时分派或全局回调。
4. 迁移先由 MSI-X 源 owner mask/readback 并改写目标，随后发布新 EIID，关闭旧 EIE、提取旧 EIP 并向新文件补发 pending，
   旧 CPU 确认后才归还旧槽。令牌保留冷锁直到恢复最新 ack 意图；取消只释放未发布的新槽，提交后不能伪装回滚。
   Wired APLIC 先屏蔽源，通过旧 CPU 的保留 EIID 收到 GENMSI marker 来确认先前 MSI 已到达，再更新目标。
   marker 前清旧 EIP 并确认，marker 后等待 Busy 清除与实际接收，不能用固定延时猜测完成。
5. STOPEI claim 原子清 EIP；处理有界 batch，不二次 EOI。APLIC level 源在 handler 后按规范 SETIPNUM 重新采样，
   IRQ/ack 只修改源使能或迁移恢复意图，不在普通 IRQ 上发 IPI、获取冷锁或分配。
   解绑停止源后清 handler/target，保留路由吸收已 claim 的旧消息，等旧 CPU 清除 EIE/EIP 后再移除索引。
   Interrupt 构造时预备退休等待节点，Drop 不会因 OOM 无法取冷锁；ARM 的同接口令牌为零大小，不承担这项分配。
6. PCI 标准 msi-parent 直连 IMSIC，没有 ITS 的硬件 DeviceID。单 PCI domain 的 RID 仅作消息组的软件独占租约，
   不从 DMA stream 猜测，也不宣称 MSI 重映射隔离。driver-manager 校验唯一 S-IMSIC、零 cell 格式及 parent 引用。

Benchmark 决策：新增槽/bitmap 的内存有界于 possible CPU × 实际 IMSIC IDs，冷路径分配与退休不设 host 时延阈值；
IRQ 仅做有界查找、源寄存器操作和本地 CSR，不逐包分配。真实 late MSI、跨 CPU 迁移、APLIC level 重武装、
固件启动与性能仍需目标机验证；host decoder 或 Clippy 不构成这些硬件语义的验收。

## 当前范围与验证

- RISC-V 产品与普通 QEMU 门禁使用 APLIC/IMSIC；固件、内核、原生消息对象与驱动发现代码已接通，尚无启动及 MSI-X 硬件验收结论。
- AArch64 产品 machine 与门禁配置启用 ITS；kernel 和 driver-manager 从同一 DTB 分别消费机制窗口与消息映射。
  产品与门禁默认网卡使用 virtio-net-pci；PCI 枚举以消息关闭状态交接，由实际 adapter 声明是否接管 MSI-X。
  当前为 VirtIO-net，其他 PCI adapter 保持 INTx；不支持 MSI 的平台仍使用硬件提供的真实 INTx。
- 网卡会话按实际 pair 数准备 2N+1 个中断（每 RX/TX 队列各一项，加 config），control VQ 明确 NO_VECTOR 并沿用冷 ACK 轮询。
  table 容量限制可广告的 pair 上限，但 control VQ 位置始终来自设备原始 pair 上限，不能使用裁剪后的上限寻址。
  所有子 binding 与表项先准备；queue_msix_vector 写入并读回确认后才 QUEUE_ENABLE；最后启用 MSI-X，再 DRIVER_OK。
  功能与未使用旧项均先 mask；驱动只初始化尚无 native binding 的表项，不重写已绑定条目的目标或 mask。
  原生中断工厂接收物理 VMO 及表偏移，不经 MsiInfo 向驱动复制消息值；设备拒绝向量或分配失败返回错误，不缩减请求或临时退回 INTx。
- 每物理 NIC 共用一个 native port 与一个等待线程，按 vector 直接唤醒所属 pair；config 变化通知全部 pair，MSI-X 不读共享 ISR。
  `Lease.generation` 独占 port key 的会话代号，失败 OPEN 也消耗且不回绕；排队的旧代 packet 不 ack/唤醒新会话。
  当前会话在 lease 锁外短暂保活与确认，不能持 lease 锁释放最后一个引用；避免 Drop 重取锁死锁。
  `ActiveNetwork.interrupts` 的 Vec 保活内核组，reset 与各 DMA 区间退休后、允许重开前清空；没有每队列线程或通知转发线程。
- 网络 v13 静态描述包含独立队列亲和能力，只有实际 MSI-X adapter 提供；持有会话控制端点可用 QUEUE_AFFINITY
  配置 pair 的 RX/TX 中断目标逻辑 CPU，配置向量不移动，不增加应用 syscall 或扩大原生 authority。
  会话的 interrupt_configuration 冷锁串行 IRQ 配置，不持 DMA/lease 锁跨可睡眠路由调用，不堵塞其他队列/IRQ。
  stop 可并发 reset：调用强引用保活当前 IRQ 句柄，结束复检 failed；不会操作下一会话的资源。
  无效 CPU/未改变状态的 OOM 或资源不足直接返回；NO_RESOURCES（如 IOVA/中断槽不足）沿 NoResources 原样传递，
  第一条尚未改变时不误停网卡。主机 socket 的通用设备错误投影使用既有 NoBuffer/ENOBUFS。
  第一条意外失败或第二条失败先关闭准入、
  再在锁外 reset 整个设备并通知全部队列，不能返回成功或保留仍在运行的半配置；不复制旧 CPU 猜测回滚。
  多核 worker 的队列 CPU 策略尚未消费；没有 MSI-X 硬件/性能验收结论。
- 多队列创建必须结合最终 worker CPU 策略逐对绑定/迁移，避免把全部未分散向量长期挤在 boot CPU 的有限 EIID 池；
  不以软件 vector 的总容量冒充单 CPU 的物理 EIID 容量，池耗尽明确报错。该策略随阶段 2 的 worker owner 落地。
- Benchmark 决策：组分配、亲和准备令牌、按物理页的 doorbell 查找/首次映射与 DMA 存储 type erasure 都是冷路径，不设独立 host 阈值。
  源状态机的 IRQ/ack 分支有界、不分配；Updating 只在冷迁移期间出现，普通已屏蔽源不重复写 mask。IRQ 上新增固定表/有界 AVL
  查找与源表项的有界 dword 读写/readback，无分配；ITS INV/SYNC 只用于冷绑定/解绑与控制器配置；控制器时延无法由 host 替身证明，最终每队列/自适应轮询集成后
  必须测目标机 IRQ、吞吐与空闲成本。网卡新增的 port、向量 Vec 和绑定只在冷打开分配；每 IRQ 一次 Weak upgrade 与定向通知，
  不逐包分配，不为未打开的 queue 分配子 Interrupt，不以 RAM 寄存器替身证明 PCI posted write 或真实 LPI 路由性能。首次分配按需保留 64 KiB command ring、每 possible CPU 64 KiB pending
  table，属性/设备/collection 表按能力分配；未使用 MSI 时不承担这些页。
- 本批优先代码审查、双架构 Clippy 与 architecture-check；没有启动虚拟机、运行全量 verify 或修改 PVE。
