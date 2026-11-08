# 计划：端到端热路径性能与资源占用

Status: Active

执行期间存在；完成后把持久事实迁入架构/契约/性能文档并删除本文件。
各项“实施前状态”是立项基线，当前源码与验收状态以末尾进度为准；源码已实现和运行已验收分别记录。

## 目标

从桌面到服务到微内核，把仍停留在“同步、逐页、逐块、全局锁”第一版的热路径改为成熟系统（Fuchsia/Zircon、
Linux）已验证的结构：降低应用启动与文件 I/O 延迟、定时与交互抖动、内核/服务元数据占用与锁竞争。
不改变应用可见的 Linux UAPI 语义；不引入双轨实现或兼容入口，旧路径在新路径落地时同一改动内删除。

## 约束

- 遵守 AGENTS.md：每项先明确所属 module、owner、interface 与依赖是否变化，同步更新对应契约；
  新增 unsafe/global/lock/Atomic/cache/flag 写明所有权证明与缺失时的失败后果。
- 内核代码与注释不出现 Linux 概念；native-abi 变化需更新 `abi/native-abi` 与契约文档。
- 每项新增热路径、锁、分配或间接层都按[性能验证范围](../development/performance-coverage.md)作 benchmark 决策：
  blocking benchmark、target static/disassembly gate，或说明只作诊断测量。
- 验证节奏：实施期间以 Clippy 与代码审查轻量验证，阶段间不反复构建、运行测试或重启采样。
  全部计划完成后统一运行 `make verify`；端到端验收由用户执行。验证项保留到最终验收，不以静态检查冒充运行时证据。
- 不修改围栏、阈值或文档声明来掩盖实现错误；范围缩减写入 ABI 矩阵或当前架构限制。

## 基线（诊断，不设阈值）

改动前后在同一架构、CPU 数、内存与设备配置下记录，作为收益证据：

1. 冷启动到桌面稳定的时间；启动 file-explorer、notepad 的时间。
2. guest 内 `dd` 顺序读/写 64 MiB 文件（冷缓存、热缓存），小块追加写（4 KiB × 1024 + fsync）。
3. `nanosleep(1ms)` 循环 1000 次的实际时长分布；空闲桌面 10 s 内 boot CPU 的定时器中断次数。
4. 多线程 futex 竞争（N 线程争同一/不同 mutex）的吞吐。
5. `MemTotal - MemAvailable`，以及内核堆、页表页、linuxd 堆在 1/8/32 个 Linux 进程下的增长。
6. 文件密集 workload 下的缺页次数与 pager 请求次数。
7. 空闲桌面的内存构成（M0 落地后按 Pss/Private/Shared 拆分）：2026-11-07 实测（AArch64/HVF、11 核、2 GiB、
   3008×1692、开着任务管理器）已用 96.4 MiB，其中 Shmem 25.4 MiB（桌面背景整屏缓冲约 19.4 MiB）、
   AnonPages 18.0 MiB、KernelStack 5.0 MiB（约 161 线程）、Slab 4.0 MiB、PageTables 2.6 MiB，其余约 40 MiB 无法归属。
8. 任务管理器、资源管理器在普通尺寸、最大化、还原后三个状态下的 Pss；最大化后稳定状态与还原后的回落。

## 阶段与验证点

顺序：M0 → 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → P2；M1–M7 与 1–9 无代码依赖的可穿插执行，依赖关系在各项注明。
1 是 2 的前提；5、6 都改 VM 内核结构，分开提交。

### 1. 块设备协议：登记 VMO + 批量请求 + 并发队列

- **实施前状态**：`RemoteBlock::read_block`（`system/device-protocol/src/block.rs` 的 `impl BlockDevice for RemoteBlock`）
  每 4 KiB 一次同步 `channel_call`；服务端（同文件 `serve_device` 的 `READ`）每次分配 `Vec`、从 DMA 弹跳缓冲复制，
  经 channel 消息再复制两次。ext4 `read_storage`（`system/ext4/src/ext4/inode/vfs.rs`）逐块循环，
  `read_fs_block_from`（`system/ext4/src/ext4/block_io.rs`）逐块调用。queue depth 实际为 1；
  virtio-blk 固定 16 个请求槽（`system/driver-host/src/drivers/virtio/blk.rs` `BLOCK_REQUEST_SLOTS`）。
- **参考**：Fuchsia block FIFO 协议（`block_fifo_request_t`：vmoid、vmo_offset、dev_offset、length、group）；
  Linux blk-mq（Bjørling et al., SYSTOR 2013）。
- **改法**：
  1. 会话协议新增“登记 VMO → vmoid”与“FIFO/共享环提交多块请求、批量完成”；块数据不再进入 channel 消息。
  2. driver-host 用 BTI pin 客户端 VMO，virtio 描述符直接指向其物理页（scatter-gather），去掉弹跳缓冲复制；
     请求槽数按 virtqueue 大小协商。
  3. `BlockDevice` trait 改为区间读写（起始块、块数、VMO+偏移），ext4 按 extent 合并连续块；
     `BlockRange` 与 linuxd `PartitionDevice` 同步迁移。旧的单块 READ/WRITE ordinal 删除。
- **契约**：`docs/architecture-contract/devices-terminal.md`、`filesystem-storage.md` 的块协议与 DMA 所有权；
  pin 寿命与 PMT quarantine 规则沿用现有契约。
- **benchmark 决策**：设备 I/O 延迟只作 guest 诊断（基线 2）；协议编解码若进入每请求热路径，加 host blocking benchmark。
- **验证点**：ext4、fshost、linuxd 块设备与分区单元测试；运行时 BusyBox fsstress、ext4 门禁（统一阶段执行）。

### 2. 文件缺页预读与 fault-around

- **实施前状态**：内核 pager 请求长度固定一页（`kernel/src/vm/vmo/pager.rs` 构造 `PagerRequest` 处 `length: PAGE_SIZE`）；
  ext4 `Content::page_request`（`system/ext4/src/content.rs`）只补请求区间，且经 `Vec` → aux VMO 两次复制。
  linuxd 自有页缓存已有预读（`system/linuxd/src/fs/page_cache/fill.rs`），ext4 主路径没有。
  `AddressSpace::fault`（`kernel/src/vm/address_space.rs`）每次只装一页。
- **参考**：Linux on-demand readahead（Wu Fengguang, 2007）与 fault-around；Fuchsia fxfs/blobfs 预读。
- **改法**：
  1. ext4 PageSource 维护每文件顺序访问状态与窗口（起步 32 KiB，倍增到上限，上限不超过 512 KiB），
     按 extent 边界与 EOF 截断，存储直接读入 aux VMO（依赖阶段 1 的 VMO 区间读），去掉中间 `Vec`。
  2. 内核缺页在同一树锁事务中顺带装入相邻已驻留页（只读或已可写的页，窗口固定小值），不对缺失页发请求。
  3. 窗口状态的 owner 是 ext4 `Content`；内核不保存预读策略。
