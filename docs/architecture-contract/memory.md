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
- 共享区页表由共享地址空间唯一拥有，成员 normal 根借用顶层入口；成员持有共享 owner 的 Arc，
  且自身根析构后才释放它。私有区由成员 normal 拥有，private 根借用同一子树；不同成员的私有区不共享。
  `SHARED_START` 按 architecture 提供的根入口粒度对齐，不在通用 VM 编码页表层级。
- 借用时只复制根入口，不分配叶表；成员创建时接入已有子树，首次访问新根分支时在 owner 装页后接入。
  被借用的顶层子表随 owner 存活，空的更低层表仍可在 fence 后回收；不预分配整个服务区的空表。
  architecture 拒绝经借用根修改子树。所有软件 walk/read 也须锁实际 owner，避免与其下级表回收并发。
  不再登记成员或逐成员复制/撤销叶项；全 CPU、全 ASID fence 使共享叶项的撤销对所有根生效。
- zap 按实际页表叶项推进，跳过缺失的中间表覆盖范围；稀疏大 VMAR 不得逐个空页遍历。
  unmap/protect 仍使用同一 TranslationCommit，跳过空洞不能跳过已安装叶项的 fence。
- 地址空间析构先全局 fence，再退休 ASID，再释放页表与映射。ASID 换代和 activation 的握手由 arch owner 保证。
- 设备页表不由 CPU 激活。IOMMU 必须先完成设备侧失效，才消费 DeviceAddressSpace 的 retirement token；
  PMT 随后才能解除 pin。该域的 create/map 只报告页表分配失败，不依赖上层对象的 status codec。

