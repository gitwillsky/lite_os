# 性能验证范围与成本契约

构建和执行入口统一由[构建与验证](build-and-verify.md)维护。

## 热路径能力与覆盖路由

实现事实由对应领域契约拥有；下表保留原性能工作项的覆盖映射，不另存实现进度或测试命令。

| 工作项 | 当前能力与事实 owner | 验证路径 |
|---|---|---|
| M0 | [物理页归属、RSS/PSS 与服务统计](../architecture-contract/memory.md) | procfs/VMO 用例、musl、同实例内存对账 |
| 1 | [登记 VMO、直接 DMA 与块 SQ/CQ](../architecture-contract/devices-terminal.md) | request-owner、ext4、BusyBox 文件 I/O |
| 2 | [内容 owner 预读与 fault-around](../architecture/memory.md) | VMO、mmap/截断及文件 I/O |
| 3 | [单次定时器与空闲停 tick](../architecture-contract/execution.md) | timer benchmark、超时与 SMP runtime |
| 4 | [分片 futex 与空间/桶锁序](../architecture-contract/execution.md) | 等待事务基准、musl 多线程 |
| 5 / M6 | [共享页表子树与全 ASID 退休](../architecture-contract/memory.md) | execution_views、restricted/ASID runtime |
| 6 / M6 | [分块页列表与压缩页槽](../architecture-contract/memory.md) | VMO COW、pin、pager、回收用例 |
| 7 / M3 | [按需切分 span 堆](../architecture-contract/userspace-abi.md) | span-heap、native_heap、blocking benchmark |
| 8 | [访问位分代回收、后台水位与零页](../architecture-contract/memory.md) | 页列表/淘汰用例、低内存 guest 文件负载 |
| 9 | [ext4 延迟分配与单份内容回写](../architecture-contract/filesystem-storage.md) | ext4/e2fsck、BusyBox fsync/释放空间 |
| P2.1 | [CBS/EDF、准入和标准调度接口](../architecture-contract/execution.md) | scheduler-unit、公平性、音频/显示 |
| P2.2 | [restricted 绑定时固定状态页](../architecture-contract/memory.md) | restricted、musl 与体系结构静态检查 |
| P2.3 | [channel 字节预算与存储复用](../architecture-contract/execution.md) | IPC 状态机、实际文件/音频 RPC |
| P2.4 | [damage、buffer age 与遮挡剔除](../architecture-contract/ui-framework.md) | CPU 像素对账、atomic KMS、GPU cadence |
| P2.5 | [socket 共享 SQ/CQ](../architecture-contract/ipc-network.md) | 12 线程 UDP、TCP/packet、poll/退出 |
| P2.6 | [物理单页 per-CPU 缓存](../architecture-contract/memory.md) | 分配/退休、SMP 与内存压力 |
| M1 | [共享缓冲内区域光栅](../architecture/ui-framework.md) | 1x/2x 像素对账、窗口 resize/maximize |
| M2 | [桌面不可变子表面](../architecture/ui-framework.md) | 桌面图标/墙纸、RFB 取帧与物理内存 |
| M4 | [弹性工作线程及栈尺寸评估](../architecture-contract/execution.md) | 并发 I/O、空闲线程退休、栈写入高水位 |
| M5 | [bootfs 派生 ELF 与文件 VMO](../architecture-contract/boot-platform.md) | 双架构启动、服务装载/监督 |
| M7 | [位压缩字形与共享图集](../architecture-contract/ui-framework.md) | 字体/位图边界、构建安装及实际桌面 |

M4 的评估结论是保留 32 KiB 栈槽；已测写入高水位是下界，不能据此声称 16 KiB 已证明安全。
硬件栈深、完整 HiDPI 交互和所有媒体负载不由单元测试或一份截图外推。

## 方法与覆盖

稳定热路径使用 blocking microbenchmark：

- release 构建，固定迭代数，先 warmup；
- 每个样本使用 `black_box`，取奇数样本的 median；
- 只设置宽但真实的绝对上限，用于阻止锁、分配、runtime dispatch 或复杂度退化；
- blocking 门槛不绑定某台开发机的测量值；诊断数据须附环境与范围，阈值变化须有实现证据，不能为通过门禁直接放宽。

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

