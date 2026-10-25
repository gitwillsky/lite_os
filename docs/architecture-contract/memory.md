# 内存契约

## Owner

- frame allocator 独占物理页容量和 buddy metadata；global allocator 独占已借入 extent 内的
  slab/direct metadata。普通 frame、用户 backing 与 DMA backing 保持分配时清零；只有
  global allocator 可通过 `alloc_heap_extent` 取得不可读的未初始化 extent，并按 Rust
  allocator 契约在 caller 初始化前禁止读取。
- frame allocator 必须在 publication 前从自身物理输入区间前缀保留足量整页 metadata，
  以一 byte/allocatable-frame 建立 buddy state；该前缀永久排除在 allocatable capacity 外。
  metadata 不得从固定 bootstrap heap 分配，否则 Guest RAM 增大时会在 frame-backed heap
  尚未发布前形成容量循环依赖。
- `MemorySet` 独占 page table、有序 VMA 集合和 program break；`mm::area` 只封装单个 VMA
  的范围、backing、resident frame 与 map/partition/merge mechanism，不取得集合 owner。
  page cache 独占 shared file page、dirty/writeback 与 reclaim state。
- `AddressSpace` 以唯一 `TaskMutex<MemorySet>` 串行化可 fault、可进入 page cache 或可被同
  mm sibling 抢占的 task-context operation；普通 `spin::Mutex<MemorySet>` 会让当前 CPU
  自旋等待 Ready sibling，调度器无法恢复真正 owner。reclaim 只能 `try_lock`，hardirq、
  kernel SSIP 与 deferred handler 禁止取得该锁。
- Generic AddressSpace root 的 strong Arc 是实际 mmget mapping user：Thread handle、
  proc get_task_mm 与 registry callback view 均保活全部 VMA/pages；Weak upgrade 相当于
  mmget_not_zero，最后 strong user Drop 才销毁 MemorySet。不得将 structural-only observer
  存成 generic root Arc，或再维护独立 mm_users counter。Process/TCB observer 只保活可空
  outer Thread handle；backend AddressSpaceToken 仅保活 arch Context capability，属于独立
  root/ASID mechanism lifetime，不将其视为 generic mmget。exit token 必须移动 Thread 的
  原 root user，不能 clone 导致最后使用者被错误判断成多使用者。
- `AddressSpace.token` 只缓存同一 `MemorySet` 生命周期内不变的 arch projection；ASID/root
  生命周期仍由 `MemorySet` 唯一拥有。缺失该只读缓存会迫使 IRQ-disabled trap return 进入
  可阻塞锁；缓存不得独立更新或跨 AddressSpace 复用。
- `FilePageRange` 独占 file mapping checked projection；`PrivateResident` 与 `SharedResident`
  独占各自逻辑映射 record/epoch，`SwappablePage` 唯一拥有用户内容页的物理 backing。
  supervisor frame 不进入该用户页 owner，仍由对应 pinned record 保活。
- `PrivateBacking::File` 表示尚未 COW 的私有文件页：保活 canonical `SharedPage`，内存文件另持
  唯一 `PageMappingRef` 登记真实 private alias/installed epoch；磁盘 cache 的固定页不登记 swap alias。
  File 永不投影为 exclusive，不取得 shared writer claim；否则首个 private 写者会改坏文件内容。
  read/execute 缺页直接以只读 leaf 映射文件页，首次 write 才沿同一 COW transaction 复制为 User。
  这与固定 Linux `mm/memory.c` 的 `do_read_fault/do_cow_fault` 一致；提前复制会冻结只读映射，
  Wine 的 session 对象与键盘共享状态会永远停在旧值。
  fork 继承相同内容与 epoch，mprotect 保持 File 只读，reclaim/truncate/unmap 在 fence 后释放 alias；
  swap revoke/unuse 经同一 mapping accessor 覆盖 File，VmSwap 不把未 COW 的 shmem 计作匿名私有页。
  OOM/IO 先于 leaf publication 返回；COW 保活 source pin 到完整 revoke→make fence 完成，无新增锁或 unsafe。
