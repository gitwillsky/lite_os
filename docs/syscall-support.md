# LiteOS Linux/riscv64 syscall 支持矩阵

> 更新日期：2026-07-12（Asia/Shanghai）
>
> Linux UAPI 基线：Linux `v7.1`，commit `8cd9520d35a6c38db6567e97dd93b1f11f185dc6`
>
> POSIX 语义基线：POSIX.1-2024 / Issue 8
>
> musl consumer 基线：musl `v1.2.6`，commit `9fa28ece75d8a2191de7c5bb53bed224c5947417`

## 1. 全局契约

- U-mode `ecall`：`a7=number`，`a0..a5=args`，`a0=result`。
- kernel error 为 `-errno`；user raw wrapper 不伪造 libc `errno`。
- `syscall-abi` 只定义下表 57 个 Linux/riscv64 number。
- dispatcher 对所有其他 number 统一返回 `-ENOSYS`。
- 没有 LiteOS 私有 syscall number、旧编号转发、deprecated 入口或 feature-flag 双轨。

状态含义：

| 状态 | 含义 |
|---|---|
| Complete | 在当前明确声明的 process/thread/fd 对象模型内，无已知的本 syscall 契约偏差。 |
| Partial | 入口可用，但已知缺失在表中精确列出。 |
| Missing | 尚无正确实现；当前 `-ENOSYS`。 |
| Not Planned | 当前收敛基线不计划接入；当前 `-ENOSYS`。 |
| Removed | 曾有错号、私有或语义不完整实现，已整链删除；当前 `-ENOSYS`。 |

`Complete` 不能外推为完整 Linux/POSIX/musl 兼容。例如 `set_tid_address` 的 clear/wake 契约成立，不表示 futex PI/requeue、所有 syscall 的 restart 或完整 pthread runtime 已成立。

## 2. 当前暴露的 57 个入口

