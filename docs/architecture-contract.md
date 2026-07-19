# LiteOS Architecture Contract

本文只维护全局 module dependency、机器读取的精确 registry、source-size review 与跨领域变更规则。领域 owner、interface 和 cleanup 约束由子契约唯一维护。

## 领域契约

| 领域 | 权威契约 |
|---|---|
| 启动与平台 | [boot-platform](architecture-contract/boot-platform.md) |
| 执行 | [execution](architecture-contract/execution.md) |
| 内存 | [memory](architecture-contract/memory.md) |
| 进程与调度 | [process-scheduling](architecture-contract/process-scheduling.md) |
| 文件系统与存储 | [filesystem-storage](architecture-contract/filesystem-storage.md) |
| IPC 与网络 | [ipc-network](architecture-contract/ipc-network.md) |
| 设备与终端 | [devices-terminal](architecture-contract/devices-terminal.md) |
| 用户态与 ABI | [userspace-abi](architecture-contract/userspace-abi.md) |

## Crate contract

- `bootloader` 是独立 firmware domain，不依赖 kernel 或 userspace。
- `syscall-abi` 只保存 dispatcher 接入的 Linux/riscv64 编号，不依赖实现 crate。
- `kernel/src/main.rs` 是唯一 composition root；初始化顺序和 adapter 装配不得下沉到 driver、filesystem 或 task。
- binary crate 的跨 module interface 默认使用最窄的 `pub(super)`、`pub(in …)` 或 `pub(crate)`；扩大范围必须说明 production caller。

## 2. Kernel dependency contract

