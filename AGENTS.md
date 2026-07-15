# LiteOS Agent Contract

## 权威资料

- 当前实现事实：`docs/architecture.md`
- module、依赖与状态 owner：`docs/architecture-contract.md`
- Linux/riscv64 ABI：`docs/syscall-support.md`
- 规范版本：`docs/standards-baseline.md`

只读取与任务相关的资料，不得在本文件复制能力清单、目录介绍、工具链版本或命令列表。
这是一个没有历史包袱的操作系统，可以实践一些领域内新的涉及、或者你认为优秀的设计方法

## 硬规则

- 遵守架构契约中的依赖矩阵和唯一状态 owner。
- 每个复合状态只有一个 owner；禁止复制状态并人工同步。
- `main.rs` 只负责装配；syscall 只负责 ABI、user-copy 与 errno；trap 只负责入口和事件投递。
- 下层不得依赖上层，具体 adapter 不得泄漏穿过其 seam。
- 默认 private；扩大任何 scoped interface 必须更新 interface contract 并说明调用者。
- 禁止私有 ABI、固定 hart 数、双轨实现、兼容入口和无领域含义的 module。
- 新增 unsafe、global、lock、Atomic、Once、cache 或 flag 必须记录证明、owner 与失败后果。
- 新能力与问题修复必须对照固定一手规范和成熟 kernel 语义；范围缩减必须在 ABI 矩阵/阶段文档精确记录，禁止以“能跑”代替正确语义。

## 修改前

明确改动所属 module、owner/interface/依赖是否变化，以及 error、exit、interrupt cleanup 路径。只修改目标所需代码，并清理本次产生的孤儿入口和文档。

## 验证

提交前运行：

```bash
make verify
```

不得修改围栏、interface baseline、验证逻辑或文档声明来掩盖实现错误。