- **契约**：`docs/architecture/memory.md`、`filesystem-storage.md` 记录预读 owner 与 fault-around 范围。
- **benchmark 决策**：fault-around 改变缺页热路径，加 kernel-unit 驻留页装入的 release 诊断；预读收益用基线 1、2、6。
- **验证点**：vmo 单元测试（fault-around 不越过映射、权限与 COW 边界）；mmap 共享/私有、截断后读零。

### 3. 高精度单次定时器与空闲停 tick

- **实施前状态**：`TICKS_PER_SEC = 100`（`kernel/src/execution/timer/mod.rs`）；截止时间等待只在 tick 的 deferred work 中到期
  （`kernel/src/thread/mod.rs` `dispatch_deferred` → `timer_queue::expire`），平均晚 5 ms、最坏 10 ms；
  boot CPU 空闲时保持 100 Hz（`suspend_local_idle_tick` 断言非 boot CPU）。
- **参考**：Linux hrtimers 与 NO_HZ_IDLE（Gleixner & Niehaus, OLS 2006）；Zircon 每 CPU timer queue 按最早截止编程。
- **改法**：
  1. 定时队列改为每 CPU（等待者入队到当前 CPU，跨 CPU 取消按 token 定位）；硬件 deadline =
     min(最早定时器, 当前线程时间片结束)。
  2. 时间片抢占改由 slice 截止驱动；runqueue 只有当前线程且无定时器时不编程时间片截止。
  3. 去掉 boot CPU 常驻 tick；现由 tick 承担的 housekeeping（crash 进度、TIMER_BACKLOG）改为显式截止事件。
- **契约**：`docs/architecture/execution.md` 与执行域契约中的 tick、空闲与计时语义；用户/内核时间记账改为在
  切换与 trap 边界累计（不再依赖 tick 采样）。
- **benchmark 决策**：保留并扩展现有 timer deadline blocking benchmark（插入/取消/到期 O(log n)）。
- **验证点**：timer/调度 kernel-unit；运行时基线 3；`/proc/stat` 时间记账门禁。

### 4. futex 分片与不在锁内缺页

- **实施前状态**：`static TABLE: TaskMutex<FallibleMap<...>>`（`kernel/src/syscall/futex.rs`）全系统一把可睡眠锁；
  wait/requeue 在持锁后 `call.read(address)`，可能缺页并等待 pager I/O，期间全系统 futex 串行。
- **参考**：Linux futex hash bucket 与 6.16 进程私有 futex hash；Zircon per-process FutexContext。
- **改法**：
  1. 私有 key 进入地址空间所属的哈希表，共享 key 进入按 VMO 的全局哈希；每桶 IRQ 安全自旋锁。
  2. 桶锁内只用不缺页的用户读（fault-recovery 失败即返回）；缺页时释放桶锁、在锁外完成缺页后重试。
  3. requeue 跨桶时按桶地址排序加锁。
- **契约**：执行域契约中的 futex owner 与锁序。
- **benchmark 决策**：加 host blocking benchmark：无竞争 wait 入队/wake 摘除的 CPU 成本。
- **验证点**：futex 单元测试（丢唤醒、超时、requeue、共享 key）；基线 4。

### 5. 共享进程的页表子树共享（unified aspace）

- **实施前状态**：每个 Linux 进程有 normal/private 两个根，私有区叶项装两遍（`kernel/src/vm/execution_views.rs`）；
  linuxd 共享区在每个成员的 normal 根各装一份，`zap_range` 遍历全部成员（`kernel/src/vm/address_space.rs`）。
  N 个进程就有 N 份 linuxd 页表，且各自缺一次页。
- **参考**：Zircon 为 Starnix restricted mode 实现的 unified aspace（顶层项引用共享/受限 aspace 的下级表）。
- **改法**：
  1. 共享区与私有区按顶层页表索引对齐划分（`SHARED_START` 对齐到顶层项粒度）。
  2. 共享区下级表只由共享地址空间拥有并安装一次；成员 normal 根的顶层项引用它。private 根与 normal 根
     引用同一私有区下级表。叶项装入/撤销只发生在唯一 owner 子树，TLB fence 覆盖全部引用该子树的 ASID。
  3. 页表页寿命：被引用子树在所有引用根退休、fence 完成后释放。AArch64 与 RISC-V 同时实现。
- **契约**：`docs/architecture-contract/memory.md` 的页表 owner、`execution_views` 不变量与 fence 规则。
- **benchmark 决策**：AArch64/RISC-V 的 PTE/TLBI 现有 static/disassembly gate 继续覆盖；页表内存用基线 5 诊断。
- **验证点**：kernel-unit 页表与 iommu/vmo 测试；restricted 隔离（private 根看不到共享区）的现有门禁。

### 6. VMO 页列表改为分块节点

- **实施前状态**：`committed: FallibleMap<usize, Page>`（`kernel/src/vm/vmo.rs`），每页一个 AVL 堆节点
  （`libraries/fallible-tree/src/lib.rs` `Node`，约 64 B/页），缺页路径分配节点并旋转；区间操作逐节点走指针。
- **参考**：Zircon VmPageList（每节点 16 页数组，WAVL 索引）；Linux XArray。
- **改法**：节点覆盖对齐的 16 或 64 页，每槽压缩为 u64（ppn + pins/cleanliness/状态位，pins 溢出时 fail-stop
  或单独计数表）；节点仍用 fallible-tree 索引以保留“准备可失败、提交不失败”的契约。zap/supply/evict/writeback
  的区间扫描改为槽数组遍历。`requested` 表同样分块或并入槽状态。
- **契约**：内存契约中的 VMO 页状态表示；页状态机不变。
- **benchmark 决策**：扩展现有 VMO snapshot release fixture，加 1024/65536 页提交、区间撤销的 release 诊断；
  元数据占用用基线 5。
- **验证点**：vmo 单元测试全集（COW、hidden 折叠、slice、pin、pager 脏页状态、淘汰）。

### 7. 服务运行时堆分配器

- **实施前状态**（`system/native-runtime/src/heap/allocator.rs`）：全局单锁；≤2 KiB 为 2 的幂分级（平均约 25% 内部碎片）；
  2 KiB 以上（恰好 64 KiB 外）每次 `Vmo::create` + map + 裁剪 unmap，释放时 unmap 并等 TLB fence；`Vec` 超过 4 KiB
  每次扩容都走此路径；小块永不归还。linuxd 每个 Linux 线程对应一个原生线程，全部争这把锁。