| Module | 允许依赖（机器读取） | 说明 |
|---|---|---|
| `arch` | `config`, `fallible_tree` | 编译期选择的 ISA mechanism；page-table frame owners 使用 fallible ordered storage 保持精确 physical identity，不消费 platform 或上层领域状态 |
| `entry` | `cpu`, `platform`, `trap` | raw boot/trap callback ABI 的唯一 codec；boot 只构造 typed `BootContext`，trap 只投递 generic semantic handler |
| `config` | 无 | 只保存无运行时依赖的常量 |
| `cpu` | `arch` | logical `CpuId`/`CpuSet`、hardware identity 映射与 online/active lifecycle 的唯一 owner |
| `platform` | `cpu`, `drivers`, `fallible_tree`, `sync` | 编译期选择的 machine/firmware adapter；拥有 DTB、SBI、PLIC、UART/VirtIO 装配，不向上泄漏 raw hardware identity/context |
| `fallible_tree` | 无 | 无状态的确定性 AVL mechanism；提供显式 OOM publication、结构化 split 与 ordered-disjoint join，不拥有领域数据 |
| `sync` | `arch`, `cpu` | 锁与 IRQ transfer 只依赖本地中断 mechanism 和 logical `CpuId`；transfer token 在错误 CPU restore 时 fail-stop，禁止把 hardware identity 引入同步领域 |
| `memory` | `arch`, `config`, `cpu`, `fallible_tree`, `id`, `platform`, `random`, `sync` | VMA/frame policy；页表只通过 `arch::mmu` 的静态 frame-owner adapter，不感知具体 ISA encoding |
| `drivers` | `arch`, `cpu`, `fallible_tree`, `memory`, `sync` | 只保存设备模型与通用 interrupt interface；具体 PLIC/DTB 装配属于 platform |
| `drm` | `drivers`, `fallible_tree`, `ipc`, `memory`, `socket`, `sync` | 只消费通用 display seam；GEM handle 使用统一 fallible ordered publication；connector mode 变化只经 socket façade 发布标准 kobject uevent，不感知 VirtIO adapter、task、filesystem 或 syscall ABI |
| `input` | `drivers`, `ipc`, `sync`, `timer` | 只消费通用 input seam，并拥有 evdev 事件域；不感知 VirtIO adapter、task、filesystem 或 syscall ABI |
| `ipc` | `id`, `sync` | 只拥有 Pipe byte/endpoint，不感知 fd、task、socket 或 syscall；`id` 仅分配 anonymous inode identity |
| `socket` | `drivers`, `fallible_tree`, `id`, `ipc`, `sync`, `timer` | 拥有 socket domain facade、AF_UNIX 与 AF_INET stack；`drivers` 只允许 network-device seam，`id` 仅分配 anonymous inode identity |
| `fs` | `drivers`, `drm`, `fallible_tree`, `input`, `ipc`, `log`, `memory`, `socket`, `sync`, `timer` | `drivers` 仅允许 `block` seam；`drm`/`input`/`log` 仅允许 OFD backend；socket 仅允许统一 OFD backend facade；`memory` 仅允许 shared-page seam |
| `task` | `arch`, `cpu`, `drivers`, `drm`, `fallible_tree`, `fs`, `input`, `ipc`, `memory`, `platform`, `socket`, `sync`, `timer` | 调度只使用 logical CPU identity；`drivers` 只安装 typed I/O wait target，并在 deferred safe point 投递 completion，不依赖 concrete adapter、ISA 或 entry |
| `trap` | `arch`, `cpu`, `drivers`, `memory`, `platform`, `syscall`, `task`, `timer` | 只处理 `arch::trap::TrapEvent`、领域投递和用户返回 orchestration，不读取 CSR |
| `syscall` | `drm`, `fs`, `input`, `ipc`, `memory`, `random`, `socket`, `system`, `task`, `timer` | DRM/evdev 只编解码标准 UAPI；不得绕过 facade 接触 adapter/scheduler/page table |
| `random` | `drivers` | entropy facade；只消费 RNG device seam，不生成伪随机 fallback |
| `system` | `arch`, `cpu`, `platform` | whole-system policy；ISA 用户事实只经 `arch::user`，CPU/firmware 只经各自 facade |
| `timer` | `arch`, `config`, `cpu`, `drivers`, `platform`, `sync` | RTC 与 per-CPU deadline 由 timer 唯一拥有 |
| `log` | `cpu`, `platform`, `sync`, `timer` | 日志策略、有界 record owner 与输出在本 module 内闭合 |
| `id` | 无 | 纯 ID allocation mechanism |
| `lang_item` | `arch`, `cpu`, `platform` | 只使用 typed diagnostic identity 与 architecture/platform fail-stop mechanism |
| `main` | `arch`, `config`, `cpu`, `drivers`, `drm`, `entry`, `fallible_tree`, `fs`, `id`, `input`, `ipc`, `lang_item`, `log`, `memory`, `platform`, `random`, `socket`, `sync`, `syscall`, `system`, `task`, `timer`, `trap` | 唯一 composition root；不含 raw firmware/trap ABI |

同一 module 内引用不构成跨 seam 依赖。`main.rs` 可以依赖所有 kernel module，但只能做装配、启动顺序和 fail-stop 策略。

### Fallible ordered-storage contract

- 每个 AVL node 的 `next` 是唯一 in-order successor link；所有 structural mutation 在独占
  map borrow 内同步维护它，split 清除跨 map link、join 恢复 boundary link、retain 重建全部
  links。`Iter` 的 immutable map lifetime 保证 raw successor target 在解引用期间存活，iterator
  因而只有一个 pointer（RV64 ≤ 16 bytes）且不清零 path stack；遗漏任一 mutation 的 link 更新
  会造成漏项或悬垂指针，必须由 topology/link invariant test fail-stop。production 单邻居查询使用
  floor/ceiling/predecessor/successor，禁止用 `iter_from/iter_after(...).next()` 重建 iterator。
  map 的 unsafe Send/Sync 只恢复 `K/V: Send/Sync` 的结构 auto-trait：跨线程移动不会移动 Box
  pointee，共享访问不能改变 links；`VacantEntry` 发布前/摘除后恒为 `next=None`，`NodeSlot`
  尚未初始化，二者只按 K/V 恢复 Send/Sync。缺失这些证明会把内部 NonNull 错误泄漏为非
  Send owner，放宽到不满足 K/V bound 则会跨线程移动领域数据。
