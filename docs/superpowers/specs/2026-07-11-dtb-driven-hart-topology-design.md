# DTB 驱动的 hart 拓扑设计

日期：2026-07-11

## 目标

LiteOS 当前把 `MAX_CORES = 8` 同时当作 QEMU `virt` 的运行时核数、静态数组长度和 linker 启动栈数量。这个设计把这三个概念拆开：

1. **实际 hart 集合**来自 DTB 的 CPU 节点和 `reg` hart ID。
2. **内核支持容量**由清晰命名的配置上限表达，例如 `MAX_SUPPORTED_HARTS`，只用于固定容量的早期表和 SBI hart mask 边界。
3. **启动顺序**改为 boot hart 先进入 kernel，kernel 解析 DTB 后通过 SBI HSM 启动 secondary hart。

这与 Linux `CONFIG_NR_CPUS`、FreeBSD `MAXCPU` 一类做法一致：固件描述决定 possible/online CPU 集合，内核保留编译期支持容量上限。

## 非目标

- 不实现无限 hart 数。RISC-V SBI mask、PLIC context、M-mode HSM 状态和 `no_std` 早期启动都需要明确容量边界。
- 不实现 CPU hotplug、ACPI、真实硬件拓扑差异或非 QEMU `virt` PLIC 拓扑。
- 不引入 bootloader heap 或复杂 M-mode 动态分配。
- 不恢复私有 syscall、测试用例或任何用户态 ABI。

## 总体决策

采用“DTB 驱动实际拓扑 + 明确容量上限 + kernel 主导 secondary 启动”的方案。

1. bootloader 仍解析 DTB 并验证 hart ID 不超过 firmware 支持容量，但不再把所有 DTB hart 自动推进 kernel。
2. cold-boot hart 是唯一首个进入 S-mode kernel 的 hart。
3. secondary hart 在 M-mode 完成本地 trap/HSM 准备后保持 `STOPPED/WFI`，等待 kernel 通过 SBI HSM `hart_start` 明确启动。
4. kernel boot hart 使用单一 linker early boot stack。secondary 不再依赖 `KERNEL_STACK_SIZE * 8` 的 linker 栈区。
5. kernel 解析 DTB 后建立 `HartTopology`，所有调度、softirq、IPI、RFENCE 和 PLIC affinity 都遍历 DTB hart mask，而不是遍历 `0..8`。

## Bootloader 设计

### 容量命名

`MAX_HART_NUM` 改为类似 `MAX_SUPPORTED_HARTS` 的名字。这个值只表示 firmware 可索引的 M-mode trap stack、HSM cell、RFENCE slot 和 CLINT MSIP/mtimecmp 上限，不表示实际启动核数。

缺失这个命名拆分会继续让代码误把容量当运行时拓扑，后续修改 QEMU `-smp` 或 DTB 时仍可能扫到不存在的 hart。

### DTB 验证

bootloader 的 `BoardInfo` 保留：

- `smp`：DTB 中 enabled CPU 节点数量；
- `hart_mask`：DTB 中唯一 hart ID 组成的 mask；
- `invalid_hart_id`：超过 firmware 容量的 hart ID。

验证规则：

1. `smp != 0`；
2. `hart_mask.count_ones() == smp`，防止重复 hart ID 或计数漂移；
3. cold boot hart 必须在 `hart_mask` 中；
4. 任意 hart ID 超过 `MAX_SUPPORTED_HARTS` 时 fail-stop。

### secondary 停留在 M-mode

当前 `start_all_cores` 会在 cold boot hart 中把所有 DTB hart 都启动到 kernel。该函数删除。

新的启动协议：

1. 所有 QEMU 送入 bootloader 的 hart 仍先完成 M-mode 本地初始化，发布 `READY_HARTS`。
2. cold boot hart 等待 DTB mask 内所有 hart 的 HSM cell ready，确保后续 kernel 发起 HSM start 时不会覆盖迟到的 local init。
3. cold boot hart 只启动自己的 HSM payload 并进入 kernel boot entry。
4. secondary hart 不主动进入 kernel，保持 HSM `STOPPED` 状态并在 M-mode trap loop 中等待 `hart_start`。

这个协议保留 firmware 作为 SBI/HSM owner，但把“哪些 hart 进入 kernel”这个决策交给 kernel。

## Kernel 启动设计

### 入口拆分

kernel 提供两个入口：

- `_start`：boot hart 入口，使用单一 linker early boot stack，参数仍为 `a0=hart_id, a1=dtb_addr`。
- `_secondary_start`：secondary hart 入口，由 kernel 通过 SBI HSM 指定，参数为 `a0=hart_id, a1=SecondaryBootPayload*`。

`_start` 不再使用 `a0 << 17` 从固定栈数组中选栈。linker 只保留一个 boot hart early stack，并给它保留 guard page。

### SecondaryBootPayload

每个 secondary hart 有一个启动 payload，至少包含：

- 目标 `hart_id`；
- DTB 地址，用于诊断和一致性检查；
- kernel page table 的 `satp` token；
- 该 hart 的 kernel stack top；
- topology generation，用于防止错误复用旧 payload。

`_secondary_start` 在不依赖栈的汇编阶段完成：

1. 校验 `a1` 非空，并从 identity-mapped payload 读取 `satp` token。
2. 写入 `satp` 并执行 `sfence.vma`，确保后续 high virtual stack 可用。
3. 读取 payload 中的 stack top，设置 `sp`。
4. 设置 `tp = a0`，进入 Rust 层 `kmain_secondary(payload)`。

如果缺少这个无栈阶段，secondary 会在 kernel 页表未启用前使用动态 virtual stack，导致早期不可恢复 fault。