- `mm::shootdown::TranslationCommit` 是一次页表 mutation 的唯一 fence-policy owner；它合并 leaf transition 与页区间，其他 module 不得用 bool、cache 或直接 platform call 复制判定。
- `arch::mmu::THREAD_TRAP_CONTEXT_RANGE` 是静态 placement fact：Sv39 声明 canonical
  high-half 且排除 signal/canonical trap/trampoline，AArch64 为 None（context 归 kernel stack）。
  mm::thread_context 在该区间按实际 ordered VMA neighbor 选择空页，不使用累计 TID。
  MemorySet.thread_context_hint 只记录下次搜索起点，MM 同锁验证；成功映射才推进，退休
  回指实际刚释放页。普通追加/回收 O(log VMA)，冲突按整 VMA 跳跃、耗尽绕回一次，OOM
  保持旧 hint/VMA/PTE。fork 排除该临时区间，不复制没有 child Thread owner 的 sibling contexts。
  context_storage 仍在一个 MM guard 下 map+bind；waiter/MM/scheduler 分配先于 mapping。
  mm::area_publication 集中真正 VMA/PTE publication、rollback fence 与 ordered accounting，
  不增加 owner；跨模块 commit/take/remove seam 仅在 mm 内可见，Task 不能直接变更 index。
  新 gap/hint 热路径需在最终实际 owner benchmark 评估，已有门禁阈值保持不变；
  host 生产 VMA/PTE/fork 回归已编写，物理 allocator/CPU fence 为明确 seam。
- `MemorySet::areas` 与 `VmaIndexState` 共同构成唯一 VMA index owner；后者只保存 RLIMIT_AS total 与 RLIMIT_DATA total，不镜像 stack identity。structural node publication/retire 必须在同一 `commit_area`/`take_area_entry` transaction 更新它，禁止旁路
  cache 或事后全表重算。
- `arch::address_space_id` 是两个静态 backend 共用的唯一 generation/bitmap/reservation owner；
  root 拥有 Arc<Context>，token 只缓存同一 root/context capability，非 Copy 且不得缓存 raw
  hardware encoding。boot 在 topology 发布后、user root 创建前按真实 CPU 数准备所有 storage；
  tagged profile 要求 hardware capacity > 2 * possible CPU 数，ASID0 只给 kernel。
  唯一 reservations spin mutex 串行化 generation、bitmap cursor、reserved 与 pending；
  activation 仅在已屏蔽 local IRQ 的同 CPU transfer window 取得它，arch 不依赖上层 sync。
  临界区仅执行有界
  local fence，禁止 allocation、领域回调或 remote wait。快路 active CAS 与 rollover swap(0)
  同位排序：前者先成功则其 ID 被 capture 保留，后者先完成则快路失败并在慢路 flush。
  多 CPU 的同一 reserved identity 必须全部续代，未切换 CPU 的 reservation 跨多轮保留。
- `shared_file` 唯一拥有 reclaimer/mapping-owner 的 Weak registry；稳定连续 key 只追加或复用
  已失效 slot，不删除节点，保证冻结 scan budget 与 round-robin cursor 不漂移。
  registry node 必须锁外准备、锁内复检再提交，未使用 node 在解锁后析构。锁内 allocation
  会在 heap/frame OOM 重入 direct reclaim 时重新请求 registry lock，形成自锁。
- architecture page table 的 active frame index 以 physical page 为唯一 key；leaf unmap 自底向上摘除空 L0/L1 table，`TranslationCommit` 保留这些 frame owners 到 local/remote revoke fence 全部完成。
- AArch64 TTBR1 high-half root 是唯一 kernel mapping owner，TTBR0 root 是每个用户地址空间
  owner；page-table descriptor 始终保存 physical address，Rust walker 只经 architecture
  kernel-mapping façade 解引用。低侧 120 GiB 使用连续 RAM direct map；顶部固定 1 GiB
  high-MMIO window 由同一 façade 的 immutable physical-range projection 管理。把 kernel
  mapping VA 写入 descriptor 或让 MMIO 保持低物理 pointer 都会在启用 MMU 后访问错误地址。
- task kernel stack 只从 architecture-owned virtual window 分配。AArch64 将 TTBR1 高半区
  划分为低侧 120 GiB direct map、中间约 135 GiB stack window 与顶部 1 GiB high-MMIO
  window；即使 120 GiB 物理容量全部用于 128 KiB stack，包含 guard 的虚拟跨度仍有余量。
  platform MMIO ranges 必须在 kernel page-table publication 前一次性提交给该 projection
  owner，重复提交、跨 direct-map 边界或超出窗口都必须 fail-stop。禁止非 canonical 地址
  或两个 owner 重叠。每个 AArch64 stack mapping 的最低页保持 guard，最高页只拥有对齐
  padding 与 `UserContext`；effective stack top 必须下移一页，context 地址只能由该固定
  边界加编译期 offset 推导，禁止冗余 pointer state。RISC-V 保留 trap-context 下方的既有
  Sv39 stack 与 supervisor trap-context VMA 布局。

