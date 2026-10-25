# SDK、二进制兼容与系统内开发（实施中）

Status: Active

阶段：用户已确认，开始实施；进度与未完成范围在本计划中维护。

目标：应用能够独立发布、兼容升级，并直接在 LiteOS 中编辑、编译、链接和运行。
这是应用框架公共接口的前置设计；先落地 ABI 与运行时，再推进工具链与应用迁移。

已确认的方向：Rust 是应用开发的一等语言，本方案不交付 C/C++ 应用 SDK。
控件必须由共享运行库提供；兼容升级后，旧应用无需重新编译即可获得控件修复。

实施进度：版本化函数表、共享库构建/安装、安全 Rust 句柄与异步适配、SDK 源码与 Cargo 示例已接入。
七个普通系统应用与终端已迁移到共同 SDK，系统内部与外部应用使用同一控件实现。
固定原生 Rust/Cargo 安装入口已实现；guest 内完整开发闭环、旧 ELF 升级验收和源码调试仍未完成。
服务端 GPU 合成已有代码接入，标准 GPU 客户端缓冲仍未实现；当前使用方式见 [SDK 开发](../development/ui-sdk.md)。
本轮按用户要求不执行测试或 GUI 验收，不以 host 编译检查代替这些验收。

## Windows 给出的经验