- `split_off` 只沿 AVL root-to-leaf path 比较并重组现有节点，再对移出的 subtree 做一次无 key comparison 的精确 cardinality 遍历；不为所有节点增加永久 subtree-size 字段，复杂度为 `O(log n + k)`。
- bulk join 只接受 `left.max < right.min` 的 ordered-disjoint 输入，边界检查必须在 mutation 前完成；重复或逆序表示 caller contract 损坏并 fail-stop，两表保持不变。join 只改动 AVL spine，复杂度为 `O(log n)`。
- `retain` 消费原 topology 后只执行一次 node ownership filter 与一次 sorted-list balanced rebuild；每个节点访问常数次、无分配、保留节点 identity，禁止逐节点 `join_*` 双轨。
- regression 的 primary metric 是 AVL order/height/length/successor-link/model 不变量、RV64 iterator ≤ 16 bytes、完整有序扫描至多一次 key comparison，以及 split 不超过原树高的 key comparison、join 一次边界 comparison 和 cardinality 每个移出节点一次 structural visit。
  host wall time 会混入建树 allocation 与 runner 调度，不作为稳定 gate；只有新增持续调用该 bulk interface 的 production 热路径时才增加 wall-clock benchmark。


## State-owner registry

每个复合状态只能由一个领域 owner 持有。精确 owner 在对应子契约维护；下表由 checker 读取，必须与 production source 双向一致。

### Persistent FallibleMap registry

下表是生产 kernel 中全部持久 `FallibleMap` 字段与 static 的精确集合。checker 对 location 与完整类型双向比对；新增、删除、换型或遗漏登记都必须先完成 owner/interface review。函数局部 transaction scratch 不属于持久 owner，不在此登记。