网络 RX 的共享环领取按固定窗口摊销短锁与 acquire/release；批 guard 不分配，也不为每个队列新增长期帧缓存。
`tcp-lifecycle-bench` 保留实际 TCP wire/NIC poll，并提供八帧 UDP 批次的每帧诊断成本；后者包含边界 peer 与读取分配，
不作为纯 DMA/解析成本或整机 pps。最终原生批处理图的 blocking CPU/零分配基准及来宾吞吐仍由路由计划完成。
同一基准增加生产 packet-wire::batch 的八帧 L2/L3 头部解析单项，复用既有每操作 CPU 阈值；输入预先构造，测量中不复制 payload 或分配。
RX 调用者提供固定元数据窗口，临时类型化头只借当前 CPU 拥有的帧，退出批次不保留解析缓存；无新增锁、全局或堆对象。
该单项覆盖批内头部机制，不包含 TCP/UDP checksum、FIB/协议状态、DMA 或跨核调度；不能据此宣称最终批处理图或实际吞吐达标。
FIB 与 RIB 使用同一 `prefix-index` Poptrie 机制；算法、事务和字典所有权见[网络契约](../architecture-contract/ipc-network.md#rib-候选目录)。
`tcp-lifecycle-bench` 使用生产 FIB 做 2,048 条与百万条随机查找、四路径带权选择、整批预备/发布/确认、独立下一跳更新及冻结快照。
原 FIB/完整 NET blocking 项仍为 200 ns/op；冷准备、codec、快照与 RIB 管理查询只作诊断，不设置窄时间门限。
来源基准沿实际正文→Source::capture_batch→Catalogue→select_resolved，分开记录编码/验证、捕获、目录、重选与冻结解析；不在产品内采样。

宿主回归的保护边界：

- 原 v4/v6 前缀线性 oracle、区间退休/恢复用例迁入 prefix-index；没有复制第二套产品索引。
- 实际接口/FIB 事务用例覆盖逐分配故障、无分配发布/撤销、来源隔离、最终地址/MTU/网关/显式源准入及热拔出。
  成员失效的临时位图只在需要时申请，退休根在 NET guard 外释放；大量撤回先预备压缩容量，失败保持原配置。
- 既有网关/多路径用例覆盖重复 alias、精确重放零分配、加权比例/同流稳定、实际 UDP 出口与源地址及 DOWN→UP 不复活。
  具名对象更新只交换槽，不重编译百万前缀；独立更新的分配故障、引用范围计数与零引用配置寿命按原事务验证。
- 快照 codec 与真实 route_netlink 用例覆盖全部成员、精确权重约分、on-link/网关、截断/交叉引用/包尾、窗口边界和超过 16 KiB 消息；
  UAPI 权重/长度溢出明确报错。接收先预备容量再推进游标，锁外退休与重取锁核对按代码审查确认，不新增生产采样或并发状态。
- 复合正文用例覆盖保持/清除、接口/FIB 两段、截断与溢出；共用一个最终索引和选择器，邻接草稿只随接口数增长。
  MTU 配置往返/默认/设备匹配沿已有协议/策略用例，DHCP 保留/替换与能力寿命按代码审查确认；没有另设每包基准。
- RIB 候选用例覆盖来源替换/撤销、旧快照、字典压缩、跨来源偏好与同源代价、v4/v6 随机线性对照。
  来源 codec 用例覆盖双栈/隐藏值/截断/引用，捕获联动用例覆盖路径比例/顺序去重、重复目标拒绝与地址族归属；既有 FIB 不覆盖这些信任边界。
  解析回归覆盖双栈深链、环路、精确权重与组寿命；重选覆盖备用破环/依赖恢复、自捕获/drop、来源撤回与接口恢复。
  聚合被 /32 遮住的用例以候选访问次数限定线性工作量，防止每个无关聚合都重编译全图；RIB→NET 沿原夹具验证共享出口、终止动作、发布/撤销/清除及编码拒绝。

成本和证明边界：

- FIB 常驻 Entry 的当前编译器布局为 64 bytes，百万属性数组约 61 MiB；RIB IPv4 来源记录 16 bytes，目录引用八字节/条。
  这些数值不含容量余量、Poptrie、路径字典、原/候选两代或 RPC/VMO 正文，不是完整峰值内存或来宾 RSS。
- Source 只捕获不可变整来源，按完整目标排序/GCD 约分后去重；没有逐路由 Arc、成员副本、实时接口状态缓存或新增每包锁。
  来源映射/解析工作栈、无环 memo、重选位图/字典可用性和组去重表均为临时状态；冻结结果只存每前缀 u32 行号及来源字典结果引用，敏感前缀试探/有环子图最坏成本和增量更新尚未验收。
- 编码只预留最终正文；RIB 只转换共享组并按需投影前缀，不收集逐行 DTO；借用解析线性验证，组查询不再扫描整组，VMO 调用前释放编码副本。
  VMO 创建/捕获仍复制正文，传输时延和峰值占用不由 host codec 用例证明。
- FIB 冷捕获/编译仍持 NET guard；快照编码后在 guard 外验证/建组索引，捕获加验证总时间不能当作持锁时间。
  设备退出才过滤/重计，普通发布/撤销不分配；空表释放索引/数组，零引用具名定义按配置寿命保留。

当前 macOS/AArch64 release host 的一次完整诊断记录（同进程先完成 FIB，再测来源/RIB）：

| 生产路径 | 实测 | 范围 |
|---|---|---|
| 百万 FIB 随机查询 / 四路径查询 | 189.50 / 190.25 ns/op | 原 200 ns 门限；2,048 条约 29.04 ns/op |
| 完整 NET poll | 26.02 ns/handle | 原门限；不是端到端包吞吐 |
| 百万 FIB 集合预备 / 激活确认 | 81.3 ms / 3.04 µs | 冷路径；不含 RPC/VMO |
| 独立共享下一跳更新预备 / 激活确认 | 2.75 / 0.83 µs | 不复制或重编译前缀 |
| 冻结快照捕获加验证 | 17.9 ms | 38,078,236 bytes、3 组；还含原 2,048 条路由，不含 VMO |
| 百万来源编码加验证 / 捕获 | 4.0 / 2.7 ms | 正文 13,000,047 bytes，类型化输入构造不计时 |
| 百万 RIB 目录预备 / 查询 | 13.8 ms / 109.71 ns/op | 来源已捕获，单属性/单下一跳；不是服务收敛 |
| 百万选路集重选解析 / 已解析视图查询 | 2.9 ms / 131.64 ns/op | 正常快路径，共用一个直接组；不代表递归图/环路最坏成本 |
| 百万选路集到原生 FIB 编码加验证 | 21.9 ms | 正文 42,000,058 bytes；只转换一个共享组，不收集百万 DTO，不含 VMO |
| 百万条共享下一跳同时失效的重选 | 8.3 ms | 无备用、无递归依赖；按对象批量暂退，最终零选择/零结果组 |

以上为宿主诊断，历史样本存在超阈值抖动，日志保留且未改阈值；并行负载下壁钟变化不能独立证明性能因果，整机与目标硬件仍须验收。
这些不替代真实 RPC/VMO、百万 IPv4/25 万 IPv6 的内存/更新/吞吐、RCU、per-VRF 与整机验收；阶段 3/6 仍未完成。

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
retain 结构 gate 覆盖每节点一次 predicate、零 key comparison 和 predicate panic 后的有效空表。
原生接口批次仅在冷配置路径分配候选根、设备依赖与预备 waiter，不新增包处理锁/缓存；故障夹具覆盖预备 OOM 和发布/撤销零分配。
大规模配置持锁时长仍属整机诊断；不把这些结构/寿命检查当成百万路由提交时延或吞吐验收。
配置提交与原文件补偿只在管理/故障路径申请空间；同一 Port 仅在有实际待处理工作时按截止重试，空闲不轮询。
存储能力故障夹具覆盖替换前后失败、撤销首次创建、撤销结果不确定和原字节未变时不重写；这些是生产补偿逻辑的故障边界，
不等于真实 FUSE/ext4 崩溃一致性或整机配置延迟验收，故不另设 host wall-clock microbenchmark。
配置 JSON 编解码与按需 Schema 生成复用 no_std + alloc 的 Serde/Schemars，只在冷管理路径分配，不引入缓存、全局状态或包处理间接层。
解除 libc 列表上限后，搜索域验证用有序集合检查大小写无关重复，避免长列表二次扫描；配置名和设备事实匹配不缓存第二份绑定。
现有协议/配置夹具覆盖历史 1–5 迁移、仅名称策略继承、重复键拒绝及超出 libc 容量的完整列表往返，不增设耗时基准；最终管理延迟和内存仍待整机诊断。
整份替换先编译全部接口再以一份 NET 批次发布，临时 DHCP 容器和批描述在激活前预留；健康网络的名称/DNS 修改不重建网络。
DHCP/禁用模式下未生效的静态输入不参与网络重建判断；整批确认失败共用截止与一次唤醒，重试合并所有待应用接口，通知/批容器 OOM 不产生过期截止忙转。
JSON 对比使用相同类型序列化的值树，递归对象、整项比较有序数组，变更记录借用前后值而不复制每份子树；属于冷审阅路径，未新增包处理分配或缓存。
扩展配置用例检查 JSON Pointer 转义、null/缺失与列表顺序，NET 整批撤销和存储补偿沿用既有故障用例；不为冷审阅单独加入时间基准。
大配置正文一律经 VMO/SCM_RIGHTS；控制帧与正文解耦，不因大文档增加 datagram 缓冲。native 独占读取缓冲使用 fallible 容量检查，私有文件不再有 48 KiB 内联上限。
Linux 输入与驱动 ELF 更新复用 RegularFile::snapshot_vmo，以 32 KiB 捕获缓冲控制临时工作集；独立页对象保证源关闭后安全，不能假设底层 pager 的用户态 owner 仍存活。
输出直接把原只读 VMO 包装为 regular OFD，无整份正文的适配层副本。IMPORT 只返回预留的固定确认，提交后不再分配文档。
控制帧用例覆盖 u64 大长度与方法/尾部约束，既有 Linux ancillary 用例扩展正文截断的 fd 退休；后者当前未在真实 Linux 运行，不能替代整机传输/内存诊断。
已发布配置改为不可变 Arc；导出与备份只增引用，在锁外编码/写盘。新 Arc 在持久副作用前准备，不改变唯一配置 owner 或持久决定语义。
自动备份仅增加一个同服务后台线程及一个短状态锁，避免周期写盘占主设备/租约 Port；禁用时无周期唤醒，不增加服务、索引文件或包处理开销。
保留阶段只枚举名字/元数据，不读取每个大备份；确认后释放大捕获缓冲，清理失败不重新创建副本，取消快照在状态锁外释放。
目录 framing 无分配、两处消费者共用同一固定格式检查；私有目录 LOOKUP/OPENDIR 逐项归还。轻量故障用例覆盖已创建但未确认和已删除但回复失败，未执行真实定时/断电或内存基准。

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

RISC-V APLIC/IMSIC 的 IRQ mask/ack 只修改源使能与本地 CSR，不逐 IRQ 发 IPI 或分配；
冷迁移通过 GENMSI marker 与本地 CSR 确认退休。完整成本与寿命边界见[硬件资源契约](../architecture-contract/hardware-resources.md#risc-v-aia)。
MMIO 时序不由 host 数值模拟；每队列路由、late MSI、level 重武装与持续 I/O 活性留待目标机统一验收。

受限硬件视图的 normal/private 根具有独立 ASID，私有区共用唯一 owner 的下级页表，共享区只由共享地址空间装入一次。
成员根只借用对应子树；已有全 ASID fence 覆盖所有别名，退休先完成 fence 再释放 owner。
成本由双架构 trap/release 门禁、65536-root churn 与实际桌面诊断裁决；host fixture 验证映射、隔离与 OOM 退休。


maps 是显式诊断读取，不进入缺页、read/write 数据面或任务管理器既有采样路径：MM 锁内一次有序元数据快照，
只复制标量与 Arc，不调用 VMA membership clone/drop；设备文件/缓冲租约不通过 Vec 克隆分配。
锁外查询 inode 的无 I/O identity 与实际路径，格式化直接追加同一个可失败输出缓冲，不逐行分配临时字符串。
新增只读 observer 使用每次 open 的 Weak MM，无全局索引、驻留 cache 或持久 VMA 镜像。成本由同实例
映射查询诊断观察，不设宿主 wall-clock 阈值；smaps 的页级查询成本见下述按需 RSS/PSS 观测。


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
按需观测的实际成本由同实例任务快照和内存采样记录，不能由 Clippy 或宿主单元测试推断。

桌面子表面的成本在最终同分辨率 M0 对账与桌面门禁比较：墙纸三种布局、纯色、缩放、图标拖动/重命名和 XOR 框选。
不新增宿主时间阈值；实际成本是共享像素驻留、合成器首次导入与子表面提交。细小图案的整数重复块上限 256 KiB，
替代按原始小图尺寸建立大量对象；解码私有像素退休，封印图像只读映射/服务端一次快照分别计账，不用估算声称已低于 100 MiB。
现有 dotted-frame 用例扩展为 1x/2x 奇偶原点的局部/整幅裁剪对账，保护子表面移动后的刷相位；随用户态库单元门禁运行；不由像素对账推断目标帧率。

截止调度新增准入账本只在配置/退休时取锁，wake/切换使用线程原有 Policy 锁，Ready 三组 heap 在线程接纳时预留。
没有常驻轮询线程、每周期分配或新 tick；内存成本为有界队列 backing 和每线程 CBS 状态，不能只计预算算术而忽略它们。
最终复用调度公平性/延迟、音频八路混音及显示节拍门禁，检查额度耗尽、跨周期债务、显式 yield、配置替换、kill 与 fork 重置。
既有 scheduler-unit 模块补一项实际 CBS 数值状态回归，覆盖 revised wakeup 和迟到补充；避免新增 fixture、宿主墙钟阈值或复制参考调度器。
合成/输入主循环 65% 帧周期与 mixer 10% 物理周期是初始配置；最终按无丢帧/无 underrun 和 CPU/RAM 对账校准，不能把准入成功当作实时性已验证。

块 I/O 准入的 descriptor credits 与 slot 共用既有 RequestOwner/queue guard，不增加锁、全局状态或 uncontended 分配。
空闲 slot 链把旧的 release 线性 contains 扫描改为固定索引操作；contended handoff 成本随实际授予的 waiter 数增长，
所有 scheduler wake 在锁外执行，不分配 wake 批次。该阶段不新增宿主 wall-clock 阈值，I/O backpressure 与延迟
在最终 guest 批量/并发 workload 诊断；已有 request-owner fixture 改为覆盖空槽仍在而 descriptor credits 不足的情况。
块 metadata 从每槽两个 VMO 改为一张固定表，16 槽下分配页数从 32 页减为 1 页（不含 payload）；
初始化预触及的只是一张 metadata 表，不预读设备输出缓冲。提交使用 cached DMA extent 的现有 static gate，
completion gate 同时约束 volatile status read 与 decode 都在 used-length 检查之后。
当前源码已接通登记 VMO、客户端区间直接 DMA 与共享 SQ/CQ；控制 channel 不再传输读写/flush 描述符。
每会话的环与本地 credits 表连接时准备；请求直接写入固定原子记录，服务用固定领取窗口非阻塞提交，不再分配批次描述符 Vec。
客户端的完整 ticket 暂存以 128 项为栈内边界，超出时在发表前一次性分配，受协商容量约束；不按 64 项接收窗口降低单调用者提交深度。
容量 128 的共享区按布局占三页，另有 pending/空闲表；这是并发 I/O 的有界存储，不随历史请求数增长。
控制与 Event 复用弹性池，客户端借当前调用者分派完成；不增加常驻桥接线程。
服务池每连接准备 port/停止 Event，每个 worker 创建前准备停止登记；停机广播不分配，空闲收缩会归还登记。
纯控制积压仍直接读取，不新增每条消息的 port/rearm 往返；仅多余空闲读者有收缩截止，保留的读者不再周期唤醒。
该变更不增加 host wall-clock 阈值：实际成本来自跨进程 cacheline、通知/等待与设备队列，host 模拟不能代表该链路；
旧逐项 Writer/Reader codec 已删除，固定记录的循环与边界由代码审查覆盖。
最终以既有 guest 顺序/并发文件 I/O、pager、fsync/fsstress 门禁诊断吞吐、延迟、线程数及回收，
特别核对 ring 多次环绕、乱序完成、flush 屏障和断开时在途 DMA 的寿命；运行验收前不声称性能收益。

块 provider 的完成队列与 caller-ID/缓冲表在连接时按硬件槽数准备；正常非阻塞接收/完成不逐请求分配或扫描在途集合。
冷准入复用已有可失败 FIFO waiter；一个队首只登记一次，grant/取消/失败都由同一个 RequestOwner 结算，不能靠广播重试竞争额度。
每轮最多 256 项提交、64 项完成，硬件完成批按目的队列分组通知，idle 队列不被其他设备 IRQ 唤醒；服务池峰值从八降至两个工作者。
内存代价是有界完成/owner 元数据，收益包含并发线程和同步补页开销；实际常驻/峰值需最终 M0 对账，不用估算冒充测量。
不新增 host 时间阈值：IRQ、port、DMA、SMP cacheline 和设备延迟决定该链路；固定批分组的 CPU 成本与已有 guest 块 I/O 诊断一起观察。
退出同时覆盖未 handoff、已 handoff 未发表、已发表未完成三类 owner；扩展现有 request-owner 用例检查取消和完整额度重用，随现有 request-owner 单元门禁运行。


## UI 辅助任务寿命

PictureLoader 每次只启动一个解码任务，结果入容量一通道后通知并退出；保留引用缓存但不保留空闲线程。
文件对话框复用原有串行队列，在最后一个对话框关闭或创建失败时撤销来源和队列，不在 UI join。
SDK Background 保留 64 项准入和最多两个并发工作者，按需启动、空闲 5 s 全部退出。
队列容量在创建时准备，准入/退休同锁，不在锁内执行用户任务、UI 唤醒或释放被拒任务；没有周期扫描。
Benchmark 决策：这些操作发生于后台工作提交/结束，不进入光栅或输入热路径；不增加 host wall-clock 阈值。
最终沿用真实文件对话框、墙纸切换、应用异步 I/O 与 M0 线程/内存采样，核对首次启动、持续任务吞吐和收缩后再次提交。
代码审查覆盖结果先于通知、EOF 与结果分别处理、句柄退出排空及提交/退休交错；实际 GUI 行为由桌面门禁与同实例操作裁决，不由代码审查推断。


## 共享系统图集

M7 的转换器只消费受版本管理且已校验的 BMP，产物按输入/转换器内容缓存，普通安装不增加 Pillow 或原始 ISO 依赖。
预乘及 16/32/64 尺寸派生只在构建期执行；运行时一次验证索引后 mmap 借用，不逐图分配、解码或缩放。
索引和未触及像素保持文件支撑；只读视图共享页缓存，不预提交整份图集。每位图只增有界的映射 owner/范围元数据。
palette 重着色和外部视频像素仍按实际改变分配，原始下标共享；没有为不可变源建立第二个私有像素副本。
Benchmark 决策：沿用 canvas 实际图像 blit 基线，验证绘制借用没有像素转换/分配；启动收益及物理占用由最终 M0 的 PSS/Private/Cached 对账诊断。
不增加 host wall-clock 阈值。现有位图边界用例扩展索引越界、对齐、尺寸溢出，防止非法元数据形成 u32 视图；随现有位图单元门禁运行，不另建 fixture。
最终安装/冷启动与 1x/2x 的图标、工具栏、地图重着色、登录位图验收仍待计划终态统一执行。


## 原生 channel 存储复用

每对 channel 用已有队列锁移交一份空消息存储，所有分配与用户缺页都在锁外；回收先清空能力，Weak 目标不保活端点。
最多额外保留一条协议容量的空存储，任一端关闭立即移出，归还晚到的消息也不重建缓存；不按历史峰值缓存多份载荷。
在途字节预算计入两个 Vec 容量和固定槽开销，防止句柄密集消息或大容量小长度消息绕过预算；不声称该预算等于 allocator 物理页计费。
CallSlot 每线程最多一份懒分配的 waiter；撤销原 channel membership 后才归还唯一 Arc，执行体退出清空，不随已退出 Thread 句柄残留。
常见串行 RPC 在容量稳定后不再为内核消息缓冲/调用 Arc 分配；并发峰值和增长仍由原有可失败堆分配满足，不增加第二个 codec 或传输入口。
句柄表移出后在锁外统一取消旧 key，覆盖写回失败后部分已发布的句柄被其他线程观察的竞态；不为每条回滚另建取消入口。
句柄编号使用 256 字节栈存储，仍维持 32 KiB 内核栈，最终 M4 栈评估包含此路径。
事务号的空隙证明让常规分配保持 O(1)；边界扫描不分配，跳过仍活跃的双方事务，不按每条调用扫描全部在途项。

Benchmark 决策：保留既有 guest RPC 密集的文件/pager、音频和桌面门禁，最终对照并发、取消/退出后的堆占用与响应延迟；
不新增 host wall-clock 阈值或模拟 channel fixture，host 模型不能覆盖真实 user-copy 缺页、跨 CPU 唤醒和内核堆竞争。
代码审查覆盖回收/关闭交错、提前完成、超时/终止与回复竞态、句柄交付失败及事务号环绕；双架构 Clippy 和现有 IPC 单元门禁覆盖静态及状态机契约；跨服务行为由 runtime gate 裁决。


## 调度计费精度

线程、restricted 执行与 CPU busy 全程使用纳秒；CFS 用预计算权重倒数乘移，不在切换热路径加入除法。
每 CPU 的短 IRQ-safe 计费锁将累计量、活动起点和锁内时钟采样作为一个事务；没有全局锁或新增堆分配。
CPU_BUSY 直接交付已有纳秒快照，删除第二份转换 Vec。代价是远端观察与本核边界间的有界锁竞争，
缺少该事务会让观察混用两代区间，或在提前采样后倒退。Benchmark 决策：最终复用调度公平性、IPC/音频延迟与 CPU 统计诊断，
不以 host 模拟时钟阈值代替 SMP 实际成本；现有 Policy 用例补纳秒短区间和活动值观察，已随 scheduler-unit 运行。
截止时间调度已包含原生 CBS、密度准入、Linux sched_setattr/getattr 与合成器/音频服务接入；计费精度与实时负载验收分别裁决。


## 输出增量合成

损伤在 commit owner 累计到呈现；Scanout 只保存 surface 身份与几何、双缓冲有效性和一帧损伤，不复制或保活像素。
场景比较使用排序 Vec 与二分查询；区域分割复用 Vec，128 块上界约束碎片化。CPU/GPU 共用可见区域，
GPU 使用标准 VirGL scissor 保持原始顶点/UV，不增加 shader 或逐像素 CPU 判定；背景只需一像素纹理。
编码固定至既有 virglrenderer 1.1.0 的 SCISSOR_STATE 与 rasterizer scissor bit；正 viewport Y scale 下由输出 owner 转换坐标。
opaque 来自 XRGB 或标准 region 的保守覆盖证明，不为判断透明度扫描全部像素；透明混合的区域必须不重叠。
Benchmark 决策：最终使用已有 canvas/真实输出交互 workload，对比局部提交、视频覆盖下的拖动/缩放、遮挡与重新露出，
分别记录场景准备、绘制/exec、flip 与内存，不增加宿主墙钟阈值。新增一个直接消费生产 Frame/CPU paint 的像素对账用例是必要的：
旧协议测试无法捕获双缓冲旧内容和透明区域重复混合；参考路径独立整幅绘制，覆盖移动、移除、重排及空损伤。
已有 region 事务用例扩展 opaque 的 pending/current 和减法空洞。上述用例随 compositor 现有单元门禁运行；实际输出仍由图形门禁裁决。
CPU 输出用 DIRTYFB 同步 Frame 的目标修复区，再通过原有同步凭据翻页；不新增 native 协议或驱动事务。
区域超过现有 32-clip UAPI 上限时取外接矩形，最多多传输，不额外分配或建立多次同步状态。
同一像素对账用例增加逐 framebuffer 的设备副本，防止误把前台 damage 当成后缓冲的完整修改集。
成本决策：CPU 路径增加一次标准 ioctl/完成往返，换取局部更新时减少 DMA 字节；沿现有纯 2D 输出门禁诊断，
全屏视频或首帧未声称更快。KMS FB_DAMAGE_CLIPS/GPU 区域 flush 的实现与成本见下文，不以代码检查替代实际 GPU/画面验收。


## KMS property blob 所有权

blob 在 CREATE 时复制一次不可变载荷，ID 与两个索引节点先准备、copyout 后无分配发布；GET 不复制到内部暂存，直接 user-copy。
复用现有 device/OFD 锁，不新增全局锁/池；弱索引随最后引用退休，close/失败不分配清理列表。空表只有固定元数据。
Benchmark 决策：blob 已用于逐帧 atomic damage；在既有显示 workload 观察每帧载荷分配/退休及区域提交延迟，不增加脱离显示硬件的 host 微基准。
代码审查覆盖两个发布阶段之间的 DESTROY、分配/用户写回失败、查询/销毁并发和 OFD 关闭；不新增模拟生命周期 fixture，实际 ioctl 纳入最终 DRM 门禁。


KMS plane/property 查询只在显示初始化/协商发生：静态 schema 不逐对象分配，当前值从原有 scanout owner 读取。
UNIVERSAL_PLANES 只用 OFD 已有锁保存一个标准能力位，无新增全局锁；动态 mode object ID 与静态范围统一，避免类型间碰撞。
user wrapper 按标准两次查询准备有限于实际 count 的数组，列表增长重试，初始化完成后不保留第二份内核可写状态。
成本决策：不为 cold query 新建 benchmark；atomic 初始化阶段查询一次属性 ID，稳态不按名字扫描 schema。
既有 drm-flip-probe 在其 GPU 与纯 2D 的 fdinfo 阶段加入实际 ioctl 校验，覆盖布局/计数、类型、OFD 能力隔离、blob 的跨 OFD 读取和显式销毁；运行归于现有 DRM 显示门禁，不以源码编译代替 ioctl 验收。
同一探针继续覆盖 atomic opt-in/隔离、TEST_ONLY 不发表、inactive MODE_ID 跨 DESTROY 保活与后继退休；GPU 节拍改用产品实际 atomic flip。
标准 framebuffer/blob 的真实 ioctl/fence/关闭语义无法由 host 审查完全证明，因此扩展既有 fixture，不新增模拟状态机或独立实例；实际运行结果由显示门禁记录。


KMS framebuffer 发布时增加一个 fallible Arc control block，扫描事务/active 共用该不可变对象，不另建资源副本或回收线程。
每次翻页的保活是有界引用操作；退休在 completion 锁外，随后重取既有锁推进回收，避免延迟到下一次无关 IRQ。
close 先将实际目标移交 reset intent；busy 时继续保活，disable 被接受后移交 pending，直到其 exact completion 才退休。
已有 disable 接管同一引用，不增加重复命令；关闭期间完成的新 scanout 替换待 reset 目标，原目标已不再被硬件引用才可释放。
page-flip event 升级的 OFD 引用同样锁外释放，防止最后引用触发 DrmFile.Drop 重入 completion 锁。
成本决策：沿最终 GPU/纯 2D 翻页节拍与关闭/重启 workload 检查引用及锁交接成本，不新增 host 微基准或弱引用模拟 fixture。
代码审查覆盖发布失败、RMFB、关闭期间 completion 与 event 最后引用交错；双架构 Clippy 覆盖条件编译；退出竞态仍由实际显示门禁裁决。


Atomic KMS 准备期在锁外解析引用；固定 schema 变更表没有每属性堆节点，提交锁内只复制配置与 Arc 引用。
legacy SETCRTC 创建一次模式 blob，重试/翻页复用；应用端请求与 clip Vec 保留容量，逐帧只创建标准不可变 damage blob。
GPU presentation 原命令批直接传递裁剪并集，空损伤只发绑定；CPU 上传缓冲修复集与输出损伤保持各自语义。
成本决策：沿现有 600 帧 GPU cadence 和真实缩放/局部更新 workload 对比 blob 分配、提交锁、绘制与主机刷新；不增加脱离硬件的 blocking 墙钟阈值。
源码审查覆盖 error/TEST_ONLY/close/master 交错，不代替最终像素、fence、内存和帧率验收。


通知区网络/USB 的两个常驻 worker 合并为一个，同一 500 ms 等待驱动原有采样；UI 仍只应用不可变结果，不承担设备/配置 I/O。
组合快照容量一，禁用请求容量一，没有新增锁、通用任务池或逐帧轮询；退出断开唯一请求端后 join，命令错误不阻止 USB 更新。
成本决策：这次删除一份线程栈和一次周期唤醒，采样频率不变；代码审查覆盖队列满/断开/在途命令，不新增微基准或重复实现的测试。
最终线程数、内存与通知区交互随原桌面 workload 统一验收。


M1 区域光栅删除整窗哈希与像素遍历；Nodes 每次可变借用推进一个局部整数版本，Tree 仅在实际待绘制帧比较每节点的已提交版本/几何。
每节点保存小型 Snapshot，没有像素副本、全局表或锁；损伤合并预算 32，相交子树之外跳过控件光栅，新增/非最新缓冲完整初始化。
布局重新放置保守失效全部放置节点；普通计时扫描只为实际视觉变化推进版本，不能让无变化动画破坏局部重绘。
Benchmark 决策：成本从整幅像素扫描转为节点元数据与损伤面积；按原窗口 workload 对比 1x/2x 普通/最大化窗口局部更新、拖动缩放、光栅/提交时间与 Pss，不增加 host 墙钟阈值。
现有 render fixture 的单次列表标签绘制不足以发现旧覆盖漏擦，因此扩展同一 fixture，用生产 Tree 损伤区域与整帧参考逐像素对账。
覆盖内容/焦点/移动/删除与菜单打开关闭，另确认空动画推进不标脏全树；已随生产 render fixture 运行；来宾帧率和内存分别取实际运行数据。


Socket 数据环以现有 netstack/BusyBox ABI workload 验证真实 TCP、UDP、packet 收发与 poll；覆盖 8 槽并发/环绕、
短读写、零长 datagram、截断/PEEK/packet info、服务终止和连接反复创建/关闭。新增共享存储算法的主要风险是
通知交错与载荷复用，Clippy/代码审查不能证明真实调度下的行为。已有 UDP fixture 补充 12 线程共享发送端点、
唯一序号/内容对账、零长与 PEEK/TRUNC，用真实调用者竞争 8 槽 credits。每线程独立接收端且最多一个未收报文，
避免接收端 8 报文槽满时的合法 UDP 丢包被误判为队列故障；不降低并发、不增加产品缓存，不新增测试程序。
Benchmark 决策：记录相同传输量/并发下吞吐、CPU、服务切换与已提交载荷页；不增加脱离 native IPC 的 host 墙钟阈值。
8 槽固定有界，载荷惰性提交且连接退出回收；持久发送暂存只保留最大 256 KiB，稳态元数据不分配。
共享环移除 channel_call/逐次 VMO 及消息 codec，poll 仍等待真实 netstack 结果，不声称零往返或性能已经提高。
EventHub 的 RAII 与精确关联退休沿现有反复 socket/设备打开关闭 workload 核对登记及内存不累积，不另建模拟生命周期测试。


ext4 原始 FUSE 接收复用 service-rpc 的弹性 Pool；RPC/自有 framing 仅增加一个静态枚举分支，原有动态 handler 调用次数不变。
无新增热路径分配、等待桥接线程或状态锁。用既有文件密集 workload 比较每挂载空闲/峰值线程数、请求延迟和 CPU，
不新增脱离 native channel 的 host 阈值。会话寿命 guard 用于等全部 handler 退出再 shutdown，原 fixed-worker 的首退即 shutdown 路径删除；
创建失败、channel 关闭、卸载与在途请求沿现有 ext4/FUSE 门禁最终验收。

可执行页的发布沿实际物理范围清理数据缓存，提交只合并一次指令同步，不构造物理页包围区间。
原有 execution_views 夹具用显式 cache adapter 验证相隔近 2 GiB 的三页只维护这三页；替换原先错误要求包围区间合并的断言。
成本随实际页数增长，无范围表、分配或新增常驻状态；实际收益通过同一 UTM 实例的 SSH 冷连接和并发 fork/文件负载作前后对照。

同实例诊断（AArch64/HVF，11 CPU，2 GiB，3008×1692，USB 直通关闭）：四路各 12 轮
BusyBox exec、16 KiB 文件创建/读校验、最终 sync 和任务快照，稀疏指令发布修复前后为 195.432/3.280 s；
SSH 新连接执行命令为 0.220 s。该结果不是跨设备通用延迟保证，不设置依赖宿主抖动的 wall-clock 阈值。
桌面与任务管理器打开时，`MemTotal - MemFree`（含可回收页）约 78–80 MiB，
`MemTotal - MemAvailable` 约 61–67 MiB；两种口径均低于 100 MB。compositor/desktop/taskmgr 的 RSS 约 9.2/5.6/8.1 MiB，
包含共享文件与窗口缓冲，不能跨行相加。截图通过原 SSH/RFB 通道取得，无 CUA 注入或独立开发虚拟机。

登记 VMO 的 ext4 数据路径以现有 BusyBox 块/分区/文件/重启持久化 gate 验证；host ext4 用例继续验证
metadata-only journal、崩溃恢复、delalloc 准入与设备写入次数，但不冒充 DMA 运行验收。
预读与回写只固定当前批 slice；读取等待 DETACH 后移交页，写入在 pin 后 BEGIN、DMA 完成及 DETACH 后 END。
普通文件内容不再复制进堆暂存或 journal。ATTACH/DETACH 是物理页代次的寿命开销，I/O 延迟作实际 guest 诊断，
不新增脱离设备的 host 微秒阈值，也不由删除复制推断整个链路已经提速。

最终压力诊断（同一 11 CPU/2 GiB/3008×1692 产品实例，USB 直通关闭）：2.3 GiB 稀疏文件映射，
64 个热页写入已知字节并 msync/fsync，每 256 个冷页重读热集，两轮扫描均验证冷页为零、热页内容一致。
两轮分别为 32.918/3.973 s，major faults 为 31861/27408；这是压力诊断，不与无回收的读盘吞吐混比。
累计前台扫描/释放 1372268/526037 页，后台 330665/174905 页，前台压力尝试 495 次；未出现 OOM、会话退出或 SSH 失联。
此前相同负载在约 1.8 GiB 后耗尽内存；补齐前台高水位节流与关中断调度安全点的远端确认后通过。
清理后桌面及任务管理器仍存活，Dirty/Writeback 为零；物理已分配 67.75 MiB，扣除可回收缓存后 61.54 MiB。
linuxd 服务真实专用内存 10.3 MiB，不再以零掩盖共享服务区归属。视觉通过 SSH/RFB 取帧确认，任务管理器显示 61.6 MiB。
该负载验证压力活性、数据保留及回收计数，不保证任意访问模式下的工作集最优性或跨设备延迟。

性能计划各项实现已迁入上述 owner；最终统一验证已覆盖主机单元、阻塞基准、双架构静态检查及现有来宾门禁。
最后数据路径修改另通过双架构 Clippy、VMO 19 项、ext4 70 项、BusyBox 块/分区/重启持久化与同一产品压力诊断。
保留 32 KiB 内核栈的测量依据、硬件/协议能力限制与未覆盖的独立 SDK 验收，不据此宣称整套系统无待办。

最大化诊断（同一分辨率、同一 taskmgr 进程）：最大化 RSS 21.1 MiB，其中窗口共享像素约 18.8 MiB；
还原后 RSS 7.6 MiB、像素约 5.3 MiB，匿名页两态均约 792 KiB，稳定状态均只有一个 classic-ui memfd 映射。
尺寸导致的增长由当前整窗 32-bit 位图面积解释，不把单份缓冲误报为三份副本，也不以此断言 GPU/分层渲染已完成。
