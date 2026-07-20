# 构建、测试与验证

本文件唯一维护开发环境、构建入口、缓存规则和质量门禁。文档只定义可重复的输入、覆盖与成功条件，不记录某次执行日期、输出或实测数值。

## 环境

- Rust 版本、组件和 target 由 `rust-toolchain.toml` 固定；精确 revision 见 [规范基线](../standards-baseline.md)。
- `ARCH` 默认 `aarch64`，只接受 `aarch64` 与 `riscv64`；它统一选择 kernel target、Linux userspace target、QEMU、musl loader 与 Alpine repository architecture，未知值在 Make 解析期失败。
- `ACCEL` 默认 `hvf`，只接受 `hvf` 与 `tcg`。`riscv64 + hvf` 在构建前硬失败；RISC-V 必须显式选择 `ACCEL=tcg`，AArch64 的 TCG 诊断路径也必须显式选择，不能从 HVF 静默回退。
- `PROFILE` 默认 `release`，只接受 `release` 与 `debug`；提交门禁以 release 产物为准。
- musl、BusyBox、APK 和 terminal font 输入必须由脚本中的固定 URL、版本与摘要构建；musl、BusyBox 与 APK 都为所选架构生成或下载原生产物，不得静默消费另一架构 cache、系统副本或滚动 latest。
- kernel 位于 `target/<kernel-target>/<profile>/kernel`；可重复 rootfs 基线位于 `target/rootfs/<arch>.img`，开发实例是 `fs-<arch>.img`，只由显式 reset 初始化且不能反向污染基线。musl、BusyBox、APK 与 runtime success cache 均带 architecture identity，不允许跨目标命中。
- `FS_IMAGE_SIZE_MIB` 只控制可写开发实例的最小容量，默认 8192 MiB；`run`、`run-gui` 与
  `run-gdb` 在 QEMU 启动前离线扩容已有实例并保留内容，较大的实例不会被缩容。缺少该扩容会让
  基线派生的 128 MiB 实例在安装 Node.js 等应用时以 `ENOSPC` 失败；runtime gate 仍消费紧凑、
  可复现的只读基线，不继承开发容量。
- AArch64 userspace compiler owner 是含 AArch64 backend 的 Clang driver、固定 Rust toolchain的
  `rust-lld` 与 hard-float AAPCS64 `aarch64-unknown-none` `compiler_builtins`；kernel 独立使用
  `aarch64-unknown-none-softfloat`，两者不得混用。任一 runtime 缺失或歧义都必须在发布 sysroot
  前失败，musl smoke 必须实际验证 `strtod` 返回与 FP arithmetic。RISC-V 保留 GCC 与其 `libgcc` runtime 路径。
- 标准 Rust userspace 由 `scripts/verify_rust_std.py` 使用固定 rust-src 构建；Cargo 禁用 bundled
  musl CRT，动态链接项目 musl，并静态链接同 revision LLVM libunwind。libunwind 是 panic/backtrace
  冷路径且固定 `-O2`；其正确性由双架构 target runtime gate 裁决，不增加失真的 host wall-clock benchmark。
- `verify-runtime-gates` 在 target owner 内串行启动 boot、musl、BusyBox 与 APK QEMU。外层即使
  使用 `-j4` 也不得并发多个 HVF VM：并发会让 QEMU `hvf_handle_exception` 在有效 guest MMIO
  workload 下触发 host `isv` assertion，并把宿主调度抖动混入 guest deadline。静态编译、clippy、
  unit 与 architecture gate 仍按 Make jobserver 并行；runtime marker 和 workload 不放宽。
- BusyBox 主组合 gate 的 90 秒是 50+ 次 UART interaction、TLS、archive、editor、并发 VFS 与
  job-control 共用的 host liveness bound，不作为 guest 性能阈值；全部 marker/workload 必须完成。
  trap/context/MMU 等性能只由 release ELF 确定性指令/事件计数门禁裁决，禁止用 wall-clock 代替。

