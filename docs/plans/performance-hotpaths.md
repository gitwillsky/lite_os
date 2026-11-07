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
（截止时间调度类）；P2 第 5 项（网络数据面）由路由计划的 netstack 重写按终态实现，本计划不单独做。

## 进度

- M0 进行中：物理页 owner 的匿名/pager/连续/bootfs 分类与 pager dirty/writeback/reclaimable 子计数已实现；
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
  当前只完成 Clippy 与代码审查，尚未更新运行实例或运行时验收；完整归属对账与最终端到端仍待完成，M0 不标完成。
  接口派生清单和最终门禁在实现终态统一生成/执行。
- 1 进行中：request owner 已同时预留 slot/descriptor credits，变长链不再依赖“有 slot 就有 descriptor”的假设；
  空闲槽释放为 O(1)，保持 FIFO、prepare OOM、terminal failure 和锁外 wake 的所有权。
  block header/status 从每槽独立 VMO 合并为一张固定表，16 槽理论少分配 31 页；已支持 backing DMA 任意区间及方向。
  登记 VMO（ATTACH/DETACH/TRANSFER）与区间传输已落地，内联 READ/WRITE 已删除，块内容不再进入 channel 消息。
  ext4 `read_storage` 按 extent 合并同一 extent 内连续的完整对齐块，经 `read_fs_blocks` 一次区间读；
  journal staged 覆盖与 cache generation 重试语义不变（单块先查 staged，多块先读 home 再在一次 journal guard 内覆盖）。
  批量提交已实现：`BlockDevice::transfer_batch`/`read_block_ranges`、协议 `TRANSFER_BATCH`（`LBLOCK03`）、
  virtio-blk 并发发布整批（自有在途占满 credit 时先收割最老的，避免自锁）、ext4 `read_storage` 把一次读里
  所有 extent 的运行合成一批；journal 的 ordered 数据 / checkpoint / replay 写 home 时连续块合并为区间写（≤64 块）。
  双架构服务 Clippy 与 ext4-unit/kernel-unit 编译检查通过，未运行块 I/O 测试或更新实例。
- 2 已实现待运行验收：ext4 `Content` 顺序预读窗口（32 KiB→512 KiB）；pager 工作线程的补页暂存区常驻映射，
  存储读直接写入，去掉 `Vec` 与 aux 复制（linuxd 持久文件与内存文件同步迁移）；内核 fault-around 在主页解析的同一树锁
  事务内只读装入同一 64 KiB 窗口中已驻留的文件页（一个 `TranslationCommit` 一次 fence）。kernel-unit 用例与双架构 Clippy 通过。
- 3 已实现待运行验收：每 CPU 定时队列（`DeadlineIndex`）+ 本地硬件 deadline 合成（时间片截止/队首/boot housekeeping 取最小），
  到期精确到队首截止时间；时间片抢占改为 slice 截止驱动（仅有竞争者时武装），忙碌无竞争与空闲 CPU 均无周期中断，
  无来源时关闭 timer source；运行时间读取时叠加当前 slice 存量，不再依赖周期 checkpoint；
  清除无消费者的 `TIMER_FROM_USER`/`last_user_return`/tick 相位计算。kernel-unit 4 项 `DeadlineIndex` 单测、
  architecture-bench `timer deadline index` 基准、双架构 Clippy 通过，未运行调度/计时运行时测试。
- 4 已实现待运行验收：私有 context 按地址空间延迟建立，共享 VMO 使用全局 context；等待索引迁入 `execution::sync::futex`，
  syscall 保留参数/地址/状态码职责；共享 key 解析 slice 别名，不混同 COW 身份。桶改为 IRQ 安全锁，桶内空间锁只 try-lock；锁忙/缺页先退出整个事务再等待。
  节点锁外准备，跨 context requeue 按身份/桶序加锁，外层 IRQ guard 最后释放；取消复核弱引用登记位置。
  同 key requeue 不反复搬动队尾，保留 FIFO。复用原队列用例与基准，运行验收待最终统一执行。
- 6 与 M6 元数据部分已实现待运行验收：已提交页压缩为非零 8 字节槽（页号/pin/回写/内容归属，预留回收代位）；
  补页请求复用 `PageList<T>` 的分块索引，不再逐页建 AVL 节点。空块预留失败回滚、`ceiling` 跨空块、稀疏失败请求扫描均补齐。
  扩展既有页列表用例与 snapshot-retirement 诊断的 1024/65536 页规模；没有运行测试，双架构 kernel 与 host Clippy 验证。
- M4 部分实现：`service-rpc::serve` 改为弹性线程池（始终保留一个空闲读者，忙时按需增长到原上限，空闲 5 s 收缩到 1 个；
  并发容量不变，块设备会话、FUSE、netstack 等的常驻空闲线程消失）；pager-pool 工作线程空闲 5 s 退出。
  内核栈尺寸评估：已修正 `stack_probe` 为非零栈染色并排除 guard/context reserve，日志只报告可用区写入深度下界；
  零初始化扫描不能可靠识别零写入。32 KiB 槽位保持不变；最终工作负载采样与深栈帧审查后再决定是否缩小，不把测量留给用户补做。
- M5 已实现待运行验收：bootfs 文件内容按页对齐（打包器把填充放进路径名字段，标准 cpio 读取器兼容）；`Image::from_vmo`
  让 ELF 段成为 bootfs VMO 的写时复制子对象（末页清零，bss 为独立零页），不再逐段复制；ext4 映像与 driver-host 映像
  以 `Bootfs::file_vmo` 子对象交付，不再 `bootfs_copy`；客户端提供的替换映像仍复制（防止之后被改写）。