| Location | Type |
|---|---|
| `kernel/src/platform/qemu_virt/plic.rs :: PlicInterruptController.affinities` | `FallibleMap < InterruptVector , CpuSet >` |
| `kernel/src/platform/qemu_virt/plic.rs :: PlicInterruptController.handlers` | `FallibleMap < InterruptVector , Arc < dyn InterruptHandler > >` |
| `kernel/src/drm.rs :: DrmDeviceState.framebuffers` | `FallibleMap < u32 , Framebuffer >` |
| `kernel/src/drm.rs :: DrmFileState.buffers` | `FallibleMap < u32 , Arc < DumbBuffer > >` |
| `kernel/src/drm/publication_order.rs :: IdAllocator.reusable` | `FallibleMap < T , () >` |
| `kernel/src/drivers/io_completion/request_owner.rs :: RequestOwner.capacity_waiters` | `FallibleMap < u64 , Arc < CapacityWait > >` |
| `kernel/src/fs/epoll.rs :: EpollState.interests` | `FallibleMap < InterestKey , Interest >` |
| `kernel/src/fs/epoll.rs :: EpollState.ready` | `FallibleMap < InterestKey , () >` |
| `kernel/src/fs/epoll.rs :: EpollMemberships.entries` | `Mutex < FallibleMap < ReverseKey , ReverseMembership > >` |
| `kernel/src/fs/epoll.rs :: static SOURCE_INDEX` | `Mutex < FallibleMap < SourceIndexKey , SourceMembership > >` |
| `kernel/src/fs/vfs/opened_index.rs :: OpenedIndex.entries` | `Mutex < FallibleMap < OpenedIndexKey , Weak < OpenedFile > > >` |
| `kernel/src/fs/ext2.rs :: Ext2FileSystem.inode_cache` | `Mutex < FallibleMap < u32 , Weak < Ext2Inode > > >` |
| `kernel/src/fs/ext2/journal.rs :: ActiveTransaction.writes` | `FallibleMap < u32 , Vec < u8 > >` |
| `kernel/src/fs/ext2/journal/commit_owner.rs :: JournalCommit.writes` | `Arc < FallibleMap < u32 , Vec < u8 > > >` |
| `kernel/src/fs/ext2/journal/commit_owner.rs :: JournalOwner::Committing[0]` | `Arc < FallibleMap < u32 , Vec < u8 > > >` |
| `kernel/src/fs/page_cache.rs :: static FILES` | `Once < Mutex < FallibleMap < SharedFileId , Arc < CachedFile > > > >` |
| `kernel/src/fs/page_cache/reclaim.rs :: CachedPages.entries` | `FallibleMap < u64 , Arc < CachedPage > >` |
| `kernel/src/arch/riscv64/page_table.rs :: PageTable.table_pages` | `FallibleMap < usize , Page >` |
| `kernel/src/memory/mm/area.rs :: MapArea.data_frames` | `FallibleMap < VirtualPageNumber , PrivateResident >` |
| `kernel/src/memory/mm.rs :: MemorySet.areas` | `FallibleMap < VirtualPageNumber , MapArea >` |
| `kernel/src/memory/mm/shared_area.rs :: AnonymousSharedBacking.frames` | `Mutex < FallibleMap < usize , Arc < FrameTracker > > >` |
| `kernel/src/memory/mm/shared_area.rs :: SharedFileArea.resident` | `FallibleMap < VirtualPageNumber , SharedResident >` |
| `kernel/src/memory/mm/shootdown.rs :: TranslationCommit.retired_table_pages` | `FallibleMap < usize , FrameTracker >` |
| `kernel/src/socket/inet.rs :: NetworkStack.endpoints` | `FallibleMap < SocketHandle , EndpointState >` |
| `kernel/src/socket/inet.rs :: NetworkStack.raw_endpoints` | `FallibleMap < SocketHandle , raw_endpoint :: RawEndpointState >` |
| `kernel/src/socket/inet.rs :: NetworkStack.tcp_endpoints` | `FallibleMap < usize , TcpEndpointState >` |
| `kernel/src/socket/inet/port_namespace.rs :: Occupancy.specific` | `FallibleMap < Ipv4Addr , AddressOccupancy >` |
| `kernel/src/socket/inet/port_namespace.rs :: PortNamespace.entries` | `FallibleMap < u16 , Occupancy >` |
| `kernel/src/socket/packet.rs :: PacketRegistry.endpoints` | `FallibleMap < usize , EndpointState >` |
| `kernel/src/socket/kobject.rs :: KobjectRegistry.endpoints` | `FallibleMap < u64 , Weak < KobjectSocket > >` |
| `kernel/src/socket/unix/namespace.rs :: static NAMESPACE` | `Once < Mutex < FallibleMap < NamespaceKey , Weak < UnixSocket > > > >` |
| `kernel/src/socket/unix/rights_graph.rs :: RightsGraph.nodes` | `FallibleMap < u64 , Arc < GraphNode > >` |
| `kernel/src/socket/unix/rights_graph.rs :: RightsGraph.uid_inflight` | `FallibleMap < u32 , usize >` |
| `kernel/src/socket/unix/stream_backlog.rs :: StreamBacklog.pending` | `FallibleMap < u64 , T >` |
| `kernel/src/task/task_manager.rs :: ProcessGraph.nodes` | `FallibleMap < usize , ProcessNode >` |
| `kernel/src/task/task_manager.rs :: ProcessGraph.groups` | `FallibleMap < (usize , usize) , ProcessGroupIndex >` |
| `kernel/src/task/task_manager.rs :: ProcessGraph.threads` | `FallibleMap < usize , ThreadIndex >` |
| `kernel/src/task/task_manager.rs :: ProcessNode.children` | `FallibleMap < usize , () >` |
| `kernel/src/task/task_manager.rs :: ProcessNode.child_waiters` | `FallibleMap < usize , Arc < TaskControlBlock > >` |
| `kernel/src/task/task_manager.rs :: ProcessState::Live[0]` | `FallibleMap < usize , Arc < TaskControlBlock > >` |
| `kernel/src/task/task_manager.rs :: ProcessGroupIndex.members` | `FallibleMap < usize , () >` |
| `kernel/src/task/task_manager.rs :: ThreadIndex.created_children` | `FallibleMap < usize , () >` |
| `kernel/src/task/task_manager/timer_queue.rs :: TimerQueue.deadline_index` | `FallibleMap < (u64 , TimerIdentity) , () >` |
| `kernel/src/task/task_manager/timer_queue.rs :: TimerQueue.posix_timers` | `FallibleMap < (usize , i32) , PosixTimer >` |
| `kernel/src/task/task_manager/timer_queue.rs :: TimerQueue.real_timers` | `FallibleMap < usize , RealTimer >` |
| `kernel/src/task/task_manager/signal/job_control.rs :: JobNotification.waiters` | `FallibleMap < usize , Arc < TaskControlBlock > >` |
| `kernel/src/task/task_manager/wait_registry/batch.rs :: ClaimedBatch.entries` | `FallibleMap < WaitIndexKey , Arc < WaitRegistration > >` |
| `kernel/src/task/task_manager/wait_registry/shard.rs :: WaitShard.index` | `FallibleMap < WaitIndexKey , Arc < WaitRegistration > >` |

