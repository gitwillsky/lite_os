# LiteOS Agent Contract

## 语言与注释

- 注释与当前代码上下文和用户对话保持一致；没有明确要求时不切换语言。
- 复杂流程按 `1. / 2. / 3.` 解释。新增 flag、cache、特殊分支时，必须说明用途及缺失时的具体失败。
- 公共 API 与类型使用 RustDoc；分别说明功能、参数、返回值和错误。自解释代码不重复注释，难懂的不变量必须说明。

## 工作方式

- 编码前明确假设、歧义与权衡；存在明显更简单的方案时先指出。不能安全推断的关键问题必须暂停并确认。
- 只实现目标所需的最小方案，不增加投机性抽象、配置或不可能分支。单次使用的逻辑不为“复用”而拆层。
- 修改必须可追溯到当前目标；保留用户已有改动，不清理无关旧代码。本次改动造成的孤儿 import、入口、分支和文档必须清除。
- 多步任务先列出“步骤 → 验证点”，持续执行到可验证成功标准达成。重构必须证明契约与行为保持一致。
- 禁止双轨实现、兼容入口、补丁式 hack、面条控制流及无领域含义的 `common/utils/helpers/misc/manager/base/shared/core` module。

## 权威资料与路由

- 所有文档入口与事实 owner：[`docs/README.md`](docs/README.md)。
- 当前设计：[`docs/architecture.md`](docs/architecture.md) 及其领域文档。
- module、接口、依赖与状态 owner：[`docs/architecture-contract.md`](docs/architecture-contract.md) 及其领域契约。
- Linux/riscv64 ABI：[`docs/syscall-support.md`](docs/syscall-support.md) 及其领域矩阵。
- 固定规范版本与来源：[`docs/standards-baseline.md`](docs/standards-baseline.md)。
- 构建、测试、性能与运行时门禁：[`docs/development/build-and-verify.md`](docs/development/build-and-verify.md)。

只读取任务相关资料，不在本文件复制能力清单、syscall 数量、工具链版本或命令清单。计划文档只在任务执行期间存在；完成后迁移持久事实并删除计划。

## 架构硬规则

- 通用 kernel 只通过编译期静态 `arch`/`platform` façade 使用后端；禁止 `dyn Architecture`、运行时架构分派、固定 CPU 数、双轨实现和兼容路径。
- target `cfg`、CSR、汇编、寄存器布局、页表编码属于 `arch`；machine、firmware、DTB、中断控制器和设备装配属于 `platform`。具体 adapter 不得穿过 seam。
- `arch` 拥有执行上下文、trap 解码和 MMU mechanism；`platform` 拥有 machine facts 与 adapter 装配；通用领域只使用 logical `CpuId`/`CpuSet`，hardware identity 不是领域索引。
- `entry` 只编解码 raw boot/trap ABI；`main.rs` 只装配；`trap` 只接收语义事件并投递领域；`syscall` 只负责 ABI、user-copy 与 errno。
- 下层不得依赖上层。每个复合状态只有一个 owner；禁止复制状态并人工同步。默认 private；扩大 scoped interface 必须更新 contract 并说明调用者。
- 禁止私有 ABI。新增 unsafe、global、lock、Atomic、Once、cache 或 flag 必须记录安全/所有权证明与缺失时的失败后果。
- 新能力与问题修复必须对照固定的一手规范和成熟 kernel 语义。范围缩减必须在 ABI 矩阵或当前架构限制中精确记录，不能以“能跑”代替正确语义。

## 修改与验证

修改前明确所属 module、owner/interface/依赖是否变化，以及 error、exit、interrupt cleanup 路径。接口、依赖或 owner 改变时同步更新对应契约；只修改目标所需代码。

单元测试、性能测试和运行时测试属于实现的一部分，必须随行为维护并实际执行。新增热路径、锁、分配、codec 或间接层时必须作出 benchmark 决策。提交前运行：

```bash
make verify
```

不得修改围栏、接口基线、阈值或文档声明来掩盖实现错误。