- **参考**：mimalloc（Leijen et al., APLAS 2019）分片空闲链与线程本地堆；snmalloc（ISMM 2019）远端释放消息。
- **改法**（区块按需切分见 M3，两者一起做）：
  1. size class 间距不超过 12.5%；线程本地小对象页，远端释放进所属页的原子链。
  2. 中等对象从启动时预留的 VMAR 区间按 span 分配，backing 用少量大 VMO，按需提交；空 span 用 VMO
     decommit（`OP_DECOMMIT`，没有则补齐原生接口）归还物理页，不 unmap。
  3. `realloc` 在 span 内原地扩展；大对象（≥256 KiB）保留独立映射。
- **契约**：`docs/architecture-contract/userspace-abi.md` 或 native-runtime 所在契约中的堆 owner 与内存归还语义。
- **benchmark 决策**：加 host blocking benchmark：小对象 alloc/free、跨线程释放、Vec 增长到 64 KiB。
- **验证点**：native-runtime 单元测试；linuxd 堆在基线 5 的增长曲线。

### 8. 页回收分代与共享零页

- **实施前状态**：`kernel/src/vm/vmo/reclaim.rs` 按 VMO 轮转、按页号顺序淘汰干净页，不看访问；只在用户缺页入口触发；
  匿名读缺页分配新零页（没有共享零页）。
- **参考**：Linux MGLRU（6.1）；Zircon page queues（accessed 位老化、后台 evictor）。
- **改法**：
  1. 收集页表 accessed 位，pager 页按代分队（槽状态位，依赖阶段 6）；淘汰从最老代取。
  2. 后台回收线程按低/高水位工作，缺页入口保留为兜底。
  3. 只读缺页未提交的匿名页映射全局只读零页，写时分配；COW/pin/DMA 路径不得拿到零页。
- **契约**：`docs/architecture.md` 当前边界“没有后台回收线程”一句与内存契约同步修改。
- **benchmark 决策**：回收质量用诊断 workload（工作集大于内存时的重复缺页数）；零页装入走既有缺页 benchmark。
- **验证点**：vmo 回收单元测试；内存压力门禁；`/proc/meminfo`/vmstat 字段一致。

### 9. ext4 写回：单份数据与延迟分配

- **实施前状态**：`write_data_locked`（`system/ext4/src/ext4/storage_mutation.rs`）非整块写从存储读出整块再改；数据块以
  `BlockKind::Data` 复制进 `StagedBlocks`（`block_io.rs` `write_data_block`），与内容 VMO 页重复；
  `Content::write`（`content.rs`）先直写存储再写 VMO。
- **参考**：Linux ext4 `data=ordered` + delalloc，数据从页缓存直接回写，不进 journal。
- **改法**：
  1. write 只写入内容 VMO 并标脏，按 delalloc 预留块配额，ENOSPC 仍在 write 返回。
  2. 回写时分配块（尽量连续 extent），ordered 数据在 commit record 前直接从 VMO 页经阶段 1 的区间写落盘。
  3. 非整块写不再读盘（页已在 VMO 中，缺页由 pager 补）。删除数据块的 staged 副本。
- **契约**：`filesystem-storage.md` 的写入、回写、fsync 与 ENOSPC 契约；ABI 矩阵中写语义不变。
- **注意**：同时复核 TODO.md “ext4 目录叶子块校验和间歇性失败”，时序变化可能使其更易或更难复现，
  不得以此为由改目录锁。
- **benchmark 决策**：基线 2 的小块追加写与 fsync 诊断。
- **验证点**：ext4 单元测试（崩溃一致性/journal replay、截断与回写竞态、ENOSPC）；fsstress。

### P2. 响应速度打磨（P1 完成后逐项评估）

1. **截止时间调度类**：`kernel/src/thread/scheduling.rs` 只有 CFS 权重。给 compositor、audio-core、输入线程
   提供原生 deadline profile（参考 Zircon deadline profile、Linux SCHED_DEADLINE；公平类可参考 EEVDF 的 lag/slice）。
   授权经现有 resource 能力，不对 Linux 应用开放新私有接口。依赖阶段 3。
2. **restricted 状态直接映射**：`kernel/src/syscall/restricted.rs` 的 `read_state`/`exit` 每次经 `Vmo::read/write`
   （取树锁、查页）。绑定时 pin 并在内核映射，进出只做内存拷贝。
3. **channel 消息**：`kernel/src/object/channel.rs` 每消息两个 `Vec`、每 call 一个 `Arc<CallWaiter>`；
   `MAX_PENDING_MESSAGES` 按条数计（4096 × 64 KiB），改为按字节上限并池化缓冲。
4. **合成器**：`user/programs/compositor/src/compose.rs`、`compose/gpu.rs` 每帧整屏重画、无遮挡剔除；
   按 damage 并集 scissor、跳过完全遮挡窗口，CPU 后端只重画 damage，扫描输出使用 `FB_DAMAGE_CLIPS`。
5. **网络数据面**：`system/network-protocol` 每次 send/recv/poll 一次 RPC，>48 KiB 每次新建 VMO；
   改为每 socket 共享数据环或原生 socket 对象承载字节流（参考 Fuchsia zx socket 数据面）。
6. **物理页 per-CPU 缓存**：`kernel/src/memory/frame_allocator.rs` 全局 `IrqMutex`；加 per-CPU 单页缓存并按水位批量归还。

### 内存占用（M 系列）

目标：不降低画质、帧率与响应，把空闲桌面的已用内存从约 96 MiB 压到约 55–60 MiB（估算，以 M0 的 Pss 实测为准），
并消除窗口最大化带来的数十 MiB 增长。不采用 16 位色缓冲、降低输出缩放或匿名内存压缩（CPU 与延迟代价不值得）。

#### M0. 内存统计口径（先做，其余各项以它验证）

- **实施前状态**：linuxd 没有 `/proc/[pid]/maps`、`smaps`、`smaps_rollup`，无法区分 Pss/Private/Shared；
  task-observer 的 `resident_bytes` 重复计入共享页；`/proc/meminfo` 的 `Cached` 恒为 0，`MemAvailable` 等于 `MemFree`，
  可淘汰的干净 pager 页未计入；内核 `SystemMemoryStats` 只有堆、内核栈、页表与淘汰计数，约 40 MiB 无法归属。