## 4. Source size contract

生产 kernel/bootloader Rust 源文件，`tools/kernel-unit/src`、`tools/scheduler-unit/src` 下的
host unit-test Rust 源文件，以及 `tools/architecture-check/src` 中以 `_tests.rs` 命名的
unit-test module 采用两级围栏：超过 600 行触发 architecture review notice，但不单独导致
验证失败；超过 1200 行默认拒绝。architecture-check 的 unit-test module 必须使用
`_tests.rs` 后缀，禁止把测试正文重新塞回 production `main.rs` 绕过围栏。
reviewer 必须检查 owner、依赖方向、公开接口与真实领域 seam。生产 source 可以拆成深
module，或在下表登记精确审查额度；登记必须给出 owner、暂不拆分的原因与消除条件。
额度是硬上限，只能随重构下降，不得为功能开发上调；文件缩短时 checker 强制同步下调。
行数只是退化信号，禁止按行数机械切片或建立 pass-through module。

host unit-test 入口只装配 production path module 与领域测试 module；测试正文按
filesystem/storage、memory、platform/execution、socket/ABI 等真实 seam 放入独立文件。
unit-test source 的 review notice 不允许把新测试继续堆进入口文件；必须先检查测试所属
owner 并迁移到对应领域 module，或新增有领域含义的 module。

`user/` 下全部 Rust/C/header/JS/TypeScript/CSS source 采用单文件 600 行硬上限，不提供 review
例外。超过上限必须按状态 owner/interface 拆分；checker 递归扫描实际文件集，避免新 crate、
应用或 native bridge 绕过围栏。

| Source | Reviewed max lines | Owner | Reason | Exit criterion |
|---|---:|---|---|---|

## Change contract

- 修改前确定所属 module、state owner、interface 与依赖变化，以及 error、exit、interrupt cleanup。
- 未列入 dependency matrix 的跨 module 依赖一律失败；同 module 内引用不构成跨 seam 依赖。
- 新 global、lock、Atomic、Once、cache、flag 或 unsafe 必须在领域契约记录 owner、安全证明与缺失时的具体失败。
- scoped interface 只能用 `cargo run -p architecture-check -- --write-interface` 生成到 [generated baseline](generated/architecture-interface.txt)，禁止手工编辑。
- 文档、单元测试、性能测试和运行时 gate 必须随契约变化同步维护；不得放宽围栏或阈值来接受实现错误。
