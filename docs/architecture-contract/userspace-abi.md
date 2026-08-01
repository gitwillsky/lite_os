# 用户态与 ABI 契约

## Owner

- `syscall-abi` 独占已接入 Linux 64-bit asm-generic number 与 RISC-V 扩展编号；dispatcher 独占 number-to-handler mapping。
- 编译期选中的 `arch::user` backend 独占 raw syscall register codec、signal machine context、
  ELF machine/flags/HWCAP 与 architecture-private syscall number decode；generic syscall、process
  与 memory 不得解释这些 layout。decoder 通过编译期选中的普通后端函数调用，禁止 capability bool、
  单次使用的 trait 或零大小 dispatch type。
- syscall module 独占 raw UAPI codec、user-copy 和 errno translation；领域 module 独占行为与状态。
- `syscall::user_iovec::UserInputStaging` 独占 write/send copyin 的 initialized prefix；stack 与 heap storage 都以 `MaybeUninit<u8>` 准备，只有成功 user-copy 的 prefix 可投影为 backend `&[u8]`。
- task loader 独占 pathname/script rewrite；memory ELF loader 独占 ELF plan、mapping、initial stack
  与 rollback。Process 的 `ProcessPaths` 在同一锁下唯一拥有 cwd 与最终 main ELF
  opened-entry identity：fork 复制两个 identity，vfork child 取得独立 path owner，exec 只原子
  替换 executable。procfs 只投影 `/proc/<pid>/exe` magic link，不缓存第二份 pathname。
- `TaskManager::TimerQueue` 独占 timerfd immutable clock、setting 与 active deadline membership；
  task-domain `TimerFd` backend 独占未读 expiration counter 和 notification endpoint。fs 只通过
  `TimerFdBackend` OFD seam 消费 counter/readiness，不反向依赖 task；deadline 到期在 timer lock
  外发布 readiness，最后一个 backend Arc 析构必须移除 record，禁止 per-tick 扫描全部 descriptor。
- userspace builder 独占 target-native compiler/linker/compiler runtime 与固定 package/key/cache 输入：
  AArch64 使用 Clang、固定 `rust-lld` 和 hard-float AAPCS64 `aarch64-unknown-none`
  `compiler_builtins`；softfloat builtins 只属于 kernel，链接进 musl 会让 FP helper return ABI
  与调用方分裂。RISC-V 使用 GCC 与 `libgcc`。产品 userspace 每个架构只保留一条 runtime。
- Rust std builder 独占固定 rust-src `std/panic_abort` 与同 revision LLVM libunwind 的 source-list
  build；Cargo 最终链接由 build-std 的 `compiler_builtins` 独占，不能再追加 musl builder 的外部
  compiler runtime。最终 ELF 必须动态依赖唯一 musl `libc.so`，libunwind 只允许静态进入 consumer。
- `user/Cargo.toml` 与 `user/Cargo.lock` 是产品 Rust userspace 的唯一 workspace/依赖解析 owner；Cargo
  直接链接 `audio-service`、`compositor`、`lite-runtime`（库）与各 app bin、`session-launch`、`terminal-session` 最终 PIE，禁止 staticlib 中间产物、手工二次
  链接或每应用 lockfile。`linux-uapi` 独占 raw musl FFI 与 Linux layout/constant；唯一例外是
  `quickjs-runtime` 内固定 vendored QuickJS ABI，其他位置的 `extern "C"`/`#[link]` 由 architecture-check 拒绝。

## Interface

- 未接入 number 返回 `ENOSYS`；不得建立私有 number、错号转发、silent flag ignore 或 userspace compatibility shim。
- syscall matrix中的每个入口必须唯一归属一个领域文件，并明确 Complete/Partial、对象范围与已知缺口。
- Linux/AArch64 与 Linux/RISC-V register convention、signal frame、ELF/TLS 与 capability query 必须经静态 ABI backend；禁止 `dyn` dispatch、运行时 architecture 分支或 generic owner 依赖具体 layout。
- AArch64 ELF 必须是 `EM_AARCH64`（183），auxv HWCAP 只公布 FP 与 ASIMD；其静态 decoder
  不接纳编号 258，dispatcher 必须返回 `ENOSYS`。RISC-V decoder 唯一接纳该编号并投递既有
  `riscv_hwprobe` UAPI codec；禁止恢复 `SUPPORTS_*` flag 或 AArch64 hwprobe 假实现。
- ET_EXEC 无论是否携带 PT_INTERP 都以零 load bias 映射；PT_INTERP 必须是独立校验的 ET_DYN。
  把动态 ET_EXEC 误判为非法会让 musl loader 尚未执行就返回 `ENOEXEC`。