- `pressure::BackgroundReclaim` 只在唯一内核线程栈上拥有 hysteresis 状态；容量/free count
  始终从 frame allocator 同锁快照投影，resident 与 dirty/writeback 不迁入后台 owner。
  composition root 在 scheduler/root 就绪后创建一次线程，注入 monotonic clock 与已预留的无分配 KernelDeadlineWait sleep；
  memory 不依赖 task/timer。线程创建失败阻止启动，不静默退化到只有 direct reclaim。
- direct/background reclaim 共用唯一 registry/cursor/adapter transaction；同锁分类累计
  statistics，procfs 的 pgscan/pgsteal_direct 与 pgscan/pgsteal_kswapd 不重复计数。

## Interface

- `MemorySet::from_elf` 只消费 `InitialStack` 中已准备的 argv/envp/execfn 与固定 16-byte
  AT_RANDOM payload。entropy acquisition 归 task exec loader，memory 不依赖 random/drivers；
  entropy 失败在新地址空间发布前返回 source I/O error，不替换旧 image。
- generic memory 只向 `arch::mmu` 提交语义权限、frame-owner adapter 与一次性 platform MMIO
  range publication；PTE bit、address token、mapping projection 与 fence instruction 不得泄漏。
- kernel identity range 只向 architecture 提交精确 `[start,end)` 与统一 permissions；Sv39 walker 在不跨该边界的前提下选择最大对齐 1GiB/2MiB/4KiB leaf。generic translation 仍返回目标 4KiB physical page，不泄漏 leaf level。
- AArch64 kernel RAM direct-map 与 Sv39 identity map 使用同一 generic range transaction；
  high-MMIO 通过 architecture projection 后仍使用同一 kernel-mapped range transaction。
  所选 backend 可在不跨权限边界时使用 1GiB/2MiB/4KiB leaf，generic caller 不得假设
  VA=PA。DEVICE permission 必须编码为 AArch64 Device-nGnRnE，不能与 normal cacheable
  DMA memory 合并。
- user-copy 必须先完整证明 range membership、fault 与权限，再复制；不得返回指向 user memory 的 Rust reference。
  preparation 在同一独占 mm owner 内临时设置完整页范围的 reclaim exclusion；成功/错误均恢复。
  自身 private reclaim 的 revoke/release 都必须跳过此范围，否则后页 fault 可逐出已准备的前页。
  preparation 后的复制不再分配/fault，AddressSpace lock 持续阻止外部 reclaim；确无可回收页时返回 OOM。
- `/dev/zero` 使用 `MemorySet::zero_user` 在一次 AddressSpace owner transaction 内 fault-in
  连续用户 range 并逐页清零；不得构造固定小 zero buffer 后重复进入 user-copy。COW 完整页替换
  只通过 frame allocator 的 full-overwrite seam 分配并复制，普通匿名/页表 frame 仍必须清零。
- 所有 fallible owner storage 必须在 PTE、VMA、cache 或 global registry publication 前 reserve。
  mprotect 在首个 VMA/PTE mutation 前为所有将恢复的 resident/device leaf 预留 table path；
  先在同一 mm guard 内 pin 并准备当前物理 identity，再按实际 logical epoch 准备 leaf；
  pin 释放后 mm guard 阻止迁移提交，不得在首个 mutation 后等待 page I/O。
  private leaf presence 必须查询页表，不得从 VMA 权限推断。PrivateResident 唯一投影 COW、
  MADV_FREE 与 private-file dirty 的 write restriction，read/execute fault 与 mprotect 共用它。
- post-storage shared-file invalidation 必须在 truncate mutation 前准备一份可复用
  `TaskMutexWaitPreparation`，commit 后逐 AddressSpace 撤销 PTE 只允许阻塞、不得再分配或
  返回 OOM。只有 RISC-V dynamic trap VMA Thread 创建预留 temporary trap-mapping retirement
  waiter；AArch64 context 由 KernelStack 直接保活，不得分配永远不用的 waiter 或制造
  AddressSpace cleanup 双轨。退出必须在仍为 current/Running 时完成 architecture 所需 cleanup，
  之后才 detach scheduler owner。
  Thread exit 必须在 graph 注销前完成 robust cleanup 与 user-context retirement，并捕获旧
  AddressSpace 的 Arc；graph 注销后的 clear-child-tid copyout 和 futex key 必须通过该旧 owner，
  不得重新读取 Process 的可被 exec 替换的 handle。token 构造无分配；copyout/futex fault
  仍保持既有 best-effort 语义，失败不清新 mm、不派生新 mm 的 key。