- **改法**：
  1. linuxd 按 Linux 规范实现 `/proc/[pid]/maps`、`smaps`、`smaps_rollup`（Rss/Pss/Shared_*/Private_*/Anonymous），
     数据来自 linuxd 已有的 VMA 与后备对象。
  2. `/proc/meminfo` 的 `Cached` 与 `MemAvailable` 计入 pager 支撑的干净页。
  3. 原生侧：task-observer 记录增加 private/shared 拆分（内核按 VMO 共享计数计算）；`SystemMemoryStats`
     补充 VMO 已提交页按类别（匿名、pager、连续/DMA）与 bootfs 的统计，使 meminfo 已用量可以完整归属。
- **契约**：ABI 矩阵（procfs 文件与字段范围）、task-observer 协议版本（改 magic）、native-abi `SystemMemoryStats`。
- **验证点**：procfs 单元测试；基线 7、8 能完整归属到各类别。

#### M1. 窗口最大化与窗口缓冲（每个窗口 1–3 倍窗口大小；最大化时数十 MiB）

- **实施前状态**：
  - `classic-ui/src/window_host.rs` 的 `paint` 每次都把整窗画进私有 `pixels: Vec<u32>`，再经 `Display::present`
    复制进共享缓冲；`wayland/frame.rs` 逐行比较前后两帧来求 damage。
  - 每窗口最多 3 个共享缓冲（`WINDOW_BUFFER_LIMIT`），前一个缓冲尚未 release 时就新建；同尺寸的空闲缓冲在
    尺寸稳定后不退休，只在尺寸变化或窗口关闭时释放。首次使用时 `preallocate` 提交全部页。
  - 3008×1692 下最大化窗口单份约 19 MiB：私有画布 1 份 + 共享缓冲 1–3 份，即约 38–76 MiB；
    还原后画布会缩回，但同尺寸以外的空闲缓冲要等下一次提交才退休。用户实测任务管理器最大化后增长数十 MiB。
- **改法**：
  1. `WindowHost` 改为经已有的 `Display::paint_frame` 直接在已释放的共享缓冲中绘制，删除私有画布与
     `present` 的整窗复制路径（`present` 只保留给确实持有外部像素的调用者，如视频帧；没有调用者时删除）。
  2. 控件树已有的失效区作为 damage 与重绘裁剪：缓冲内容保留时只重画失效区；拿到的不是上一帧缓冲时，
     按 buffer age 语义（参考 `EGL_EXT_buffer_age`）从上一帧复制失效区之外的变化，或整窗重画。删除逐行像素比较。
  3. 稳定状态只保留 1 个缓冲：合成器 GPU 路径在提交时读取后即 release，第二个缓冲只在上一帧未释放时临时分配，
     frame callback 到达且没有待绘制失效后退休多余缓冲。
- **契约**：`docs/architecture/ui-framework.md` 与 UI 框架契约中“每窗口至多三个提交缓冲”“WindowHost 拥有 CPU 绘图存储”的表述。
- **benchmark 决策**：`cargo bench -p canvas` 诊断局部重绘与整窗重绘的成本；窗口交互按
  [性能验证范围](../development/performance-coverage.md)分别测应用绘制、缓冲提交、合成与 flip。
- **验证点**：classic-ui 单元测试（缓冲复用、busy 时临时第二缓冲、退休、buffer age 正确性）；基线 8：
  最大化后稳定 Pss 约为窗口大小的 1 倍，还原后回落；缩放拖动过程帧率不下降。

#### M2. 桌面背景整屏缓冲（约 19 MiB）

- **实施前状态**：`user/programs/desktop/src/paint.rs` 的 `background_frame` 把纯色、墙纸、图标与标题画进一块按物理分辨率
  分配的整屏共享缓冲（3008×1692×4 ≈ 19.4 MiB）。
- **改法**（只用标准 Wayland 协议）：
  1. 纯色底：使用 classic-ui 已有的 solid viewport（单像素缓冲 + `wp_viewporter`）。
  2. 墙纸：缓冲按图片原始尺寸；居中用子表面定位；拉伸用 viewport 目标尺寸（合成器最近邻采样，与 GDI 拉伸一致）；
     平铺让同一 wl_buffer 挂到多个子表面。合成器 CPU 后端同样支持这些几何。
  3. 图标与标题：一个透明子表面，大小为图标网格包围盒；重命名编辑框、拖动图标沿用同一层。
- **验证点**：桌面交互门禁（图标选择、拖动、重命名、墙纸三种方式与纯色）；基线 7 中 Shmem 下降。

#### M3. 服务堆分配器按需切分区块（与第 7 项一起做）

- **实施前状态**：`system/native-runtime/src/heap/allocator.rs` 首次使用某尺寸级别时，把整个 64 KiB 区块切开并逐块写入空闲链指针，
  整块页被提交；8 个级别使每个原生进程至少约 512 KiB，全系统约 23 个原生进程。
- **改法**：区块内维护分配游标，只在真正分配时触及对应页（mimalloc 的增量空闲链扩展）；空 span 按第 7 项 decommit。
- **验证点**：native-runtime 单元测试；M0 中各原生服务的 Private 下降（估算合计 5–8 MiB）。

#### M4. 空闲线程（约 161 个，每个至少 32 KiB 内核栈）

- **实施前状态**：块设备服务端每会话固定 8 个工作线程（`system/device-protocol/src/block.rs` `WORKERS`，virtio-block 宿主 28 线程）；
  ext4 FUSE 4 个、netstack 4 个；`pager-pool` 工作线程只增不减（上限 64）；每个驱动宿主至少 5 个线程，
  component-manager 10 个；桌面常驻 `picture-decode` 等线程。
- **改法**：
  1. 块协议改为批量异步后块服务只需 1–2 个线程（依赖第 1 项）。
  2. pager 工作线程空闲超时退出；按服务审查线程，能并入 port reactor 的并入；桌面辅助线程空闲时退出。
  3. 用栈染色测出内核栈峰值深度，再评估 `KERNEL_STACK_SIZE` 从 32 KiB 降到 16 KiB（保留守护页，留足余量）。
- **契约**：内核栈尺寸变化更新执行域契约与 `kernel/src/config.rs` 的依据说明。
- **验证点**：taskmgr 线程数；M0 中 KernelStack 与各服务 Private 下降（估算合计 4–6 MiB）。

#### M5. bootfs 零复制装载

- **实施前状态**：bootfs（约 4.6 MiB）常驻；ELF 段模板装载时复制段内容并全部提交（`system/native-runtime/src/elf.rs`），
  ext4 映像经 `bootfs_copy` 再复制一份交给 fshost（`system/component-manager/src/main.rs`）。
- **改法**：bootfs 文件按页对齐（参考 Zircon bootfs 格式）；ELF 段直接作为 bootfs VMO 的写时复制子对象，
  交给其他服务的文件以 VMO 子对象传递，不复制。
- **契约**：启动与平台契约中的 bootfs 格式；构建脚本生成格式同步修改。
- **验证点**：服务启动与重启（含驱动换版）门禁；M0 中模板与 bootfs 不再重复（估算 4–5 MiB）。

