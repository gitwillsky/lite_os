# LiteOS Phase 2：bootloader、arch、trap、interrupt、timer 与 SMP

> 审计日期：2026-07-11（Asia/Shanghai）
> 代码基线：分支 `std` 上 Phase 1 未提交工作树
> 规范基线：[standards-baseline.md](standards-baseline.md) 中固定的 RISC-V Privileged Architecture `v20260120`、SBI `v3.0`、RISC-V ELF psABI `e03d44ae2f0e1144f9498c2896b5ae25b0449398`。
> 验证约束：不维护、不修正、不执行测试；只使用构建、ELF/符号/反汇编检查和非测试 QEMU 启动观察。

## 1. 阶段范围

本阶段只处理 M-mode firmware 到 S-mode kernel 的控制权移交，以及所有 hart 共享的最低层执行契约：hart 身份、启动栈、全局初始化发布、CSR、trap 上下文、SBI、TIME、IPI、RFENCE、panic 和 idle。调度策略、进程模型、用户内存语义、信号语义与设备状态机分别留在后续阶段。

## 2. 修改前实现

- bootloader 由 QEMU 同时送入的任意 cold-boot hart 竞争 `GENESIS`，清 BSS、解析 DTB、建立 RustSBI，然后主动启动 DTB 计数范围内的其他 hart。
- kernel `_start` 用 `a0 << 17` 从固定八个 128 KiB 栈中选栈，把 `a0` 写入 `tp`，由 hart 0 清 BSS并完成所有全局初始化。
- kernel 在用户页表中映射 trampoline 与 TrapContext；用户 trap 先切回 kernel `satp`，kernel trap 使用独立直接入口。
- TIME 通过 SBI `TIME/set_timer` 编程，`time` CSR 作为单调计数源；定时器硬中断再置本地 SSIP 执行 timer softirq。
- 普通 IPI、信号唤醒与所谓 TLB shootdown 共用 SSIP；接收端无条件执行全局 `sfence.vma`，发送端不等待完成。
- 用户 trap 不保存 `tp` 或浮点状态；kernel context switch 也不保存 psABI 要求的 callee-saved 浮点寄存器。

## 3. 关键调用链

1. QEMU reset → bootloader `_start` → `trap_stack::locate` → `rust_main` → HSM `start` → M-mode `fast_handler` → `mret`/S-mode kernel `_start`。
2. kernel `_start` → per-hart boot stack/`tp` → BSS barrier → `kmain` → boot hart global init → Release/Acquire barrier → secondary hart 激活 kernel `satp` → `run_tasks`/`wfi`。
3. U-mode trap → trampoline `__alltraps` → kernel `satp`/stack → `trap_handler` → `trap_return` → `__restore` → U-mode。
4. S-mode trap → `__kernel_trap` → `rust_trap_from_kernel` → `sret`。
5. timer deadline → M timer interrupt → bootloader 置 STIP → S timer interrupt → 下一 deadline + timer softirq。
6. SBI IPI → CLINT MSIP → M software interrupt代理 → SSIP → S software interrupt。

## 4. 关键数据结构

- bootloader：`ROOT_STACK[8]`、每 hart `HsmCell<Supervisor>`、`BoardInfo`、全局 RustSBI 实例、CLINT 指针。
- kernel：八个 linker boot stack、`TrapContext`、`TaskContext`、per-hart processor、`INIT_READY`、timer interval、softirq pending bitmap。
- 地址空间：kernel `satp`、task user `satp`、所有地址空间共用的 trampoline、每 task TrapContext 页。

## 5. 修改前不变量及证据

- kernel boot stack 设计为每 hart 唯一，链接区间为 `KERNEL_STACK_SIZE * 8`；但入口在验证 ID 前计算地址，因此当前不变量未成立。
- `tp` 被当作 kernel hart ID 唯一来源；但用户态也按 psABI 使用 `tp` 作为 TLS 基址，当前 trap 跳过 x4，二者发生冲突。
- `INIT_READY.store(Release)` 与 secondary `load(Acquire)` 已表达 global init 发布；bootloader 的 `GENESIS.swap(Acquire)` 却在 BSS 清零和全局对象构造完成前允许其他 hart 继续。
- S trap 期间硬件清 SIE，handler 不主动重开，因此当前路径不允许 nested S interrupt；该行为应保持显式。
- SBI RFENCE 要在返回前完成目标 hart 的远程 fence；当前“发送普通 IPI 后立即返回”不满足同步完成语义。

## 6. 已确认问题

