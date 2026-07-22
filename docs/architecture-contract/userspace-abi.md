# 用户态与 ABI 契约

## Owner

- `syscall-abi` 独占已接入 Linux 64-bit asm-generic number 与 RISC-V 扩展编号；dispatcher 独占 number-to-handler mapping。
- 编译期选中的 `arch::user` backend 独占 raw syscall register codec、signal machine context、
  ELF machine/flags/HWCAP 与 architecture-private syscall number decode；generic syscall、process
  与 memory 不得解释这些 layout。decoder 通过编译期选中的普通后端函数调用，禁止 capability bool、
  单次使用的 trait 或零大小 dispatch type。
- syscall module 独占 raw UAPI codec、user-copy 和 errno translation；领域 module 独占行为与状态。
- `syscall::user_iovec::UserInputStaging` 独占 write/send copyin 的 initialized prefix；stack 与 heap storage 都以 `MaybeUninit<u8>` 准备，只有成功 user-copy 的 prefix 可投影为 backend `&[u8]`。
- task loader 独占 pathname/script rewrite；memory ELF loader 独占 ELF plan、mapping、initial stack 与 rollback。
- userspace builder 独占 target-native compiler/linker/compiler runtime 与固定 package/key/cache 输入：
  AArch64 使用 Clang、固定 `rust-lld` 和 hard-float AAPCS64 `aarch64-unknown-none`
  `compiler_builtins`；softfloat builtins 只属于 kernel，链接进 musl 会让 FP helper return ABI
  与调用方分裂。RISC-V 使用 GCC 与 `libgcc`。产品 userspace 每个架构只保留一条 runtime。
- Rust std builder 独占固定 rust-src `std/panic_abort` 与同 revision LLVM libunwind 的 source-list
  build；Cargo 最终链接由 build-std 的 `compiler_builtins` 独占，不能再追加 musl builder 的外部
  compiler runtime。最终 ELF 必须动态依赖唯一 musl `libc.so`，libunwind 只允许静态进入 consumer。
- `user/Cargo.toml` 与 `user/Cargo.lock` 是产品 Rust userspace 的唯一 workspace/依赖解析 owner；Cargo
  直接链接 `compositor`、`lite-ui`、`terminal-session` 最终 PIE，禁止 staticlib 中间产物、手工二次
  链接或每应用 lockfile。`linux-uapi` 独占 raw musl FFI 与 Linux layout/constant；唯一例外是
  `quickjs-runtime` 内固定 vendored QuickJS ABI，其他位置的 `extern "C"`/`#[link]` 由 architecture-check 拒绝。

## Interface

- 未接入 number 返回 `ENOSYS`；不得建立私有 number、错号转发、silent flag ignore 或 userspace compatibility shim。
- syscall matrix中的每个入口必须唯一归属一个领域文件，并明确 Complete/Partial、对象范围与已知缺口。
- Linux/AArch64 与 Linux/RISC-V register convention、signal frame、ELF/TLS 与 capability query 必须经静态 ABI backend；禁止 `dyn` dispatch、运行时 architecture 分支或 generic owner 依赖具体 layout。
- AArch64 ELF 必须是 `EM_AARCH64`（183），auxv HWCAP 只公布 FP 与 ASIMD；其静态 decoder
  不接纳编号 258，dispatcher 必须返回 `ENOSYS`。RISC-V decoder 唯一接纳该编号并投递既有
  `riscv_hwprobe` UAPI codec；禁止恢复 `SUPPORTS_*` flag 或 AArch64 hwprobe 假实现。
- AArch64 CPU 即使能 decode 未公布的 SVE/SME probe，也不得为其建立第二套 context state；Unknown、
  SVE-access 与 SME-access exception 必须统一强制投递 `SIGILL/ILL_ILLOPC`，使标准用户 signal
  handler 能恢复 feature probe。blocked/ignored consequence 仍按同步 fault policy 收敛为 default。
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
- `ppoll` raw `pollfd` array 必须整批 copyin、解析并整批 copyout，不能按 fd 做 8-byte/2-byte
  微拷贝。DRM/evdev destructive event dequeue 必须先验证完整 batch，随后整批编码并一次 scatter；
  EFAULT 只允许保留此前完整 batch/vector 的 partial progress。
- pipe/socket/regular-file write 必须使用同一个 `UserInputStaging` seam；memory user-copy 直接初始化 `MaybeUninit<u8>` destination，不得为形成 `&mut [u8]` 预清零随后完整覆盖，也不得在各 syscall 保留 unsafe 转换分支。
- userspace application 不得依赖 LiteOS 私有 runtime、init、device protocol 或第二条 rootfs path。
- Rust application 必须使用标准 Linux/musl target；禁止 `os=none` custom target、预编译 bundled
  musl/CRT 或 LiteOS std fork。验证 fixture 只允许进入 disposable gate image，产品 rootfs 必须拒绝。
- 应用优先使用 `std`；稳定 `std` 缺失的 Linux 专有机制只能通过
  `linux-uapi::{drm,input,pty,process,unix}` 的安全 typed interface。`display-proto` 独占 wire 与
  SCM_RIGHTS 帧语义，但 fd ancillary mechanism 委托 `linux-uapi::unix`。raw syscall、应用私有 ABI、
  裸 fd/GEM owner 和并行兼容路径均禁止。
- 标准 `Command` spawn 的 AF_UNIX `SOCK_SEQPACKET|SOCK_CLOEXEC` socketpair 是 exec error
  publication owner；kernel 必须保留消息边界、peer-close EOF/hangup 与 `SO_TYPE=5`。只开放
  socketpair，seqpacket bind/listen/connect 仍明确返回不支持，不能在应用退回多线程不安全的 raw fork。
- APK 只接受所选 architecture repository 的固定摘要与精确 `.PKGINFO`。只有 `ca-certificates-bundle`、`git-init-template` 与 `ncurses-terminfo-base` 三个固定数据包预期 `noarch`；其余包必须精确匹配目标架构，禁止 blanket `noarch` 放宽。

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
