# 用户态与 ABI 当前架构

## 当前设计

- kernel 暴露固定 Linux 64-bit asm-generic UAPI 子集。syscall dispatcher 使用共享编号 crate；寄存器调用约定、signal frame、ELF machine/flags/HWCAP 与 architecture-specific query 由编译期静态 userspace ABI backend 提供，未接入编号返回 `ENOSYS`。
- ELF loader 支持当前声明的 AArch64 与 RV64 静态/动态 ET_EXEC、动态 PIE、PT_INTERP、TLS、RELRO、
  auxv 与 Linux script rewrite；filesystem 只提供 executable source seam，memory 拥有映射
  与 initial stack。AArch64 只接受 `EM_AARCH64`（183），向 auxv 公布 FP 与 ASIMD HWCAP；
  RISC-V 保留既有 ELF flags、HWCAP 与 hwprobe 投影。
- Apple Silicon/HVF 可能让 EL0 probe 被 CPU decode 为 SVE/SME access trap，即使 auxv 未公布该能力；
  backend 把 Unknown/SVE/SME probe 统一投递为可捕获的 `SIGILL/ILL_ILLOPC`，不保存或启用扩展 state。
- AArch64 每个 CPU 初始化时通过 `CNTKCTL_EL1.EL0VCTEN` 开放只读虚拟计数器，并通过
  `SCTLR_EL1.UCT/UCI/DZE` 开放 cache geometry 读取、当前地址空间指令发布与 cache-line zero，
  供 V8 等标准 runtime 计时和发布 JIT code；物理计数器、event stream 与虚拟/物理 timer control
  仍保持 trap。
- Linux `riscv_hwprobe` 编号 258 只由 RISC-V backend 开放；AArch64 没有该 key space，必须返回 `ENOSYS`，不能伪造空 capability success。
- 用户态非法指令生成 thread-directed forced SIGILL；首个可见 standard siginfo 使用
  `ILL_ILLOPC` 与 fault PC (`si_addr`)。caught 且未屏蔽时进入已注册 handler；blocked 或
  `SIG_IGN` 时恢复默认 disposition 并解除屏蔽，默认动作对 PID 1 也不豁免。RISC-V lazy FP
  指令必须先由 architecture backend 激活并原 PC 重试，只有未被该机制消费的指令生成 SIGILL。
- 用户态 breakpoint 生成可捕获的 forced `SIGTRAP/TRAP_BRKPT`，而不是由 trap layer 直接终止；
  signal handler 可按标准 machine context 决定恢复位置。
- Process 的 `ProcessPaths` 在同一锁下保存 cwd 与最终 main ELF 的 VFS opened-entry
  identity；fork/vfork child 复制 path owner，exec 原子替换 executable。`/proc/<pid>/exe`
  因此可跟随 rename/unlink 并作为标准 magic link 再打开当前 executable。
- timerfd 是标准匿名 OFD：`TimerQueue` 独占 clock/setting/deadline index，`TimerFd` 独占未读
  expiration counter 与 poll/epoll readiness；最后一个 fd/OFD 引用释放时同步撤销 timer record。
- 产品 userspace 是按所选架构原生构建的固定 musl runtime、BusyBox `init + ash`、普通 Rust `std`
  binary `audio-service`/`compositor`/`desktop` 等 app bin/`terminal-session`、
  `audio-proto`/`quickjs-runtime`/`display-proto`/`linux-uapi` library 和单 ELF
  `liteos-stress` diagnostics。`user/` 是单一 Cargo workspace 与 lockfile；
  kernel、rootfs、APK 与 cache 都携带同一个 architecture identity。
- 标准 Rust consumer 使用官方 `aarch64-unknown-linux-musl`/`riscv64gc-unknown-linux-musl`
  target 与普通 `fn main`；builder 从固定 rust-src 构建 `std + panic_abort`，从同一源码树构建并
  静态链接 LLVM libunwind，最终动态 runtime 仍只有固定 musl。`rust-std-smoke` 只注入 disposable
  gate image，覆盖 allocator/RandomState、filesystem、Thread/TLS、process、AF_UNIX 与 IPv4 client，
  不进入产品 rootfs。
- Rust `std` 已按 target 提供文件、socket、process、thread/TLS、时间与集合等稳定 OS façade，因此
  应用不再重复声明这些 FFI。ALSA/DRM/evdev/PTY ioctl、memfd、`poll` 与 SCM_RIGHTS 等 Linux
  专有 UAPI 不属于跨平台 `std` 稳定 surface，由内部
  `linux-uapi::{alsa,drm,input,pty,process,shared_memory,unix}` 深模块独占 raw musl
  FFI、layout/常量和 RAII；应用、`audio-proto` 与 `display-proto` 只消费安全 typed interface。
- write/send 的 stack/heap staging 统一由 `UserInputStaging` 管理 initialized prefix，memory copyin 直接写未初始化 storage。代表样本包含两条 64 KiB socket staging 和一条 1 MiB regular staging，共 1,179,648 bytes；其 copyin 前预清零成本降为 0。
- 普通 fork 从多线程 parent 只复制 calling Thread，child 取得独立 process owner 与 COW
  AddressSpace；这使 Node/libuv 等标准 launcher 可在 sibling Thread 存活时 fork/exec，
  而 parent 的其他 Thread 不进入 child graph。多线程 Process 原地 exec 仍未开放。
- rootfs 由对应 Alpine architecture repository 的固定 package/key/摘要输入构造；运行时
  `/etc/apk/repositories` 只启用同一稳定分支的 `main` 与 `community`，不接入 edge/testing。
  BusyBox `env` 同时发布标准 `/bin/env` 与 `/usr/bin/env`；login profile 与图形 Terminal 的
  PTY 会话都把 `/usr/local/bin` 纳入同一标准 PATH，使 npm/pnpm 等全局安装的
  `#!/usr/bin/env node` 入口和开发实例中的 Codex 可直接执行。增量用户态同步拥有
  `/etc/profile`，已有持久镜像不会继续保留旧路径。
  图形 Terminal 的确定性 PTY 环境还显式发布 `USER=root` 与 `SHELL=/bin/sh`；Claude 等需要
  user/shell discovery 的原生 CLI 不得依赖被 `env_clear` 移除的宿主环境。
  BusyBox 同时提供 `add-shell`/`remove-shell`，使 Bash 等 stable APK 的 maintainer script
  通过标准 `/etc/shells` owner 完成安装。
  应用与 terminal 只通过标准 Linux process、fd、PTY、termios、socket 和 ELF ABI 交互。
- 固定产品 rootfs 不安装 Codex/Claude。AArch64 持久开发实例通过显式
  `prepare-agent-development` 离线安装固定 Node/npm APK closure，再由 Guest npm 从
  registry SRI 固定的 cache 全局安装 Codex 与 Claude；安装只发布 `/usr/local` 的 npm owner，
  不保留 raw binary 或 Claude APK 第二路径，也不修改产品 `/etc/apk/repositories`。

## Known limits

- 支持矩阵只证明列出的 syscall、对象类型和 consumer，不宣称完整 Linux、POSIX 或任意 musl 程序兼容。
- Rust std gate 只证明列出的 vertical slice；不外推 panic unwind、全部 allocator size、IPv6、
  async runtime、直接使用 raw syscall 的 crate 或完整 `std::os::linux` 能力。
- AArch64 与 RISC-V backend 只声明各自门禁覆盖的 register、signal、ELF/TLS 与 capability 语义；共享 asm-generic 编号不意味着 architecture-specific UAPI 可互换。
