# 性能验证范围与成本契约

构建和执行入口统一由[构建与验证](build-and-verify.md)维护。

## 方法与覆盖

稳定热路径使用 blocking microbenchmark：

- release 构建，固定迭代数，先 warmup；
- 每个样本使用 `black_box`，取奇数样本的 median；
- 只设置宽但真实的绝对上限，用于阻止锁、分配、runtime dispatch 或复杂度退化；
- 文档不记录本机测量值；阈值变化必须有实现和环境证据，不能为通过门禁直接放宽。

当前 blocking benchmark 覆盖 span 堆（`span-heap-bench`：小块 alloc+free、跨线程释放、16 B→64 KiB 增长）、timer deadline、AArch64 VA39 index/TLBI operand projection 与
AArch64 semantic PTE encode/decode、production HAL MMIO window 的检查后 read/write、
seccomp disabled/单 filter/八 filter，以及实际 bdev claim owner 的 raw-read cache policy projection
（一个整盘、一个整盘加八个分区，沿用每项 200 ns/op 上限）。claim gate 只衡量
无竞争 tree/lock CPU 成本，不声称跨 CPU 竞争、alias eviction 或设备 I/O 延迟。HAL 用
对齐的 host buffer 隔离边界/对齐检查的 CPU 成本，不宣称设备延迟或 IRQ 时序。target-specific 零成本边界还必须通过 release target
build、static architecture fence、symbol 与 disassembly 检查；host wall-clock 不能冒充
target instruction cost。RISC-V 保留 backend 的 PTE 与 trap 性能约束由其 unit、release
static gate 与 disassembly gate 继续负责。

新增 hot path、lock、allocation、codec 或 indirection 时必须明确：加入 blocking benchmark、加入 target static/disassembly gate，或说明为何只需要 diagnostic measurement。
whole-machine latency、boot time 与网络吞吐受宿主抖动影响，只作诊断，不作窄阈值 blocking gate。

桌面用户态库使用各 crate 的 `cargo bench -p canvas -p typeface` 作诊断，不设宿主时间阈值。
单项光栅成本不能推导整帧瓶颈；窗口交互须分别测量应用绘制、像素传输、合成与显示完成，
以实际分辨率、多窗口和缩放 workload 比较，不能由宿主微基准直接宣称 guest FPS。

桌面交付前要在同一架构、CPU 数、内存、输出分辨率及设备配置下比较冷启动后的稳定桌面、窗口空闲、
空白处指针移动、窗口缩放和视频播放。物理内存用 `MemTotal - MemAvailable` 记录，并分别追踪可回收缓存、
内核堆/栈/页表、进程私有页、共享对象及显示缓冲。逐进程 RSS 仅用于定位，不能相加代替物理内存；
未归属的页保留为待核查差额。缓存须有容量与回收边界，新增占用要有命中率或延迟收益。
渲染以相同操作下的重绘次数、传输量和帧时分布验证，分开记应用光栅、缓冲提交、合成及 flip 完成。

音频逐流增益使用 `cargo run --release -p architecture-bench --bin audio-mixer-bench` 诊断生产
Accumulator 的单路/八路 period CPU 成本；host 硬件浮点不代表 linuxd softfloat，故不设置 host 时间阈值。
steady period 不分配、不查 control registry；新增乘法的真实可持续性由 AArch64/HVF 八路、六秒、
零 XRUN 的 workload 和设备 WAV 分析裁决，不声称长时间播放或游戏延迟已经达标。
VMO snapshot/hidden-parent 退休成本由实际生产 owner 的 release host fixture 作诊断：
`cargo test -p kernel-unit --release --test vmo snapshot_retirement_cpu_cost -- --ignored --nocapture`。
覆盖多个存活 sibling 与重叠的暂态分支，以及 1024 页驻留、父进程不写入的反复快照和退休。
物理页内容分类沿用该 fixture 诊断生命周期成本：只在页框提交、归属/脏页状态变更和最后释放时更新 Relaxed 原子计数，
不新增锁、分配或每页额外空间；普通已脏页读写不更新计数。分类完整性由同实例静止后的物理页总量核对，
宿主成本不能代替 SMP 目标上的竞争观测，不单独设时间阈值。
DMA pin 的 slice owner 投影只在实际 slice 上取得树锁，普通 VMO 不增加一次锁获取；
这是登记/解除固定路径的正确性修复，随既有 DMA/pager 生命周期诊断，不为这一分支另设吞吐门槛。完整索引移交或复用 AVL 节点均不分配，
宿主锁/frame adapter 不代表目标 TLB 或调度延迟，不设置时间阈值。