- AArch64 CPU 即使能 decode 未公布的 SVE/SME probe，也不得为其建立第二套 context state；Unknown、
  SVE-access 与 SME-access exception 必须统一强制投递 `SIGILL/ILL_ILLOPC`，使标准用户 signal
  handler 能恢复 feature probe。blocked/ignored consequence 仍按同步 fault policy 收敛为 default。
- AArch64 CPU-local initialization 必须把 `CNTKCTL_EL1` 精确设置为仅
  `EL0VCTEN`，并在既有 `SCTLR_EL1` 上设置 `UCT/UCI/DZE`，允许 EL0 读取 `CNTVCT_EL0` 与 cache
  geometry、执行当前地址空间指令发布和 cache-line zero；不得同时开放物理计数器、event stream
  或用户态 virtual/physical timer control。缺失这些权限时，V8 等标准 runtime 会在计时或
  `FlushICache` 陷入 unsupported system-register exception。
- signal frame capture、SA_RESTART 与 sigreturn register restore 都通过 Thread context owner；frame
  copyout 成功前不得发布 handler registers，clone child 可取得一次完整 machine snapshot。
- `ContextOwner<UserContext>` 必须用两个短 transaction 调用静态 backend 的
  illegal-instruction seam：第一次只产生 typed probe，transaction 外完成可能阻塞的指令读取，
  第二次提交 retry/fault。RISC-V 可在精确 F/D/FP-CSR 且 `FS=Off` 时返回 retry；AArch64
  直接返回 typed fault，不得保留恒 false decoder/activation compatibility pipeline，也不得让
  context claim 跨越 AddressSpace lock。trap/task signal owner 必须把未被
  architecture seam 消费的非法指令发布为当前
  Thread 的 forced SIGILL generation；首个 fault siginfo 编码 `si_code=ILL_ILLOPC` 与
  `si_addr=PC`。caught+unblocked disposition 保持 handler；blocked 或 `SIG_IGN` 必须在同一
  generation 事务中恢复 `SIG_DFL` 并解除屏蔽，forced consequence 必须绕过 PID 1
  `SIGNAL_UNKILLABLE`。同号 standard signal 已 pending 时保留首个可见 siginfo，仅合并 forced
  consequence；缺失该合并会让同步 fault 返回同一 PC 无限 trap 或错误吞掉 capability probe。
- architecture breakpoint 必须发布当前 Thread 的 forced `SIGTRAP/TRAP_BRKPT` 与 fault PC；
  trap layer 不推进 architecture PC，也不得绕过已注册 signal handler 直接退出。
- timerfd create/set/get/read 必须使用 Linux asm-generic 编号与 64-bit itimerspec/counter layout；
  timer replacement、deadline index 与 unread counter reset 构成同一串行 operation，readiness 必须进入
  既有 poll/epoll source。`TFD_TIMER_CANCEL_ON_SET` 在 realtime clock-set ABI 开放前明确返回 `EINVAL`。
- `ppoll` raw `pollfd` array 必须整批 copyin、解析并整批 copyout，不能按 fd 做 8-byte/2-byte
  微拷贝。DRM/evdev destructive event dequeue 必须先验证完整 batch，随后整批编码并一次 scatter；
  EFAULT 只允许保留此前完整 batch/vector 的 partial progress。
- pipe/socket/regular-file write 必须使用同一个 `UserInputStaging` seam；memory user-copy 直接初始化 `MaybeUninit<u8>` destination，不得为形成 `&mut [u8]` 预清零随后完整覆盖，也不得在各 syscall 保留 unsafe 转换分支。
- userspace application 不得依赖 LiteOS 私有 runtime、init、device protocol 或第二条 rootfs path。
- Rust application 必须使用标准 Linux/musl target；禁止 `os=none` custom target、预编译 bundled
  musl/CRT 或 LiteOS std fork。验证 fixture 只允许进入 disposable gate image，产品 rootfs 必须拒绝。
- 应用优先使用 `std`；稳定 `std` 缺失的 Linux 专有机制只能通过
  `linux-uapi::{alsa,drm,input,pty,process,shared_memory,unix}` 的安全 typed interface。
  `audio-proto`/`display-proto` 分别独占音频/显示 wire 与 SCM_RIGHTS 帧语义，但 fd ancillary
  mechanism 委托 `linux-uapi::unix`。raw syscall、应用私有 ABI、裸 fd/device owner 和并行兼容路径均禁止。
- 标准 `Command` spawn 的 AF_UNIX `SOCK_SEQPACKET|SOCK_CLOEXEC` socketpair 是 exec error
  publication owner；kernel 必须保留消息边界、peer-close EOF/hangup 与 `SO_TYPE=5`。只开放
  socketpair，seqpacket bind/listen/connect 仍明确返回不支持，不能在应用退回多线程不安全的 raw fork。