#### M6. 内核元数据（第 5、6 项的内存收益）

第 5 项避免 linuxd 共享区页表随 Linux 进程数复制，第 6 项把每页约 64 B 的 AVL 节点降到约 10 B。
当前规模估算 2–3 MiB，Linux 进程越多收益越大；验证并入第 5、6 项与基线 5。

#### M7. 较小项（M0 实测后决定是否做）

- 字形缓存：Win2000 风格的无抗锯齿字形按 1 bit/像素存储；缓存满时由整体清空改为 LRU（`user/libraries/typeface/src/catalog.rs`）。
- 主题图标：每进程各自解码，改为预解码的共享图集文件经 mmap 跨进程共享。

## 已确认不需要改动的部分

硬件光标、SHM 按 damage 上传纹理、字体 mmap 共享、epoll 就绪集合、内核堆 per-CPU 缓存、buddy O(1) 索引、
RemoteFs 直接读内容 VMO、channel_call 回复留在当前 CPU。

## 后续计划的依赖

[路由系统计划](router.md)依赖本计划的阶段 1（DMA 直写服务 VMO）、阶段 3（高精度定时器）与 P2 第 1 项
（截止时间调度类）及本计划 P2 第 5 项的共享 socket 数据传输；路由计划负责后续转发引擎，不重复保留旧 RPC。

## 进度

- M0 已实现待运行验收：物理页 owner 的匿名/pager/连续/bootfs 分类与 pager dirty/writeback/reclaimable 子计数已实现；
  linuxd Cached/MemAvailable 改为全系统 pager 来源，删除局部缓存扫描。内存供页由实际内容 owner 标明，避免混入存储脏页。
  修正 slice pin 的 backing owner 登记。
  此前 AArch64/HVF 基线三次对账均为 0 页差额（内核堆/栈/页表 + VMO 匿名/pager/bootfs/连续）；
  创建 4 MiB 内存文件后 Cached/Shmem 增长，Dirty/Writeback 保持 0，清理已完成。
  该历史对账只代表当时的设备与代码配置，不替代 RISC-V 及真实 IOMMU 场景的完整归属验收。
  maps 的普通文件/设备/匿名映射描述已实现，保留实际文件身份、偏移、exec 授权与 Weak MM 寿命；
  观察不克隆 VMA membership，避免改变 SysV nattch/时间。设备的文件/缓冲租约不再用 Vec 混存。
  SysV 与共享匿名 mmap 已补真实内部 tmpfs 文件，内容仍是原匿名 VMO，未引入 pager 或内容副本。
  stat/statm/status RSS 已改为实际 RAM 用户翻译，补齐 COW 祖先，排除 MMIO 与服务镜像；statm shared 使用文件/共享内存 RSS。
  smaps/rollup 的驻留、逐页定点 PSS、Private/Shared、Anonymous 与三类 PSS 已实现，支持任意 VMA 页边界交集。
  task-observer 升级 LTASK002，原生 TASK_STATS 增加 private/shared/scaled shared；服务页显示专用/共享内存。
  共享次数来自现有 COW 树与实际逻辑映射，同一服务映射的硬件镜像只计一次；无新增全局页索引或 fault 热路径 mapcount。
  分段读取复用 per-open 文本，offset 0 刷新，不反复全量扫描。
  源码已通过 Clippy 与代码审查并更新同一产品运行实例；启动与诊断链路暴露的问题正在逐项修复，完整归属对账与最终端到端仍待完成，不标运行验收通过。
  接口派生清单和最终门禁在实现终态统一生成/执行。
- 1 已实现待运行验收：request owner 同时预留 slot/descriptor credits，O(1) 空闲槽释放；header/status 共用一张 metadata VMO。
  登记 VMO 与区间直接 DMA、按 virtqueue 容量协商槽数、virtio-blk 批内并发发布/收割、ext4 extent 合并和 journal 连续写已接通。
  块协议升级 LBLOCK04：控制仅处理登记/派生/取得队列；读写与 flush 经共享 SQ/CQ，删除 TRANSFER/TRANSFER_BATCH RPC。
  环与 credits 表连接时准备，直接写固定原子记录、批量完成；完整 request identity 防止槽复用 ABA，全部完成后才归还借用缓冲。
  GROUP_LAST 保留调用边界，聚合批错误不能污染相邻调用的成功结果。
  同一弹性池接收控制和 Event，客户端由当前调用者分派完成；无新增常驻转发线程。
  提交可覆盖全部协商容量，领取保持 64 项有界窗口；范围/分区、远端和 virtio 均在批内记录无效项并完成其余有效项。
  flush 封闭后续批次并等待此前 active DMA；协议/source 失败先排空在途 handler，最后 SessionDrop 发布 closed。
  会话结束也退休目录观测句柄，避免死亡服务被观测引用保活、控制 call 永久等回复。
  双架构静态检查与运行验收分开；尚未运行块 I/O、环绕/断开或性能门禁，不把源码接通记作运行已通过。
- 2 已实现待运行验收：ext4 `Content` 顺序预读窗口（32 KiB→512 KiB）；pager 工作线程的补页暂存区常驻映射，
  存储读直接写入，去掉 `Vec` 与 aux 复制（linuxd 持久文件与内存文件同步迁移）；内核 fault-around 在主页解析的同一树锁
  事务内只读装入同一 64 KiB 窗口中已驻留的文件页（一个 `TranslationCommit` 一次 fence）。kernel-unit 用例与双架构 Clippy 通过。
- 3 已实现待运行验收：每 CPU 定时队列（`DeadlineIndex`）+ 本地硬件 deadline 合成（时间片截止/队首/boot housekeeping 取最小），
  到期精确到队首截止时间；时间片抢占改为 slice 截止驱动（仅有竞争者时武装），忙碌无竞争与空闲 CPU 均无周期中断，
  无来源时关闭 timer source；运行时间读取时叠加当前 slice 存量，不再依赖周期 checkpoint；
  清除无消费者的 `TIMER_FROM_USER`/`last_user_return`/tick 相位计算。kernel-unit 4 项 `DeadlineIndex` 单测、
  architecture-bench `timer deadline index` 基准、双架构 Clippy 通过，未运行调度/计时运行时测试。
- 4 已实现待运行验收：私有 context 按地址空间延迟建立，共享 VMO 使用全局 context；等待索引迁入 `execution::sync::futex`，
  syscall 保留参数/地址/状态码职责；共享 key 解析 slice 别名，不混同 COW 身份。桶改为 IRQ 安全锁；先在桶外阻塞取得实际空间 owner 并补齐翻译，guard 保活用户字，随后取得桶锁再原子比较与入队，避免 try-lock 重试活锁。
  节点锁外准备，跨 context requeue 按身份/桶序加锁，外层 IRQ guard 最后释放；取消复核弱引用登记位置。
  同 key requeue 不反复搬动队尾，保留 FIFO。复用原队列用例与基准，运行验收待最终统一执行。
