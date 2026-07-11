# Phase 16：RV64 signal frame 与 thread-directed delivery

## 固定 ABI

实现以项目固定的 Linux v7.1 commit `8cd9520d35a6c38db6567e97dd93b1f11f185dc6` 为准：`sigset_t` 为 8 bytes，RV64 sigaction 为 handler/flags/mask 三个 word，rt frame 为 128-byte siginfo 加 952-byte ucontext。sigcontext 保存 32-word `user_regs_struct` 与 528-byte FP/extension union。

## 所有权

- `Arc<Process>` 的 signal-actions table 是 disposition 唯一 owner，fork 复制、thread clone 共享、exec 重置。
- `ThreadContext` 唯一拥有 mask 和 coalesced standard-signal pending bitset。
- 用户栈上的 rt_sigframe 是 handler 活跃期间唯一 saved-context owner；kernel 不保留第二份 shadow frame。

## Delivery 与 return

trap return 选择最低未屏蔽 pending signal。默认 disposition终止，SIG_IGN/SIGCHLD default 被忽略；handler delivery 保存 GPR、FP、fcsr、PC 和旧 mask，设置 a0/a1/a2，并将 RA 指向与 kernel trampoline 同物理页的独立 U|RX signal-return alias。`rt_sigreturn` 从 sp 指向的 Linux frame恢复完整上下文，并要求当前未支持的 extra-extension reserved word 与 END header 保持为零。

当前 `tgkill` 对 running/ready target 成立；blocked futex/deadline/wait4 尚不因 signal 移除 membership，因此不声明 EINTR/SA_RESTART。无 altstack、queued realtime payload、vector/CFI extension context、process-directed kill 与自动 SIGCHLD。

## 启动验收

init 安装 SIGUSR1 handler，先屏蔽并 `tgkill` 自身，确认 pending 未提前执行；解除屏蔽后 handler 经 rt frame执行并通过 signal trampoline `rt_sigreturn`，恢复到原控制流并输出 `signal ok`。QEMU `-smp 1/3/8` 均必须通过。

最终验收执行 `make verify`：格式、架构围栏、workspace check、clippy、三组件构建、ELF 静态检查与 QEMU `-smp 1/3/8` 冷启动全部通过。