| 编号 | Linux 名称 | 参数 / userspace ABI | 返回与 errno | POSIX / musl 路径 | 状态与精确边界 | 代码 |
|---:|---|---|---|---|---|---|
| 17 | `getcwd` | `char *buf, size_t size` | 含 NUL 的长度；`ERANGE/EFAULT/ENOENT` | POSIX `getcwd`；musl direct wrapper | **Complete**（当前单 root namespace）。从 cwd inode 沿 VFS 目录项反向生成 raw absolute path，不缓存 rename 后会漂移的字符串；目录不可达返回 `ENOENT`。 | `kernel/src/syscall/fs.rs`, `kernel/src/fs/vfs.rs` |
| 23/24 | `dup` / `dup3` | fd、目标 fd、`O_CLOEXEC` | 新 fd；`EBADF/EINVAL` | POSIX dup；Linux dup3 | **Complete**。fd entry 复制后共享同一 OFD offset/status flags；descriptor flag 独立。 | `kernel/src/syscall/fs.rs` |
| 25 | `fcntl` | fd、command、argument | command 对应值；`EBADF/EINVAL` | POSIX fcntl | **Partial**。实现 `F_DUPFD/F_GETFD/F_SETFD/F_GETFL/F_SETFL/F_DUPFD_CLOEXEC`；`F_SETFL` 当前只允许修改 `O_APPEND`。 | `kernel/src/syscall/fs.rs` |
| 29 | `ioctl` | fd、request、request-specific argument | 0；`EBADF/ENOTTY/EFAULT/EINVAL/EPERM` | musl termios；BusyBox init/ash | **Partial**。唯一 Terminal OFD 支持 `TCGETS/TCSETS*`、`TIOCSCTTY`、`TIOCGPGRP/TIOCSPGRP`、`TIOCGWINSZ/TIOCSWINSZ`、`TIOCGSID`；无伪成功 fallback。尚无完整 VMIN/VTIME、TCSETSW drain、TCSETSF flush、TIOCNOTTY 与 device node。 | `kernel/src/syscall/tty.rs`, `kernel/src/fs/file.rs` |
| 34/35 | `mkdirat` / `unlinkat` | dirfd、raw path、mode/`AT_REMOVEDIR` | 0；标准 pathname errno | POSIX mkdirat/unlinkat | **Partial**。支持绝对路径、`AT_FDCWD` 和目录 fd；无 credentials、mount point 与 symlink following。 | `kernel/src/syscall/fs.rs` |
| 46 | `ftruncate` | fd、64-bit length | 0；`EBADF/EISDIR/ENOSPC/EIO` | POSIX ftruncate | **Complete**。支持稀疏扩展、尾块清零、direct/三级 indirect 回收并维护 `i_blocks`。 | `kernel/src/syscall/fs.rs` |
| 49 | `chdir` | NUL 结尾 raw pathname | 0；`EFAULT/ENOENT/ENOTDIR/ELOOP/ENOMEM/EIO` | POSIX chdir；musl direct wrapper | **Partial**。absolute/relative、`.`/`..` 与重复分隔符统一经 VFS cwd inode 解析；fork 共享初始 identity 后各 Process 独立替换。无 credentials/execute permission、mount 与 symlink following。 | `kernel/src/syscall/fs.rs`, `kernel/src/task/model.rs` |
| 56/57 | `openat` / `close` | dirfd、raw path、flags/mode；fd | fd/0；标准 fd/path errno | POSIX openat/close | **Partial**。regular/directory OFD、create/excl/trunc/append/directory/cloexec 完整；无 permissions、symlink following 和设备节点。 | `kernel/src/syscall/fs.rs` |
| 59 | `pipe2` | `int[2]`、`O_CLOEXEC/O_NONBLOCK` | 0；`EFAULT/EINVAL/EMFILE/ENOMEM` | POSIX pipe；BusyBox pipeline | **Complete**（当前 fd/wait 模型）。64 KiB ring、4096-byte PIPE_BUF atomic write、blocking/nonblocking、EOF、EPIPE+SIGPIPE、fork/dup endpoint lifecycle 与 exit-time fd close 共用单一 Pipe owner。 | `kernel/src/ipc.rs`, `kernel/src/syscall/fs.rs` |
| 61 | `getdents64` | fd、`linux_dirent64` buffer、length | bytes；`EBADF/ENOTDIR/EFAULT/EINVAL` | libc readdir backend | **Complete**。目录 OFD 保存逐项位置，记录按 8 bytes 对齐并包含 `d_type`；并发目录变更后的 cookie 与 Linux 一样不承诺快照语义。 | `kernel/src/syscall/fs.rs` |
| 62 | `lseek` | fd、signed offset、whence | new offset；`EBADF/EINVAL/ESPIPE` | POSIX lseek | **Complete**。实现 `SEEK_SET/CUR/END`，offset 位于共享 OFD。 | `kernel/src/syscall/fs.rs` |
| 63-66 | `read/write/readv/writev` | fd、buffer 或 RV64 iovec | byte count；partial/EOF；标准 fd/stream errno | POSIX I/O；musl/BusyBox | **Partial**。regular file 共享 offset；Terminal 使用 termios；Pipe 支持 blocking/nonblocking、scatter readv、gather writev、EOF 与 EPIPE/SIGPIPE。iovec 最大 1024、总长不超过 SSIZE_MAX。无 socket；Terminal 仍缺完整 VMIN/VTIME/background enforcement。 | `kernel/src/syscall/fs.rs` |
| 73 | `ppoll` | pollfd array、relative timespec、可选 8-byte sigmask | ready count/0；`EINTR/EFAULT/EINVAL/ENOMEM` | POSIX poll/ppoll；musl/BusyBox line editing | **Complete**（当前 OFD kinds）。一次 registration 可索引多个 Pipe/Console source；regular inode按请求立即 ready，invalid fd 返回 POLLNVAL，Pipe 提供 POLLIN/POLLOUT/HUP/ERR，timeout 使用 monotonic deadline，临时 mask 在 ready/timeout 或 signal frame 正确恢复。 | `kernel/src/syscall/poll.rs`, `kernel/src/task/task_manager.rs` |
| 79/80 | `newfstatat` / `fstat` | dirfd/fd、path、RV64 `struct stat` | 0；fd/path/copyout errno | POSIX stat/fstatat | **Partial**。128-byte asm-generic 布局与 512-byte `st_blocks` 正确；`newfstatat` 当前只接受 flags 0。 | `kernel/src/syscall/fs.rs` |
| 81 | `sync` | 无参数 | 按 Linux ABI 固定返回 0，单个 writeback error 不从该入口报告 | POSIX sync；BusyBox sync applet | **Complete**（当前单 root filesystem）。经 VFS 唯一 root seam 等待已提交写并在 VirtIO 声明 FLUSH feature 时发送 device flush；后续冷启动读回验证持久化结果。 | `kernel/src/syscall/fs.rs`, `kernel/src/fs/vfs.rs` |
| 82 | `fsync` | fd | 0；`EBADF/EIO` | POSIX fsync | **Complete**。等待所有同步写完成，并在 VirtIO 声明 FLUSH feature 时发送 flush request。 | `kernel/src/syscall/fs.rs` |
| 276 | `renameat2` | old/new dirfd/path、flags | 0；标准 rename errno | Linux renameat2 | **Partial**。支持跨目录移动、原子串行化替换和 `RENAME_NOREPLACE`；无其他 rename flags、mount/symlink 语义。 | `kernel/src/syscall/fs.rs` |
| 93 | `exit` | `int status` | 不返回 | POSIX `_exit`；musl `_Exit` fallback | **Complete**。当前调用 thread 是 Process 的唯一 thread；释放 TCB 后只保留可由 parent 消费的最小 exit record。 | `kernel/src/syscall/process.rs` |
| 94 | `exit_group` | `int status` | 不返回 | Linux extension；musl `_Exit` 首选 | **Partial**。与 thread exit 共用清理路径；当前不主动终止仍在其他 CPU 的 sibling，调用方必须先 join/clear-tid。 | `kernel/src/syscall/process.rs` |
| 96 | `set_tid_address` | clear-child-tid pointer | calling TID | Linux thread runtime；musl pthread | **Complete**。零清除 registration；thread exit 写零并 futex-wake 一个 waiter。 | `kernel/src/syscall/process.rs` |
| 98 | `futex` | uaddr、op、val、relative timeout、uaddr2、val3 | 0/wake count；`EAGAIN/EFAULT/EINTR/EINVAL/ETIMEDOUT/ENOSYS` | Linux thread synchronization；musl pthread | **Partial**。实现 address-space-keyed WAIT/WAKE、PRIVATE flag、WAIT monotonic relative timeout 和 thread-directed signal interruption；无 timeout 的 WAIT 在 handler 含 `SA_RESTART` 时重放，带 relative timeout 的 WAIT 保持 `EINTR`，避免错误重置 timeout。统一 wait registration 使 wake/timeout/signal 只能消费一次。无 requeue、PI 和 bitset。 | `kernel/src/syscall/futex.rs`, `kernel/src/task/task_manager.rs` |
| 99 | `set_robust_list` | head、len | 0；`EINVAL` | Linux robust mutex；musl pthread | **Complete**（当前 Thread 模型）。RV64 24-byte head；exit 有界遍历，原子设置 OWNER_DIED并 wake，非法用户链停止清理。 | `kernel/src/syscall/process.rs`, `kernel/src/task/model.rs` |
| 101 | `nanosleep` | `const struct timespec *req, struct timespec *rem`；RV64 `i64,i64` | `0`；`EFAULT/EINVAL/EINTR` | POSIX `nanosleep`；musl wrapper | **Partial**。monotonic deadline 到期返回 0；未屏蔽 thread-directed signal 取消 wait、返回 `EINTR` 并 copyout 剩余时间，即使 handler 含 `SA_RESTART` 也不自动重启。无其他 signal source；时长超过 `u64` ns 返回 `EINVAL`。 | `kernel/src/syscall/timer.rs` |
| 113 | `clock_gettime` | `clockid_t, struct timespec *`；RV64 `i64,i64` | `0`；`EINVAL/EFAULT` | POSIX `clock_gettime`；musl vDSO fallback | **Partial**。只支持 `CLOCK_REALTIME(0)` 和 `CLOCK_MONOTONIC(1)`；其他 Linux clock ID 返回 `EINVAL`。 | `kernel/src/syscall/timer.rs` |
| 124 | `sched_yield` | 无参数 | `0` | POSIX `sched_yield`；musl direct wrapper | **Complete**。当前 task 回到唯一 CFS-like runqueue。 | `kernel/src/syscall/process.rs` |
| 131 | `tgkill` | tgid、tid、signal | 0；`ESRCH/EINVAL` | Linux thread-directed signal；musl pthread | **Partial**。支持存在性 probe、standard signal pending 与 deadline/futex/child wait interruption；未屏蔽、非忽略 signal 在 trap return delivery。无 permission check/process-directed selection。 | `kernel/src/syscall/signal.rs`, `kernel/src/task/task_manager.rs` |
| 133 | `rt_sigsuspend` | 8-byte sigset | 固定 `EINTR` | POSIX sigsuspend；BusyBox wait | **Complete**（当前 signal set）。临时 mask 与 Signal membership 原子封闭 lost wakeup；pending bit 不被 waiter 消费，trap frame 恢复调用前 mask。 | `kernel/src/syscall/signal.rs`, `kernel/src/task/model.rs` |
| 134/135 | `rt_sigaction` / `rt_sigprocmask` | RV64 24-byte action、8-byte sigset | 0；`EFAULT/EINVAL` | POSIX signal API；musl wrappers | **Partial**。Process disposition、per-Thread mask、SIGKILL/SIGSTOP 不可屏蔽；handler 的 `SA_RESTART` 可重放 blocking `wait4` 与无 timeout 的 futex WAIT。无 altstack、queued realtime value 与其他 syscall 的完整 restart coverage。 | `kernel/src/syscall/signal.rs` |
| 137 | `rt_sigtimedwait` | 8-byte sigset、可选 128-byte siginfo、可选 relative timespec | signal number；`EAGAIN/EINTR/EFAULT/EINVAL` | POSIX sigwaitinfo/sigtimedwait；BusyBox init | **Partial**。从 Thread 唯一 pending owner 消费 coalesced standard signal，支持无限等待、零/有限 monotonic timeout、`SI_TKILL` 与 `SIGCHLD/CLD_EXITED` siginfo；registration 共用 indexed wait/deadline registry。无 realtime queued value、process-directed selection 和完整多线程 SIGCHLD target selection。 | `kernel/src/syscall/signal.rs`, `kernel/src/task/task_manager.rs` |
| 139 | `rt_sigreturn` | frame 隐式位于 sp | 恢复 a0/context；坏 frame 最终终止 | Linux RV64 signal ABI | **Partial**。恢复 32 GPR、32 FP、fcsr、PC 与 mask；frame 使用固定 Linux v7.1 RV64 layout和 U|RX return trampoline。无 vector/CFI extension context。 | `kernel/src/syscall/signal.rs`, `kernel/src/task/model.rs` |
| 142 | `reboot` | magic1、magic2、command、argument | CAD command 返回 0；reset 成功不返回；`EINVAL/EIO` | BusyBox init shutdown policy | **Partial**。校验 Linux magic，`CAD_OFF/CAD_ON` 更新唯一 system policy，`RESTART/HALT/POWER_OFF` 映射 SBI SRST cold reboot/shutdown。platform 无 restart-reason channel，因此拒绝 `RESTART2`；无 kexec/suspend。 | `kernel/src/syscall/reboot.rs`, `kernel/src/system.rs` |
| 154-157 | `setpgid/getpgid/getsid/setsid` | PID/PGID 或无参数 | 0/ID；`ESRCH/EPERM` | POSIX session/job control；BusyBox init/ash | **Partial**。process graph 唯一拥有 SID/PGID；fork 继承，setsid 创建新 session/group，setpgid 校验 direct-child、session leader 与同 session group。尚无 exec-generation 的 setpgid `EACCES`、credentials/namespace 与 stopped/continued lifecycle。 | `kernel/src/syscall/process.rs`, `kernel/src/task/task_manager.rs` |
| 172 | `getpid` | 无参数 | TGID | POSIX `getpid`；musl direct wrapper | **Complete**。返回 Process owner 的 TGID，不从 scheduler ID 推导。 | `kernel/src/syscall/process.rs` |
| 173 | `getppid` | 无参数 | parent TGID；init 为 0 | POSIX `getppid`；musl direct wrapper | **Complete**。读取 TaskManager 唯一 parent edge；orphan 重新指向 PID 1。 | `kernel/src/syscall/process.rs` |
| 174-177 | `getuid/geteuid/getgid/getegid` | 无参数 | 0 | POSIX identity；BusyBox prompt | **Complete**（固定 root identity 基线）。当前没有 credential mutation ABI，real/effective UID/GID 均为 root 0。 | `kernel/src/syscall/process.rs` |
| 178 | `gettid` | 无参数 | TID | Linux extension；musl pthread internals | **Complete**。单线程模型中 TID == TGID，但值来自 ThreadContext owner。 | `kernel/src/syscall/process.rs` |
| 214 | `brk` | `unsigned long new_brk` | 成功返回新 break；失败返回未改变旧 break，无负 errno | Linux legacy VM；musl compatibility path | **Complete**。越界/OOM 保持旧 break；页映射变化后同步跨 hart TLB。 | `kernel/src/syscall/memory.rs` |
| 215 | `munmap` | page-aligned address、nonzero length | `0`；`EINVAL/EACCES` | POSIX `munmap`；musl allocator | **Partial**。支持 anonymous private VMA 删除、洞忽略和左右拆分；触及 ELF/stack/heap 等非 anonymous VMA 返回 `EACCES`。 | `kernel/src/syscall/memory.rs`, `kernel/src/memory/mm.rs` |
| 220 | `clone` | flags、stack、parent_tid、tls、child_tid | parent=child PID/TID、child=0；标准 errno | Linux process/thread primitive；musl fork/pthread | **Partial**。支持 fork-shaped process clone，且忽略 flags 未启用的尾部参数；支持 VM/FS/FILES/SIGHAND/THREAD/SYSVSEM/SETTLS 配合 parent/child-set/clear-tid 的 thread clone，并按 Linux 语义忽略历史 `CLONE_DETACHED`。多线程 fork 返回 `EAGAIN`，无 vfork/namespace/pidfd flags。 | `kernel/src/syscall/process.rs`, `kernel/src/task/task_manager.rs` |
| 221 | `execve` | `const char *path, char *const argv[], char *const envp[]`；raw NUL bytes | 成功不回旧映像；标准 errno | POSIX `execve`；musl direct wrapper | **Partial**。static ET_EXEC/initial stack/rollback 完整；多线程 Process 返回 `EAGAIN`，避免 sibling stale context。缺失 symlink/script/PIE/dynamic、credentials 与 signal reset。 | `kernel/src/syscall/process.rs`, `kernel/src/memory/mm.rs` |
| 222 | `mmap` | address、length、prot、flags、fd、offset | address；`EINVAL/EACCES/EEXIST/ENOMEM` | POSIX `mmap`；musl allocator/loader | **Partial**。只支持 eager `MAP_PRIVATE|MAP_ANONYMOUS`，含 `PROT_NONE`，可附加 `MAP_FIXED_NOREPLACE`；fd 必须为 -1、offset 为 0。无 destructive `MAP_FIXED`、file/shared/lazy mapping，强制 W^X。 | `kernel/src/syscall/memory.rs`, `kernel/src/memory/mm.rs` |
| 226 | `mprotect` | page-aligned address、length、prot | `0`；`EINVAL/EACCES` | POSIX `mprotect`；musl loader | **Partial**。完整 anonymous private 区间可拆分、在同一 frame 上切换 `PROT_NONE`/leaf PTE 并合并等价 VMA；缺页整体失败。强制 W^X，非 anonymous VMA 返回 `EACCES`。 | `kernel/src/syscall/memory.rs`, `kernel/src/memory/mm.rs` |
| 260 | `wait4` | pid、status、options、rusage | child PID/0；标准 errno，含 `EINTR` | POSIX waitpid backend；musl direct wrapper | **Partial**。单线程 Process 支持正 PID/`-1`、blocking/`WNOHANG`、signal interruption、copyout-before-reap；handler 含 `SA_RESTART` 时透明重放，否则返回 `EINTR` 且不消费 child record。多线程调用返回 `EAGAIN`。rusage 必须为空，无 process-group/stopped/continued/signal status。 | `kernel/src/syscall/process.rs`, `kernel/src/task/task_manager.rs` |

