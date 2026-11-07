# 内存契约

## Owner 与接口

| Owner | 唯一状态与接口 |
|---|---|
| `kernel::memory` | 物理页分配/归还、分配级别、保留 boot 区间与内核堆 |
| `kernel::vm::vmo` | COW 树、页内容、pin、pager 请求/脏页及映射的 Weak 登记 |
| `kernel::vm::address_space` | 每个地址空间的 VMAR 树、页表、Mapping publication 与共享成员 |
| `kernel::vm::translation` | PTE mutation 的 fence 与待退休页表 owner |
| `kernel::vm::device_space` | 设备页表及等待 IOMMU 失效的 retirement token |
| `linuxd::memory::mm` | Linux VMA/heap/stack 与 native 映射的一致事务 |
| `linuxd::fs::page_cache` | 文件 pager、后端读取与回写策略 |

通用 kernel 不解释 mmap flags、RLIMIT、文件偏移、Linux errno 或进程身份。Linux syscall 只解码 UAPI、
校验用户输入并交给 linuxd 的相应 owner。应用不调用 native ABI。

## COW 树与生命周期

1. 同一 COW 树的 topology、已提交页和 pager 请求由一把 task mutex 串行。VMO 内的短锁只承载 Rust
   同步访问；普通状态访问须持树锁，`size` 的单字段投影和 pager 完成观察只取状态短锁。
2. Relation 的强引用只沿子到父；children 和 Mapping registry 只用 Weak。快照把源在原父对象中的
   Weak 登记替换为新隐藏节点，并登记源/快照为该隐藏节点的两个直接子对象，不能遗留旧 topology。
3. 只剩一个存活子对象的隐藏节点可合并进该对象。普通访问折叠直接祖先；快照慢路径遍历祖先，
   使通知退出与映射退休分离时，中间节点同样被回收。Drop 不取树锁，避免树锁内的最后引用析构死锁。
4. 合并按偏移与可见窗口求交；子对象已有页覆盖祖先页。转移页与索引节点不分配，页框身份不变。
   偏移为零且隐藏节点全范围可见时保留较大的页索引，只移交较小索引中的节点；幸存对象的私有页始终优先。
   缩短后的继承窗口不能在重新增长或再次合并时扩大。
5. slice 的 DMA pin 先把完整窗口逐层投影到实际 backing owner；固定与解除固定都作用于它，
   `PinnedRange` 保活该 owner。不能只返回 slice 解析出的物理地址而漏记父对象 pin。
   覆盖、decommit、resize 或 pager supply 必须先撤销对应对象及依赖子对象的翻译并完成全部 fence，
   再释放不再可见的页框。pin 期间不得撤销/替换页；不能冻结仍被 DMA 固定的页。
6. 分配失败不能发布半个 topology：隐藏节点、子节点及 children 容量在移交已提交页与 Relation 前准备。
   快照写保护失败仍须保留所有内容 owner，不能让已有翻译指向已释放页。
7. 祖先链的释放深度恒定：每个常驻快照保留一层隐藏节点，链长随常驻子对象数增长。`Vmo` 的析构逐层取走父引用，
   是最后一个强引用时把父对象移出 `Arc` 再析构，不经 `Relation` 递归释放；递归释放会在批量退出时耗尽内核栈。
   `vm::vmo::tests` 在 64 KiB 栈上释放 4096 层链守住这一点。

COW 生命周期回归执行生产 VMO，覆盖重复快照/退出、有存活兄弟、延迟映射退休、偏移与截断窗口。
宿主适配器只替代物理页、调度等待与 MMU；真实页表、TLB 与承载线程退出交错仍由双架构 runtime gate 裁决。

## 页表、共享成员与退休

- 锁序为 VMO 树 → 地址空间；共享地址空间 → 成员空间。缺页不得持地址空间锁等待树锁。
- Mapping 发布后不可变。`MapOptions` 同时保存 placement、当前权限、rights ceiling 与 allow-faults；
  boot 严格映射要求 backing 窗口有效，linuxd 显式允许超出 backing 的延迟故障，不能以延长共享 VMO
  来伪造 Linux shared mmap/mremap 的 SIGBUS 边界。
- 部分 unmap/protect 准备 replacement 后在空间锁内替换，live 在同锁清除/复查；
  迟到的缺页不能重新安装已退休映射。protect 不得超过创建时的 rights ceiling。
