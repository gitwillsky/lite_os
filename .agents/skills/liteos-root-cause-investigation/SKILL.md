---
name: liteos-root-cause-investigation
description: Trace the first broken contract in LiteOS when QEMU or a runtime gate exits or hangs, a guest command fails, or behavior changes across SMP/HVF/TCG/reboot and may cross kernel ownership boundaries.
---

# LiteOS 根因定位

使用“证据漏斗”把混杂症状收敛为 **first broken contract**：最早出现的无效状态、被违反的
LiteOS 契约、唯一 owner，以及它传播成用户症状的完整路径。

只有同时得到以下证据时才称为根因：

- 一个可信的红色复现和只差一个变量的绿色对照；
- `触发条件 + 执行上下文 + 第一个无效状态 + 被违反契约 + owner + 传播链`；
- 移除被指认的触发条件或修复 owner 后，原复现按预测转绿。

## 1. 冻结红色样本

1. 读取 `docs/README.md`，由文档索引选择本症状的事实 owner。
2. 记录 `ARCH`、`ACCEL`、`PROFILE`、`QEMU_SMP`、`QEMU_MEMORY`、镜像路径与容量、
   kernel/rootfs artifact、完整命令和串口输出。
3. 固定同一组产物与输入复现。QEMU 或镜像参与时，读取并完整执行
   [`references/probes-and-proof.md`](references/probes-and-proof.md) 的“可信复现”。

**完成标准：** 第三方能用记录的 artifact、private image 和命令重现同一失败；旧进程输出、
终端回显、共享镜像锁和 warm state 均不能伪造结果。

## 2. 路由到 owner

1. 读取 [`references/symptom-routing.md`](references/symptom-routing.md)，执行与第一个症状匹配的
   区分性探针。
2. 写出真实执行链，例如：
   `shell → launcher → runtime → ELF → syscall → domain owner → arch/platform → QEMU device`。
3. 为每个仍存活的边界记录输入、输出、owner、成功契约和可观测点；通过 `docs/README.md`
   打开的领域文档才是契约事实。

**完成标准：** 未知范围已经收敛为有限的 owner 和边界；每个存活假设都有可证伪预测。

## 3. 做边界二分

每次只改变一个变量，并优先选择能排除最多层级的对照：

- launcher 与 native ELF；
- 单线程与多线程；
- SMP1 与 SMP2+；
- AArch64/HVF 与显式诊断配置；
- 干净基线与持久实例；
- warm run 与 cold boot；
- 真实 workload 与最小 musl/kernel fixture。

沿执行链缩短红色路径，不以更换多个配置后“偶尔成功”作为证据。

**完成标准：** 已得到最小红色路径和紧邻的绿色对照，两者只有一个已命名变量不同。

## 4. 暴露 first broken contract

当红色路径进入内核时，先读取
[`references/kernel-owner-map.md`](references/kernel-owner-map.md)，再沿真实调用者追踪：

`syscall/trap decode → ABI adapter → domain owner → wait/wakeup → task synchronization → arch/platform`。

标注当前是 task、trap、hardirq、deferred、notifier、boot 还是 CPU retirement 上下文。探针只记录
能区分当前假设的 caller、CPU、task/tgid、errno、owner/generation/ack 或 wait 状态，并保持在最小
失败路径上。

**完成标准：** 已指出第一个无效状态发生在哪个 owner、哪个执行上下文和哪条调用链；该机制能解释
红色样本、绿色对照及此前看似矛盾的主要现象。

## 5. 做反事实证明

在修改前保存红色证据，并写下根因假设尚未用于推导的新预测。然后只改变被指认的契约或触发条件，
重复完全相同的探针。

**完成标准：** 原复现由红转绿、邻接对照保持绿色、至少一个新预测命中；结果不依赖临时绕过、
放宽 marker、延长无依据 timeout 或更换 workload。

## 6. 按授权范围收敛

任务包含修复或实现时：

1. 在对应领域 owner 内修复，并检查正常、error、exit、interrupt 和 rollback 路径。
2. owner、interface 或依赖变化时同步权威契约；ABI 行为变化时同步 `docs/syscall-support/`。
3. 把最小复现固化为最窄的 production-path 回归测试或 runtime fixture。
4. 删除临时日志、caller trace、诊断 flag、镜像和双轨入口。

任务只要求诊断或评审时，保持代码不变，交付 first broken contract、反事实证据和最小修复边界。

**完成标准：** 修复分支中每一行改动均可追溯到 first broken contract，回归在旧实现上为红、
新实现上为绿，且诊断残留为零；诊断分支明确声明未修改代码。

## 7. 走证明梯度

读取 [`references/probes-and-proof.md`](references/probes-and-proof.md) 的“证明梯度”。修复分支
依次执行：

`模型/单元测试 → 最小 fixture → 目标 runtime gate → 真实命令 → cold boot → make verify →
RISC-V secondary`。

诊断分支重复原始红色命令和绿色反事实，并运行足以验证相关契约的现有只读检查。

确认每个 gate 实际到达目标 phase。提前拦截的无关门禁单独报告，并补跑尚未覆盖且可独立执行的相关
门禁。

**完成标准：** 修复分支的真实入口在干净冷基线上通过；两个分支的相关门禁均有明确结果；
未执行、目标失败与无关 blocker 被分别列出。

## 交付

最终报告必须包含：

```text
原始症状：
任务范围（诊断/修复）：
可信 reproducer：
最小红色路径：
绿色对照：
first broken contract：
owner 与执行上下文：
触发条件与传播链：
反事实证据：
修复与回归：
真实命令及 cold-boot 结果：
完整门禁、未执行项与无关 blocker：
已删除的诊断产物：
```
