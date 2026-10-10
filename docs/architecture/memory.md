# 内存当前架构

## 当前设计

- frame allocator 唯一拥有物理页容量与 buddy metadata；初始化时从 kernel image 后的物理区间
  前缀保留一 byte/frame 状态表，保留页不进入 buddy capacity，因此 metadata 不依赖固定
  bootstrap heap 且可随 guest RAM 扩展。global allocator 从它临时取得未初始化 slab/direct
  extent，不复制容量 owner，也不为 Rust allocator 本就不可读的 payload 产生整段 dead
  zero-fill。用户页与 DMA backing 仍由普通清零 seam 分配。
- `MemorySet` 唯一拥有 page table、program break 与有序 VMA。ELF、stack、anonymous、file、shared/private mapping 使用同一 VMA lifecycle。
- `AddressSpace` 使用 task-context mutex 保护 `MemorySet`；page fault、user-copy 与 procfs
  统计发生竞争时进入 scheduler Blocked/FIFO handoff，不在同 CPU 自旋。每个 AddressSpace
  增加一个 8-byte immutable token projection；只有使用动态 supervisor trap VMA 的 RISC-V
  Thread 在创建时增加一个 pointer-sized retirement preparation，保证 exit/rollback 不在提交后
  分配 waiter。AArch64 context 随 KernelStack 退休，不分配该 waiter。uncontended acquisition
  先命中 `try_lock`，每次 waiter allocation 固定为 0；truncate 每个 transaction 只准备 1 个
  pointer-sized waiter，并在全部 live AddressSpace 间复用，allocation 数不随 owner 数增长。
- generic memory 只提交 READ/WRITE/EXECUTE/USER/GLOBAL/DEVICE 等语义权限；PTE 编码、canonical address、address-space token、direct-map projection 和 local fence 属于 `arch::mmu`。
- user-copy 在 AddressSpace lock 内先完成全范围 fault-in 与权限证明，再复制；不会向 Rust 返回可逃逸的用户 frame reference。
- file mapping range、page-cache resident、private resident、COW 与 futex key 各有单一 owner，OOM 在 publication 前显式返回。
- reclaim 使用有界 cursor 和 fixed batch；页表撤销决定 TLB flush，不能以 frame 最终释放代替 translation invalidation。
  reclaimer/mapping-owner 注册使用稳定 Weak slot；节点准备在 registry lock 外，OOM 不发布，
  dead slot 原地复用不分配，避免 allocator 重入 reclaim 自锁。
  registry 采用现有 FallibleMap owner transaction；lookup 从 O(1) 变为 O(log slot count)，
  direct reclaim 的 64-adapter 固定预算与每 adapter 扫描预算保持不变。正常 frame 分配不查
  registry；该取舍用 allocator 重入/OOM 回归约束，不增加宿主时序阈值。
- leaf mutation 统一经 `TranslationCommit` 分类：publication/permission relax 只做 local translation fence，revoke/restrict/frame replacement 才向其他 online CPU 发 shootdown；lazy mmap 不产生 leaf，因此不 fence。
- page fault publication 每页只产生一次 local fence。以 1 MiB、256 页 first-touch 为确定性指标，
  remote target 总数从 `256 × (online_cpus - 1)` 降为 `0`；revoke batch 的 remote target
  数保持为 `online_cpus - 1`。合并跨度至多 64 页时保留精确 range；更大或稀疏跨度
  固定为 1 次 full fence，避免按最大/最小地址之间的空洞逐页循环。
- VMA AVL 的 ordered neighbors 直接处理 overlap 与 anonymous merge；`VmaIndexState` 随 node transaction 增量维护资源 totals。
  grow-down stack 的 split fragments 与 ELF 均遵循普通 user protect/unmap；stack fault 用有序
  ceiling 找最近的实际 fragment，无唯一栈 key/top 镜像。mmap/shmat fixed replacement 与新
  publication 共用一次 MM guard，先校验净限额，旧资源在 guard 外 fence 后退休。
  100 VMA 下，1 MiB prepare-user-write 的 stack lookup 从 25,600 次节点访问降为 0 次全扫描；hinted mmap 从 5 次、至少 500 次节点访问降为 0 次全扫描，仅保留 O(log VMA) 邻居查找。
