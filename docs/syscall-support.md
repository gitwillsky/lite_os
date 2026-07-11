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
- `syscall-abi` 只定义下表 35 个 Linux/riscv64 number。
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

`Complete` 不能外推为完整 Linux/POSIX/musl 兼容。例如 `set_tid_address` 的 clear/wake 契约成立，不表示 futex timeout、signal interruption 或完整 pthread runtime 已成立。

## 2. 当前暴露的 35 个入口

| 编号 | Linux 名称 | 参数 / userspace ABI | 返回与 errno | POSIX / musl 路径 | 状态与精确边界 | 代码 |
|---:|---|---|---|---|---|---|
| 17 | `getcwd` | `char *buf, size_t size` | 含 NUL 的长度；`ERANGE/EFAULT` | POSIX `getcwd`；musl direct wrapper | **Complete**。无 `chdir`，因此当前 cwd 唯一值为 `/`；copyout 契约完整。 | `kernel/src/syscall/fs.rs` |
| 23/24 | `dup` / `dup3` | fd、目标 fd、`O_CLOEXEC` | 新 fd；`EBADF/EINVAL` | POSIX dup；Linux dup3 | **Complete**。fd entry 复制后共享同一 OFD offset/status flags；descriptor flag 独立。 | `kernel/src/syscall/fs.rs` |
| 25 | `fcntl` | fd、command、argument | command 对应值；`EBADF/EINVAL` | POSIX fcntl | **Partial**。实现 `F_DUPFD/F_GETFD/F_SETFD/F_GETFL/F_SETFL/F_DUPFD_CLOEXEC`；`F_SETFL` 当前只允许修改 `O_APPEND`。 | `kernel/src/syscall/fs.rs` |
| 34/35 | `mkdirat` / `unlinkat` | dirfd、raw path、mode/`AT_REMOVEDIR` | 0；标准 pathname errno | POSIX mkdirat/unlinkat | **Partial**。支持绝对路径、`AT_FDCWD` 和目录 fd；无 credentials、mount point 与 symlink following。 | `kernel/src/syscall/fs.rs` |
| 46 | `ftruncate` | fd、64-bit length | 0；`EBADF/EISDIR/ENOSPC/EIO` | POSIX ftruncate | **Complete**。支持稀疏扩展、尾块清零、direct/三级 indirect 回收并维护 `i_blocks`。 | `kernel/src/syscall/fs.rs` |
| 56/57 | `openat` / `close` | dirfd、raw path、flags/mode；fd | fd/0；标准 fd/path errno | POSIX openat/close | **Partial**。regular/directory OFD、create/excl/trunc/append/directory/cloexec 完整；无 permissions、symlink following 和设备节点。 | `kernel/src/syscall/fs.rs` |
| 61 | `getdents64` | fd、`linux_dirent64` buffer、length | bytes；`EBADF/ENOTDIR/EFAULT/EINVAL` | libc readdir backend | **Complete**。目录 OFD 保存逐项位置，记录按 8 bytes 对齐并包含 `d_type`；并发目录变更后的 cookie 与 Linux 一样不承诺快照语义。 | `kernel/src/syscall/fs.rs` |
| 62 | `lseek` | fd、signed offset、whence | new offset；`EBADF/EINVAL/ESPIPE` | POSIX lseek | **Complete**。实现 `SEEK_SET/CUR/END`，offset 位于共享 OFD。 | `kernel/src/syscall/fs.rs` |
| 63/64 | `read` / `write` | fd、用户 buffer、count | byte count；允许 partial result | POSIX read/write | **Partial**。regular file 共享 offset，write 支持 append；console 通过同一 OFD 路径，当前 console input 返回 EOF；无 pipe/socket。 | `kernel/src/syscall/fs.rs` |
| 79/80 | `newfstatat` / `fstat` | dirfd/fd、path、RV64 `struct stat` | 0；fd/path/copyout errno | POSIX stat/fstatat | **Partial**。128-byte asm-generic 布局与 512-byte `st_blocks` 正确；`newfstatat` 当前只接受 flags 0。 | `kernel/src/syscall/fs.rs` |
| 82 | `fsync` | fd | 0；`EBADF/EIO` | POSIX fsync | **Complete**。等待所有同步写完成，并在 VirtIO 声明 FLUSH feature 时发送 flush request。 | `kernel/src/syscall/fs.rs` |
| 276 | `renameat2` | old/new dirfd/path、flags | 0；标准 rename errno | Linux renameat2 | **Partial**。支持跨目录移动、原子串行化替换和 `RENAME_NOREPLACE`；无其他 rename flags、mount/symlink 语义。 | `kernel/src/syscall/fs.rs` |
| 93 | `exit` | `int status` | 不返回 | POSIX `_exit`；musl `_Exit` fallback | **Complete**。当前调用 thread 是 Process 的唯一 thread；释放 TCB 后只保留可由 parent 消费的最小 exit record。 | `kernel/src/syscall/process.rs` |
| 94 | `exit_group` | `int status` | 不返回 | Linux extension；musl `_Exit` 首选 | **Partial**。与 thread exit 共用清理路径；当前不主动终止仍在其他 CPU 的 sibling，调用方必须先 join/clear-tid。 | `kernel/src/syscall/process.rs` |
| 96 | `set_tid_address` | clear-child-tid pointer | calling TID | Linux thread runtime；musl pthread | **Complete**。零清除 registration；thread exit 写零并 futex-wake 一个 waiter。 | `kernel/src/syscall/process.rs` |
| 98 | `futex` | uaddr、op、val、timeout、uaddr2、val3 | 0/wake count；`EAGAIN/EFAULT/EINVAL/ENOSYS` | Linux thread synchronization；musl pthread | **Partial**。实现 address-space-keyed WAIT/WAKE 与 PRIVATE flag，无 lost wakeup；timeout、requeue、PI、bitset 和 signal interruption未实现。 | `kernel/src/syscall/futex.rs`, `kernel/src/task/task_manager.rs` |
| 99 | `set_robust_list` | head、len | 0；`EINVAL` | Linux robust mutex；musl pthread | **Complete**（当前 Thread 模型）。RV64 24-byte head；exit 有界遍历，原子设置 OWNER_DIED并 wake，非法用户链停止清理。 | `kernel/src/syscall/process.rs`, `kernel/src/task/model.rs` |
| 101 | `nanosleep` | `const struct timespec *req, struct timespec *rem`；RV64 `i64,i64` | `0`；`EFAULT/EINVAL/EINTR` | POSIX `nanosleep`；musl wrapper | **Partial**。有 monotonic deadline wait；无 signal interruption/restart 闭环，`rem` 只在早醒分支生效；时长超过 `u64` ns 返回 `EINVAL`。 | `kernel/src/syscall/timer.rs` |
| 113 | `clock_gettime` | `clockid_t, struct timespec *`；RV64 `i64,i64` | `0`；`EINVAL/EFAULT` | POSIX `clock_gettime`；musl vDSO fallback | **Partial**。只支持 `CLOCK_REALTIME(0)` 和 `CLOCK_MONOTONIC(1)`；其他 Linux clock ID 返回 `EINVAL`。 | `kernel/src/syscall/timer.rs` |
| 124 | `sched_yield` | 无参数 | `0` | POSIX `sched_yield`；musl direct wrapper | **Complete**。当前 task 回到唯一 CFS-like runqueue。 | `kernel/src/syscall/process.rs` |
| 172 | `getpid` | 无参数 | TGID | POSIX `getpid`；musl direct wrapper | **Complete**。返回 Process owner 的 TGID，不从 scheduler ID 推导。 | `kernel/src/syscall/process.rs` |
| 173 | `getppid` | 无参数 | parent TGID；init 为 0 | POSIX `getppid`；musl direct wrapper | **Complete**。读取 TaskManager 唯一 parent edge；orphan 重新指向 PID 1。 | `kernel/src/syscall/process.rs` |
| 178 | `gettid` | 无参数 | TID | Linux extension；musl pthread internals | **Complete**。单线程模型中 TID == TGID，但值来自 ThreadContext owner。 | `kernel/src/syscall/process.rs` |
| 214 | `brk` | `unsigned long new_brk` | 成功返回新 break；失败返回未改变旧 break，无负 errno | Linux legacy VM；musl compatibility path | **Complete**。越界/OOM 保持旧 break；页映射变化后同步跨 hart TLB。 | `kernel/src/syscall/memory.rs` |
| 215 | `munmap` | page-aligned address、nonzero length | `0`；`EINVAL/EACCES` | POSIX `munmap`；musl allocator | **Partial**。支持 anonymous private VMA 删除、洞忽略和左右拆分；触及 ELF/stack/heap 等非 anonymous VMA 返回 `EACCES`。 | `kernel/src/syscall/memory.rs`, `kernel/src/memory/mm.rs` |
| 220 | `clone` | flags、stack、parent_tid、tls、child_tid | parent=child PID/TID、child=0；标准 errno | Linux process/thread primitive；musl fork/pthread | **Partial**。支持 fork-shaped process clone，以及 VM/FS/FILES/SIGHAND/THREAD/SYSVSEM/SETTLS 配合 parent/child-set/clear-tid 的 thread clone；多线程 fork 返回 `EAGAIN`，无 vfork/namespace/pidfd flags。 | `kernel/src/syscall/process.rs`, `kernel/src/task/task_manager.rs` |
| 221 | `execve` | `const char *path, char *const argv[], char *const envp[]`；raw NUL bytes | 成功不回旧映像；标准 errno | POSIX `execve`；musl direct wrapper | **Partial**。static ET_EXEC/initial stack/rollback 完整；多线程 Process 返回 `EAGAIN`，避免 sibling stale context。缺失 symlink/script/PIE/dynamic、credentials 与 signal reset。 | `kernel/src/syscall/process.rs`, `kernel/src/memory/mm.rs` |
| 222 | `mmap` | address、length、prot、flags、fd、offset | address；`EINVAL/EACCES/EEXIST/ENOMEM` | POSIX `mmap`；musl allocator/loader | **Partial**。只支持 eager `MAP_PRIVATE|MAP_ANONYMOUS`，可附加 `MAP_FIXED_NOREPLACE`；fd 必须为 -1、offset 为 0。无 `PROT_NONE`、destructive `MAP_FIXED`、file/shared/lazy mapping，强制 W^X。 | `kernel/src/syscall/memory.rs`, `kernel/src/memory/mm.rs` |
| 226 | `mprotect` | page-aligned address、length、prot | `0`；`EINVAL/EACCES` | POSIX `mprotect`；musl loader | **Partial**。完整 anonymous private 区间可拆分、变更 PTE 并合并等价 VMA；缺页整体失败。无 `PROT_NONE`，强制 W^X，非 anonymous VMA 返回 `EACCES`。 | `kernel/src/syscall/memory.rs`, `kernel/src/memory/mm.rs` |
| 260 | `wait4` | pid、status、options、rusage | child PID/0；标准 errno | POSIX waitpid backend；musl direct wrapper | **Partial**。单线程 Process 支持正 PID/`-1`、blocking/`WNOHANG`、copyout-before-reap；多线程调用返回 `EAGAIN`。rusage 必须为空，无 process-group/stopped/continued/signal status。 | `kernel/src/syscall/process.rs`, `kernel/src/task/task_manager.rs` |

