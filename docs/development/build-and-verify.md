# 构建、测试与验证

本文件唯一维护开发环境、构建入口、缓存规则和质量门禁。文档只定义可重复的输入、覆盖与成功条件，不记录某次执行日期、输出或实测数值。

## 环境

- Rust 版本、组件和 target 由 `rust-toolchain.toml` 固定；精确 revision 见 [规范基线](../standards-baseline.md)。
- 当前 target 是 `riscv64gc-unknown-none-elf`，运行环境需要 `qemu-system-riscv64`。
- musl、BusyBox、APK 和 terminal font 输入必须由脚本中的固定 URL、版本与摘要构建；不得静默消费系统副本或滚动 latest。
- `target/rootfs.img` 是可重复基线；开发用 `fs.img` 只由显式 reset 初始化，不能反向污染基线。

## 常用入口

```bash
make build
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

当前 blocking benchmark 覆盖 timer deadline、Sv39 index projection 与 semantic PTE encode/decode。target-specific 零成本边界还必须通过 release target build、static architecture fence、symbol 与 disassembly 检查；host wall-clock 不能冒充 target instruction cost。

新增 hot path、lock、allocation、codec 或 indirection 时必须明确：加入 blocking benchmark、加入 target static/disassembly gate，或说明为何只需要 diagnostic measurement。
whole-machine latency、boot time 与网络吞吐受宿主抖动影响，只作诊断，不作窄阈值 blocking gate。

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
- release kernel/bootloader 和 userspace ELF 必须通过 target、segment、W^X、stack、interpreter、dynamic/RELRO 及 target-specific static-call 检查。

## 运行时门禁

完整验证从同一个只读 rootfs baseline 派生相互隔离的可写镜像，并覆盖：

- boot、CPU topology、interrupt、timer 与基础 filesystem；
- musl ELF/TLS/thread/signal/process consumer；
- BusyBox init/ash、TTY、filesystem、IPC 与 network consumer；
- APK 应用的 TLS/HTTP、SQLite journal/lock 和 Git object/ref/worktree vertical slice。

`make -j4 verify-runtime-gates` 是 QEMU 编排并发的唯一 owner，各顶层门禁使用独立镜像、
success stamp 和 host port domain。APK 内部不再嵌套 QEMU 并发：curl/Git 的 TLS/HTTP 竖切
共用一台 4-CPU guest，SQLite 独占持久化/断电恢复镜像；这避免把完整门禁从 4 台/16 guest
vCPU 放大为 6 台/24 guest vCPU，并把网络应用冷启动从 2 次减为 1 次。guest 内被测的
SQLite 双 writer 与 curl 四路传输仍保持并发，覆盖与 deadline 不变。
任一子门禁失败即整体失败，不能抽样或提前发布成功。

HTTPS origin 的 raw accept 与 TLS handshake 分属 server/connection worker owner；启动时持有
一个不发送 ClientHello 的连接，并要求 10 个合法握手在 2 秒总 deadline 内全部完成。该自检
阻止把 TLS 重新包装到 listening socket、恢复单连接 head-of-line blocking。

## 完整成功条件

`make verify` 必须依次通过 format、clippy、全部单元测试、blocking benchmark、release build、architecture/documentation fence、artifact/static-call gate、全部 runtime gate 与 `git diff --check`。