MemoryFile 的稀疏补页复用唯一文件 pager 工作池，每次真实 page request 增加一次 PageSource 调用；
quota lease 与 AVL 节点只在首次文件页提交时分配，普通洞读不分配，连续驻留读写仍批量调用 native copy。
冷补页的 IPC/调度成本由实际 guest 稀疏、映射、配额失败与回收 workload 诊断，不用 host 虚构 native syscall 延迟，
既有原生 VMO/translation 和 regular-write 门槛保持原值。

StorageBudget 新锁只用于页/inode 创建、最后释放及 remount/statfs，不进入已有内容的 read/write；
critical section 仅固定整数运算，无分配/回调。生产 quota 的同类 release fixture 单独测
`production_storage_quota_cpu_cost`（`--test storage_budget --ignored --nocapture`），其 host IRQ
adapter 只代表无竞争计数成本，不宣称实际 local IRQ 指令/设备延迟，不新增虚假的窄时序门槛。
硬件光标静态门禁固定 cursor queue 与 latest-position 合并；窗口交互与内核显示/GPU 门禁分别裁决。

Linuxd 的同地址空间复制在驻留页上省去逐块进程内存 syscall。首次访问仍走 VM 缺页与页表发布，
故障边界由 BusyBox 的 memfd 截断和正常读写验证。性能应在 guest 上按驻留和缺页分别观察，
现有显示门禁不以这项改变推断窗口缩放延迟。