- 页框/页表页的退休顺序固定为：撤销 PTE → local/remote translation fence 完成 → 释放 owner。
  fence 失败必须保活 owner 并 fail-stop；不能用错误展开提前释放。
- 共享区撤销必须传播到全部已登记成员。成员的私有区不跨空间共享页表；硬件身份不作为 logical CPU 索引。
- zap 按实际页表叶项推进，跳过缺失的中间表覆盖范围；稀疏大 VMAR 不得逐个空页遍历。
  unmap/protect 仍使用同一 TranslationCommit，跳过空洞不能跳过已安装叶项的 fence。
- 地址空间析构先全局 fence，再退休 ASID，再释放页表与映射。ASID 换代和 activation 的握手由 arch owner 保证。
- 设备页表不由 CPU 激活。IOMMU 必须先完成设备侧失效，才消费 DeviceAddressSpace 的 retirement token；
  PMT 随后才能解除 pin。该域的 create/map 只报告页表分配失败，不依赖上层对象的 status codec。

## 受限执行视图

1. 共享进程成员有独立 normal/private 根和独立 ASID context；VMAR、映射、VMO 与页寿命只保留一个 owner。
   normal 可见成员私有区与共享服务区；private 只安装私有区 USER 叶项，supervisor trampoline/context 在两根中均可用。
2. 同一地址空间锁内安装、写保护和撤销两个视图；同一个全 ASID translation commit 覆盖两根后才释放页表页。
   private 根不借用 normal 的 table-frame ownership；错误清理不能只撤销其中一根。
3. private 缺页先校验私有区边界，不能调用共享区补页。最终返回前由对象 mode 投影一次 ExecutionView，
   thread 不复制模式状态；IRQ mask 后只取得预存根 token，不进入可睡眠锁。restricted enter 仅接受共享进程成员。
4. context 部分映射失败先撤销已发布的叶项、完成 fence，再释放 context frame；地址空间退休先全 CPU fence，
   随后退还两个 ASID。host fixture 执行实际页表/视图/retirement owner；musl guest 验证直接访问服务区为 SIGSEGV。

### 应用内存复制

- Linuxd 在 MM 锁内验证 VMA 边界和权限。当前承载线程映射同一成员地址空间时，经架构叶函数直接复制。
  exec 在发布新 MM 之前登记接班承载线程；交接前及后台线程的远程访问使用原生进程读写接口。
  调试访问保留强制语义，可执行映射写入沿用原生指令翻译撤销。
- 承载线程启动时登记一次共享映像中的恢复区间和入口。普通缺页仍由 VM 处理并重试。
  仅未解决的 normal-mode 数据异常同时命中登记 PC 与成员私有地址区间时转向恢复入口，
  由 linuxd 返回 EFAULT。其他异常继续使用既有分派。登记随线程退休，无额外锁或逐次 syscall。
