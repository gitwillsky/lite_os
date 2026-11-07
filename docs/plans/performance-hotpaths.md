# 计划：端到端热路径性能与资源占用

Status: Active

执行期间存在；完成后把持久事实迁入架构/契约/性能文档并删除本文件。

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
- 验证节奏：实施期间每阶段做两架构编译/clippy、architecture-check 与相关 `kernel-unit`/crate 单元测试；
  运行时门禁与完整 `make verify` 在全部阶段完成后经用户允许统一执行。
- 不修改围栏、阈值或文档声明来掩盖实现错误；范围缩减写入 ABI 矩阵或当前架构限制。

## 基线（诊断，不设阈值）

改动前后在同一架构、CPU 数、内存与设备配置下记录，作为收益证据：

1. 冷启动到桌面稳定的时间；启动 file-explorer、notepad 的时间。
2. guest 内 `dd` 顺序读/写 64 MiB 文件（冷缓存、热缓存），小块追加写（4 KiB × 1024 + fsync）。
3. `nanosleep(1ms)` 循环 1000 次的实际时长分布；空闲桌面 10 s 内 boot CPU 的定时器中断次数。
4. 多线程 futex 竞争（N 线程争同一/不同 mutex）的吞吐。
5. `MemTotal - MemAvailable`，以及内核堆、页表页、linuxd 堆在 1/8/32 个 Linux 进程下的增长。
6. 文件密集 workload 下的缺页次数与 pager 请求次数。

## 阶段与验证点

顺序：1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → P2。1 是 2 的前提；5、6 都改 VM 内核结构，分开提交。

### 1. 块设备协议：登记 VMO + 批量请求 + 并发队列

- **现状**：`RemoteBlock::read_block`（`system/device-protocol/src/block.rs` 的 `impl BlockDevice for RemoteBlock`）
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

- **现状**：内核 pager 请求长度固定一页（`kernel/src/vm/vmo/pager.rs` 构造 `PagerRequest` 处 `length: PAGE_SIZE`）；
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

- **现状**：`TICKS_PER_SEC = 100`（`kernel/src/execution/timer/mod.rs`）；截止时间等待只在 tick 的 deferred work 中到期
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

- **现状**：`static TABLE: TaskMutex<FallibleMap<...>>`（`kernel/src/syscall/futex.rs`）全系统一把可睡眠锁；
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

- **现状**：每个 Linux 进程有 normal/private 两个根，私有区叶项装两遍（`kernel/src/vm/execution_views.rs`）；
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

- **现状**：`committed: FallibleMap<usize, Page>`（`kernel/src/vm/vmo.rs`），每页一个 AVL 堆节点
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

- **现状**（`system/native-runtime/src/heap/allocator.rs`）：全局单锁；≤2 KiB 为 2 的幂分级（平均约 25% 内部碎片）；
  2 KiB 以上（恰好 64 KiB 外）每次 `Vmo::create` + map + 裁剪 unmap，释放时 unmap 并等 TLB fence；`Vec` 超过 4 KiB
  每次扩容都走此路径；小块永不归还。linuxd 每个 Linux 线程对应一个原生线程，全部争这把锁。
- **参考**：mimalloc（Leijen et al., APLAS 2019）分片空闲链与线程本地堆；snmalloc（ISMM 2019）远端释放消息。
- **改法**：
  1. size class 间距不超过 12.5%；线程本地小对象页，远端释放进所属页的原子链。
  2. 中等对象从启动时预留的 VMAR 区间按 span 分配，backing 用少量大 VMO，按需提交；空 span 用 VMO
     decommit（`OP_DECOMMIT`，没有则补齐原生接口）归还物理页，不 unmap。
  3. `realloc` 在 span 内原地扩展；大对象（≥256 KiB）保留独立映射。
- **契约**：`docs/architecture-contract/userspace-abi.md` 或 native-runtime 所在契约中的堆 owner 与内存归还语义。
- **benchmark 决策**：加 host blocking benchmark：小对象 alloc/free、跨线程释放、Vec 增长到 64 KiB。
- **验证点**：native-runtime 单元测试；linuxd 堆在基线 5 的增长曲线。

### 8. 页回收分代与共享零页

- **现状**：`kernel/src/vm/vmo/reclaim.rs` 按 VMO 轮转、按页号顺序淘汰干净页，不看访问；只在用户缺页入口触发；
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

- **现状**：`write_data_locked`（`system/ext4/src/ext4/storage_mutation.rs`）非整块写从存储读出整块再改；数据块以
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

## 已确认不需要改动的部分

硬件光标、SHM 按 damage 上传纹理、字体 mmap 共享、epoll 就绪集合、内核堆 per-CPU 缓存、buddy O(1) 索引、
RemoteFs 直接读内容 VMO、channel_call 回复留在当前 CPU。

## 进度

- 未开始。