显示/GPU 的 guest vblank 性能门禁、画面交接及 Wayland 验证由
[Wayland 与经典控件验证](wayland-ui-verification.md#显示性能与帧交接)维护，沿用固定阈值与完整帧对应关系。

idle tick suppression 不增加 host microbenchmark：收益来自 HVF/TCG 的 whole-machine exit 次数，
host unit loop 无法代表它。改动必须通过双 architecture compile/static gate，并以单 QEMU、完整 SMP
拓扑的多窗口 host CPU 采样作诊断；secondary idle→IPI→task 与 timeout runtime gate 负责活性语义。

资源记账不新增独立锁/atomic：native SchedulingState 同锁投影成功 restricted fault，Policy 提交 normal/restricted CPU 时间。
host 生产 Policy 回归验证 checkpoint/mode 切换恰好计费一次；musl resource-usage.c 验证应用可见投影。
计数边界由 ABI 文档声明，不能用 host 计时替代 trap 往返。

io_uring 不新增 host blocking benchmark：inline regular read 最多暂存 64 KiB，经同一个 VMO tree owner
确认完整驻留后再交付用户数据；暂存有分配与额外复制，不能仅凭绕开 worker 声称吞吐提高。
页缺失、缓存失效、direct I/O、position 争用与 write/fsync 仍使用有界阻塞车道；流式请求由 reactor
按 readiness 推进，不要求应用调整共享 OFD flags。成本边界与能力由实际 guest `io-uring.c` 验证：
缓存命中须在 enter 返回前完成，冷读不能卡住无关请求，多个提交者可同时首次完成与退休。
完整 I/O 延迟与吞吐需要同一 guest workload 比较，不用只测 host key 构造的数值替代；当前不声明磁盘吞吐收益。
lane 停驻复查只在 idle wait publication 后取得已有 Pool 锁，扫描至多八个 worker 的认领状态；
没有新增锁对象、分配或逐字节路径。确定性生产车道 fixture 强制早到通知窗口，guest FUSE/SQE128
验证实际停驻与完成，不以 host 微秒阈值代替调度/IPC 延迟。
链 admission、CQ 发布和等待通知沿用同一 owner；通知 ticket 边界的 CPU 成本由实际 WaitRegistry 的
futex-transactions benchmark 报告，阈值不变。致命 signal 的 generation 复用原 group-exit 的成员遍历与唤醒，
无新增锁对象或分配；mask 更新先取得既有 group-state guard，冻结实际接收集合。
退出状态竞态以真实 SMP fork/pthread/kill/exit_group 回归裁决，不设宿主时序门槛。
原生线程启动闭包只在创建/退出时调用，改由 Descriptor 保活到 TERMINATED，不改变 syscall/trap 热路径，
不新增分配或锁对象；启动 guard 不再跨 body 释放。已有双核 65,536 次 carrier churn 同时验证有界内存和回收，
不以 host 闭包微基准代替实际直接退出路径。

选核轮转不新增 benchmark：每次抢占只多一次本 CPU 私有计数的 Relaxed 读写和一次 Ready/Running 计数读取，选核仍是
O(CPU 数) 的单趟扫描，没有新增锁、分配或跨 CPU 访问；收益是公平性而非延迟，由 `verify-runtime-scheduler` 与
`scheduler-unit` 的策略单测裁决，host 微基准无法代表多 CPU 时间片轮转。

TLB shootdown 不增加 host wall-clock benchmark：成本主要来自 target firmware/IPI，宿主计时不能稳定代表它。
blocking gate 使用 production `TranslationCommit` 派生的确定性计数：
lazy mmap 为 `0 local / 0 remote`；1 MiB、256 页 first-touch 为 `256 local / 0 remote target`；
revoke/replace batch 为 `online_cpus - 1` 个 remote target；合并跨度不超过 64 页时执行精确逐页 fence，
超过 64 页时规范化为 1 次 full fence，防止稀疏 VMA teardown 把跨度页数变成无界指令循环。
RISC-V release target 继续验证 range `SFENCE.VMA` mechanism。AArch64/HVF 是明确的平台例外：
动态 TTBR1 kernel-stack retirement 的本地 31 页 range 固定强化为 1 次 `VMALLE1`，
避免 HVF 保留已经撤销的 global stack translation；
任一 remote revoke 先执行 full `VMALLE1IS`，再向每颗目标 vCPU 发送 SGI；目标以 barrier
越过 hypervisor flush point 并发布 generation ack，owner 在全部 ack 完成前不得释放
frame/page table。kernel-stack retirement 还必须在释放 `KERNEL_SPACE` lock 后等待，
避免 IRQ-masked target 等待同一锁形成环。该路径规避 broadcast 指令返回后其他 vCPU
仍持有 stale TTBR0/TTBR1 translation，不改变 publication/relax 的 local-only 热路径。
成本由 architecture static gate、四 CPU COW migration 与双 CPU kernel-stack churn
runtime gate 裁决，不用失真的 host wall-clock 阈值掩盖。

设备模型与驱动注册的间接层按成本归属分别裁决，不增加 host wall-clock benchmark：

- `DRIVER_IO` completion dispatch 是唯一位于每次块/RNG/声卡 completion 上的新增路径：completion 源存放在
  只追加的 `Once` 槽位，dispatch 无锁遍历、不克隆 `Arc`。`deferred_context_contract` 解析该函数并禁止
  `lock`/`clone`/`cloned`，mutation 测试证明恢复加锁会失败。
- 字符设备读写经一次 `dyn DeviceFile` 调用与 `UserOutput`/`UserInput` 游标：每次 syscall 一次虚调用，
  用户事务数由 `io_copy_cost` 的确定性计数固定（1 MiB `/dev/zero` 一次事务，evdev/DRM 每批 validate 与
  copy 各一次），不需要 wall-clock 阈值。
- 设备类注册表、字符/块设备注册表、mount 事务锁与 `JobControl`/`PipeScheduler` hook 只在 open、
  mount、启动装配或 TTY 访问判定上取得；TTY 判定在改造前已取 process graph lock，新增的只是一次 hook
  虚调用。这些都不在每字节或每 completion 路径上，只作诊断，不增加 blocking gate。
- 设备类 deferred vector 每个已发布 bit 一次函数指针调用，handler 表为只追加的 `Once` 槽位，无锁无分配。
- mmap 的 `MappedFile` 在每次映射建立时分配一次；page fault 是一次 `dyn SharedFileMapping` 调用，进入
  page cache 或 `MemoryFile`（按 `Inode::data_backing` 在建立映射时选定）。
- 私有文件 read/execute fault 保留 canonical 文件页，省去原来的每页私有 frame、VMO 私有页
  分配与 4KiB 复制；首次 write 沿既有 COW copy/fence 路径提交。File resident 只增加页 owner
  的记录，不增加锁、全局状态或逐字节间接层。沿用 translation/COW 成本门禁，
  用实际 memfd/磁盘文件、fork、mprotect 与 truncate workload 验证，不新增墙钟阈值。

inotify 的变更点不增加 benchmark：没有 watch 时每个 `notify_*` 是一次 Relaxed 原子读取；有 watch 时才做 inode
身份查询与一次注册表查找，成本归属于被监视的对象，不在无关文件的 read/write 路径上。

内存型文件（tmpfs、memfd）同样不增加 wall-clock benchmark：

- `MemoryFile` 的 read/write 在每次 syscall 取一次 `state` 锁，页查找是 `FallibleMap` 的 O(log n)；
  它替换的是先前 memfd 的单 mutex 字节缓冲，并去掉 tmpfs 经 page cache 的第二份拷贝，没有新增每字节路径。
  稀疏页、洞读零、短写与截断语义由 host 单元测试确定性裁决。
- tmpfs 目录的名字索引与 cookie 索引各一次 O(log n) 树操作；`getdents` 每项一次额外名字查找。
  树结构的比较次数上界已由 `FallibleMap` cost gate 固定，目录 cookie 的稳定性由 host 测试裁决。
- tmpfs 的 `namespace` 与 inode 状态锁只在创建、删除、rename、link 上取得；read/write 与 mmap 完全不经过它们。

VMA hot path 使用 deterministic structure gate，不增加受宿主调度影响的 wall-clock benchmark。
production `VmaIndexState` transition tests 覆盖 stack grow、split/protect/merge、fork/exec 与
unpublished rollback；AVL neighbor tests 对 16,384 entries 校验 comparison bound。
FallibleMap cost gate 还固定 RV64 iterator 不超过 16 bytes、4,096 项 persistent scan 至多一次
key comparison、随机 `BTreeMap::retain` 模型、successor-link 与 retained node identity；
architecture-check 禁止 production one-shot iterator lookup、fixed path stack、逐节点 retain
join 和 AF_UNIX detach 全图 retain。
architecture fence 要求 100 VMA、1 MiB prepare-user-write 为 0 次 stack 全扫描，hinted mmap 为 0 次 range/RLIMIT/push/merge 全扫描。

UDP/TCP ephemeral allocator 使用 16,384-bit 占用投影；固定 N=1,024 endpoint
样本下，旧 port-range×endpoint 模型为 16,777,216 次 endpoint probe，新模型为
0 次 endpoint probe，耗尽时最多读取 257 个 bitmap word。`port_namespace_cost`
同时阻止恢复 endpoint 扫描、丢失 exact-address 索引或 specific port-0 分类。

未初始化 input staging gate 以两个 64 KiB socket buffer、一个 1 MiB regular-write
buffer 和三个 1,024-fd pselect bitmap 为代表样本：copyin 前死写由 1,180,032 bytes 降为 0；
publication 必须经过同一 `UserInputStaging` initialized-prefix proof，禁止恢复预清零双轨。

PLIC level mask 不增加 benchmark：以一次 priority 写入替代逐 CPU enable bitmap 修改，
unmask 保留原 affinity 装配并写回有效 priority。MMIO 时序不由 host 数值模拟；实际寄存器 owner
回归固定 mask/complete 的 enable 不变量，RISC-V 混合 DMA 冷启动与持续 I/O 裁决活性。

受限硬件视图使成员创建及私有区 first-touch 多分配一份页表，normal/private 的叶项在同一事务内更新；
返回只从已有 ThreadOwner 安全点移交 transient view，最终 token 获取不加锁、不分配、不新增 runtime dispatch。
两根具有独立 ASID，已有全 ASID fence 同时覆盖，撤销不增加第二轮 IPI。成本由原双架构 trap/release
门禁、完整 65536-root churn 与实际桌面帧门禁裁决，不放宽阈值；host fixture 只验证映射和 OOM 退休。


maps 是显式诊断读取，不进入缺页、read/write 数据面或任务管理器既有采样路径：MM 锁内一次有序元数据快照，
只复制标量与 Arc，不调用 VMA membership clone/drop；设备文件/缓冲租约不通过 Vec 克隆分配。
锁外查询 inode 的无 I/O identity 与实际路径，格式化直接追加同一个可失败输出缓冲，不逐行分配临时字符串。
新增只读 observer 使用每次 open 的 Weak MM，无全局索引、驻留 cache 或持久 VMA 镜像。成本由同实例
映射查询诊断观察，不设宿主 wall-clock 阈值；smaps 的页级查询成本另在 M0 完整实现时评估。


共享匿名 mmap 与 SysV 的真实 inode/path 只在对象创建和最后关闭时装配/释放；保活同一匿名 VMO，
不新增 pager，不在页提交、访存、futex 或附加 open/close 路径增加文件操作。只读映射描述复制文件 Arc，
不复制 VMA membership。新增构造成本在计划最终的 mmap/IPC 及启动诊断中观察，不新设宿主时间阈值或专用用例。

VMAR RSS 与 PSS 是按需诊断路径，不在 fault、map/unmap、pager supply 或调度热路径新增 mapcount 状态。
轻量 RSS 只观察目标映射的实际 RAM 翻译与内容来源；详细归属每棵相关 COW 树构建一次临时物理页数组，
排序后以二分取得共享次数，成本随该树实际驻留翻译增长，不扫描空虚拟页。临时空间为树内物理页索引、
当前映射的翻译列表与输出区间；没有持久全 RAM 数组。它不等价于零成本，收益/暂停时间在最终 guest 诊断观察，
不以宿主微秒阈值代替页表/IPC 开销。原生观察器与 smaps 只在请求时执行，基本 maps 不查询 PSS。
proc 每个实际 open 只格式化一次连续读取序列，offset 0 重新观测；分段读取不重复构造页索引。
原生对象查询的布局/容量检查只有固定整数比较，属于现有诊断控制路径，不新增热路径 benchmark。
实施阶段只做 Clippy 和代码审查；运行时采样与完整门禁集中在全部性能计划完成后执行。

块 I/O 准入的 descriptor credits 与 slot 共用既有 RequestOwner/queue guard，不增加锁、全局状态或 uncontended 分配。
空闲 slot 链把旧的 release 线性 contains 扫描改为固定索引操作；contended handoff 成本随实际授予的 waiter 数增长，
所有 scheduler wake 在锁外执行，不分配 wake 批次。该阶段不新增宿主 wall-clock 阈值，I/O backpressure 与延迟
在最终 guest 批量/并发 workload 诊断；已有 request-owner fixture 改为覆盖空槽仍在而 descriptor credits 不足的情况。
块 metadata 从每槽两个 VMO 改为一张固定表，16 槽下分配页数从 32 页减为 1 页（不含 payload）；
初始化预触及的只是一张 metadata 表，不预读设备输出缓冲。提交使用 cached DMA extent 的现有 static gate，
completion gate 同时约束 volatile status read 与 decode 都在 used-length 检查之后。
当前源码已接通登记 VMO、客户端区间直接 DMA 与共享 SQ/CQ；控制 channel 不再传输读写/flush 描述符。
每会话的环与本地 credits 表连接时准备；请求直接写入固定原子记录，服务描述符使用栈内已初始化前缀，无批次描述符 Vec。
客户端的完整 ticket 暂存以 128 项为栈内边界，超出时在发表前一次性分配，受协商容量约束；不按 64 项接收窗口降低单调用者提交深度。
容量 128 的共享区按布局占三页，另有 pending/空闲表；这是并发 I/O 的有界存储，不随历史请求数增长。
控制与 Event 复用弹性池，客户端借当前调用者分派完成；不增加常驻桥接线程。
该变更不增加 host wall-clock 阈值：实际成本来自跨进程 cacheline、通知/等待与设备队列，host 模拟不能代表该链路；
旧逐项 Writer/Reader codec 已删除，固定记录的循环与边界由代码审查覆盖。
最终以既有 guest 顺序/并发文件 I/O、pager、fsync/fsstress 门禁诊断吞吐、延迟、线程数及回收，
特别核对 ring 多次环绕、乱序完成、flush 屏障和断开时在途 DMA 的寿命；运行验收前不声称性能收益。