- 两个静态 MMU backend 共用 generation ASID owner，startup 探测硬件宽度，boot 按真实
  logical CPU topology 准备 bitmap、active/reserved 与 pending flush。user root 只分配软件
  Context，首次 activation 领取非零硬件 ID；编号耗尽时换代，保留仍在执行或尚未切换
  CPU 的编号，其他编号在该 CPU 完成本地全量失效后复用。kernel 永久使用 ASID0。
  稳定切换通过 generation 检查与 per-CPU active CAS，不加锁、不分配；首次 CPU
  publication 做 scoped fence，换代后首次激活做 full local fence，普通 trampoline 不加 fence。
  地址空间析构仍保活完整 mapping/frame owner，完成所有 online CPU 全量失效后退休
  Context，再释放 root。retirement 不清当代 bitmap，下一轮统一重建，避免提前复用。
- AArch64 使用 4KiB granule、39-bit/3-level translation：TTBR1_EL1 唯一承载高半 kernel/direct map，TTBR0_EL1 承载当前用户地址空间与非零 ASID；PAN 保持开启且不建立 KPTI 双轨。页表按对齐和权限边界选择 1GiB/2MiB block 或 4KiB page，MMIO
  使用 MAIR Device-nGnRnE。
- AArch64 将 TTBR1 高半区静态划为低侧 120 GiB RAM direct map、中间约 135 GiB task
  kernel-stack window 与顶部 1 GiB high-MMIO window；当前 QEMU virt guest RAM 上限约
  119 GiB，超出时在 direct-map publication 前明确失败。平台发布的每个高地址 MMIO
  range 只经 architecture-owned mapping projection 映射一次，不能退回 guessed physical
  pointer 或改变 QEMU high-memory 拓扑。每个 AArch64 mapping 仍保留最低 guard page，
  最高页由 KernelStack 独占为对齐 padding 与 `UserContext`，实际 SP 从该页下边界开始向下
  增长，context 由该边界加固定 offset 直接推导而不保存冗余 pointer；RISC-V 继续使用
  trap-context 下方的既有 Sv39 stack 与 supervisor trap-context VMA layout。
- AArch64 remote TLB retirement 使用 `VMALLE1IS` + 逐 vCPU SGI rendezvous：source 先
  broadcast，每颗目标以 barrier 越过 HVF flush point 并回写 generation completion，
  retirement owner 等待全部 ack 后才释放 frame/page table。Apple HVF 的 broadcast
  指令返回本身不能证明其他 vCPU 已清除旧 TTBR0/TTBR1 translation，而 target-local TLBI
  会卡死低地址异常向量；VBAR 因而永久使用 TTBR1 high vector，user return 不再切换到
  TTBR0 alias。空 target 仍为 no-op，publication/relax 仍只做 local fence。
  kernel-stack PTE 在 `KERNEL_SPACE` 锁内摘除并保活，锁释放后才等待 ack，避免
  IRQ-masked target 等待同一锁。instruction publication 优先使用 CTR_EL0 的 IDC/DIC
  能力，否则执行精确 DC clean/IC invalidate range。
- AArch64 动态 TTBR1 kernel-stack retirement 在本 CPU 把 31 次 range `VAAE1` 强化为一次
  `VMALLE1 + DSB + ISB`，再释放 stack backing。该路径不在 syscall/page-fault 热路径；
  确定性 fence 指令数从 31 降为 1，因此无需另设 microbenchmark，runtime gate 以连续 fork/exit
  后的 stack VA/物理页复用验证不会写坏新 owner。