| 严重度 | 问题 | 直接后果 |
|---|---|---|
| Blocker | `arch::hart::hart_id()` 把 `tp >= 8` 静默映射为 0 | 两个 hart 可同时获得 CPU0 的栈、processor 与软中断状态，产生内存破坏 |
| Blocker | kernel 与 bootloader 均在入口验证 hart ID 前索引固定栈 | 越界 ID 先使用预留区外内存，无法再安全报告错误 |
| Blocker | 用户 x4/`tp` 未保存恢复，trap 后保留 kernel hart ID | musl TLS 和任何 psABI TLS consumer 立即失效 |
| Blocker | user/kernel trap 与 `__switch` 未保存必要浮点状态 | 中断或任务切换可静默串改另一执行流的 FP 寄存器/fcsr |
| Critical | bootloader 在清 BSS 前把 genesis 标志改为 false | secondary 可同时读取正被清零的 `Once`/SBI 存储，且 HSM start 可被后续本地初始化覆盖 |
| Critical | RustSBI 全局通过并发 `assume_init_mut()` 构造多个 `&mut` | 即使实现只读也违反 Rust aliasing，不具备并发安全依据 |
| Critical | M-mode handler 把任意 exception 模式匹配成 SBI ecall | 非 S-mode ecall 异常可能按攻击者寄存器内容调用 SBI dispatch |
| Critical | TLB 广播无目标 online mask、原因位和完成确认 | 页表回收可早于远端 stale TLB 消失；idle 内核 trap 还完全漏掉 fence |
| High | `TICK_INTERVAL_VALUE` 是跨 hart `static mut` | 并发读写构成数据竞争 |
| High | per-CPU processor 用跨 hart `static mut`，远端直接窃取另一 hart scheduler | 多个 `&mut` 与无同步 scheduler 访问违反唯一所有者不变量 |
| High | legacy SBI console 与 v0.2+ EID/FID 混用 | kernel 的 firmware ABI 不是单一现代 SBI 契约 |
| High | user 可触发的未枚举 exception 会 panic 整个 kernel | 用户输入可转化为 kernel panic |
| Medium | panic 仅本 hart 关闭中断并永久 WFI | 其他 hart 继续运行已失效系统状态 |

## 7. 对应标准

- Privileged Architecture：trap delegation、`mstatus.MPP/MPIE`、`sstatus.SPP/SPIE/SIE/FS`、`satp`、`SFENCE.VMA` 与 CSR 可见性。
- RISC-V psABI：`gp`/`tp` 不可随意破坏，`tp` 是线程指针；LP64D 下 `fs0-fs11` 是被调用者保存寄存器，`fcsr` 具有线程存储期。
- SBI：v0.2+ 的 `a7=EID`、`a6=FID`、`a0/a1=error/value`；TIME、IPI、RFENCE、HSM、SRST、DBCN 的标准编码与同步返回语义。
- Rust/RVWMO：发布前普通写必须先于 Release，消费方访问已发布对象前必须完成配对 Acquire；唯一 hart 拥有的可变对象不能被远端直接借用为 `&mut`。

## 8. 目标模型

- 固件用三阶段协议启动：唯一 cold-boot hart 清 BSS并构造全局对象；Release 发布全局就绪；每个合法 hart 初始化自身 trap/HSM 后发布 ready bit；cold-boot hart 等待完整 ready mask 后才写入 HSM start 数据。
- 固件中 RustSBI 使用 `spin::Once` 发布不可变共享实例，不再创建全局可变引用。
- kernel 入口先检查 ID，再选择栈；`hart_id()` 对越界直接触发不变量失败，不提供 CPU0 fallback。
- TrapContext 保存所有用户 GPR、`gp`/`tp`、全部 FP register 与 `fcsr`；进入 kernel 后恢复 kernel `gp`/`tp`，返回前恢复用户值。kernel trap保存异步可能破坏的全部 GP/FP 状态；TaskContext 保存 LP64D callee-saved FP 状态。
- TLB shootdown 使用标准 SBI RFENCE。firmware 为目标 hart 发布 fence 请求、发送 MSIP、等待每个 Acquire/Release ack 后才从 SBI 返回；普通 IPI只表示唤醒，不再隐式刷新 TLB。
- per-hart processor 只有所属 hart 能访问可变 scheduler/current/idle context；远端只能访问独立的原子负载与 inbound queue。删除跨 hart 直接 work stealing草稿。
- panic 优先通过标准 SBI SRST 关闭整个系统；失败时本 hart 才进入中断关闭的 WFI 循环。

## 9. 删除项

