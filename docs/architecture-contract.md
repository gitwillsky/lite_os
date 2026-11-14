# LiteOS Architecture Contract

各领域的详细契约见[文档索引](README.md#架构契约)。

## Kernel dependency contract

通用机制与系统服务的边界见 [架构](architecture.md)；领域理由见 [依赖说明](architecture-contract/dependency-owners.md)。
矩阵只登记内核领域。外部 crate 的调用受 Cargo 依赖与服务边界围栏约束，不伪装成内核 module。

| Module | 允许依赖（机器读取） | 说明 |
|---|---|---|
| `arch` | `config` | 静态机制与显式 owner 依赖 |
| `config` | 无 | 静态机制与显式 owner 依赖 |
| `id` | 无 | 静态机制与显式 owner 依赖 |
| `cmdline` | 无 | 静态机制与显式 owner 依赖 |
| `interrupt` | 无 | 静态机制与显式 owner 依赖 |
| `boot` | 无 | 只声明子领域 |
| `execution` | 无 | 只声明子领域 |
| `cpu` | `arch` | 静态机制与显式 owner 依赖 |
| `serial` | `sync` | 输入字节与读者等待的唯一 owner |
| `sync` | `arch`,`cpu` | 静态机制与显式 owner 依赖 |
| `deferred` | `arch`,`cpu`,`platform` | 合并工作 owner；本地 software interrupt 与跨 CPU 的静态平台 IPI edge |
| `platform` | `arch`,`cpu`,`interrupt`,`serial`,`sync` | 静态机制与显式 owner 依赖 |
| `barrier` | `cpu`,`platform` | 静态机制与显式 owner 依赖 |
| `timer` | `arch`,`cpu`,`platform` | 静态机制与显式 owner 依赖 |
| `memory` | `arch`,`config`,`cpu`,`deferred`,`id`,`platform`,`sync` | 分配器水位在锁内只发布 deferred 边沿；上层观察者由 main 注入，不依赖 VM/object/thread |
| `vm` | `arch`,`barrier`,`cpu`,`id`,`memory`,`platform`,`sync`,`timer` | 回收策略经 sync task seam 协作，读取单调时钟设无进展重试截止；远端翻译同步注入无阻塞 barrier 本地确认，不依赖 Thread |
| `iommu` | `arch`,`memory`,`sync`,`vm` | 静态机制与显式 owner 依赖 |
| `log` | `cpu`,`deferred`,`platform`,`sync`,`timer` | 静态机制与显式 owner 依赖 |
| `crash` | `arch`,`cpu`,`log`,`memory`,`platform`,`timer`,`vm` | 静态机制与显式 owner 依赖 |
| `random` | `arch`,`platform`,`sync` | 静态机制与显式 owner 依赖 |
| `thread` | `arch`,`barrier`,`cpu`,`crash`,`deferred`,`id`,`log`,`memory`,`platform`,`serial`,`sync`,`timer`,`vm` | 调度安全点推进 barrier/TLB 完成，其余为静态机制与显式 owner 依赖 |
| `object` | `barrier`,`cpu`,`deferred`,`id`,`interrupt`,`iommu`,`memory`,`platform`,`sync`,`thread`,`timer`,`vm` | 静态机制与显式 owner 依赖 |
| `syscall` | `barrier`,`cpu`,`crash`,`log`,`memory`,`object`,`platform`,`random`,`serial`,`sync`,`thread`,`timer`,`vm` | 静态机制与显式 owner 依赖 |
| `trap` | `arch`,`barrier`,`cpu`,`crash`,`deferred`,`log`,`platform`,`serial`,`thread`,`timer`,`vm` | 静态机制与显式 owner 依赖 |
| `entry` | `cpu`,`platform`,`trap` | 静态机制与显式 owner 依赖 |
| `bootstrap` | `arch`,`memory`,`object`,`platform`,`thread`,`vm` | 静态机制与显式 owner 依赖 |
| `lang_item` | `crash` | 静态机制与显式 owner 依赖 |
| `main` | 所有已登记领域（仅装配） | 静态机制与显式 owner 依赖 |

同一领域内引用不跨 seam。目录分组不得充当依赖别名；`main` 只装配。
依赖矩阵必须无环，AST 与宏 token 检查覆盖 `$crate`、分组和别名导入。
`arch` 拥有寄存器、上下文与表项编码，`platform` 拥有 machine facts 与装配。
驱动分配的 IOMMU 表/队列由 `arch::dma::DmaFrame` 提供生命周期，编码后端不反向依赖通用内存领域。

## State-owner registry

### Persistent FallibleMap registry

持久 `FallibleMap` 字段/static 的精确集合；checker 双向比对 location/完整类型，新增/删改/遗漏须 owner/interface review；局部 scratch 不登记。[AVL 与审查规则](architecture-contract/ordered-storage.md)。

| Location | Type |
|---|---|
| `kernel/src/arch/aarch64/iommu.rs :: Backend.contexts` | `Mutex < fallible_tree :: FallibleMap < u32 , F > >` |
| `kernel/src/arch/aarch64/page_table.rs :: PageTable.table_pages` | `FallibleMap < usize , Page >` |
| `kernel/src/arch/riscv64/iommu.rs :: Backend.leaves` | `Mutex < FallibleMap < usize , F > >` |
| `kernel/src/arch/riscv64/page_table.rs :: PageTable.table_pages` | `FallibleMap < usize , Page >` |
| `kernel/src/execution/sync/futex/queue.rs :: Bucket.waiters` | `FallibleMap < Slot , W >` |
| `kernel/src/iommu/mod.rs :: Domain.allocated` | `FallibleMap < usize , usize >` |
| `kernel/src/iommu/mod.rs :: Iommu.domains` | `TaskMutex < FallibleMap < u32 , Domain > >` |
| `kernel/src/object/task.rs :: JobState.children` | `FallibleMap < u64 , Weak < Job > >` |
| `kernel/src/object/task.rs :: JobState.processes` | `FallibleMap < u64 , Weak < Process > >` |
| `kernel/src/object/task.rs :: Lifecycle.threads` | `FallibleMap < u64 , Weak < Thread > >` |
| `kernel/src/platform/qemu_virt/aarch64/its.rs :: Properties.routes` | `IrqMutex < FallibleMap < u32 , Arc < Device > > >` |
| `kernel/src/platform/qemu_virt/aarch64/gicv3.rs :: GicV3.handlers` | `FallibleMap < InterruptVector , Arc < dyn InterruptHandler > >` |
| `kernel/src/platform/qemu_virt/riscv64/aia.rs :: Routes.vectors` | `FallibleMap < u32 , Route >` |
| `kernel/src/platform/qemu_virt/riscv64/aia.rs :: Cold.devices` | `FallibleMap < u32 , () >` |
| `kernel/src/thread/timer_queue/index.rs :: DeadlineIndex.map` | `FallibleMap < Key , V >` |
| `kernel/src/vm/address_space.rs :: SpaceInner.contexts` | `FallibleMap < VirtualPageNumber , FrameTracker >` |
| `kernel/src/vm/address_space.rs :: SpaceInner.nodes` | `FallibleMap < u64 , VmarNode >` |
| `kernel/src/vm/address_space.rs :: VmarNode.children` | `FallibleMap < usize , Child >` |
| `kernel/src/vm/kernel_space.rs :: KernelSpace.framed` | `FallibleMap < VirtualPageNumber , FramedRegion >` |
| `kernel/src/vm/translation.rs :: TranslationCommit.retired_table_pages` | `FallibleMap < usize , TableFrame >` |
| `kernel/src/vm/vmo/page_list.rs :: PageList.chunks` | `FallibleMap < usize , Chunk < T > >` |
| `kernel/src/vm/vmo/reclaim.rs :: static PAGED` | `IrqMutex < FallibleMap < u64 , Weak < Vmo > > >` |

## 4. Source size contract

已审核文件拆分后继续保留收紧的上限，即使低于首次 review 阈值，也不撤销递减约束。

| Source | Reviewed max lines | Owner | Reason | Exit criterion |
|---|---:|---|---|---|
| `system/linuxd/src/fs/tmpfs.rs` | 984 | `fs` | inode 权限与通知事务 | 新增 metadata family 前拆分 |
| `system/linuxd/src/task/process_table/signal.rs` | 723 | `task` | graph 下的生成与子进程通知事务 | 新增独立 signal producer 前拆分 |
| `system/linuxd/src/fs/procfs.rs` | 849 | fs | ProcNode 投影，目录枚举归 procfs/directory，lookup 归 procfs/lookup，MM 文本归 procfs/maps | 新增独立目录 family 前拆分 |
| `kernel/src/vm/vmo.rs` | 577 | `vm` | COW 树、页解析与映射退休。构造归 creation，物理归属/驻留归 residency；补页归 pager，别名登记/投影归 mapping，祖先折叠/释放归 release，固定归 pin，淘汰归 reclaim | 增加新的 backing kind 前审查页解析职责 |
| `kernel/src/vm/address_space.rs` | 917 | `vm` | VMAR 树与页表修改同锁裁决 | 新增 mapping operation 前拆出独立 transaction |
| `system/network-protocol/src/lib.rs` | 224 | `lib` | 通用格式与 Handler；INET/packet 控制各归 inet/packet，地址/接口归 address/interface，选项归 options，域操作归 namespace，路由归 routes | 增加 wire family 前按领域继续拆分 |
| `system/ext4/src/fuse_server.rs` | 998 | `fuse_server` | FUSE 操作分派与每文件内容缓存的生命周期在同一会话事务内（lookup/forget、写回与 syncfs） | 新增 FUSE 操作 family 前把内容缓存表拆到独立 owner |

## Change contract

- 修改前确定所属 module、state owner、interface 与依赖变化，以及 error、exit、interrupt cleanup。
- 新 global、lock、Atomic、Once、cache、flag 或 unsafe 必须在领域契约记录 owner、安全证明与缺失时的具体失败。
- scoped interface 只能用 `cargo run -p architecture-check -- --write-interface` 生成到 [generated baseline](generated/architecture-interface.txt)，禁止手工编辑。
- 文档、单元测试、性能测试和运行时 gate 必须随契约变化同步维护；不得放宽围栏或阈值来接受实现错误。