### HartTopology

kernel 新增 `arch::hart::HartTopology` 作为运行时拓扑权威，内容包括：

- `possible_mask`：来自 DTB 的 hart mask；
- `possible_count`：DTB hart 数量；
- `boot_hart`：首个进入 kernel 的 hart；
- per-hart runtime slot：processor、softirq pending、secondary boot payload、stack owner、online/active 状态。

`hart_id()` 只验证当前 `tp` 是否属于 `possible_mask`，不再只检查 `< MAX_CORES`。在 DTB 尚未发布前，只允许 `_start` 和 panic 诊断使用 `raw_hart_id()`。

### 栈所有权

boot hart 使用单一 linker early boot stack，并在 topology 中登记为 boot hart 的 permanent kernel stack。secondary stack 在 `memory::init()` 之后分配，使用现有 framed stack 映射和 guard page 策略。

这样 linker 不再含有 `KERNEL_STACK_SIZE * 8`，但每个运行 hart 仍有明确 owner 和 guard page。

## 调度、softirq 与 IPI

`task::processor` 和 `trap::softirq` 的固定 `[...; MAX_CORES]` 改为基于 `HartTopology` 的 per-hart slot。实际遍历只使用 `possible_mask` 或 active mask：

1. `select_cpu` 从 active hart 集合中选择负载最低者。
2. `deliver_ready_entry` 只允许投递到 active hart。
3. `raise` 和 `dispatch_current_cpu` 使用当前 hart 的 slot。
4. `NEXT_CPU` 只生成扫描起点，不再对容量上限直接取模后遍历不存在 hart。

如果保留 `0..MAX_SUPPORTED_HARTS` 的调度遍历，稀疏 DTB hart ID 会让调度器扫描不存在 CPU，PLIC affinity 和 SBI IPI 也可能指向 absent hart。

## TLB shootdown 与 online mask

`ONLINE_HARTS` 保留为运行时 online mask，但它必须始终是 `possible_mask` 的子集。

TLB 刷新流程保持：

1. 本 hart 先执行本地 `sfence.vma`。
2. 读取 online mask，排除当前 hart。
3. 通过 SBI RFENCE 同步刷新目标 hart。

新增约束是 RFENCE target 必须先与 `possible_mask` 相交，避免把 DTB 不存在的 hart 传给 firmware。

## PLIC 与设备 affinity

PLIC 初始化不再传固定 `num_harts = 8`。它使用 `HartTopology.possible_mask` 和 DTB 最大 hart ID：

1. MMIO context 窗口验证必须覆盖 DTB 中最大 hart ID 对应的 S-mode context，而不是只覆盖连续 `0..count`。
2. threshold 初始化、enable、affinity 遍历 `possible_mask`。
3. block IRQ 默认绑定到 boot hart 或 DTB mask 中的第一个 online hart，不再写死 CPU0。

这对稀疏 hart ID 很关键，因为 QEMU PLIC S-mode context 是 `2 * hart + 1`，不能把 hart 数量误当成连续 hart ID 上界。

## 错误处理

- DTB 无 CPU、重复 hart ID、cold boot hart 不在 mask 内、hart ID 超过支持容量：启动期 fail-stop。
- kernel HSM `hart_start` 失败：fail-stop。当前系统没有 CPU hotplug 或 degraded SMP 模型，不能悄悄少启动一个 hart。
- secondary payload hart ID 与 `a0` 不一致：fail-stop。
- PLIC context 窗口无法覆盖 DTB 最大 hart ID：启动期 fail-stop，不能继续使用错误 context。

## 文档更新

需要同步更新：

- `README.md`：从“最多 8 hart”改为“DTB 描述的 hart 集合，当前配置容量上限为 8”。
- `docs/architecture.md`：更新 bootloader 与 kernel SMP 启动链路。
- `docs/phase-2-architecture.md` 或新增阶段记录：说明旧 `MAX_CORES` 风险如何收敛。

## 验证

遵守仓库规则，不维护、不修正、不执行测试用例。验证只做构建、静态检查和非测试冷启动观察：

1. `git diff --check`；
2. `cargo check --workspace`；
3. `make build-bootloader`、`make build-kernel`、`make build-user`、`python3 create_fs.py create`；
4. `llvm-objdump` 检查 `_start` 不再按 hart ID 索引固定启动栈，`_secondary_start` 在使用 stack 前写入 `satp`；
5. QEMU `virt` 冷启动观察：`-smp 1`、`-smp 2`、`-smp 8` 均由 DTB mask 决定启动 hart 数，日志中 online hart 与 DTB mask 一致，最终输出 `LiteOS init`。

## 成功标准

- 源码中不再存在把 `MAX_CORES` 当运行时 CPU 数的路径。
- linker 不再为 `8` 个 kernel boot stack 预留空间。
- bootloader 不再主动 `start_all_cores`。
- kernel 通过 SBI HSM 启动 secondary hart。
- 调度、softirq、PLIC、IPI、RFENCE 都以 DTB hart mask 作为实际目标集合。
- 支持容量上限仍存在，但命名和文档明确表示它不是运行时核数。

## 被拒绝的方案

“完全无容量上限”被拒绝。原因是 RISC-V SBI hart mask、M-mode trap/HSM/RFENCE 状态、early stack 和 PLIC context 验证都需要边界。主流 OS 也通常保留 `CONFIG_NR_CPUS` 或 `MAXCPU` 这类容量配置。LiteOS 当前阶段更适合采用同样的模型：DTB 决定实际拓扑，配置上限决定内核可支持的最大索引空间。