- hart ID → CPU0 的静默 fallback及所有“记录错误后继续”的越界分支。
- legacy SBI console putchar/getchar路径和 bootloader 对 legacy console probe 的伪装。
- SSIP 接收端无条件 `sfence.vma` 的错误兼容行为。
- 远端直接访问另一个 hart scheduler 的 work-stealing实现。
- 从未被消费的 `NEED_RESCHED`/伪抢占标志；真正抢占留给 Phase 6 统一状态机。

## 10. 修改计划

1. 重写 bootloader 全局初始化与 per-hart HSM ready 发布 → verify: start 数据只在目标 HSM 初始化完成后写入，Release/Acquire 有成对注释。
2. 在两级 `_start` 和所有 hart 索引入口强制边界 → verify: 无 `hart_id` fallback、无验证前栈地址计算。
3. 将 RustSBI 实例改为 `Once`，收紧 M exception 匹配并迁移 DBCN → verify: 无 `static mut SBI`、无 legacy EID。
4. 实现同步 SBI RFENCE 并将 kernel TLB 广播迁移到该接口 → verify: RFENCE 返回前逐目标 ack，普通 SSIP 不再执行 fence。
5. 保存/恢复 user/kernel GP、TP、FP 与 fcsr → verify: `TrapContext`/`TaskContext` 布局和汇编偏移逐项一致，反汇编存在预期 CSR/FP 指令。
6. 将 timer interval 改为原子发布，统一 user/kernel supervisor-soft处理 → verify: 相关路径无 `static mut`、idle 与 user trap消费相同 IPI机制。
7. 把 per-hart processor 改为本地唯一可变所有权 + 远端 mailbox → verify: 无远端 scheduler借用、无 `static mut [Option<Processor>]`。
8. 收紧 user exception 与 panic/idle行为 → verify: 用户未实现 exception 只终止当前任务，kernel exception仍 fail-stop，panic调用 SBI SRST。
9. 执行 workspace/组件构建、ELF/符号/反汇编与 8-hart QEMU启动观察 → verify: init进入调度循环、无 panic、八个合法 hart均完成发布；不运行测试。

## 11. 验证方式

- `cargo check --workspace`、`make build-kernel`、`make build-bootloader`、`make build-user`。
- `git diff --check`，并检查新增 warning、失效 import/feature、`static mut`、Relaxed 发布和越界 fallback。
- 用 `llvm-readelf`/`llvm-nm`/`llvm-objdump` 核对入口、trampoline、TrapContext相关浮点指令、SBI EID/FID和栈对齐。
- 使用 QEMU `virt -smp 8` 做非测试启动观察；记录随机 cold-boot hart、所有 secondary 发布、kernel/init启动，无需执行任何测试程序。
- 心智验收：逐条模拟 cold-boot/secondary先后到达、user trap、kernel timer trap、普通 IPI、RFENCE、非法 hart、SBI失败和 panic。

## 12. 风险

- 当前目标硬件仍是 QEMU `virt` 且最多八个、XLEN=64 的 hart；稀疏 hart ID 将按 DTB mask支持，但超过固定上限会 fail-stop，不动态扩展数组。
- RustSBI `0.4.0` 对外报告 SBI 2.0；本阶段按固定 SBI 3.0 规范实现其仍有效的 TIME/IPI/RFENCE/HSM/SRST/DBCN 子集，不虚假宣称 firmware 已实现 SBI 3.0 全集。
- ASID 尚未分配，当前 `satp` 使用 ASID 0，因此 RFENCE 先采用全地址空间刷新；ASID/active-hart address-space mask 留给 Phase 4。
- PLIC context/affinity、VirtIO DMA 与设备中断状态机留给 Phase 10；本阶段只保证 S external trap入口不破坏执行上下文。
- 调度器抢占、等待队列与公平性留给 Phase 6；删除错误 work stealing不会被表述为完成调度目标。

## 13. 修改前启动基线

2026-07-11 的 QEMU `virt -smp 8` 非测试观察中，cold-boot hart 为 5，firmware 成功请求启动 0、1、2、3、4、6、7，kernel 仍固定由 hart 0 执行全局初始化并创建 `/bin/init`。10 秒观察窗口未 panic，但该运行结果不能证明上述竞态、上下文保存或 TLB 同步正确。

## 14. 完成结论

Phase 2 已于 2026-07-11 完成。本阶段没有新增用户 syscall；内部 firmware ABI 从 legacy console 统一迁移到 SBI v0.2+ EID/FID，并补齐 DBCN、TIME、IPI、RFENCE、SRST 与 required-extension probe。`TrapContext` 扩展为 72 个 machine word，`TaskContext` 扩展为 27 个 machine word；这两个结构只在 kernel 内部使用，不形成用户 ABI。