- fork 在复制首个 page/backing reference 前创建并登记空 child AddressSpace；COW 初始化
  在 parent/child task-only mm owner 下填充，child root/Context/token 不替换，不能再返回持有
  backing 的裸 MemorySet 后延迟登记。失败的未激活 child 仍由同一 retirement owner 释放；
  注册表 callback 遵守对应 mm lock，不观察半填充状态。truncate 已公布长度但等待 parent
  mm 时，晚登记 child 可能不在该次扫描内；首次继承 shared/private file resident 必须按
  已公布 EOF 过滤整页，并只发布过滤后 records 的 leaf/translation slot。保留 VMA、文件
  offset、部分末页及其 dirty/discardable metadata；parent 仍由原 truncate callback 撤销。
- VMA split 必须结构化 partition resident owner 节点；相邻 anonymous VMA merge 只允许使用已证明 `left.max < right.min` 的 ordered-disjoint join，不得逐 entry remove/reinsert 或覆盖重复 residency。
- `AnonymousSharedBacking.statistics` 只由胜出的 indexed SwappablePage 取得统计 membership。
  页的 canonical `State` 持 ledger Arc，不持 backing Arc、不另存 residency 标记；换入、换出及
  private discard 全经唯一 `State::replace_backing`，从 actual old/new Backing 派生同锁计数变更，
  State 最后析构按 actual Backing 归还。候选构造、失败 IO、fork/observer Arc 不增加收费。
  backing pages → PageState → statistics 为唯一锁序；SHM_INFO 只取 statistics 短锁，O(1) 读取
  一致 resident/swapped pair，不扫描页、不等 IO。统计 ledger 的独立 Arc 不形成 backing/pages 环。
  host fixture 在实际 owner module 内投影统计 Arc，用于观察最后页退休退款；production 字段保持 private。
- System V 映射复用 AnonymousSharedBacking；SharedMemoryVmaMembership 是每个实际 VMA 的
  唯一 nattch capability，source/backing/lifetime Arc clone 不计挂接。初始映射、fork 和非空
  split 调用 open，完整 partition 转移原 capability；retirement fence 后 close。
  task 装配的 SharedMemoryVma 实现每次取得实际 current TGID/realtime，不持 Task/mm 或
  PID membership，不冻结 creator 为后续执行者。初始失败由仍在 guard 外持有的临时
  reservation 保活；fork/split 回滚有原 VMA，因而不会在 MM guard 内最终销毁 backing。
  SHM_RDONLY 保存独立 VM_MAYWRITE ceiling，IPC_SET/mprotect 不能扩大它。
  shmdt 以原 base、实际 backing page offset 和同一次 shmat 的 lifetime identity 选择
  fragments；中间被其他映射替换的区间不被误删。
- overlap 判定和 anonymous merge 必须使用 ordered VMA index 的 floor/ceiling/predecessor/successor；hinted mmap、stack fault 与单邻居 merge 不得创建全表 iterator。grow-down VMA 可因 mprotect/munmap 拆分，grow 从 ordered ceiling 选择地址上方最近的真实
  VMA，仅其具备 Stack kind 时允许 expansion；top 从该 fragment 的真实 end 推导。
  不保存唯一原始栈 key 或冻结 top。默认 256-page stack_guard_gap 只针对 accessible 非 grow-down
  邻居，已映射 range 一律不得覆盖。benchmark 决策：每次 fault 增加 O(log VMA) ceiling 查询，
  支持真实 split lifecycle，继续禁止全表扫描；最终 VMA/page-fault 门禁检查成本。
- futex key 只能由 AddressSpace identity 或 backing identity + offset 归一化；syscall/task 不得重建 mapping identity。
- `KERNEL_SPACE` 是唯一 kernel page-table ordinary lock；VirtIO fixed DMA storage 只能在 adapter
  初始化、descriptor publication 之前取得该锁并一次缓存物理 segments。steady-state submission、
  hardirq 与 kernel SSIP 不得再次取得该锁或逐页 translation；禁止保留 runtime slice fallback。
