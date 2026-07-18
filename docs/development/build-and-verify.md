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
- `scheduler-unit`：run membership、CPU projection、wait、signal 和 lifecycle transaction 测试。
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

运行时门禁可并发，但使用独立镜像、success stamp 和 host port domain。任一子门禁失败即整体失败，不能抽样或提前发布成功。

## 完整成功条件

`make verify` 必须依次通过 format、clippy、全部单元测试、blocking benchmark、release build、architecture/documentation fence、artifact/static-call gate、全部 runtime gate 与 `git diff --check`。