文件 ABI 与原有 process/time/memory ABI 的精确边界均以本表为准；“Complete”只覆盖当前明确存在的对象类型和单线程 Process 模型。

## 3. 第一阶段标准 ABI 缺口

下表是当前路线中的标准 Linux/riscv64 入口。它们都不在共享 ABI crate/dispatcher 中占位，所以当前结果为 `-ENOSYS`。

| 编号 | Linux 名称 | 状态 | 处理结论 |
|---:|---|---|---|
| 29 | `ioctl` | Missing | 无标准 device file/ioctl UAPI，不用私有设备 syscall 替代。 |
| 59 | `pipe2` | Not Planned | 无 pipe buffer、阻塞唤醒、poll 与 SIGPIPE 闭环前不接入。 |
| 65/66 | `readv` / `writev` | Missing | 尚未实现 iovec user-copy 与 partial-transfer 语义。 |
| 129 | `kill` | Removed | 已删除无 action/mask/frame 闭环的 handler。 |
| 130/131 | `tkill` / `tgkill` | Not Planned | 需完整 signal + thread group；不单独恢复过时 `tkill`。 |
| 134/135 | `rt_sigaction` / `rt_sigprocmask` | Not Planned | 不暴露部分 signal ABI。 |
| 139 | `rt_sigreturn` | Removed | 已删除私有 frame 和取指地址 0 fallback。 |
| 146 | `setuid` | Removed | 已删除只有 real/effective UID 的伪 credential state。 |
| 261 | `prlimit64` | Not Planned | 当前 init 启动不需 resource limits，不返回伪 infinity 表。 |
| 278 | `getrandom` | Missing | 无经证明 entropy source/CRNG，不以 RTC/timer 冒充随机。 |

更详细的参数、userspace 结构体、flags、errno、POSIX 和 musl 路径审计见 [Phase 11 记录](phase-11-syscall-abi.md)。其 `execve` 行是 Phase 12 修复前的历史状态；当前结论以本文为准。

## 4. musl 结论

当前 35 个入口和静态 initial stack 只足以支撑仓库自带最小 runtime。常规 musl 程序至少被下列缺口阻断：

1. futex timeout/requeue/PI、完整 clone/exit_group 语义；
2. signal ABI；
3. `AT_RANDOM/getrandom`、HWCAP 和 dynamic interpreter/relocation；
4. file/shared mapping、`PROT_NONE` 与 destructive `MAP_FIXED`。

因此不能将“编号与 musl header 一致”或“具有 Linux 格式 auxv”提升为 musl 兼容声明。