- 普通 `fork` 从多线程 parent 只复制 calling Thread，child 以独立 process owner、COW AddressSpace
  与调用时的 fd/credential/signal-action snapshot 发布；parent sibling 不进入 child graph。
  child 在 exec 前只能依赖 POSIX async-signal-safe 路径，kernel 不复制用户态锁状态。
- 多线程图形进程只能通过 `SessionChild` 的固定 `/bin/session-launch` 单轨启动 session child。
  parent 的标准 `Command` 不得安装 `pre_exec`；单线程 trampoline 依次设置 parent-death signal、
  复检 parent identity、`setsid` 并同 PID `exec`。parent 独占 exec-status 读端，trampoline
  独占 CLOEXEC 写端；固定 frame 发布所有确定性 setup/exec 错误，空 EOF 表示没有这类已发布错误，
  不虚报异步致死与成功 exec 后立即退出可区分。错误、非空截断或非法 frame 必须 kill/wait 回收；
  禁止普通 spawn、raw fork 或重试兼容路径。
- APK 只接受所选 architecture repository 的固定摘要与精确 `.PKGINFO`。只有 `ca-certificates-bundle`、`git-init-template` 与 `ncurses-terminfo-base` 三个固定数据包预期 `noarch`；其余包必须精确匹配目标架构，禁止 blanket `noarch` 放宽。
- `/etc/apk/repositories` 必须精确启用固定 Alpine stable branch 的 `main` 与 `community`；
  `main` 缺失会破坏基础包解析，`community` 缺失会让 npm 等标准拆分包不可见，禁止加入
  edge/testing 或其他 branch 形成未固定 ABI 回退。
- 产品 rootfs 必须把同一个 BusyBox `env` inode 发布到 `/bin/env` 与 `/usr/bin/env`；缺失后者会让
  npm/pnpm 等已成功安装的 `#!/usr/bin/env node` 入口在 exec 时错误返回 `ENOENT`。
- 产品 login profile 与图形 Terminal 的 PTY 会话必须使用同一标准 PATH 并包含
  `/usr/local/bin`；npm global prefix 和 Agent 开发环境发布的 command 位于该目录，任一
  owner 缺失时包已成功安装但对应交互 shell 仍会错误报告 command not found。增量用户态同步
  必须拥有 `/etc/profile`，否则旧持久镜像不会随产品契约修复。
- 产品 BusyBox 必须发布 `add-shell` 与 `remove-shell` applet；缺失时 Bash 等 stable APK 的
  maintainer script 无法通过标准 owner 更新 `/etc/shells`，安装只能留下部分配置。
- Codex/Claude 只属于 AArch64 持久开发实例 owner：固定 Node/npm APK closure 先离线安装，
  Guest npm 再从 host 按 registry SRI 校验并固定的 cache 把两个官方 package 全局安装到
  `/usr/local`。该 owner 必须验证 npm package identity、平台 optional package、APK metadata、
  package database、唯一 command 入口与真实 CLI 启动；不得污染产品 rootfs、加入滚动
  repository、保留 raw binary/Claude APK 第二路径，或把 namespace sandbox 宣称为已支持。

## Failure and cleanup

- exec 在 point of no return 前完成 source、ELF、stack 与 owner allocation；失败保持旧 image，提交后只允许新 image 或进程退出。
- ABI copyout 失败不得发布不可回收的 fd、timer、mapping、socket control 或 process identity；
  `recvmsg` 收到的 fd 必须等 name、control 和 msghdr metadata 全部 copyout 成功后整批发布。
- copyin fault 只允许发布已完成的 initialized prefix；atomic socket message 丢弃该 prefix，regular write 可按既有 partial-write policy 提交它，任何路径都不得读取未发布 suffix。
- compiler、linker、compiler runtime、ELF machine 或 APK name/version/arch/摘要不匹配时，必须在 sysroot、rootfs 或 cache generation 发布前 fail-stop；临时下载和未发布 generation 必须清理，其他架构 cache 不得作为回退。
- rust-src、LLVM libunwind input、build-std feature、target linker 或唯一动态 dependency 不匹配时，
  必须在 std consumer generation 发布前 fail-stop；未完成的 object/Cargo target directory 必须清理。
- Linux `clone` 的 `CLONE_PARENT_SETTID`/`CLONE_CHILD_SETTID` 是例外的 best-effort store：
  Thread identity 先发布但保持 `New`，store fault 不回滚、不改成功返回；全部 store 尝试
  完成后才按 process job-control 原子转为 Ready/Stopped。并发 `exit_group` 已提交时，新
  child 在变为 Ready 前继承 kernel SIGKILL，parent-visible exit status 仍由首次
  group-exit owner 决定。