## 常用入口

```bash
make build
make build-rust-std
make run
make run-gui
make verify-unit
make verify-architecture-benchmark
make verify
```

`make verify` 是提交前完整入口；局部门禁用于开发反馈，不能替代完整验证。

## 单元测试

`make verify-unit` 必须执行：

- `architecture-check`：dependency、owner、interface、文档索引/链接/事实归属与退化模式的纯函数测试。
- `kernel-unit`：复用 production path 的内存、文件、IPC、socket、codec、数据结构与错误边界测试。
- `scheduler-unit`：preallocated ready heap 的 capacity/compaction/fail-stop 与 signal selection/generation 测试。
- run membership/CPU projection 由 `architecture-check` 静态围栏，wait/deferred 边界由 `kernel-unit`，完整 lifecycle 由对应 runtime smoke 覆盖；不得把这些范围虚报为 `scheduler-unit` 用例。
- filesystem 可睡眠 owner 由静态字段/adapter contract 与真实 `TaskMutex` FIFO handoff host
  test 共同约束：两个排队 waiter 必须按 ticket 获取，handoff 期间 `try_lock` 不得插队，
  completion-before-arm/arming/sleeping 三种顺序均不得丢 wake。
- `syscall-abi`：编号唯一性与 ABI 表一致性测试。

新增行为先补正常、边界、错误、回滚和并发状态转换用例；重构必须保持同一契约。测试不得复制 production algorithm 来制造自洽结果。

## 性能测试

稳定热路径使用 blocking microbenchmark：

- release 构建，固定迭代数，先 warmup；
- 每个样本使用 `black_box`，取奇数样本的 median；
- 只设置宽但真实的绝对上限，用于阻止锁、分配、runtime dispatch 或复杂度退化；
- 文档不记录本机测量值；阈值变化必须有实现和环境证据，不能为通过门禁直接放宽。

当前 blocking benchmark 覆盖 timer deadline、AArch64 VA39 index/TLBI operand projection 与
AArch64 semantic PTE encode/decode。target-specific 零成本边界还必须通过 release target
build、static architecture fence、symbol 与 disassembly 检查；host wall-clock 不能冒充
target instruction cost。RISC-V 保留 backend 的 PTE 与 trap 性能约束由其 unit、release
static gate 与 disassembly gate 继续负责。

新增 hot path、lock、allocation、codec 或 indirection 时必须明确：加入 blocking benchmark、加入 target static/disassembly gate，或说明为何只需要 diagnostic measurement。
whole-machine latency、boot time 与网络吞吐受宿主抖动影响，只作诊断，不作窄阈值 blocking gate。

idle tick suppression 不增加 host microbenchmark：收益来自 HVF/TCG 的 whole-machine exit 次数，
host unit loop 无法代表它。改动必须通过双 architecture compile/static gate，并以单 QEMU、完整 SMP
拓扑的多窗口 host CPU 采样作诊断；secondary idle→IPI→task 与 timeout runtime gate 负责活性语义。

TLB shootdown 不增加 host wall-clock benchmark：成本主要来自 target firmware/IPI，宿主计时不能稳定代表它。
blocking gate 使用 production `TranslationCommit` 派生的确定性计数，并由 architecture fence 阻止 direct whole-machine flush：
lazy mmap 为 `0 local / 0 remote`；1 MiB、256 页 first-touch 为 `256 local / 0 remote target`；
revoke/replace batch 为 `online_cpus - 1` 个 remote target；合并跨度不超过 64 页时执行精确逐页 fence，
超过 64 页时规范化为 1 次 full fence，防止稀疏 VMA teardown 把跨度页数变成无界指令循环。
release target build 继续验证 range `SFENCE.VMA` mechanism。

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

## 架构与产物门禁