- invalid→valid publication 与权限增加只提交当前 CPU 的 translation fence；valid→invalid、
  权限收紧与物理 frame replacement 必须提交 local fence，并同步所有其他 online/possible CPU。
  单个 commit 的合并跨度不超过 64 页时保留精确 range，超过 64 页时由同一
  `TranslationCommit` 规范化为一次 full local/remote fence；禁止把稀疏跨度逐页执行。
  lazy VMA 未写 leaf PTE，fence 数必须为零。AArch64 动态 TTBR1 kernel-stack range 是唯一
  local-strengthening 例外：retirement 必须用一次 `VMALLE1` 越过 Apple HVF 的本 CPU flush
  point，再释放 backing；仅逐页 `VAAE1` 会让复用的 stack VA 写入已归还并重新分配的旧物理页。
- huge leaf revoke 必须记录完整 leaf span；只允许从 leaf-aligned virtual page 撤销，禁止把中间 4KiB unmap 静默扩大到整个 huge leaf。
- fixed RISC-V Privileged 规范允许 publication/relax 暂时命中旧 invalid/restrictive translation；对应 page fault 必须先执行当前 CPU range fence 再重试。地址空间 activation 的 full local fence 不得作为 mutation 兼容路径。
- AArch64 remote revoke 必须由 source 执行 full `VMALLE1IS`，随后向每颗目标 vCPU 发布
  generation 并发送 SGI；handler 在 EOI 后以 barrier 越过 HVF flush point，再以 Release
  发布 completion，retirement owner 取得所有 Acquire ack 后才能释放 frame/page table。
  Apple HVF 的 broadcast 指令返回本身不能证明其他 vCPU 已丢弃旧 TTBR0/TTBR1
  translation，而在目标 SGI handler 内执行 local TLBI 会卡死低地址异常向量。VBAR
  必须永久指向 TTBR1 high vector，user return 只切换 TTBR0；缺少
  broadcast+ack 会让复用后的 kernel-stack frame 仍被旧 translation 写入，或让迁移后的
  task 命中 fork/COW 前的 writable translation。空 target 才允许 no-op；并发 caller 必须
  主动消费自身 pending request，避免双方等待。持有 `KERNEL_SPACE` ordinary lock 时不得
  等 remote ack：kernel-stack retirement 必须锁内摘除 PTE/保活 owner，解锁后再同步，
  否则 IRQ-masked target 等待同一锁会形成死锁。instruction publication 按 CTR_EL0
  IDC/DIC 选择零维护快路径或精确 DC/IC range，两条路径都必须在返回前完成 architecture
  ordering。
- address-space retirement 是唯一 full remote fence 例外：完整 `MemorySet` owner 必须保活到全部 CPU fence 完成，随后才能退休 Context 并释放 page-table/frame owner。
- executable mapping publication 或权限首次增加 EXECUTE 必须由 `TranslationCommit` 在 instruction bytes 写完后提交本地 data/`fence.i` 与全部 online remote `FENCE.I`；trap return 不得作为 instruction-cache publication 兼容路径。

## Failure and cleanup

- mmap/shmat 共用 UserMappingRequest 与 MemorySet::prepare_user_mapping；长度校验独立于地址
  选择，零 hint 表示内核选址，不能调用要求非零地址的 exact-range 校验。仅 fixed/fixed-noreplace
  在撤销前验证完整地址范围。fixed replacement 和新 VMA publication 必须处于一次
  AddressSpace MM guard；禁止 syscall 先 munmap 再另取 MM guard。
  backing ceiling、完整地址范围及 replacement 净 RLIMIT usage 在旧 PTE/VMA 撤销前验证。
  与固定 Linux mm/vma.c __mmap_setup/__mmap_new_file_vma 一致，后续安装失败可留下已解除的
  旧 range，但不能泄漏部分新 VMA/PTE；返回的 PreparedUserMapping 在成功和失败均保留旧
  backing 至 guard 外 translation fence。普通 munmap/SHM_REMAP 允许 ELF/Stack，内部 System
  VMA 仍拒绝 user mutation。source clone 只 pin 真实 owner，guard 外释放，不能增加 shm nattch。
- VMA split/protect/unmap 的节点 allocation 必须在首个 structural mutation 前完成；未发布 token 与 PTE rollback 不得改变 `VmaIndexState`。fork/exec 从空 state 逐 node publication 建立独立 totals，不能复制可漂移 cache。
- page-table rollback、TLB invalidation 与 frame release 分开证明；仍被其他 owner 引用的 frame 不得抑制 translation flush。
- COW physical replacement 必须先准备 replacement frame、canonical page/映射 reference 与缺失 table path；mapped leaf
  经 arch revoke_leaf 保留 table owners，完整 remote revoke fence 后才能 make 新 physical identity，
  publication fence 后才换 resident/drop old frame 并提交 dirty/discardable。revoke→make 期间禁止
  其他 leaf mutation 与 allocation；该 seam 只接受 4KiB leaf，不能把 huge leaf 隐式拆分。
