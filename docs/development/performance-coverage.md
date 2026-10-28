# 性能验证范围与成本契约

构建和执行入口统一由[构建与验证](build-and-verify.md)维护。

## 方法与覆盖

稳定热路径使用 blocking microbenchmark：

- release 构建，固定迭代数，先 warmup；
- 每个样本使用 `black_box`，取奇数样本的 median；
- 只设置宽但真实的绝对上限，用于阻止锁、分配、runtime dispatch 或复杂度退化；
- 文档不记录本机测量值；阈值变化必须有实现和环境证据，不能为通过门禁直接放宽。

当前 blocking benchmark 覆盖 timer deadline、AArch64 VA39 index/TLBI operand projection 与
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

音频逐流增益使用 `cargo run --release -p architecture-bench --bin audio-mixer-bench` 诊断生产
Accumulator 的单路/八路 period CPU 成本；host 硬件浮点不代表 kernel softfloat，故不设置 host 时间阈值。
steady period 不分配、不查 control registry；新增乘法的真实可持续性由 AArch64/HVF 八路、六秒、
零 XRUN 的 workload 和设备 WAV 分析裁决，不声称长时间或 Wine 游戏延迟已经达标。
swap 的 area/slot CPU 成本以生产模块的 release host fixture 单独诊断：
`cargo test -p kernel-unit --release --test fork_file_eof production_swap_slot_cpu_cost -- --ignored --nocapture`。
固定 1/32 area、100,000 次预热和五个样本中位数，测 reserve/retire、clone/retire 与 statistics；
不代表设备 I/O 或竞争，不沿用 syscall 整数投影的纳秒门槛。32 area publication 上限和有界 scan
由生产结构与功能回归约束。canonical resident pin 与 weak page registry 的成本使用
`cargo test -p kernel-unit --release --test fork_file_eof canonical_page_pin_and_bounded_registry_cpu_cost -- --ignored --nocapture`
单独诊断：pin固定10万次预热、五次中位数；registry保留512/4096活页，每批512个新owner，
物理输入在计时前准备。host TaskMutex/物理分配 seam 不代表 native 调度或设备 I/O；
每次最多八个 tombstone probes 的结构界限与故障回归约束复杂度，不放宽时序门槛。
System proc projection 的 resident 遍历从全部页降为零，
通过 scope guard 保证；不引入并行 RSS cache，guest 压力回归验证统计与回收行为。
StorageBudget 新锁只用于页/inode 创建、最后释放及 remount/statfs，不进入已有内容的 read/write；
critical section 仅固定整数运算，无分配/回调。生产 quota 的同类 release fixture 单独测
`production_storage_quota_cpu_cost`（`--test storage_budget --ignored --nocapture`），其 host IRQ
adapter 只代表无竞争计数成本，不宣称实际 local IRQ 指令/设备延迟，不新增虚假的窄时序门槛。
硬件光标静态门禁固定 cursor queue 与 latest-position 合并；窗口交互与内核显示/GPU 门禁分别裁决。

显示/GPU 的 guest vblank 性能门禁、画面交接及 Wayland 验证由
[Wayland 与经典控件验证](wayland-ui-verification.md#显示性能与帧交接)维护，沿用固定阈值与完整帧对应关系。

idle tick suppression 不增加 host microbenchmark：收益来自 HVF/TCG 的 whole-machine exit 次数，
host unit loop 无法代表它。改动必须通过双 architecture compile/static gate，并以单 QEMU、完整 SMP
拓扑的多窗口 host CPU 采样作诊断；secondary idle→IPI→task 与 timeout runtime gate 负责活性语义。

资源使用记账（`getrusage`）不新增 benchmark：用户态 trap 返回多一次 counter 读和一次本 CPU 私有 Relaxed 存储，
缺页多一次 Relaxed 读和一次原子加，上下文切换多两次原子加，tick 多一次原子加，都没有新增锁或分配。
host 微基准无法代表 trap 往返；user/system 拆分的纯算术由 `scheduler-unit` 单测，
端到端由 musl smoke 的 `resource-usage.c` 裁决。

io_uring 当前切片不新增 blocking benchmark：同步路径是对共享页的有界内存循环加一次无竞争的 `TaskMutex` 快路径，CQE 发布是一次 `spin::Mutex` 内的
16 字节写与一次 release 存储，稳态没有分配（溢出链表只在 CQ 满时增长，且有硬上限）。读写类 opcode 的成本由已有的 `read(2)`/`write(2)` 核心决定，新增部分只有
一次 NOWAIT 尝试。阻塞路径每次唤醒重新登记一次 `wait_for_poll`（O(挂起请求数) 个 key），挂起数量大时这是线性成本，但它只在请求真的阻塞时发生，
host key 构造微基准无法衡量批量提交的 syscall 摊销或块层异步重叠；blocking benchmark 需要可比较的实际 I/O 工作负载。
每条挂起的链是一个 `Box`ed future（fallible 分配）：不会等待的 op（普通文件、显式偏移、fsync、close、cancel、NOP）直接同步完成、不分配；
会等待的 op（pipe/socket 流式读写、poll、timeout）无论是否真的阻塞都分配一次 future，因为 future 必须在首次 poll 之前就位于堆上（poll 过的 future 不能搬动）。
持久文件读的新增成本：page cache 命中路径只多一次 `storage_epoch` 读取与一次 `NowaitRead` 状态构造（无分配）；
未命中每页多一个 `Box`ed `StorageRead`、一个 `PendingRead` 与预留的 cache 节点和页，这些原本也要分配。阻塞车道每个作业一次 `Box` 与一个结果槽 `Arc`。
正确性由 host 单测与 guest 冷读检查点裁决。
链新增的热路径成本是每个 SQE 一次 `Member` 构造与每条链一次 `Ring::admit`（`spin::Mutex` 内的有序 `Vec` 追加，链完成时 `binary_search` 加 `remove`），
批内 SQE 不分配；提交/完成顺序、溢出、阻塞唤醒、取消、读写与 epoll 通知由 `io-uring.c` 烟测与 `abi.rs` 单测裁决，尚无吞吐测量。

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
- 私有文件 read/execute fault 保留 canonical 文件页，省去原来的每页私有 frame、SwappablePage
  分配与 4KiB 复制；首次 write 沿既有 COW copy/fence 路径提交。File resident 只增加页 owner
  与可选 swap alias 的记录，不增加锁、全局状态或逐字节间接层。沿用 translation/COW 成本门禁，
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