- leaf unmap 会自底向上摘除空 Sv39 L0/L1 table；单个孤立 mapping 的 active table pages 从 unmap 后 3 降为仅 root 1。摘除的最多两页由 `TranslationCommit` 保活，remote revoke fence 完成后才回到 frame allocator。
- kernel identity mapping 按每个 VMA 的精确权限边界选择最大对齐 Sv39 leaf；128MiB 对齐 physmap 的 leaf PTE 数从 32,768 个 4KiB leaf 降为 64 个 2MiB leaf，translate façade 仍投影逐页 PPN。
- executable leaf publication 与新增 EXECUTE 权限由同一 `TranslationCommit` 记录；instruction bytes 完成后执行本地 data fence/`fence.i` 并同步所有 online remote CPU。普通 trap return 不执行 `fence.i`，后上线 CPU 在 startup 做一次本地初始化 fence。

- 通用后台回收使用同一 resident registry 与安全 writeback/revoke 路径：低水位为容量
  1/64（至少 256 页），高水位为容量 1/32（至少低水位加 256 页），均不超过容量。
  空闲时每 100ms 取一次 allocator 快照；低水位开始后每批最多 64 页、1024 resident scans，
  有进展睡 10ms，无进展退避 100ms，到高水位结束。deadline node 在启动时预留并反复重装，
  经唯一 TimerQueue 与无分配 TaskEvent 阻塞；OOM 不能跳过 sleep 变成 busy loop。不改变 allocation reserve，不增加
  allocator 热路径锁/Atomic/wake；这是采样式压力策略，尚无 Linux kswapd 的分配触发唤醒、
  NUMA/zone/compaction。后台通过单一 canonical page owner 将 private、共享 COW、shared anonymous
  与 tmpfs/memfd 内容换至 active block/file swap；direct reclaim 不执行 swap I/O。
  策略参考固定 Linux mm/vmscan.c 的低/高水位与退避，
  不宣称完整 Linux reclaim policy。性能决策以确定性 batch/scan 上限和 guest 无 allocstall
  的实际回收、数据保持验证，不引入宿主时序阈值。

## Known limits

- tagged ASID profile 要求硬件 capacity 超过两倍 possible CPU 数；更窄硬件在 boot 明确拒绝。
  当前容量为 8/16-bit，CPU 集合最多 native-word 位数；软件 generation 耗尽 fail-stop。
- block/file swap 已接入统一用户页迁移：物理 backing 只有一份，mm record 持独立 installed
  epoch，page-in 不为其他 alias 计 RSS。共享匿名/tmpfs/共享 COW 路径纳入
  AArch64/RISC-V 各 1/2 CPU TCG 的完整 musl 产品门禁。
  swapoff 在 page/MM work unit 之间和最终槽退休等待支持信号中断与保留 lease 的重试。
  ext4 定期写回线程继续拥有 journal aging，通用后台回收复用现有
  安全 dirty-page reclaim，不替代 filesystem durability owner 或提供独立脏页比例控制。

私有驻留页与有效 leaf 是独立状态：有界回收在 fence 后达到目标时可保留其余 resident，
read/execute/write fault 都从原记录恢复映射；fork 对缺失 parent leaf 不执行 set_flags，
mprotect 按实际 leaf 状态更新，并在 destructive mutation 前预留 table paths。
MADV_FREE、clean MAP_PRIVATE 与共享 COW 的写限制由 PrivateResident 一次投影。
COW 换页遵循 break-before-make，原表路径保留到无分配 publication 完成。
fork 先登记空 child 再在两个 mm guard 内填充；复制文件 resident 时按已公布 EOF 过滤整页，
child leaf/PROT_NONE slot 只来自保留的 records，关闭 truncate 等待 parent 的晚登记逃逸窗口。
这些是已有 fault/mprotect 慢路径的正确性修复，不引入正常 copy 页的额外 fence；
性能验证沿用 translation commit 成本门禁，主验证是双 backend 和生产 owner 的失败原子性回归。

完整 user-copy preparation 临时保护本次页范围免于自身回收，避免后续缺页逐出已准备的前页。
该范围只在同一 mm 独占事务中设置/恢复，不另持 frame owners；确无范围外可回收页时返回 OOM。