- full-overwrite frame 在进入 page table/Arc 前必须完整复制一页；partial source 或复制前 publication
  会把旧 owner 数据暴露给新地址空间，必须 fail-stop，不能退回“先清零再复制”的双轨。
- heap extent 不得经普通 zeroed frame seam 分配；slab free-chain/header publication 与
  direct header publication 只建立 allocator owner，返回 payload 仍是 Rust 定义的
  uninitialized storage。该 seam 禁止用于 user mapping、DMA read buffer 或 partial-init 后
  可读的对象，否则旧物理页内容会泄漏。
- file/device mmap 保留实际 OFD 生命周期，memory 仅通过无回调 `MappedFileOwner` seam 持有资源；
  regular mapping adapter 与 device view 的 split/fork 都不能把 vm_file 降成 dentry。
- 主程序 executable_owner 是 actual open/deny-write 的无回调 MappedFileOwner Arc，from_elf 准备，
  fork 继承，最后 mm 在 translation fence 后释放；解除全部 VMA 不清除它。Any 只允许 task 地址空间
  对同一 Arc 做显式类型检查并取得 path，memory 不回调 FS/task，也不拥有另一份 executable path。
- `mremap` 由 `MemorySet::prepare_user_remap` 在一次 MM lock 内完成，返回 `PreparedUserRemap`（结果加 fence token）；
  先校验与预留（最多四个 VMA 节点和一个退休 Vec），`FIXED` 目标解除是唯一破坏性前置步骤，之后只剩不分配的操作。
  搬迁沿用 munmap 的 left/middle/right 切分，把 middle 连同全部 resident owner 用 `MapArea::rebase` 换到新地址，
  不复制页、不预建 PTE；旧翻译随 fence 在 lock 外同步。`DONTUNMAP` 在旧范围发布新的空匿名 VMA。
  共享匿名的 `page_offset` 与共享文件页范围相对 VMA 起点，rebase 不改它们；增长出的页越过 backing 时缺页返回 BusError。
- munmap 与 brk 收缩使用同一 `PreparedAreaRetirement` owner：MM lock 内完成全部预分配、
  writeback、VMA/PTE 摘除，锁外同步 fence 并析构 file/backend/frame。single token 不分配，
  batch 使用首个 mutation 前预留的 Vec；漏消费或 fence 失败保留全部 backing 并 fail-stop。
- file/device mapping admission 在取得 MM lock 前保留 source pin，失败返回先释放 guard，
  再放开 pin；unpublished VMA rollback 不能在 MM lock 内执行最后一次 OFD 或 device Drop。
- translation retire 必须严格按 `撤销 PTE -> 同步完成全部目标 CPU fence -> 释放 frame/device/writer owner` 提交；fence 失败时必须保留 owner 并 fail-stop，禁止由错误展开提前 Drop。
- 空 table retirement 同样遵循 retire 顺序；active AVL node token 与 frame owner 必须无分配移交给 commit 的 retirement map，OOM rollback 不得再次申请 retention storage。commit 未 fence 或 fence 失败时只能泄漏 table frame，不能让 stale parent PTE
  walk 到复用内存。
- ASID retirement 严格按 `保活完整地址空间 -> local/remote full fence -> Context retired ->
  frame/page-table Drop` 提交；不清当代 bitmap，rollover 统一重建。retired sentinel 只检测
  misuse，不代替 caller root pin；current task 与 IRQ-disabled transfer window 必须保活 root
  到安装硬件 token。activation 返回本次握手选定 scalar，不得事后重读 Context identity；
  两 backend 在 noreturn 前显式 Drop token Arc，否则每次 syscall 泄漏引用。
- Context 换代先清 seen 再发布 identity；旧 activation 的迟到 fetch_or 可跨代。该 CPU 已被
  capture 且 active=0/pending=true，下次必进慢路 full flush，不能因 seen 位跳过复用失效。
  scoped/full local fence 均在 active/hardware installation 前完成，不替代 mapping mutation fence。
- shared-file writer claim 必须覆盖远端 stale writable translation 的完整生命周期：增加写权限在 PTE publication 前 acquire，收紧写权限在 remote fence 完成后 release。
- shared-file resident 缺页恢复若 leaf 已满足访问，必须通过 `TranslationCommit::stale_fault`
  刷新本 CPU 旧翻译后才返回 Handled；`set_flags` 在权限不变时不记录 fence，否则原指令可能反复缺页。