共享/受限子树的寿命参照 [Zircon unified aspace](https://fuchsia.googlesource.com/fuchsia/+/c9e12e24495130db393289d9f948f43b1b81e80e/zircon/kernel/arch/arm64/mmu.cc)。
LiteOS 复用已有地址空间 Arc 保活，不另设逐页引用计数；新分支按需接入，避免预分配全部顶层子表。
Benchmark 决策：叶项只修改一次，撤销不再按成员数遍历；复用页表 static/disassembly 门禁，最终用原基线比较页表内存与多进程缺页成本。

## 受限执行视图

1. 共享进程成员有独立 normal/private 根和独立 ASID context；VMAR、映射、VMO 与页寿命只保留一个 owner。
   normal 可见成员私有区与共享服务区；private 只安装私有区 USER 叶项，supervisor trampoline/context 在两根中均可用。
2. 同一地址空间锁内只修改 normal 拥有的私有用户子树；private 根引用同一页表页，无重复装页/清理。
   supervisor trampoline/context 子树分别拥有，防止与服务区共用根入口时泄露 USER 叶项。
   同一个全 ASID translation commit 覆盖全部引用根后才释放下级表；两个根均退休后才析构其 frame owner。
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

- requested 分块列表是未完成请求的唯一 owner；投递失败在同一树事务中撤销登记，不能留下永不完成的请求。
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
  → AwaitingClean（`OP_WRITEBACK_BEGIN`）→ Clean（`OP_WRITEBACK_END`，存储确认之后）。脏区间查询包含尚未确认的 AwaitingClean，
  读出或写盘失败后仍可重试；内容 owner 串行回写，不允许较旧确认覆盖较新回写。直写后同步进缓存的区间一次
  完成两步。只有 Clean 且未固定的页可被淘汰；缺失 AwaitingClean 时回写读取前被淘汰的页会从存储补回旧内容，修改丢失。
- 淘汰（`vm::vmo::reclaim`）由后台内核线程推进，用户缺页入口保留兜底；只能从无内核锁的 task context 进入。
  仅 try-lock VMO 树；先采集本对象、COW 子孙及 slice 别名的访问位，匹配物理身份，不能把 COW 遮盖页误计到祖先。
  `vmo::mapping` 的登记、撤销和采集共用继承窗口投影；撤销与采集均用显式工作栈，别名深度不消耗内核调用栈。
  truncate 的根撤销范围不能按更新后的 size 裁剪；子孙按各自 COW/slice 窗口求交。任何采集失败保留页框，已清除的位仍须 fence。
  映射 Vec 在树锁保活的事务内移出、准备容量后归还，不持 state spin guard 分配；回收登记为 koid 有序弱索引，IRQ 锁只保护索引，放锁后才取树锁；节点锁外准备，VMO 析构摘除。
  AArch64/RISC-V 用原子 PTE RMW 保留其他硬件状态；clear 后全 CPU/ASID fence，软件 access fault 恢复访问位。
  `address_space::view` 的内核用户地址复制/不缺页 futex 读取也标记 AF，避免 direct-map 访问绕过观测；
  已置位时不做原子写，恢复才本地刷新，无远端等待或分配，统计路径不标记。
  内核直接读取也标记页槽的当前代，fault-around 的相邻预装不算访问；观察到访问的页晋升到最新代，获第二次机会。
  八代滑动窗口只有一个规范原子快照（youngest 与跨度），扫描协调器是唯一写者；页槽保存截断代号。
  只有最老代的完整登记遍历确认队列为空且无锁忙/采集失败，才推进 oldest、复用环槽；不以截断值猜年龄。
  各 VMO 的页块位图/双向链定位一代的干净未固定成员，不扫描其他页；状态变更经 PageList 事务同步更新位图。
  dirty/pin 离队，完成回写或解除最后一个 pin 时以当前代重入；否则页可能携带已被复用的代号。
  一次先取完整页块的候选再轮转该块，不拆分块消耗剩余样本，避免小热点块长期挡住下一块尾部。
  协调器仅 try-lock；每批至多 256 个页样本和 64 个 VMO 登记，页块/登记/代遍历进度跨批次保留。
  每轮记录开始时的 koid 上界；新登记在发表前为空，只接收当前代页，不破坏旧代排空证明。
  最老代有进展就继续优先处理它；旧代锁忙且无进展时也遍历后续活动代，不能由一个忙对象阻止全系统回收。
- 后台线程与用户线程使用同一调度/阻塞/退休机制，以最低公平权重运行，通过 execution task seam 在预算后 yield。
  不在 VM 引入 Thread 依赖或私有 syscall。分配器低/高水位变化只发布 boot CPU deferred 边沿，不持 allocator 锁唤醒或回收。
  正常时等待压力事件，无周期唤醒；压力下分批推进，暂时没有进展时等待 10 ms 重试，让写回和持锁者运行。
  单个 VMO 每批最多 64 个合资格页样本；同批全局预算不足一个完整样本窗口时留给下一批，避免超支或反复跳过块尾。
  该预算约束页候选与对象遍历数；一次候选的别名访问位采集仍须遍历其实际映射，不声明固定耗时。
- 先 zap 本对象、COW 子孙与 slice 的翻译并完成 fence，再释放页框；Dirty/AwaitingClean/pin 页从不淘汰，匿名内存不换出。
  每个 PageSource 必须为缺页补页或 OP_FAIL，不能以“内容已存在”跳过：内存文件只允许重新补入未写过的零页。
  `SYSTEM_GET_EVENT`（system MEMORY 授权）只可等待；原生事件由 `object::event` 拥有，水位只由 frame allocator 拥有。
  首次事件带水位快照，发表后请求一次校准；后续只在 boot CPU deferred observer 投影 MEMORY_PRESSURE/MEMORY_NORMAL，
  不再维护 syscall 层水位镜像，也不依赖“下一次用户缺页”纠正后台回收后的状态。
  Benchmark 决策：复用工作集大于内存的回收质量诊断，比较重复缺页、pager 请求和前台延迟；统一验收前不声称性能收益。
- 回收统计由扫描器按前台/后台批次分别累计样本及 fence 后释放页数；前台压力尝试另计，包括协调器竞争失败。
  MEMORY_STATS 与 vmstat 只读取该 owner，不把后台释放计入前台或把扫描等同释放；Relaxed 独立快照不参与策略或同步。
  每个非空样本批次至多两次原子累加，没有逐页状态、分配或新锁；成本随压力诊断观察，不另设宿主阈值。
- VMO 的已提交页和在途补页请求分别使用 `PageList<T>`：64 槽一块的稀疏数组，块由有序表索引。
  每个 `Option<Page>` 与请求 `Option<Arc<PageRequestCompletion>>` 槽都是 8 字节，不再每页分配 AVL 节点；
  稠密索引为每 64 槽一份块头/树节点；仅 pager 的已提交页块另有 192 字节分代位图/链，摊销约 12 字节/页。
  匿名、bootfs、请求列表不分配分代头和位图；稀疏开销限于实际触及的块，块/分代元数据都在提交前可失败地准备。
  `Page` 非零 u64 唯一保有单页：44 位页号、12 位 pin 计数、2 位回写状态、2 位内容归属，3 位环形访问代号和 1 位 owner 标记。
  不复制该 owner；创建消费单页 FrameTracker，最终 Drop 恢复一次 FrameTracker 归还物理页，移动/COW 折叠不重复计费。
  `FrameTracker::from_raw` 除原有 heap extent 退休外，仅供此最终析构恢复；纯观测不重建 frame owner。
  同页最多同时固定 4095 次，溢出沿既有 pin 不变量 fail-stop，不允许溢出覆盖相邻状态位。页号容量覆盖两个后端支持的物理范围。
- 区间扫描按块遍历，最后一槽移除立即释放块。不可失败提交须先 `reserve_range`/`reserve_for_shifted`：
  `supply_pages` 在移交前预留目标，COW 折叠只为真实可见页预留；预留失败清理空块，既有槽位不变。
  源映射撤销失败也清理未提交空块；`ceiling` 必须跨过准备阶段的空块，不能漏掉后续驻留页/请求。
  补页失败/失效按实际请求的 successor 推进，不扫描稀疏虚拟窗口的每一页；页仍在 zap/fence 完成后才释放。
  Benchmark 决策：扩展既有 snapshot/retirement 诊断到 1024/65536 页的提交与整段撤销，不新增运行入口或时间阈值；
  最终统一运行，页元数据物理占用并入 M0 基线。
- fault-around（`Vmo::with_resolved_around` / `AddressSpace::install_around`）与主页在同一树锁事务内安装：
  回收的 zap 与安装由树锁串行，若释放锁后再装，会留下指向已退休页框的悬空翻译。它只装入 pager 支撑且已驻留的页，
  一律去掉写权限，不覆盖已有翻译，共用主页的 `TranslationCommit`（一次 fence）；页表分配失败只是放弃，
  缺失窗口裁剪时翻译会越过映射边界。Benchmark 决策：读/执行缺页多一次 ≤16 页的驻留查询，
  不改变 PTE/TLBI 的 static/disassembly gate；收益用基线 6 诊断。
- 单页分配与释放先经每 CPU 缓存（`frame_allocator::CpuFrames`，上限 32 页，空时一次补 16 页、满时还一半），
  缓存只在所属 CPU 屏蔽本地 IRQ 时访问，不取全局 allocator 锁。缓存里的页在伙伴系统看来仍是已分配，不参与合并，
  所以：内存压力（`UNDER_PRESSURE`）下停用囤积并直接归还；连续分配失败后先把本地缓存还回再重试一次；
  对外统计把各 CPU 缓存页计入空闲页。补充只用 `Reclaimable` 类，不会借此动用 kernel progress reserve。
  Benchmark 决策：热路径从“每次一把全局 IRQ 锁”变为“本地数组出入”，不新增分配或 Atomic 读-改-写；
  锁争用收益用多核缺页诊断（基线 6）观察。
- 共享零页（`vmo/zero_page.rs`）：没有祖先的根匿名 VMO 的读缺页若该下标尚未提交，翻译指向全局只读零页（由连续 VMO 持有，
  计入既有“连续”类别），不占内存、不清零；写解析、`pin`（DMA）与 `OP_COMMIT` 永远得到真实页。对象置 `zero_mapped`，
  之后首次在某下标提交真实页时撤销该下标的翻译，否则读者继续读到零页而看不到写入。有祖先的对象、pager 对象不使用零页：
  祖先的后到写入必须对读者可见，而祖先的 zap 覆盖不到把零页映射在子对象里的翻译。观测（RSS/PSS）对无已提交页的下标返回空，
  零页翻译因此不计入驻留。Benchmark 决策：读缺页少一次页分配与清零，不新增锁或间接层。
- FaultSource 是单次解析结果：是否等待过 pager，不能作为持久 residency cache。只有成功安装翻译并完成 fence
  的 restricted fault 才进入线程统计。normal mode 服务缺页和失败解析不混入该计数。
- restricted 状态 VMO 在 `RESTRICTED_BIND_STATE` 时固定其页（`RestrictedStateBinding`），`enter`/`exit` 经 direct map
  拷贝寄存器状态，不再每次取 VMO 树锁、查页；固定使用户对该 VMO 的 decommit、快照被拒绝，所以物理页号在绑定期间有效，
  缺失固定时内核会写入已释放的页框。解除绑定时随绑定一起解除固定。Benchmark 决策：删除了每次进出的树锁与查页，
  新增的只有绑定时一次固定，不加基准。
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

原生 `TASK_STATS` 的 mapped bytes 是进程自身 VMAR 的映射字节和；resident bytes 是实际 RAM 用户翻译之和，
不含 MMIO。private/shared 按同一物理页的逻辑虚拟映射次数区分，不按共享映射策略、句柄或 Arc 引用数判断。
scaled shared 逐页除以映射次数，累加 12 位小数后取整；它与 private 之和为 PSS。不同虚拟别名分别计数，
normal/private 的硬件镜像不重复计数；成员的共享服务区归实际共享 owner，不再重复进入每个成员的统计。

`VMAR_RESIDENCY` 返回实际 RAM 驻留区间与内容来源，供轻量 RSS 使用；`VMAR_MAPPING_STATS` 另外查询共享次数，
返回同一归属的驻留区间及每页定点分摊值。区间可在任意页边界拆分，不能把一个映射的平均占用分配到未驻留部分。
两者均要求 INSPECT，不向调用者暴露物理地址，不 fault-in、等待 pager 或提交页面。VMAR 退休返回错误，分配失败向调用者传播，
不能把未知驻留量报告为零或整个虚拟区间。

地址空间先复制 Mapping 引用并释放空间锁，再按 VMO 树锁 → 地址空间锁观察翻译。`vmo::attribution` 每次查询按树构造
临时物理页索引，扫描已有映射登记并排序、二分计数；不新增全局页索引、页热路径 mapcount、锁或原子更新。
树锁保活页框，空间锁保护 live 与页表读取；跨空间 publication/munmap 仍可变化，因此是瞬时观测，不声明全系统原子快照。
共享服务区只观察唯一 owner 的子树，不扫描成员或构造硬件副本并集。
页内容来源由真实 Page owner 给出，沿 COW/slice 偏移与继承上限解析；页表的缺失页不会因 backing 已提交而被计入。

`vmo::creation` 集中构造 VMO 与树；发布 Arc 前登记连续页，构造失败与正常析构共用同一释放路径。

`SYSTEM_INFO(MEMORY_STATS)` 的物理分类由 `vm::vmo::residency` 从唯一页框 owner 维护：
匿名、pager、连续分配、bootfs 四类互斥；COW 祖先、slice、句柄与映射不重复计费。供页只在真正移交到目标时
从原类别转入 pager，多余供页直接释放；快照/隐藏节点折叠只转移 Page，不改变计数。bootfs 由 bootstrap 创建时
标明，后续复制出的服务 ELF/数据页属于匿名类；连续 VMO 按实际 buddy 分配计数，包含请求尾部取整。
`Page` 的内容类别与回写、pin、访问代复用非零 8 字节 owner 槽，不另增逐物理帧元数据；具体位布局见上文页列表契约。

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
TASK_STATS 使用实际用户翻译，不能由物理内容总数反推某个进程的实际驻留。


## 进程映射观察

linuxd 的 `MemorySet::mapping_snapshot` 只在所属 MM 锁内复制有序 `MappingSnapshot`（标量与提供者 Arc）；
不能克隆 Area/Backing：SysV membership 的 clone/drop 会修改 nattch、操作时间和最后操作者。task 在锁外转换
`ProcMapping`，FS 只负责 maps codec 和实际路径投影。快照不是第二份可修改 VMA 索引，不触发内容页缺页。
设备映射分别持有实际文件租约与可选缓冲租约，不用 Vec 混存两种 owner；mmap 入口设置文件身份，设备提供者
设置缓冲寿命；设备文件偏移来自 mmap 输入，独立于 backing 内部偏移，split 按窗口内位移投影。
fork/split 和只读描述都只克隆 Arc，不分配 owner 列表。文件 mapping 由真实 `OpenedFile` 建立，
与原 backend/打开租约一起保活；mmap、ELF source、FUSE passthrough 共用该入口，不从 inode 号、VMO koid 或名称猜路径。内存层只持 `SharedFileMapping`，FS 在自己的 adapter 中
恢复实际打开身份，保留 memory → FS 的依赖边界；重命名/删除与挂载视图由已有路径 owner 投影。

`ProcSource::open_memory_maps` 在目标 exec credential guard 下捕获并授权实际 MM； READ_FSCREDS 沿用
commoncap 与 UserNS 规则，同一 MM 可读，初始域 PERFMON/SYS_ADMIN 遵循固定 Linux `may_access_mm`。
成功 open 仅保存 Weak MM（mmgrab），每次读取 upgrade 临时取得 MM user（mmget_not_zero）；最后 user 退出
返回 EOF，exec 不把旧 fd 重定向到新映像。PID 查找、MM 锁、FS 属性/路径查询和 user-copy 不交叉持锁。
该观察接口只走标准 procfs，没有新增应用 ABI 或 syscall；普通数据访问与缺页路径不增加锁或分配。


共享匿名 mmap 在破坏 MAP_FIXED 旧映射前准备真实 `/dev/zero (deleted)` 文件与原匿名 VMO；文件长度取页对齐的
映射长度。SysV backing 在 IPC registry guard 外由调用方工厂一次准备匿名 MemoryObject 与真实内部 tmpfs 文件，
`SharedMemoryBacking` 同时保活它们；文件 inode 只持同一 MemoryObject，不反向持 backing、segment 或 namespace。
segment/VMA 退休后，单独存在的文件观察引用不阻止 nattch 归零，也不使 shmid/key 继续存在；该 inode/VMO 可以
随文件引用独立退休。文件与 IPC 权限各有 owner，文件名使用创建时的 key，不随 RMID 的 key 撤销改写。

`DataBacking::SharedVmo` 与 `page_cache::vmo_file` 统一消费 inode 外部 owner 导出的内容，覆盖 ext4 RemoteFs
与 SysV 文件；不增加本地缓存、不增加第二个 VMO、不把 SysV 页改为 pager 页，也不改变 SHM_LOCK 的既有语义。
共享匿名与 SysV maps 描述只复制文件身份 Arc 与映射偏移，不克隆附加 membership。普通文件、设备和 SysV 的路径均由真实
OpenedFile 投影，绝不以 shmid/koid 冒充 inode。

linuxd 的 maps/smaps/smaps_rollup 共用同一 READ_FSCREDS 授权与 Weak MM 观察引用。详细统计在 MM 锁内取得 VMA 与原生驻留区间，
按真实交集累加，不要求 Linux VMA 与内核 Mapping 边界完全相同（命名等操作可只拆分 VMA）。匿名提交/COW、文件页、
无存储 pager、共享匿名与 SysV 的分类由真实 backing/provider 提供；statm shared 是文件与共享内存 RSS，非物理共享次数。
每次实际 open 保留一个文本快照，reader TaskMutex 串行读/定位；offset 0 刷新，后续分段读取不重扫或拼接不同快照。
快照只保留文本，不延长 MM、TCB、SysV membership 或内容租约寿命；失败不发布部分文本。
Dirty 沿页 owner 的保留/回写状态投影，不把共享映射的可写权限当成脏页；不可变 bootfs 为 clean。