- P2.6 已实现待运行验收：物理页单页分配/释放经每 CPU 缓存（32 页上限、16 页补充、压力下停用、连续分配失败先还缓存）。
- P2.2 已实现：restricted 状态 VMO 绑定时固定，进出经 direct map 拷贝。P2.3 部分实现：channel 积压改为按字节限额
  （4 MiB，含每消息 64 B 开销）；消息缓冲与 `CallWaiter` 的分配由内核堆每 CPU 缓存承担，未另建池。
- 7 与 M3 已实现待运行验收：`libraries/span-heap`（线程本地分片空闲链、增量切分、远端释放原子栈 + pending 队列、
  线程退出孤儿化/认领、空 span decommit 不 unmap、12.5% 级别、大对象独立映射）替换原全局单锁分级分配器；
  host 并发单测 9 项、`span-heap-bench`、kernel-unit `native_heap` 接线夹具 4 项通过，双架构 Clippy 通过。
  顺带修复 `netstack-unit` 夹具缺少 `NetworkDevice::link` 实现导致 architecture-bench 无法编译。
- M1 部分实现：`WindowHost` 删除私有画布与 `present` 整窗复制，经 `Display::render_frame` 直接在已释放的共享缓冲中整幅重绘；
  损伤由上一次提交画面的分块哈希推导；稳定状态只留一块空闲缓冲，第二块 release 后立即退休。classic-ui 单测与 Clippy 通过。
  尚缺：区域级光栅裁剪（只重画失效区）。控件树的 `nodes` 是公开 map、到处直接 `get_mut`，没有统一的变更入口，
  需要先给控件树引入节点变更版本（封装可变访问）才能安全地由它推导失效区。
- M7 部分实现：字形缓存满时由整体清空改为 LRU（批量淘汰最久未用的四分之一）；点阵与不抗锯齿轮廓字形（只含 0/255）
  改为 1 位/像素存储（`typeface::Coverage::Mono`，BDF 位图直接沿用），canvas 新增 `BitMask`/`draw_bit_mask`，
  单测证明与 8 位蒙版逐像素一致。尚缺：预解码共享主题图标图集。
- 移交前验证记录（不代表当前树已验收）：`architecture-check` 通过（已按设计登记 FallibleMap 持久表、校准源文件行数上限、补局部 SAFETY 证明，
  `--write-interface` 重新生成接口清单）；ext4-unit 69 项、kernel-unit 非 DTB 夹具、scheduler-unit、netstack-unit、
  span-heap 全部通过（DTB 夹具需要 `make verify-unit` 生成 QEMU DTB）。完整 `make verify` 与运行时验收待用户执行。
- 8 部分实现：共享零页已落地；双架构 accessed 采集/清除、全 ASID fence、软件 access fault 恢复已接通；
  COW/slice 访问观测按物理身份与继承窗口归并，页槽使用既有 3 位记录访问代，批量候选按代龄优先并给实际访问第二次机会。
  后台内核线程按低/高水位推进，allocator 锁内只发布 boot CPU deferred 边沿，空闲无周期唤醒；无进展时限速重试。
  原生压力事件移出 syscall 水位镜像，由同一实际 owner 的边沿更新；扩展现有视图用例覆盖 AF 清除/软件恢复，无新增测试实体。
  访问采集与映射撤销已共用显式别名工作栈，根 truncate 旧窗口和 COW/slice 继承边界保持正确；登记扩容不在 spin guard 内执行。
  尚缺：完整 oldest-first 分代索引（当前只在批量候选内排序）、全局扫描预算与多代完整遍历推进边界。
  这些性能与深栈要求尚未被证明，不能以后台线程接通代替阶段 8 完成。
- 9 已实现待运行验收：`Content::write` 改为延迟分配（预留 + 大小/时间提交 + 只写内容 VMO），回写时才分配并只映射 block，
  脏页限流（单文件 8 MiB）；journal 写 home 的连续块区间合并；ext4-unit 70 项（含 e2fsck 校验的预留/回写用例）通过。
  数据不再同时存在于 journal staged 副本与内容 VMO。非整块写不再读盘（页由 pager 补入内容 VMO）。
- 5 与 M6 页表部分已实现待运行验收：AArch64/RISC-V 根入口借用同一 owner 子树；共享区只在共享地址空间装页/撤销，
  成员 normal/private 共用私有区下级表；取消成员登记与 O(N) 叶项撤销。沿现有 Arc/ExecutionViews 寿命保活，
  被借用的顶层子表不再空表剪枝，较低层表仍按全 CPU/ASID fence 退休；新根分支按需接入，不预分配空子树。
  共享区软件 walk/read 锁实际 owner；supervisor 子树保留隔离。扩展既有隔离用例覆盖共享末页撤销/重装，无新增测试实体。
  双架构 kernel 与 kernel-unit（含现有用例的编译）Clippy 通过；没有运行测试或更新来宾。
- 尚未完成：
  8 余项（完整分代索引/全局扫描边界）、P2.1（截止时间调度类）、
  P2.4（合成器 damage 裁剪与遮挡剔除）、M1 区域级光栅裁剪、M2（桌面背景子表面，需要 Display 子表面能力）、
  M4 线程整合与内核栈尺寸评估、M7 余项、P2.3 缓冲复用；
  P2.5 的终态仍属于路由计划的 socket 共享环，当前未实现，不以转交计划代替能力完成。所有项最后统一运行验收。

本轮源码验证：双架构 kernel、kernel-unit（含测试的编译）与 architecture-bench Clippy 通过；
接口派生清单已由唯一生成器更新，architecture-check 通过。没有运行测试、基准、来宾采样或 `make verify`。