- 6 与 M6 元数据部分已实现待运行验收：已提交页压缩为非零 8 字节槽（页号/pin/回写/内容归属，预留回收代位）；
  补页请求复用 `PageList<T>` 的分块索引，不再逐页建 AVL 节点。空块预留失败回滚、`ceiling` 跨空块、稀疏失败请求扫描均补齐。
  扩展既有页列表用例与 snapshot-retirement 诊断的 1024/65536 页规模；没有运行测试，双架构 kernel 与 host Clippy 验证。
- M4 已实现并完成栈尺寸评估：`service-rpc::serve` 改为弹性线程池（始终保留一个空闲读者，忙时按需增长到原上限，空闲 5 s 收缩到 1 个；
  并发容量不变，服务 RPC 的常驻多余空闲线程消失）；pager-pool 工作线程空闲 5 s 退出。
  netstack 独立的多 channel dispatcher 原先未覆盖，现已补齐同样的弹性准入与收缩，并保留原四线程并发上限。
  最后一个空闲 RPC 读者永久等真实通知，取消每 5 s 的周期检查；只为多余空闲读者武装收缩截止，忙线程不占读者名额。
  停止 Event 的每 worker 预留登记无分配广播，保证无限等待的读者也能在 source 失败时退出；空闲收缩自动退订。
  该缺口已在源码补齐：BlockProvider/BlockIoQueue 原生非阻塞准入与完成 FIFO，块服务直接等待提交/设备完成两个来源，峰值两个 worker、空闲一个。
  原同步批等待已从服务数据路径删除；64 项仅为领取窗口，256 项为单轮预算，在途请求按实际设备容量推进。
  保留的阻塞 BlockDevice 视图共享同一硬件 RequestOwner，未保留旧 RPC 或阻塞伪 async。
  Busy 队首通过原有 FIFO 获得额度；取消/关闭归还未发表 grant；已接收请求由队列 Drop 自行排空再发布 closed。
  完成按目的队列分组通知、逐请求交付结果，不扫描在途集合或广播无关会话；扩展现有取消/额度用例，尚未运行块 I/O 或性能验收。
  UI 辅助任务已继续收敛：PictureLoader 解码/通知后退出，最后一个文件对话框关闭时退休队列/watch；
  SDK Background 保持 64 项队列、至多两个并发任务，按需启动、空闲 5 s 收缩至零，提交/退休同锁防止遗留任务。
  桌面网络/USB 托盘已整合到同一通知区 worker 与 500 ms 截止，组合快照容量一；读取/禁用仍在 UI 外，删除两个专用线程与重复退出逻辑。
  命令完成会刷新通知区可用状态，命令失败不妨碍 USB 快照应用；源码审查与 user Clippy，不新增测试实体。
  非零染色已排除 guard/context reserve；11 CPU 产品实例的完整桌面、四路并发 fork/文件读写/同步及 12 线程 socket 负载
  峰值写入深度为 4120 字节，但仍是下界。16 KiB 槽在 AArch64 仅余 8 KiB，未证明所有平台/硬中断深路径的余量，
  因此评估结论保留 32 KiB。通过移除大栈帧和多余线程降内存，不靠缩小未证明安全的边界；桌面内存目标已独立达到。
- M5 已实现待运行验收：bootfs 文件内容按页对齐（打包器把填充放进路径名字段，标准 cpio 读取器兼容）；`Image::from_vmo`
  让 ELF 段成为 bootfs VMO 的写时复制子对象（末页清零，bss 为独立零页），不再逐段复制；ext4 映像与 driver-host 映像
  以 `Bootfs::file_vmo` 子对象交付，不再 `bootfs_copy`；客户端提供的替换映像仍复制（防止之后被改写）。
- P2.6 已实现待运行验收：物理页单页分配/释放经每 CPU 缓存（32 页上限、16 页补充、压力下停用、连续分配失败先还缓存）。
- P2.2 已实现：restricted 状态 VMO 绑定时固定，进出经 direct map 拷贝。
- P2.3 已实现待运行验收：channel 积压仍为 4 MiB 字节预算，补齐两个 Vec 的容量及队列槽开销，句柄密集消息不再漏计。
  每对 channel 由原队列锁移交一份空消息存储，任一端关闭清空，迟到归还不重建；没有新增全局池。
  原生 Thread 保有一个懒分配 CallSlot，撤销登记且回复取空后复用唯一 Arc，退出时清空；没有增加通用调度 Thread 的 IPC 职责。
  句柄编号改为有界栈存储，批量移入/移出保留消息 Vec 容量，单句柄插入删除两个临时 Vec；原有原子转移与锁外清理语义保留。
  单次/批量移出在表锁外统一注销旧 key，修复发布/交付写回失败回滚中遗漏的等待取消，删除 syscall 重复注销。
  事务号环绕跳过双方活跃 ID，空隙证明保持普通分配 O(1)；已完成代码审查，运行/性能与并发取消验收留待计划终态。
- 7 与 M3 已实现待运行验收：`libraries/span-heap`（线程本地分片空闲链、增量切分、远端释放原子栈 + pending 队列、
  线程退出孤儿化/认领、空 span decommit 不 unmap、12.5% 级别、大对象独立映射）替换原全局单锁分级分配器；
  host 并发单测 9 项、`span-heap-bench`、kernel-unit `native_heap` 接线夹具 4 项通过，双架构 Clippy 通过。
  顺带修复 `netstack-unit` 夹具缺少 `NetworkDevice::link` 实现导致 architecture-bench 无法编译。
- M1 已实现待运行验收：WindowHost 已删除私有画布/整窗复制，经 Display.paint_frame 在释放后的共享缓冲中按区域重绘。
  Nodes 封装可变访问并推进局部版本；Tree 对上次成功提交的版本/几何/焦点/按压求旧新覆盖，移除节点保留退休区域。
  菜单、组合框下拉和工具提示复用实际绘制几何；异步图片完成显式使依赖 Canvas 失效，空动画检查不标脏全树。
  WindowHost 合并损伤、重建局部底色与相交子树，发送物理 damage；非最新缓冲、新规格、装饰/主题变化完整初始化。
  删除 render_frame 和整窗分块哈希；稳定状态只留一块空闲缓冲，等待/失败不消费 Tree 基线，失败缓冲下一次完整修复。
  callback 延后到绘制/分配/写入准备成功后才登记，失败不遗留永不完成的 callback ID。
  原 render fixture 增加生产 Tree 局部/整帧像素对账，覆盖文字、焦点、移动、菜单覆盖层关闭、删除与 1x/2x；只 Clippy 编译，未执行。
