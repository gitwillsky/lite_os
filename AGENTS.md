# LiteOS Agent Contract

本文件只保留能指导后续决策的原则和长期约束。复盘先提炼可迁移的判断，并入已有原则。具体事实、使用方法和验证边界由领域文档维护。

## 语言与注释

- 注释与当前代码上下文和用户对话保持一致；没有明确要求时不切换语言。
- 复杂流程按 `1. / 2. / 3.` 解释。新增 flag、cache、特殊分支时，必须说明用途及缺失时的具体失败。
- 公共 API 与类型使用 RustDoc；分别说明功能、参数、返回值和错误。自解释代码不重复注释，难懂的不变量必须说明。

## 工作方式

- 编码前明确假设、歧义与权衡；存在明显更简单的方案时先指出。不能安全推断的关键问题必须暂停并确认。
- 先沿实际执行路径追到原因和职责归属，再选改动位置。优先用现有设计和标准约定完成最小改动。只有能说明现有方案的具体缺口，才引入新的职责或约定。修复共性问题时检查同一职责下的其他调用方。
- 修改必须可追溯到当前目标；保留用户已有改动，不清理无关旧代码。本次改动造成的孤儿 import、入口、分支和文档必须清除。
- 多步任务先列出“步骤 → 验证点”，持续执行到可验证成功标准达成。重构必须证明契约与行为保持一致。
- 禁止双轨实现、兼容入口、补丁式 hack、面条控制流及无领域含义的 `common/utils/helpers/misc/manager/base/shared/core` module。

- 本工程需长期复用的下载、上游源码、ISO、离线包、密钥与实验环境放在工程内的 `.local/`，该目录必须被 Git 忽略，禁止暂存或提交其中内容。
  一次性诊断脚本、镜像副本、提取中间文件和临时日志使用系统临时目录，结束后清理，不得放入 `.local/`；正式构建产物与门禁日志沿用 `target/`。
  产品代码依赖的 ISO 提取资源必须放在受 Git 管理的 `assets/classic/`，由唯一导入器与 manifest 管理；禁止本地覆盖、替代素材双轨及无引用产物。

## 权威资料与路由

- 所有文档入口与事实 owner：[`docs/README.md`](docs/README.md)。
- 当前设计：[`docs/architecture.md`](docs/architecture.md) 及其领域文档。
- module、接口、依赖与状态 owner：[`docs/architecture-contract.md`](docs/architecture-contract.md) 及其领域契约。
- Linux/riscv64 ABI：[`docs/syscall-support.md`](docs/syscall-support.md) 及其领域矩阵。
- 固定规范版本与来源：[`docs/standards-baseline.md`](docs/standards-baseline.md)。
- 构建、测试、性能与运行时门禁：[`docs/development/build-and-verify.md`](docs/development/build-and-verify.md)。
- 来宾操作与宿主取证按[开发与诊断入口](docs/development/build-and-verify.md#通过-ssh-开发和诊断)选择通道。

只读取任务相关资料，不在本文件复制能力清单、syscall 数量、工具链版本或命令清单。计划文档只在任务执行期间存在；完成后迁移持久事实并删除计划。

## 架构硬规则

- 通用 kernel 只通过编译期静态 `arch`/`platform` façade 使用后端；禁止 `dyn Architecture`、运行时架构分派、固定 CPU 数、双轨实现和兼容路径。
- target `cfg`、CSR、汇编、寄存器布局、页表编码属于 `arch`；machine、firmware、DTB、中断控制器和设备装配属于 `platform`。具体 adapter 不得穿过 seam。
- `arch` 拥有执行上下文、trap 解码和 MMU mechanism；`platform` 拥有 machine facts 与 adapter 装配；通用领域只使用 logical `CpuId`/`CpuSet`，hardware identity 不是领域索引。
- `execution::entry` 只编解码 raw boot/trap ABI；`main.rs` 只装配；`execution::trap` 只接收语义事件并投递领域；`syscall` 只负责通用 ABI、user-copy 与 errno；设备专属 UAPI 由设备子系统经注册的文件操作拥有。
- 下层不得依赖上层。每个复合状态只有一个 owner；禁止复制状态并人工同步。默认 private；扩大 scoped interface 必须更新 contract 并说明调用者。
- 涉及 Wayland 时核对固定协议及两端实现，按标准补齐能力。渲染优化须保留 buffer、commit、damage、frame 与同步和释放语义，GPU 共享缓冲走标准协议与 DRM UAPI。
- 禁止应用可见的私有 ABI；应用只使用 Linux UAPI。内核原生对象接口（`native-abi`）是系统服务接口，不是应用 ABI。通用内核机制不得引入 Linux 概念，Linux 语义只属于 Linux 领域
  （边界见 [`docs/architecture-contract/dependency-owners.md`](docs/architecture-contract/dependency-owners.md)）。
  兼容 Linux 是为了运行 Linux 应用，不把系统能力限制在 Linux 之内：LiteOS 系统服务协议可以提供给指定的系统程序，
  但必须经显式授权，有版本化的协议 crate 与契约文档，只走标准传输（socket/fd），不得新增私有 syscall/ioctl，也不得默认对所有应用可见。
  新增 unsafe、global、lock、Atomic、Once、cache 或 flag 必须记录安全/所有权证明与缺失时的失败后果。
- 新能力与问题修复必须对照固定的一手规范和成熟 kernel 语义。范围缩减必须在 ABI 矩阵或当前架构限制中精确记录，不能以“能跑”代替正确语义。

## Rust 测试布局

- 新增测试前说明它能捕获哪种真实回归、现有验证为何不足，以及成本是否与风险相称。优先扩展已有生产路径用例，审查时去掉重复验证实现或没有新增保护的断言。
- 单元测试使用模块末尾的 `#[cfg(test)] mod tests`（也可声明独立测试文件）；测试模块之后不再添加生产条目。
- 集成测试放在 `tests/`，guest ABI/设备验收使用现有 runtime fixture；不在生产控制流中加入诊断采样或测试入口。
- 所有第一方 crate 继承 workspace Clippy lint；`items_after_test_module` 为 deny，不用 allow/expect 绕过。
  裸机/条件编译源码同时由 architecture-check 的 Rust AST 检查覆盖，完整 `make verify` 必须包含该检查。
- root/user workspace 与独立 bootloader 使用同一 Clippy 严格策略，具体规则与门禁覆盖见 `docs/development/build-and-verify.md`。
  新告警先修复实际问题；必须保留的例外使用最窄的 `expect(..., reason = "...")`，说明所有权或成本依据，禁止批量 allow。

## 修改与验证

修改前明确所属 module、owner/interface/依赖是否变化，以及 error、exit、interrupt cleanup 路径。接口、依赖或 owner 改变时同步更新对应契约；只修改目标所需代码。

优先通过代码审查验证能静态判定的行为。只有审查无法覆盖具体风险时才运行最窄的现有测试，并按行为变化维护必要的断言。
新增热路径、锁、分配、codec 或间接层时必须作出 benchmark 决策。完整 `make verify` 用于明确要求的全量验收或提交前确认，日常修改不默认执行。

不得修改围栏、接口基线、阈值或文档声明来掩盖实现错误。