文件 ABI 与原有 process/time/memory ABI 的精确边界均以本表为准；“Complete”只覆盖当前明确存在的对象类型和单线程 Process 模型。

## 3. 第一阶段标准 ABI 缺口

下表是当前路线中的标准 Linux/riscv64 入口。它们都不在共享 ABI crate/dispatcher 中占位，所以当前结果为 `-ENOSYS`。

| 编号 | Linux 名称 | 状态 | 处理结论 |
|---:|---|---|---|
| 129 | `kill` | Missing | 尚无 process-directed selection、permission 与 job-control group 语义。 |
| 130 | `tkill` | Not Planned | 使用 thread-group-aware `tgkill`，不恢复过时入口。 |
| 146 | `setuid` | Removed | 已删除只有 real/effective UID 的伪 credential state。 |
| 261 | `prlimit64` | Not Planned | 当前 init 启动不需 resource limits，不返回伪 infinity 表。 |
| 278 | `getrandom` | Missing | 无经证明 entropy source/CRNG，不以 RTC/timer 冒充随机。 |

更详细的参数、userspace 结构体、flags、errno、POSIX 和 musl 路径审计见 [Phase 11 记录](phase-11-syscall-abi.md)。其 `execve` 行是 Phase 12 修复前的历史状态；当前结论以本文为准。

## 4. musl 结论

当前 57 个入口和静态 initial stack 已支撑固定 musl v1.2.6 pthread consumer，并覆盖 Pipe writev/readv/EOF 与 multi-source ppoll。BusyBox gate 额外覆盖外部 applet pipeline、文件重定向、后台 subshell+wait、UART foreground Ctrl-C、CAD policy 与跨冷启动 ext2 持久化。这不表示常规 musl 程序可运行；其扩展仍被下列缺口阻断：

1. futex requeue/PI/bitset、完整 clone/exit_group 语义；
2. 其他 syscall 的 restart coverage、带 relative timeout futex 的正确剩余时间、altstack、queued realtime signal 和 process-directed delivery；
3. `AT_RANDOM/getrandom`、HWCAP 和 dynamic interpreter/relocation；
4. file/shared mapping 与 destructive `MAP_FIXED`。

因此不能将固定 smoke 通过、编号一致或具有 Linux 格式 auxv 提升为通用 musl 兼容声明。