- M7 已实现待运行验收：字形缓存满时由整体清空改为 LRU（批量淘汰最久未用的四分之一）；点阵与不抗锯齿轮廓字形（只含 0/255）
  改为 1 位/像素存储（`typeface::Coverage::Mono`，BDF 位图直接沿用），canvas 新增 `BitMask`/`draw_bit_mask`，
  先前单测覆盖与 8 位蒙版逐像素一致。
  系统 UI 位图已由唯一 BMP 输入在构建时预乘/派生尺寸，生成 LUIA0001 图集，经原安装清单发布。
  Bitmap 直接借用只读 mmap 的像素及下标；Weak 目录只合并活引用，最后引用释放映射，无逐图解码或缺尺寸私有缩放。
  palette 重着色共享下标，只有新颜色像素私有；原逐图产品 BMP 由离线同步退休，无错误回退路径。
  扩展现有位图边界用例编译覆盖非法索引，未运行生成/图形/内存验收；不以源码实现代替内存目标达成。
- 移交前验证记录（不代表当前树已验收）：`architecture-check` 通过（已按设计登记 FallibleMap 持久表、校准源文件行数上限、补局部 SAFETY 证明，
  `--write-interface` 重新生成接口清单）；ext4-unit 69 项、kernel-unit 非 DTB 夹具、scheduler-unit、netstack-unit、
  span-heap 全部通过（DTB 夹具需要 `make verify-unit` 生成 QEMU DTB）。完整 `make verify` 与运行时验收待用户执行。
- 8 已实现待运行验收：共享零页、双架构访问位采集/清除、全 ASID fence、软件 access fault 恢复已接通；
  COW/slice 访问观测按物理身份与继承窗口归并，采集/撤销共用显式别名工作栈。
  完成八代 oldest-first 滑动窗口：页槽仍为 8 字节，pager 页块额外位图/链表索引干净未固定成员；脏页/pin 离队，清洁/解 pin 以当前代重入。
  扫描跨批保留登记和代游标，每批全局至多 256 页样本/64 个 VMO；只有最老代完整遍历证明排空才推进窗口，避免代号绕回误分类。
  完整块样本轮转防止热点头部饿死块尾；锁忙/采集失败保留页框并阻止错误的排空证明。
  登记改为锁外准备的 koid 弱索引，Drop 摘除；不在 IRQ guard 下分配或取树锁。
  后台线程按实际低/高水位推进、批次间 yield，正常无周期唤醒，无进展限速重试；原生压力事件同源投影。
  复用现有页列表/淘汰用例扩展索引资格与块轮转覆盖；源码检查与运行验收分开，尚未运行回收质量/内存压力验收。
- 9 已实现待运行验收：`Content::write` 改为延迟分配（预留 + 大小/时间提交 + 只写内容 VMO），回写时才分配并只映射 block，
  脏页限流（单文件 8 MiB）；journal 写 home 的连续块区间合并；ext4-unit 70 项（含 e2fsck 校验的预留/回写用例）通过。
  数据不再同时存在于 journal staged 副本与内容 VMO。非整块写不再读盘（页由 pager 补入内容 VMO）。
- 5 与 M6 页表部分已实现待运行验收：AArch64/RISC-V 根入口借用同一 owner 子树；共享区只在共享地址空间装页/撤销，
  成员 normal/private 共用私有区下级表；取消成员登记与 O(N) 叶项撤销。沿现有 Arc/ExecutionViews 寿命保活，
  被借用的顶层子表不再空表剪枝，较低层表仍按全 CPU/ASID fence 退休；新根分支按需接入，不预分配空子树。
  共享区软件 walk/read 锁实际 owner；supervisor 子树保留隔离。扩展既有隔离用例覆盖共享末页撤销/重装，无新增测试实体。
  双架构 kernel 与 kernel-unit（含现有用例的编译）Clippy 通过；没有运行测试或更新来宾。
- P2.1 已实现待运行验收：计费已统一为纳秒，CFS vruntime 使用加权纳秒倒数乘移；native runtime/CPU_BUSY 不再经过微秒截断。
  per-CPU 累计量与活动区间在同一 IRQ-safe 锁内取时钟并发表，修复切换边界的重复计数/跨代观察；CPU_BUSY 删除转换 Vec。
  原生分区 EDF、CBS/约束截止 revised wakeup、95% 密度准入与预算补充队列已接通；耗尽和显式 yield 均不能立即重入。
  超额债务跨周期保留，终止先撤销预算，配置替换使旧 Ready generation 失效；远端 checkpoint 结算 active 区间实际 CPU。
  原生设置经 system::SCHEDULE resource + MANAGE，普通应用只使用标准 sched_setattr/getattr；Linux 处理 CAP_SYS_NICE、
  版本化尺寸/尾部零、KEEP_POLICY/PARAMS、reset-on-fork/EAGAIN、root-domain affinity 与 EBUSY 准入失败。
  合成/输入主循环按实际 KMS 周期申请 65%，音频混音按实际 PCM 周期申请 10%；音频 RPC 只通知已有端口，不绕过预算执行混音。
  这些是待真实负载校准的初始配置，不作为已满足帧率/音频截止的证据；拒绝准入显式报错，不伪装普通类为已获得保证。
  现有 scheduler-unit 扩展生产 CBS 的隐式/约束截止、让出、迟到补充与超额债务对账；仅 Clippy 编译，未执行。
