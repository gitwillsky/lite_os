# DTB 驱动的动态 hart 拓扑设计

日期：2026-07-11

## 目标

LiteOS 不再把 `MAX_CORES = 8` 同时当作运行时核数、per-hart 数组长度和 linker 栈数量：

1. 实际 hart 集合只来自 DTB CPU 节点的 hart ID；
2. bootloader 只让 cold-boot hart 首先进入 kernel；
3. kernel allocator 可用后按 DTB 集合构造动态 `HartTopology`；
4. kernel 通过标准 SBI HSM 启动 secondary；
5. scheduler、softirq、RFENCE 与 PLIC 只遍历 DTB hart 集合。

系统不声明固定最大核数。当前容量由可用 kernel heap、单字 SBI hart mask、QEMU PLIC context 窗口和 CLINT 索引能力共同约束。

## 非目标

- 不实现 CPU hotplug、ACPI、NUMA 或真实硬件拓扑差异；
- 不跨多个 XLEN hart-mask group 扩展 SBI target 表达；
- 不引入 bootloader heap；
- 不恢复任何已删除的用户态 ABI。

## Bootloader 协议

QEMU 仍会把 DTB hart 送入 M-mode firmware。每个 hart 完成本地 trap stack 和 HSM cell 初始化后发布 ready；cold-boot hart等待 DTB mask 全部 ready，但只为自己写入 kernel start payload。

因此：

- cold-boot hart 进入 kernel `_start`；
- 其他 DTB hart 保持 HSM `STOPPED` 并在 M-mode WFI；
- kernel 后续 `hart_start` 写入相同 `_start`、DTB opaque，并用 CLINT MSIP 唤醒目标。

bootloader 的 trap/HSM/RFENCE storage 按 `usize::BITS` 提供。这是 SBI 单字 hart mask 的表示宽度，不是运行时核数；DTB mask 决定哪些 slot 可被访问。

## Kernel BoardInfo

kernel DTB parser 保存：

- `hart_mask`：所有可表达的唯一 DTB hart ID；
- `hart_count`：DTB CPU 数量；
- `max_hart_id`：PLIC context 窗口验证所需的最大 ID；
- `invalid_hart_id`：无法由单字 SBI mask 表达的 ID。

allocator 初始化前，cold-boot 路径验证：

1. `hart_count != 0`；
2. `hart_mask.count_ones() == hart_count`；
3. 当前 hart 位于 mask；
4. 不存在 `invalid_hart_id`。

## 动态 HartTopology

4 MiB kernel buddy allocator 可用后，boot hart 按 mask 中的置位 ID 递增构造紧凑的 `Box<[HartState]>`。不存在的稀疏 ID 不占 entry。

每个 `HartState` 持有：

- hart ID；
- 动态 startup stack owner 与稳定 stack top；
- processor slot；
- softirq pending；
- online 与 active 原子状态。

`HartTopology` 另外保存 `hart_mask`、`hart_count`、`max_hart_id` 和 boot hart。表完成构造后，以 Release 发布表长度和地址；发布前只有 cold-boot `_start` 可运行。

## 统一 `_start`

boot hart 和 secondary 使用同一个 `_start`，ABI 始终为：

- `a0 = hart_id`；
- `a1 = dtb_addr`。

无栈汇编阶段读取动态 table address：

1. 地址仍是 `.data` 非零哨兵时，当前是唯一 cold-boot hart，使用 linker 中唯一 early stack、清 BSS并进入 `kmain_boot`；
2. 地址已发布时，以 acquire fence 消费动态表，按 `a0` 查找 `HartState`；
3. 找到后读取其 stack top，初始化 `gp/tp/sscratch` 并进入 `kmain_secondary`；
4. hart 不在表中则关闭中断并永久 WFI。

firmware 在 HSM start 前把 `satp` 清零，动态 startup stack 位于 identity-mapped kernel BSS heap，因此 secondary 取栈不需要私有 payload、第二入口或预先启用页表。

## HSM 启动与上线

boot hart 完成 page table、设备、文件系统和 PID 1 初始化后，以 Release 发布 `INIT_READY`。它按动态 states 遍历所有非 boot hart，逐个调用：

```text
sbi_hart_start(hart_id, _start, dtb_addr)
```

secondary 进入 `kmain_secondary`，以 Acquire 消费 `INIT_READY`，验证 DTB opaque，激活共享 kernel page table，再初始化 timer/interrupt。

每个 hart 最后发布自己的 online 状态。boot hart 等待动态 online mask 与 DTB possible mask 完全相等，避免把“HSM 已接受请求”误当作“hart 已上线”。

## Scheduler 与 softirq

旧的固定 processor/softirq 数组删除：

- local processor 通过当前 hart ID 查找对应动态 state；
- `select_cpu` 轮转紧凑 state 切片，只考虑 active hart；
- mailbox 投递要求目标 state 存在且 active；
- softirq pending 位于相同 `HartState` 中；
- online/active 都是 possible mask 的自然子集。

## RFENCE、IPI 与 PLIC

- kernel RFENCE target 由动态 online states 生成，并与 possible mask 相交；
- scheduler IPI 只投递给动态 active state；
- PLIC 以 DTB hart mask 构造，用 `max_hart_id` 隐含的最高 context 验证 MMIO 窗口；
- threshold、enable 和 affinity 逐个消费 mask 置位，不把 `hart_count` 当连续 ID 上界；
- block IRQ 绑定实际 cold-boot hart，不写死 CPU0。

## 失败策略

- DTB CPU 集非法、hart ID 无法由 SBI mask 表达、cold-boot hart 不在 mask：启动期 fail-stop；
- 动态 state/stack 分配失败：allocator fail-stop；
- SBI HSM start 失败：fail-stop，不降级为少核运行；
- secondary hart 不在动态表、DTB opaque 不一致：fail-stop；
- PLIC MMIO 无法覆盖最大 hart ID context：驱动初始化失败。

## 验证

仓库禁止维护、修正或执行测试用例。验证使用：

1. `git diff --check`、rustfmt、workspace/三组件构建；
2. 静态检索确认无 `MAX_CORES`、固定 8 元素 kernel per-hart 数组或 `0..8` 拓扑遍历；
3. `llvm-objdump` 确认 `_start` 只有一个 linker early stack入口，secondary 从动态表加载 `sp`；
4. QEMU `virt -smp 1/2/8` 冷启动确认 `online mask == DTB mask` 且进入 `/bin/init`。