- `architecture-check` 校验 dependency matrix、concrete backend containment、global owner、unsafe proof、fallible collection、source size、ABI dispatch、文档 fence 与 generated interface freshness。
- generated interface 只能由 checker 的 `--write-interface` 更新；任何差异都视为 architecture interface change。
- release kernel、需要时的 bootloader 和原生 userspace ELF 必须通过 target、segment、W^X、stack、interpreter、dynamic/RELRO 及 target-specific static-call 检查；kernel、rootfs、APK image 与所有构建 cache 必须属于同一个 architecture identity。

## 运行时门禁

完整验证从同一个只读 rootfs baseline 派生相互隔离的可写镜像，并覆盖：

- boot、CPU topology、interrupt、timer 与基础 filesystem；
- AArch64 `run-gui` 同构的 GPU、keyboard、tablet VirtIO 拓扑，以及桌面全链路：`desktop`
  modeset、AF_UNIX + SCM_RIGHTS 握手、客户端 surface 映射与 `terminal` PTY 监督各自发布
  启动 marker，gate 逐条裁决；gate 使用无 host 窗口的一 CPU
  guest，只裁决设备初始化与 HVF MMIO 指令兼容性，真实 11-CPU 全拓扑由同一静态路径覆盖；
- musl ELF/TLS/thread/signal/process consumer；
- 标准 Rust `std` 的 allocator/entropy、filesystem、Thread/TLS、process、AF_UNIX 与 IPv4 client；
- BusyBox init/ash、TTY、filesystem、IPC 与 network consumer；
- APK 应用的 TLS/HTTP、SQLite journal/lock 和 Git object/ref/worktree vertical slice。

`make -j4 verify-runtime-gates` 是 QEMU 编排的唯一 owner，并串行运行各顶层门禁；每项仍使用
独立镜像、success stamp 和 host port domain。APK 内部同样保持单一 QEMU owner：curl/Git 的
TLS/HTTP 竖切共用一台 4-CPU guest，SQLite 独占持久化/断电恢复镜像。guest 内被测的 SQLite
双 writer 与 curl 四路传输仍保持并发；SQLite writer A 必须在持有 `BEGIN IMMEDIATE` 后发布
guest 内握手，writer B 才能进入 blocking record-lock 路径。SQLite crash gate 保持一个
已 INSERT、未 COMMIT 的 WAL transaction，再在 guest 内精确 `SIGKILL` sqlite process；
重新打开后必须 `integrity=ok` 且未提交 row 不可见。它不把无 journal ext2 的物理掉电恢复
伪装成 SQLite 能力；QEMU SIGKILL 掉电仍由 filesystem 专项 gate 独立裁决。
SQLite 第一阶段 `sync` 后由 host 结束 VM，再冷启动同一持久化镜像执行恢复策略；禁止在 HVF
进程内用 system reset 串联两阶段，因为 QEMU 的 HVF exception handler 会触发 host `isv`
assertion。该生命周期边界不替代 guest `sync`、journal integrity 或 SQLite process-crash 门禁。
任一子门禁失败即整体失败，不能抽样或提前发布成功。

HTTPS origin 的 raw accept 与 TLS handshake 分属 server/connection worker owner；启动时持有
一个不发送 ClientHello 的连接，并要求 10 个合法握手在 2 秒总 deadline 内全部完成。该自检
阻止把 TLS 重新包装到 listening socket、恢复单连接 head-of-line blocking。

## 完整成功条件

默认 `make verify` 以 AArch64/HVF 作为 first-class 提交门禁，必须依次通过 format、clippy、
全部单元测试、blocking benchmark、release build、architecture/documentation fence、
artifact/static-call gate、全部 runtime gate 与 `git diff --check`。最后还必须以
`riscv64 + tcg + release` 执行 RISC-V secondary 的 compile、static/artifact 和 boot 门禁；
该 secondary 保留 backend 可构建与可启动契约，不冒充 AArch64 完整 runtime consumer 覆盖。