- private reclaim 的独占性由真实 `PageMappingRef` 数量而非临时 Arc pin 判定。fence 后
  release replay 消费映射 reference；只有成功取得 page operation 并清空最后一个 private
  backing 才累计物理回收数。达到 target 后保留其余 record，即使 leaf 已撤销，fault 仍可重建。
- private reclaim 的 round-robin cursor 由 `PrivateReclaimWalk` 唯一拥有，revoke 扫描与 release replay
  共用同一状态机：committed cursor 只推进到最后一个实际扫描页的下一位置（未扫描任何页时保持
  initial），wrap 只改变 probe 位置、不得提交 cursor。否则 replay 走完全部 scanned 页后停在
  `after(last_scanned)`，与已被 wrap 清零的 final cursor 必然 diverge。

## Swap owner 与当前集成边界

- `swap::SWAP_AREAS` 是 activated area 的唯一 registry；有限 32-slot array 只保存已有 Arc。
  activation/drain 由同一 task-only operation lock 串行；正常 reserve/page-in 不取得该锁。
  area 的 `SlotState` 在同一 spinlock 拥有 phase、每页 reference、free bitmap/count 与 cursor；
  bitmap/count 是同锁投影，不保留另一份 slot map。header page、坏页和 bitmap tail 不可分配。
- Linux SWAPSPACE2 v1 header 以固定 4KiB profile 解析，支持两种 endianness、inclusive last page；
  完整验证实际容量和 bad-page 范围，所有 refs/bitmap/path/Arc allocation 在 area publication 前。
- `SwappablePage.state` 是 Resident(frame, optional retained slot)/Swapped(slot)/Discarded 与
  映射总数、private 数、stale 数、epoch/version 的唯一短锁 owner；page operation 是唯一可跨
  I/O 的 TaskMutex。用户 private、共享 COW、shared anonymous、tmpfs/memfd 都通过它迁移，
  不保存每 mm 的第二份 swap backing。普通 disk page cache 不进入该 owner。
- `PageMappingRef` 是真实 mm alias 的 RAII capability；fork 继承原 installed epoch，不强制
  page-in，不为 stale record 发布 leaf。临时 Arc 与 weak registry 不计入 alias 数。COW 只有
  private-origin 页的唯一真实 private alias 可原地放宽，其余先复制再提交 replacement。
- background capability 随 `ReclaimRequest` 传给同一 reclaimer registry；direct reclaim 没有
  swap I/O 权限。`reclaim_address_space` 在 mm guard 内有界选择候选，释放 guard 后迁移；
  MemoryFile/shared-anonymous 的粗粒度 reclaimer 同样先释放内容 map guard。清洁回收仍
  使用原 retirement transaction，clean/swap 在同一轮 cursor 内分类，不拆成两个扫描算法。
- 迁移只 try-lock page operation；持有它时只 try-lock 每个 mm，绝不等待 mm。普通 fault、
  COW、fork、mprotect/user-copy 在 mm 内等待 page operation 后使用当前 identity。
  弱 mm 无法升级但仍有字段待 Drop 时，其真实 alias 仍计数，缺少覆盖必须取消迁移。
  mapping-owner registration 对同一 Arc 幂等，不能靠重复 callback 凑足覆盖数量。
- 换出在同一 page operation 下领取/复用 Active slot，撤销所有 alias leaf、完成全部 CPU
  fence 并退休 writer claim；覆盖数必须等于同一 version 的真实总数。写完完整页后在同一
  state lock 再校验 version/total/count，成功才发布 Swapped 并推进 epoch。OOM/I/O/忙 mm/
  引用变动保持原 Resident 和 epoch；已撤 leaf 按原 record 重建，无第二份回滚 PTE 算法。
- page-in 的 frame、Arc 与完整读取成功才发布 Resident；仍有 stale alias 时保留唯一 slot，
  恢复物理页不改变其他 alias 的 installed epoch。后续换出必须重写完整 Resident 内容，
  Draining slot 不得复用。fault/table preparation 失败保留可重试 backing。
  最后 stale alias 安装或 Drop 时释放 retained slot；discard 同样必须处理该释放，即使
  还有 resident sibling，否则 swapoff 会永久等待。最后 private alias 可在 fence 后清空
  backing；shared-origin 内容由文件/VMA backing 保活，不能按 mm 数为零丢弃。