Windows 应用通过 SDK 的声明与受支持的系统 DLL 接口交互；头文件同时声明目标系统版本。
参考 [Windows SDK 头文件](https://learn.microsoft.com/en-us/windows/win32/winprog/using-the-windows-headers)。
这使应用面向系统接口构建，系统实现使用的编译器无需成为所有应用的精确版本约束。
C++ 对象、STL、CRT 与跨模块释放仍有额外限制；Windows 2000 时代也不能任意混用 C++ 工具链的内部对象。
现代 MSVC 的指定版本系列有明确的二进制兼容范围，仍受链接器、runtime 与 LTO 等条件限制。
参考 [MSVC 兼容范围](https://learn.microsoft.com/en-us/cpp/porting/binary-compat-2015-2017)。

因此，公开接口的调用约定、数据布局、所有权、语义和兼容政策才是 SDK 承诺。
库后缀、使用 Rust 实现、或统一某一版编译器，都不能单独提供这一承诺。

## LiteOS 当前事实

- 官方 Linux/musl target、ELF 动态加载和自带 musl 开发头文件、CRT、静态库已有基础。
- 系统组件以 Rust dylib 共享 std；libliteos_runtime.so 是同一构建集合内的实现方案。
  将它类比 msvcrt.dll 只能说明职责，不能说明具有同等级二进制兼容契约。
  [Rust ABI 不提供稳定性保证](https://doc.rust-lang.org/reference/items/external-blocks.html#abi)。
- Rust std gate 在宿主构建程序、在 guest 验证运行；未证明 guest 内 rustc/cargo 的完整编译闭环。
- Agent 开发镜像安装 Node/npm 等工具，并未验证原生 Rust 编译工具链。
- 有 UART shell、PTY/termios、共享控件记事本与终端应用；后两者当前实现尚未进行 guest/UI 验收。
- 当前 ABI 矩阵未声明 ptrace；现有宿主内核 GDB 工作流不能等同于 guest 内应用源码调试。

## 建议的 ABI 边界

```text
应用自己的业务代码与语言运行时
    → 随应用编译的 Rust 安全 SDK
    → SDK 内维护的版本化稳定二进制边界（目标 C 调用约定）
    → 框架运行库的一份内部实现
    → 标准 Wayland、Linux UAPI、musl
```

- 公开运行库可由 Rust [cdylib](https://doc.rust-lang.org/reference/linkage.html) 实现，内部 Rust 依赖与 std 优先静态纳入该边界并隐藏非公开符号。
  不直接暴露 Rust dylib metadata、Rust 符号或系统 nightly 版本给第三方应用。
- Rust SDK 是普通源码 crate，提供 RAII、类型化事件、闭包和错误封装，内部调用稳定 ABI。
  采用目标 C 调用约定是二进制互操作机制，不要求开发者写 C，也不承诺 C/C++ SDK。
- 共享运行库拥有控件交互、布局、绘制与默认行为；Rust SDK 只拥有安全绑定及应用回调适配。
  若把控件实现内联进 SDK，旧应用将无法获得这部分修复，因此不能以静态控件 crate 替代共享运行库。
- 应用通常自行编译链接自己的 Rust std 与纯 Rust 依赖，不强制链接系统 libliteos_runtime.so。
  系统内部仍可按一个 release 集合统一构建；第三方只要求受支持的架构、ABI、SDK 和最低运行库版本。
- 稳定边界可能增加每个应用的运行时代码体积；先测量，再优化共享范围，不把“全系统一份 std”作为 SDK 硬约束。
- 不并行维护两套控件实现；迁移后系统应用与外部应用消费同一份公共框架运行库。
  内部 crate 的源码拆分不必等同于对外共享库的拆分，不为每个小模块承诺永久二进制 ABI。

这属于进程内公开 toolkit API，不新增私有 syscall、Wayland 扩展或应用绘图 IPC。
第三方 toolkit 仍可以直接使用标准 Wayland，无须依赖 classic-ui。

## 必须先定的公共契约

| 项目 | 建议 |
|---|---|
| 函数与数据 | 标准 C 调用约定、明确尺寸的整数、约定布局的结构与不透明对象句柄 |
| 语言类型 | String/Vec/Result/trait object/闭包可出现在安全 Rust API 中，但不直接穿过 ABI |
| 字符串与缓冲 | 明确 UTF-8、长度、借用期限、可写范围；保留方必须按约定复制或接管 |
| 创建与释放 | 分配模块提供配对释放；不跨模块释放 Rust 对象；句柄失效及重复销毁有明确结果 |
| 回调与线程 | UI 对象绑定创建线程；安全绑定不允许任意 Send/Sync；撤销回调后不得再使用 userdata |
| 错误 | 明确状态与错误结果；Rust panic 不得跨接口边界，不能用 catch_unwind 掩盖 panic=abort |
| ABI 演进 | SONAME 主版本；同主版本保留旧符号、语义和布局；扩展结构按尺寸/版本协商 |
| 未知数据 | 事件标签在线上用整数表示；旧 SDK 可跳过未知事件，不构造无效 Rust enum |
| 应用依赖 | 声明最低运行库版本；旧应用运行于兼容的新运行库，新应用所需的新能力不默认存在于旧系统 |
| 版本独立 | 分别记录 OS release、SDK API、运行库 ABI、应用工具链；不以 rustc 版本代替 ABI 版本 |

所有权规则也适用于 Windows DLL，参见 [跨 DLL 的 CRT 与内存问题](https://learn.microsoft.com/en-us/cpp/c-runtime-library/potential-errors-passing-crt-objects-across-dll-boundaries)。
冻结 ABI 前需补齐各目标调用约定和数据布局的一手规范基线，按目标 C ABI 验证。

仅把 dylib 改成 cdylib，或加 extern "C"/repr(C)，仍不足以承诺这些兼容规则。
当前 FFI 围栏把所有 C 声明限制在 linux-uapi；正式实施时需按 owner 区分设备/libc FFI 与公开 SDK bindings，
更新契约和对应围栏，不能把上层 UI 绑定塞入 linux-uapi，也不能绕开检查。

ABI 回调在一次事件分发中完成；回调请求销毁对象时延迟回收，异步结果校验对象代际后才投递。
运行库不持锁或保留可变借用调用应用代码；闭包的注册、撤销、销毁必须有完整的安全证明。
绘制接口按文本、图元或批次传递，避免逐像素、逐字符调用 ABI；最小切片测量跨边界成本再冻结粒度。

## Rust 异步 API 与稳定边界

安全 SDK 提供可组合的 Future 与 async/await；具体任务执行模型由[框架方案](classic-ui-framework.md#rust-编程模型与异步执行)拥有。
Future、Pin、Waker、trait object 和闭包的 Rust 布局不跨共享库边界；应用侧 SDK 拥有其 Future 存储与轮询适配。
运行库以不透明操作标识、完成结果与配对取消/释放接口对接，由其唯一事件循环调度通知。
完成通知通过稳定调用约定的 thunk 进入 SDK，不能把某个 Rust executor 的内部对象暴露成系统契约。
已撤销回调的 userdata 只有在运行库保证不再调用后才释放；取消与完成竞争、重复唤醒和迟到通知必须有明确状态转移。
非 Send 的 UI Future 留在 UI 线程；跨线程的唤醒与任务数据分别证明安全，不用 unsafe Send 绕过约束。
旧 SDK 生成的异步应用也纳入运行库升级兼容验证；不能只验证同步窗口创建仍可链接。

## 运行库与资源升级

兼容升级在应用重启后生效，不承诺对正在运行的对象热替换函数实现。
安装器原子发布新文件，不原地改写已映射的共享库；现有进程继续使用其已加载的版本。
字体、主题、图标和布局资源也必须保持旧 schema 可读，或按版本保留旧进程需要的资源；
仅原子替换 .so 而使资源随即失效，不能算兼容升级。

## SDK 与开发环境交付

运行库包提供执行所需共享库和资源；SDK 包提供 Rust crate、链接元数据、模板、文档与示例。
开发工具包先提供经验证的 rustc/cargo、匹配的标准库和链接器。
Rust crate 的原生依赖若需要 C 编译工具等，按实际依赖单独验证，不扩大为 C/C++ 应用 SDK 承诺。
这些是拟定的包职责，沿用现有包管理，不新建并行安装器。

先以 AArch64 Linux/musl 为完整本机开发目标；上游已有带 host tools 的该 target，但在 LiteOS 上仍须实际验证。
参考 [Rust 原生 host tools 支持](https://doc.rust-lang.org/rustc/platform-support.html)。
不要求应用安装内核的 nightly，不另设 LiteOS 专用 target 或修改编译器绕过缺失 syscall。
外部交叉开发使用 SDK sysroot，guest 本机开发使用安装后的同一套公开接口，产物不得依赖宿主绝对路径。
Rust 绑定的源码兼容遵循明确的最低 Rust 版本；不同工具链无需与 OS 精确相同，但必须在支持范围内。

## 步骤 → 验证点

1. 公共 ABI 最小切片：窗口、事件循环、按钮和资源释放；先明确契约，再冻结第一版接口。
   → 两个受支持 Rust 工具链构建的应用连接同一运行库；替换兼容运行库后，保留的旧 ELF 不重编仍通过。
   → 旧应用重启后获得控件修复；未知事件、回调内关闭、异步完成/取消、分配释放与错误清理符合契约。
2. 本机编译闭环：通过现有包管理安装固定工具链与 SDK，从 UART shell 起步。
   → guest 内创建新项目、编译链接、执行、修改后增量编译、重启后再执行，全程不从宿主同步编译产物。
   → 覆盖普通 crate、build.rs、proc-macro、Cargo 离线依赖与一项实际需要的原生依赖构建。
     编译期间的子进程、链接与文件发布均须在 guest 完成，不能只测 rustc --version 或单个 hello。
3. 迁移系统应用与交付开发工作流。
   → 同一框架实现供内置/第三方应用使用；终端应用复用现有 PTY/termios 与终端模型，GUI 由用户验收。
   → 支持项目级 build/run/test、错误定位、增量构建与正常用户工作目录，无需重建系统镜像。
4. 应用源码调试作为独立标准能力建设。
   → 按原生调试器实际需求补齐 ptrace、wait/信号、寄存器和内存访问；验证断点、单步、变量与多线程停止/恢复。
   → 保留旧 SDK 编译的二进制验兼容，不用新编译的“旧源码”替代真正的旧二进制。

这些前置工作应排在大规模控件 API 和应用迁移之前；完整 IDE 可以随后接入。