- 例程在 Rust 引用之外读写应用地址；调用方独占服务缓冲并持有 MM 锁，MM 可在复制期间缺页。
  应用侧的多字节访问不跨页，复制失败可留下已写入前缀，未初始化缓冲仅在完整成功后发布。
  参考[固定 Fuchsia Starnix 的 usercopy](https://fuchsia.googlesource.com/fuchsia/+/da8b4012beb596ba222a55b2fa6ba11336c0fc9b/src/starnix/lib/usercopy/src/lib.rs)
  对指令区间和受限地址的双重判定。LiteOS 由内核恢复 PC，沿用自己的异常分派。

## Pager 与故障信息

- requested map 是未完成请求的唯一 owner；投递失败在同一树事务中撤销登记，不能留下永不完成的请求。
  每次登记拥有一个 Arc 完成记录，树锁内的 supply/FAIL 在摘除登记时以 Release 发布结果；等待者以 Acquire
  观察自己保活的记录，等待时释放树锁。成功供页后合法 decommit、重新请求或下一次 FAIL 不得改变旧请求的结果。
  供页完成不保证页持续驻留；解析循环必须重新校验并在作废后再次请求，不能伪造 EIO/SIGBUS。
  真实 FAIL 只影响该次请求，下一次访问可独立重试。记录在登记与最后等待者均退休后释放；无新锁或全局索引。
  正常投递不增加分配或锁；出口拒绝只在原树事务中回收既有节点，错误路径由实际 VMO fixture 覆盖，不另设吞吐 benchmark。
- supply 复用源页节点，先保证目标与源窗口合法、源页完整且未固定；目标已存在的页保持不变。
  移交前先撤销源对象的映射并完成 fence（两棵树锁全程持有），反序会让重复供页的源页在仍被映射时释放。
- 缩小 VMO 时，越过新末尾的读取请求以“已完成”结束（Zircon `InvalidateReadRequestsLocked`），等待者回到解析循环
  按新大小判定越界；之后重新增长得到新的请求。
- 内容 owner 退休前先 `OP_DETACH`（Zircon `pager_detach_vmo`）：在途请求以 `ERR_IO` 结束，之后的缺页不再请求而直接
  失败，迟到的请求 packet 可以忽略。缺失时仍存活的 VMO 副本或子对象会把请求投给不再处理它的服务，等待者永久睡眠。
- port 出队与交付是一个事务：出队保留该 packet 的队列空位，复制给调用者在队列锁外进行，失败时无分配地放回队首；
  缺失时复制失败会永久丢失 pager 请求 packet。
- dirty/writeback 与写保护遵循同一树/映射契约，回写开始后的新写入重新置脏。页的回写状态为 Clean → Dirty（写入）
  → AwaitingClean（`OP_WRITEBACK_BEGIN`）→ Clean（`OP_WRITEBACK_END`，存储确认之后）；直写后同步进缓存的区间一次
  完成两步。只有 Clean 且未固定的页可被淘汰；缺失 AwaitingClean 时回写读取前被淘汰的页会从存储补回旧内容，修改丢失。
- 淘汰（`vm::vmo::reclaim`）只在用户缺页的异常入口执行：此处不持任何内核锁，树锁只 `try_lock`，被占用的树跳过。
  先 `zap` 本对象、COW 子孙与 slice 子对象的翻译并完成 fence，再释放页框；slice 因此登记在父对象的 `children` 中。
  每个 PageSource 必须为任何缺页请求补页或 `OP_FAIL`，不能以“内容已存在”跳过：tmpfs/memfd 对已计费的页重新补零页
  （内存文件从不回写，可淘汰的只有从未写过的零页）。
  在分配失败的现场回收会在持地址空间锁时再取同一把锁。内存压力事件（`SYSTEM_GET_EVENT`，system 授权 `MEMORY`）
  只可等待，`MEMORY_PRESSURE`/`MEMORY_NORMAL` 互斥置位，由同一入口在回收后切换。Benchmark 决策：无压力时每次缺页
  只多读两个 Relaxed 标志；淘汰的成本只在压力下出现、每次有上限，不另设基准。
- FaultSource 是单次解析结果：是否等待过 pager，不能作为持久 residency cache。只有成功安装翻译并完成 fence
  的 restricted fault 才进入线程统计。normal mode 服务缺页和失败解析不混入该计数。
- restricted fault/pager-wait 计数与既有线程状态同锁更新，无新锁、分配或页级影子索引；native runtime
  只投影该 owner。linuxd 在线程退休时累加，投影 minor/major，删除旧异常层计数，避免重复记账。

## 验证与成本

COW 回收只转移已有页/AVL 节点；祖先遍历位于快照慢路径，普通解析只检查直接父节点。生产 host fixture
提供保留 272 个兄弟的快照/退休 CPU 诊断，并对比 1024 个驻留页中零页或一页被幸存对象写入的场景。
不设宿主纳秒阈值；它不能替代共享 VMAR fence 或 native 调度成本。
缺页记账仅在缺页完成时增加既有短锁中的整数更新，由真实匿名缺页、pager 与资源使用 runtime 检查验证。
pager 完成记录仅在缺页首次登记时增加一次 fallible Arc 分配，合并等待者只克隆引用；驻留读取与
READ_DONT_WAIT 不分配该记录。host fixture 确定性交错 supply/decommit/FAIL/重试，防止旧结果被新请求覆盖。
不另设宿主延迟 benchmark：其等待 adapter 不执行真实阻塞/IPC；成本与容量由原 musl 压力、BusyBox
并发文件及音频/桌面门禁裁决，不放宽截止时间或成功条件。

## 非等待读取

`VMO_READ` 的 `READ_DONT_WAIT` 是系统服务的通用对象机制：树锁争用或 pager 页缺失返回
`ERR_SHOULD_WAIT`，不登记等待者、不发 pager 请求；读取与撤销由同一树锁串行，不能先查询 residency
再另行读取。失败时暂存区前缀可已修改，调用者只有完整成功后才能交付；普通读取仍沿原 pager 路径。
linuxd 的 inline read 使用 64 KiB 暂存预算，无效的 FUSE 属性租期、待作废数据、direct I/O、缺页、
position 争用或更大请求均交给阻塞车道，不复制驻留状态。VMO host 回归覆盖缺页不发请求、补页后读取、
COW 祖先与树锁争用；musl runtime 固定验证真实缓存命中在提交返回前完成。复制成本沿现有 VMO read
路径，新增一次 try-lock 与状态分支；宿主指针替代 MMU 的纳秒值不能代表 native ABI 开销。

Pager 补入的干净页在 read fault 时只授予只读翻译；首次 write fault 才置脏并放开写权限。
writeback begin 先写保护全部别名并完成 fence，再清脏；失败保留 dirty，再读也不能重新授予写权限。此转换与供页/失败在 vmo/pager 复用同一树/页 owner，
没有另一份 dirty bitmap；否则先读后写的硬件访问绕过 dirty tracking，文件回写会遗漏内容。

## 观测口径

原生 `TASK_STATS` 的 mapped bytes 是映射范围之和；resident bytes 是这些范围已有 RAM backing 的字节和，
包含 COW 祖先和 slice，排除 MMIO 物理 VMO。不同虚拟映射可引用同一物理页，故它不是 PSS，不可跨进程相加。
地址空间先复制 Mapping 引用并释放空间锁，再按 VMO 树锁查询 backing，保持 tree → address-space 的锁序；
统计不会 fault-in、等待 pager 或提交页面。稀疏 VMO 按实际 committed entries 合并可见区间，不扫描整个虚拟范围。
分配失败向调用者返回错误，不能把未知驻留量报告为零。


`vmo::creation` 集中构造 VMO 与树；发布 Arc 前登记连续页，构造失败与正常析构共用同一释放路径。

`SYSTEM_INFO(MEMORY_STATS)` 的物理分类由 `vm::vmo::residency` 从唯一页框 owner 维护：
匿名、pager、连续分配、bootfs 四类互斥；COW 祖先、slice、句柄与映射不重复计费。供页只在真正移交到目标时
从原类别转入 pager，多余供页直接释放；快照/隐藏节点折叠只转移 Page，不改变计数。bootfs 由 bootstrap 创建时
标明，后续复制出的服务 ELF/数据页属于匿名类；连续 VMO 按实际 buddy 分配计数，包含请求尾部取整。
`Page` 的一字节内容类别复用 padding，64 位目标下仍为 24 字节，不增加每帧元数据、全局对象索引或扫描锁。

`pager_create_vmo` 的 `PAGER_ANONYMOUS` 由实际内存内容提供者指定；linuxd MemoryFile 设置它，
存储 pager 不设置。两者仍共用同一供页、dirty、防淘汰和退出路径，没有第二套实现。
内容类别随供页转入页框；不能由进程名、路径、权限或对象引用数猜测。这一事实用于区分“必须保留的内存修改”
与“等待存储写回的修改”，不改变配额或释放语义。

pager 的 dirty、writeback、reclaimable 是 pager 页总数的子集；前两项仅含有存储后备的 pager，
匿名供页的修改不进入落盘量。reclaimable 包含两类 pager，但只含 Clean 且 pin=0 的页，
由 dirty/writeback/pin 状态转换和析构维护，与实际回收谓词共用。Relaxed 计数只作观测，不发布内容或翻译；
各字段不是跨 CPU 的一致快照，不能用短暂差额推断泄漏。新计数不替代既有 kernel heap/stack/page-table owner。
查询不分配、不锁树；页首次提交/最终释放各一次原子更新，已脏页再次写入不做原子操作。

linuxd 的 `/proc/meminfo` Cached 投影全系统 pager 驻留页（包含 tmpfs/memfd）；Dirty/Writeback 与
`/proc/vmstat` nr_dirty 只投影其中的存储写回计数，
覆盖 ext4 等独立内容服务；MemAvailable 只加干净且未固定的可淘汰子集，并限制在 MemTotal 内。
删除 linuxd 局部缓存扫描，避免漏计远端文件页和把查询失败当作零。此物理分类不是进程 RSS/PSS：
TASK_STATS 仍遵循上述 backing 口径，不能由这些分类反推某个进程的实际页表驻留。