- canonical page weak registry 的 slot 稳定，锁外准备 AVL node，锁内复检；复用每次最多
  检查八个 tombstone，禁止每次创建全表扫描。registry 不作为 reclaim candidate 准入；
  mm/file owner 才提供候选，避免未发布 ELF leaf 被错误迁移。
- swapoff 的 mm waiter 在 Draining 前准备；禁止新 reserve 后先遍历 canonical page registry
  等待 page operation 并恢复该 area 的物理 backing（包括未映射 tmpfs/anon 内容），再按
  generation-aware MappingRegistry 恢复各 mm 的 logical epoch。PROT_NONE 恢复内容但不
  发布 leaf，已 installed record 不重复 map。append/reuse 与 fork publication 增加 generation，
  改变就重扫逻辑 mm。物理页登记先于 reserve，Draining 封闭目标槽集合；stable page slot
  只需单轮 snapshot，不能等待无关新页的全局 generation 静止。失败恢复 Active 并保留
  剩余引用/lease。Weak 无法升级而字段尚未析构时，
  用预置 TaskEvent 可中断地等待最后 slot 释放；全部归零后移除 area 并显式 close storage lease。
  signal checkpoint 位于 physical page/MM work unit 之间；Interrupted 统一重验 area usage，
  空 area/最后槽退休优先成功，其余恢复 Active 返回 EINTR，已恢复内容与剩余 lease 保留。
  PageOp/MM/mutex handoff 与同步 I/O 仍是单个工作单元内部不可中断等待，不提前释放 DMA 槽。
- `SharedPagePin` 保活 cached frame 或 canonical page operation，PPN/bytes 只在 pin 中读取；
  文件 I/O 与 mmap 不得返回可跨迁移的裸 frame reference。`UserPageStatistics` 从同一 record
  epoch 投影 RSS，VmSwap 只计 private anonymous stale entry，排除 shmem；物理 page-in
  不替其他 alias 增加 RSS。task/procfs 只消费此 projection，不重新构造 residency 判定。
  Resident 的唯一 CachedSlot capability 以 RAII 发放/归还 SwapCached 页计数，临时 Slot/
  PagePin clone 不计数；无独立 backing map/flag。全局 Atomic 仅给 O(1) vmstat 近似投影，
  不承担内容 ordering、不扫描页面。构造只在 frame/IO 成功后的同锁 Resident 提交，退休
  capability 移出 PageState 后 Drop；失败保持旧计数，System proc scope 保持零 mm 扫描。
- storage adapter 仅提供 identity/capacity/原逻辑header/同步4KiB读写、种类投影与显式 close；memory 不认识 BlockNode。
  块层通过同一 DiskClaims.extents 排他占用 swap extent，阻止 overlapping writer/mount/swap；
  cache admission 绕过所有 swap aliases，claim 后锁外逐出旧 cache，失败回滚。
- block/file swap 使用统一用户页 owner；共享页集成纳入双 backend 的单/双 CPU musl
  产品门禁。新增慢路径必须验证生产 owner/failure atomicity；真实设备延迟不设
  CPU 纳秒门槛，slot/registry/canonical pin 成本另作固定规模测量。

- shared write freeze 与 swapoff 复用 generation-aware MappingRegistry walk；registry lock 不跨
  callback。mm 撤销实际共享 leaf W、全 CPU fence 后才 release writer，VMA W 保持不变。
  新 MAP_SHARED W 在 MAP_FIXED replacement 前及 mm admission 判 ETXTBSY；既有 VMA
  mprotect 加 W 可成功，后续 frozen write fault SIGBUS，private/read-only mapping 仍可用。
  freeze 失败/成功 swapoff 恢复 Free 后，resident write fault 与 fork 为新 W leaf acquire claim。

futex access：FutexKey::private 仅验证 native alignment/range，不要求 VMA 或 fault-in。WAIT/CMP_REQUEUE 通过 MemorySet::load_user_u32 单次 aligned atomic load，避免 byte-copy 撕裂；普通 private WAKE 无需 MM guard。两地址访问的
FutexAccessError 保留先解析 source 后验证 target alignment 的顺序。WAKE_OP 的 UserAtomicU32 只借用实际 MM guard 下 U|W page，生存期不得越过 MemorySet 借用；其物理 AtomicU32 RMW 返回旧值，Task 不能保留裸指针/重建 backing key。所有新
unsafe 的范围在 user_access 逐段证明，统一验证待后续执行。