- P2.4 已实现待运行验收：commit/同步缓存的损伤累计到呈现，opaque 提示按双缓冲状态生效；XRGB/ARGB 格式切换不复用旧 alpha 快照。
  Scanout 按旧新几何、层序、仿射源和客户端损伤求输出并集，双缓冲按年龄补画；CPU/GPU 共用不重叠可见区域，跳过完全遮挡项。
  GPU 用标准 scissor 保持原顶点/UV，删除整屏 clear；背景仅一像素纹理，未损伤活视图不反复创建，关闭 overlay 释放局部画布。
  新增生产区域/CPU 绘制与整帧参考的像素对账用例，扩展既有 region 事务覆盖 opaque 空洞；只作 Clippy 编译，未执行。
  CPU 输出已接入标准 DIRTYFB：同步完整后缓冲修复集再翻页，复用既有 adapter 同步凭据跳过全幅 transfer；32 clip 之外合并外接矩形。
  KMS property blob 创建/查询/销毁已接入标准 ioctl，framebuffer/blob 共用对象 ID；OFD 创建者引用与全局弱索引分离，锁外退休与 copyout 失败回滚已接通。
  已接通 per-OFD UNIVERSAL_PLANES、primary plane/属性标准查询与 typed user wrapper；schema、对象 ID/type、计数及枚举由两端标准 ABI 对接。
  复用 drm-flip-probe 增加真实查询/格式/OFD 隔离、blob 全局读取/销毁覆盖，只编译未运行；无新增 runtime fixture。
  现有 legacy/pending/active scanout 已改为持有真实不可变 framebuffer 对象；close 的 reset intent 到 pending disable 接力保活，完成后锁外退休。
  修复 event 临时 OFD 最后引用在 completion 锁内释放的重入自锁；已有 disable 接管 close 引用，关闭期间新 scanout 完成不提前丢掉硬件目标。
  原子提交已接通：per-OFD opt-in、固定 schema 的逐项校验/最后赋值、TEST_ONLY/NONBLOCK/ALLOW_MODESET/EVENT、逻辑候选与 exact completion。
  legacy/atomic/disable 共用不可变配置；mode/ACTIVE/路由/plane 分开表达，模式与 damage 引用跨 DESTROY 保活，错误/覆盖/close 锁外退休。
  合成器两后端改用标准 atomic KMS，初始化查询属性 ID，稳态复用请求与 clip 暂存；CPU 上传完整后缓冲修复区，FB_DAMAGE_CLIPS 描述相对前台的输出差异。
  GPU 原 presentation 批接收裁剪后的损伤并集，空损伤省略 RESOURCE_FLUSH；旧 master 关闭不能 reset 后继 master 已接受的替换，非扫描配置不重复 reset。
  标准 timing 提示归一化、损伤 16-byte s32 布局及 master 复核锁序已对照固定 Linux 源码；实际 ioctl/保活覆盖并入原 drm-flip-probe，GPU cadence 使用 atomic。
  当前仅 Clippy 与代码审查，未运行像素/退出竞态/帧率验收，不把源码实现记作运行通过。
- M2 已实现待运行验收：Display 接通标准同步子表面、空输入 region 与不可变原尺寸共享图像，桌面根只提交单像素底色。
  墙纸居中/拉伸/平铺均由子表面几何表达；小平铺源增加最多 256 KiB 的整数重复块，避免海量协议对象，像素相位不变。
  图标按实际覆盖包围盒光栅，框选仅四条细边，保持根坐标 XOR 相位与输入路由；替换经父 commit 原子撤下后才销毁旧角色。
  解码私有像素在发布后退休，客户端只保留封印只读映射；合成器仅对真实封印 pool 懒建一次共享快照，不信任可变客户端。
  多表面使用不可变 buffer 不依赖 release；没有新增私有协议或整屏应用像素副本。Clippy/审查通过，内存与视觉尚待最终来宾验收。
- P2.5 已实现待运行验收：socket send/receive/poll 迁移到版本化固定 SQ/CQ、持久惰性载荷槽。
  两端批量领取并合并通知；客户端调用者领队处理完成，不增加转发线程；旧数据 RPC、逐次 payload VMO 与 codec 删除。
  credit 保留到完成载荷复制后，单调 ticket/有界距离与同宽原子访问防止复用和损坏对端造成越界/数据竞争。
  协议引擎仍决定短读写、截断、PEEK、元数据与错误；poll 查询实际状态，不用复制的就绪缓存换取虚假的无往返。
  一并补齐 CLOSE 退休、peer 关闭唤醒与 EventHub RAII 关联清理；具体边界见 IPC/网络和设备契约。
  双架构 netstack/linuxd/DHCP/device-protocol 与 host kernel-unit/netstack-unit Clippy 通过；已有 UDP fixture
  增加 12 线程共享发送 socket 内容/唯一性对账及零长、PEEK/TRUNC，接收端各自独立，避免合法 UDP 满队列丢包。
  单/双 CPU musl 全部通过；此前同一产品实例的并发调用耗时为 0.238 s。
- M4 原始 FUSE 接收已补齐源码：复用 service-rpc 弹性消息分派，不再固定四个 worker；FUSE 线格式不变。
  最后一个 handler 退出后由 FuseSession 执行 finish，空闲 worker 退休不再提前关闭仍有在途请求的挂载。
- 最终启动发现并修复：M5 bootfs 派生 ELF 缺少执行能力；P1 到期队首在 IRQ 内重复编程造成 level 风暴、deferred 看门狗停机；
  M4 裸探针参数误传 init argv。前两者已有相同入口红/绿证据，探针改为 native 命名空间并同步文档。
  清零 syscall 删除整页栈 staging，编译器帧由 4256 降至 288 字节，字节/COW/pager/dirty 路径仍归原写解析。
- 同实例运行诊断已确认：完整桌面与任务管理器驻留后，物理已分配内存低于 100 MB；显示、壁纸与控件经 SSH/RFB 取帧确认。
  可执行数据清理改为 Zircon 的实际范围准备/末尾指令同步；同一四路进程/文件负载从 195.432 s 降到 3.282 s，SSH 命令为 0.220 s。
  futex 桶/空间锁重试活锁、PL011 有限预算误清 RX 电平同时修复；均保留红/绿日志，临时内核/服务探针已删除。
- 最终文件回收审查已补齐：最后 FORGET 的无链接内容不再先分配块回写；现有 BusyBox 删除/最后关闭归还空间验收通过。
  Content 复用预留锁串行写入与回写确认，失败只撤销新增预留；pager 查询包含尚未确认页以便重试，不以旧快照覆盖并发新写。
  VMO 19 项与 ext4 70 项现有用例通过，双架构 Clippy 通过。所有计划源码与 M4 评估已完成；最终剩余门禁继续执行。

原生 channel 存储/等待对象复用的双架构 kernel Clippy 与 architecture-check 已通过。
分代索引的双架构 kernel 与 kernel-unit Clippy 已通过；共享块环及其 driver-host/ext4/fshost/linuxd 依赖的双架构 Clippy 已通过。
UI 辅助任务与图集相关的 user workspace Clippy（含现有用例编译）及 architecture-check 已通过；图集生成器只作语法与代码审查。
接口派生清单由唯一生成器更新。已运行针对真实回归的现有夹具与同一产品实例诊断；最终统一执行一次 `make verify`，不把历史验证记录当作当前树的验收。

截止调度、计费与 linuxd/audio-core/component-manager 的双架构 Clippy、scheduler-unit/kernel-unit Clippy 已通过；
桌面子表面与输出增量合成的 user Clippy（含用例编译）通过。

atomic KMS、GPU 区域刷新与通知区线程整合已完成源码实现；双架构/用户侧 Clippy 与结构检查分别记录，来宾验收待计划终态统一执行。
