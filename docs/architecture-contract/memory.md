# 内存契约

## Owner

- frame allocator 独占物理页容量和 buddy metadata；global allocator 独占已借入 extent 内的
  slab/direct metadata。普通 frame、用户 backing 与 DMA backing 保持分配时清零；只有
  global allocator 可通过 `alloc_heap_extent` 取得不可读的未初始化 extent，并按 Rust
  allocator 契约在 caller 初始化前禁止读取。
- `MemorySet` 独占 page table、有序 VMA 集合和 program break；`mm::area` 只封装单个 VMA
  的范围、backing、resident frame 与 map/partition/merge mechanism，不取得集合 owner。
  page cache 独占 shared file page、dirty/writeback 与 reclaim state。
- `AddressSpace` 以唯一 `TaskMutex<MemorySet>` 串行化可 fault、可进入 page cache 或可被同
  mm sibling 抢占的 task-context operation；普通 `spin::Mutex<MemorySet>` 会让当前 CPU
  自旋等待 Ready sibling，调度器无法恢复真正 owner。reclaim 只能 `try_lock`，hardirq、
  kernel SSIP 与 deferred handler 禁止取得该锁。
- `AddressSpace.token` 只缓存同一 `MemorySet` 生命周期内不变的 arch projection；ASID/root
  生命周期仍由 `MemorySet` 唯一拥有。缺失该只读缓存会迫使 IRQ-disabled trap return 进入
  可阻塞锁；缓存不得独立更新或跨 AddressSpace 复用。
- `FilePageRange` 独占 file mapping checked projection；`PrivateResident` 与 `SharedResident` 分别独占对应 residency record。
- `mm::shootdown::TranslationCommit` 是一次页表 mutation 的唯一 fence-policy owner；它合并 leaf transition 与页区间，其他 module 不得用 bool、cache 或直接 platform call 复制判定。
- `MemorySet::areas` 与 `VmaIndexState` 共同构成唯一 VMA index owner；后者只保存唯一 stack start key、RLIMIT_AS total 与 RLIMIT_DATA total。structural node publication/retire 必须在同一 `commit_area`/`take_area_entry` transaction 更新它，禁止旁路 cache 或事后全表重算。
- `arch::mmu` 的 ASID bitmap 是 address-space identity 的唯一 lifecycle owner；ASID bit 从分配保持到完整 `MemorySet` owner 完成全 CPU retirement fence，其他 module 不得直接复用或释放。
- architecture page table 的 active frame index 以 physical page 为唯一 key；leaf unmap 自底向上摘除空 L0/L1 table，`TranslationCommit` 保留这些 frame owners 到 local/remote revoke fence 全部完成。

## Interface

- generic memory 只向 `arch::mmu` 提交语义权限和 frame-owner adapter；PTE bit、address token 与 fence instruction 不得泄漏。
- kernel identity range 只向 architecture 提交精确 `[start,end)` 与统一 permissions；Sv39 walker 在不跨该边界的前提下选择最大对齐 1GiB/2MiB/4KiB leaf。generic translation 仍返回目标 4KiB physical page，不泄漏 leaf level。
- user-copy 必须先完整证明 range membership、fault 与权限，再复制；不得返回指向 user memory 的 Rust reference。
- `/dev/zero` 使用 `MemorySet::zero_user` 在一次 AddressSpace owner transaction 内 fault-in
  连续用户 range 并逐页清零；不得构造固定小 zero buffer 后重复进入 user-copy。COW 完整页替换
  只通过 frame allocator 的 full-overwrite seam 分配并复制，普通匿名/页表 frame 仍必须清零。
- 所有 fallible owner storage 必须在 PTE、VMA、cache 或 global registry publication 前 reserve。
- post-storage shared-file invalidation 必须在 truncate mutation 前准备一份可复用
  `TaskMutexWaitPreparation`，commit 后逐 AddressSpace 撤销 PTE 只允许阻塞、不得再分配或
  返回 OOM。Thread 创建同样预留 temporary trap-mapping retirement waiter；退出必须在仍为
  current/Running 时完成所有 address-space cleanup，之后才 detach scheduler owner。
