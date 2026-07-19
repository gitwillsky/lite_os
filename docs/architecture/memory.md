# 内存当前架构

## 当前设计

- frame allocator 唯一拥有物理页容量与 buddy metadata；global allocator 从它临时取得未初始化 slab/direct extent，不复制容量 owner，也不为 Rust allocator 本就不可读的 payload 产生整段 dead zero-fill。用户页与 DMA backing 仍由普通清零 seam 分配。
- `MemorySet` 唯一拥有 page table、program break 与有序 VMA。ELF、stack、anonymous、file、shared/private mapping 使用同一 VMA lifecycle。
- `AddressSpace` 使用 task-context mutex 保护 `MemorySet`；page fault、user-copy 与 procfs
  统计发生竞争时进入 scheduler Blocked/FIFO handoff，不在同 CPU 自旋。每个 AddressSpace
  增加一个 8-byte immutable token projection；每个 Thread 创建时增加一个 pointer-sized
  retirement preparation，保证 exit/rollback 不在提交后分配 waiter。uncontended acquisition
  先命中 `try_lock`，每次 waiter allocation 固定为 0；truncate 每个 transaction 只准备 1 个
  pointer-sized waiter，并在全部 live AddressSpace 间复用，allocation 数不随 owner 数增长。
- generic memory 只提交 READ/WRITE/EXECUTE/USER/GLOBAL 等语义权限；PTE 编码、canonical address、address-space token 和 local fence 属于 `arch::mmu`。
- user-copy 在 AddressSpace lock 内先完成全范围 fault-in 与权限证明，再复制；不会向 Rust 返回可逃逸的用户 frame reference。
- file mapping range、page-cache resident、private resident、COW 与 futex key 各有单一 owner，OOM 在 publication 前显式返回。
- reclaim 使用有界 cursor 和 fixed batch；页表撤销决定 TLB flush，不能以 frame 最终释放代替 translation invalidation。
- leaf mutation 统一经 `TranslationCommit` 分类：publication/permission relax 只做 local translation fence，revoke/restrict/frame replacement 才向其他 online CPU 发 shootdown；lazy mmap 不产生 leaf，因此不 fence。
- page fault publication 每页只产生一次 local fence。以 1 MiB、256 页 first-touch 为确定性指标，
  remote target 总数从 `256 × (online_cpus - 1)` 降为 `0`；revoke batch 的 remote target
  数保持为 `online_cpus - 1`。合并跨度至多 64 页时保留精确 range；更大或稀疏跨度
  固定为 1 次 full fence，避免按最大/最小地址之间的空洞逐页循环。
- VMA AVL 的 ordered neighbors 直接处理 overlap 与 anonymous merge；`VmaIndexState` 随 node transaction 增量维护唯一 stack key 和资源 totals。
  100 VMA 下，1 MiB prepare-user-write 的 stack lookup 从 25,600 次节点访问降为 0 次全扫描；hinted mmap 从 5 次、至少 500 次节点访问降为 0 次全扫描，仅保留 O(log VMA) 邻居查找。
- RISC-V startup 按固定 Privileged ISA 方法探测 `satp.ASID` 宽度；每个 live page table 持有唯一非零 ASID。每个 CPU 首次观察该 ASID 时做一次 ASID-scoped ordering fence，此后普通 kernel/user switch 只写两次 `satp`，不做 full fence。
  地址空间析构保活完整 mapping/frame owner，完成所有 online CPU 的全量失效后才归还 ASID。
- leaf unmap 会自底向上摘除空 Sv39 L0/L1 table；单个孤立 mapping 的 active table pages 从 unmap 后 3 降为仅 root 1。摘除的最多两页由 `TranslationCommit` 保活，remote revoke fence 完成后才回到 frame allocator。
- kernel identity mapping 按每个 VMA 的精确权限边界选择最大对齐 Sv39 leaf；128MiB 对齐 physmap 的 leaf PTE 数从 32,768 个 4KiB leaf 降为 64 个 2MiB leaf，translate façade 仍投影逐页 PPN。
- executable leaf publication 与新增 EXECUTE 权限由同一 `TranslationCommit` 记录；instruction bytes 完成后执行本地 data fence/`fence.i` 并同步所有 online remote CPU。普通 trap return 不执行 `fence.i`，后上线 CPU 在 startup 做一次本地初始化 fence。

## Known limits

- 当前 RISC-V backend 使用 Sv39；硬件若不提供非零 ASID 会在 startup fail-stop，不保留 ASID=0 兼容路径。
- 没有 swap，也没有后台 page-cache reclaim/writeback worker。