### 14.1 已建立的不变量

1. bootloader 和 kernel 都在计算或索引 per-hart 栈前验证 hart ID；非法 hart 关闭中断并 fail-stop。`hart_id()` 不再回退 CPU0。
2. firmware 使用 `PENDING → INITIALIZING → READY` 和 Release/Acquire 发布全局对象；每个 DTB hart 独立发布 ready，cold-boot hart 等齐精确 hart mask 后才写 HSM start payload。
3. RustSBI 与 board info 通过 `spin::Once` 发布；per-hart trap stack 只允许 owner hart 可变访问，remote HSM 状态与 stack 存储分离。
4. kernel 的任意首达合法 hart 负责 BSS 清零，boot owner 完成全局初始化后再 Release 发布 `INIT_READY`；各 hart 在启用中断前完成页表、trap、timer 与 online 状态建立。
5. user trap 保存并恢复全部 GPR、`gp`、`tp`、32 个 FP 寄存器与 `fcsr`；kernel trap 保存异步可破坏的完整 GP/FP 状态；任务切换保存 LP64D callee-saved `fs0..fs11` 与 `fcsr`。
6. 普通 IPI 只负责唤醒/SSIP；TLB shootdown 使用同步 SBI RFENCE，并在所有 online target hart 完成 fence 和 ack 后才返回。
7. per-hart processor 的 scheduler/current/idle 仅由 owner hart 在关闭 SIE 时可变访问；远端任务只进入带锁 mailbox。跨 hart 直接 work stealing、CPU fallback、`NEED_RESCHED` 草稿和未使用的 `'static mut TrapContext` 接口均已删除。
8. 用户未支持 exception 终止当前 task，不再导致 kernel panic；kernel exception 仍 fail-stop。panic 优先调用标准 SBI SRST。

### 14.2 构建与静态验收

- `git diff --check`、`cargo check --workspace`、`make build-kernel`、`make build-bootloader`、`make build-user` 全部成功；未执行、维护或修正测试。最终告警计数为 kernel 370、bootloader 19、user 3，均未作为本阶段外的清理任务扩散修改。
- `llvm-nm` 确认 kernel `_start`、`__alltraps`、`__kernel_trap`、`__restore`、`__switch` 和 `__global_pointer$` 存在；kernel ELF 仍具有独立 `.text/.rodata/.data/.bss`，bootloader 具有 `.text/.rodata/.data/.bss`。
- `llvm-objdump` 确认 kernel `_start` 在栈计算前执行 `hart_id < 8`，bootloader `trap_stack::locate` 在栈定位前读取并验证 `mhartid`；trap 和 switch 产物包含完整预期的 `fsd/fld/frcsr/fscsr`、`gp/tp` 保存恢复与 `sfence.vma`。
- bootloader M software trap 产物包含 RFENCE request 的 `amoswap.d.aq`、`fence.i/sfence.vma` 和 ack 的 `amoswap.d.rl`，普通 SSIP 仍被转发。
- 源码检索未发现 legacy EID、hart→CPU0 fallback、`NEED_RESCHED`、跨 hart `try_global_steal` 或 SSIP 隐式 TLB flush。范围内剩余 `static mut` 仅为 linker BSS 边界符号；剩余 Relaxed 使用只承担 hart-local 状态或调度提示，并有意图说明。

### 14.3 修改后启动观察

最终构建在 QEMU `virt -smp 8` 上做了 10 秒非测试观察：cold-boot hart 随机为 6，firmware 报告 hart mask `0xff` 并成功启动 0、1、2、3、4、5、7；kernel 完成内存、timer、VirtIO block 与 ext2 初始化，挂载根文件系统并创建 init 进程。观察窗口内无 panic、死锁或异常输出，之后通过 QEMU 控制台正常终止。此前另两次修改后观察的 cold-boot hart 分别为 4 和 3，结果一致；该证据只证明所观察启动路径，不替代后续阶段的并发正确性证明。

### 14.4 剩余风险与下一阶段

- 当前 RFENCE 对 ASID 0 做保守全地址空间刷新；per-address-space active-hart mask、ASID 与用户页表生命周期仍由 Phase 4 完成。
- signal 的进程/线程归属、等待与普通 IPI 唤醒仍需 Phase 5/7 收敛；PLIC、VirtIO DMA 和设备状态机仍由 Phase 10 审计。
- scheduler 的 task/process 重复状态、抢占、公平性和统一 wait queue 尚未解决；Phase 3 先建立 interrupt-safe 同步基础，Phase 5 统一 process/thread 身份和资源所有权，Phase 6 再处理调度状态机。