- VMA split 必须结构化 partition resident owner 节点；相邻 anonymous VMA merge 只允许使用已证明 `left.max < right.min` 的 ordered-disjoint join，不得逐 entry remove/reinsert 或覆盖重复 residency。
- overlap 判定和 anonymous merge 必须使用 ordered VMA index 的 floor/ceiling/predecessor/successor；hinted mmap、stack fault 与单邻居 merge 不得创建全表 iterator。stack VMA 在一个 MemorySet 内必须唯一，grow 只 rekey 该 authoritative key。
- futex key 只能由 AddressSpace identity 或 backing identity + offset 归一化；syscall/task 不得重建 mapping identity。
- `KERNEL_SPACE` 是唯一 kernel page-table ordinary lock；VirtIO fixed DMA storage 只能在 adapter
  初始化、descriptor publication 之前取得该锁并一次缓存物理 segments。steady-state submission、
  hardirq 与 kernel SSIP 不得再次取得该锁或逐页 translation；禁止保留 runtime slice fallback。
- invalid→valid publication 与权限增加只提交当前 CPU 的 translation fence；valid→invalid、
  权限收紧与物理 frame replacement 必须提交 local fence，并同步所有其他 online/possible CPU。
  单个 commit 的合并跨度不超过 64 页时保留精确 range，超过 64 页时由同一
  `TranslationCommit` 规范化为一次 full local/remote fence；禁止把稀疏跨度逐页执行。
  lazy VMA 未写 leaf PTE，fence 数必须为零。
- huge leaf revoke 必须记录完整 leaf span；只允许从 leaf-aligned virtual page 撤销，禁止把中间 4KiB unmap 静默扩大到整个 huge leaf。
- fixed RISC-V Privileged 规范允许 publication/relax 暂时命中旧 invalid/restrictive translation；对应 page fault 必须先执行当前 CPU range fence 再重试。地址空间 activation 的 full local fence 不得作为 mutation 兼容路径。
- address-space retirement 是唯一 full remote fence 例外：完整 `MemorySet` owner 必须保活到全部 CPU fence 完成，随后才能归还 ASID 并释放 page-table/frame owner。
- executable mapping publication 或权限首次增加 EXECUTE 必须由 `TranslationCommit` 在 instruction bytes 写完后提交本地 data/`fence.i` 与全部 online remote `FENCE.I`；trap return 不得作为 instruction-cache publication 兼容路径。

## Failure and cleanup

- `MAP_FIXED` 等 destructive mutation 必须在所有可前置验证完成后提交；失败不留下部分 unmap、PTE 或 owner publication。
- VMA split/protect/unmap 的节点 allocation 必须在首个 structural mutation 前完成；未发布 token 与 PTE rollback 不得改变 `VmaIndexState`。fork/exec 从空 state 逐 node publication 建立独立 totals，不能复制可漂移 cache。
- page-table rollback、TLB invalidation 与 frame release 分开证明；仍被其他 owner 引用的 frame 不得抑制 translation flush。
- full-overwrite frame 在进入 page table/Arc 前必须完整复制一页；partial source 或复制前 publication
  会把旧 owner 数据暴露给新地址空间，必须 fail-stop，不能退回“先清零再复制”的双轨。
- heap extent 不得经普通 zeroed frame seam 分配；slab free-chain/header publication 与
  direct header publication 只建立 allocator owner，返回 payload 仍是 Rust 定义的
  uninitialized storage。该 seam 禁止用于 user mapping、DMA read buffer 或 partial-init 后
  可读的对象，否则旧物理页内容会泄漏。
- translation retire 必须严格按 `撤销 PTE -> 同步完成全部目标 CPU fence -> 释放 frame/device/writer owner` 提交；fence 失败时必须保留 owner 并 fail-stop，禁止由错误展开提前 Drop。
- 空 table retirement 同样遵循 retire 顺序；active AVL node token 与 frame owner 必须无分配移交给 commit 的 retirement map，OOM rollback 不得再次申请 retention storage。commit 未 fence 或 fence 失败时只能泄漏 table frame，不能让 stale parent PTE walk 到复用内存。
- ASID retirement 必须严格按 `保活完整地址空间 -> local/remote full fence -> bitmap release -> frame/page-table Drop` 提交；每 CPU seen bit 只能在新分配后清零，首次 activation 必须先做 ASID-scoped fence 再发布 seen。缺失 release 只允许耗尽 ID，绝不能提前复用。
- shared-file writer claim 必须覆盖远端 stale writable translation 的完整生命周期：增加写权限在 PTE publication 前 acquire，收紧写权限在 remote fence 完成后 release。
- private reclaim 的 Arc owner count 只能在 fence 后 release replay 时决定实际回收数；revoke-time count 仅用于有界扫描节奏，不能跨 fence 断言稳定。达到 request target 后必须保留其余 resident owner，即使其 PTE 已撤销，后续 fault 仍可重建 translation。
