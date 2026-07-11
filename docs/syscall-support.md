# LiteOS Linux/riscv64 syscall 支持矩阵

> 更新日期：2026-07-11（Asia/Shanghai）
>
> Linux UAPI 基线：Linux `v7.1`，commit `8cd9520d35a6c38db6567e97dd93b1f11f185dc6`
>
> POSIX 语义基线：POSIX.1-2024 / Issue 8
>
> musl consumer 基线：musl `v1.2.6`，commit `9fa28ece75d8a2191de7c5bb53bed224c5947417`

## 1. 全局契约

- U-mode `ecall`：`a7=number`，`a0..a5=args`，`a0=result`。
- kernel error 为 `-errno`；user raw wrapper 不伪造 libc `errno`。
- `syscall-abi` 只定义下表 12 个 Linux/riscv64 number。
- dispatcher 对所有其他 number 统一返回 `-ENOSYS`。
- 没有 LiteOS 私有 syscall number、旧编号转发、deprecated 入口或 feature-flag 双轨。

状态含义：

| 状态 | 含义 |
|---|---|
| Complete | 在当前明确声明的单进程、无 fd 等系统模型内，无已知的本 syscall 契约偏差。 |
| Partial | 入口可用，但已知缺失在表中精确列出。 |
| Missing | 尚无正确实现；当前 `-ENOSYS`。 |
| Not Planned | 当前收敛基线不计划接入；当前 `-ENOSYS`。 |
| Removed | 曾有错号、私有或语义不完整实现，已整链删除；当前 `-ENOSYS`。 |

`Complete` 不能外推为完整 Linux/POSIX/musl 兼容。例如 `exit_group` 在当前只有一个 thread 的 thread group 内语义完整，但 LiteOS 并不支持创建线程。

## 2. 当前暴露的 12 个入口

| 编号 | Linux 名称 | 参数 / userspace ABI | 返回与 errno | POSIX / musl 路径 | 状态与精确边界 | 代码 |
|---:|---|---|---|---|---|---|
| 17 | `getcwd` | `char *buf, size_t size` | 含 NUL 的长度；`ERANGE/EFAULT` | POSIX `getcwd`；musl direct wrapper | **Complete**。无 `chdir`，因此当前 cwd 唯一值为 `/`；copyout 契约完整。 | `kernel/src/syscall/fs.rs` |
| 64 | `write` | `unsigned fd, const void *buf, size_t count` | byte count；`EBADF/EFAULT/EIO`；允许 partial result | POSIX `write`；musl direct wrapper | **Partial**。只支持 fd 1 -> SBI DBCN bootstrap console；无 fd table/OFD、file/pipe/socket/offset/append 语义。 | `kernel/src/syscall/fs.rs` |
| 93 | `exit` | `int status` | 不返回 | POSIX `_exit`；musl `_Exit` fallback | **Complete**。当前调用 thread 是 Process 的唯一 thread；无 wait consumer，不保留 zombie。 | `kernel/src/syscall/process.rs` |
| 94 | `exit_group` | `int status` | 不返回 | Linux extension；musl `_Exit` 首选 | **Complete**。当前 thread group 恰好只有 calling thread，与 `exit` 共用唯一终止路径。 | `kernel/src/syscall/process.rs` |
| 101 | `nanosleep` | `const struct timespec *req, struct timespec *rem`；RV64 `i64,i64` | `0`；`EFAULT/EINVAL/EINTR` | POSIX `nanosleep`；musl wrapper | **Partial**。有 monotonic deadline wait；无 signal interruption/restart 闭环，`rem` 只在早醒分支生效；时长超过 `u64` ns 返回 `EINVAL`。 | `kernel/src/syscall/timer.rs` |
| 113 | `clock_gettime` | `clockid_t, struct timespec *`；RV64 `i64,i64` | `0`；`EINVAL/EFAULT` | POSIX `clock_gettime`；musl vDSO fallback | **Partial**。只支持 `CLOCK_REALTIME(0)` 和 `CLOCK_MONOTONIC(1)`；其他 Linux clock ID 返回 `EINVAL`。 | `kernel/src/syscall/timer.rs` |
| 124 | `sched_yield` | 无参数 | `0` | POSIX `sched_yield`；musl direct wrapper | **Complete**。当前 task 回到唯一 CFS-like runqueue。 | `kernel/src/syscall/process.rs` |
| 172 | `getpid` | 无参数 | TGID | POSIX `getpid`；musl direct wrapper | **Complete**。返回 Process owner 的 TGID，不从 scheduler ID 推导。 | `kernel/src/syscall/process.rs` |
| 173 | `getppid` | 无参数 | `0` | POSIX `getppid`；musl direct wrapper | **Complete**。当前唯一 Process 是 kernel-created PID 1，无 parent。引入 process creation 时必须与 parent owner 同步扩展。 | `kernel/src/syscall/process.rs` |
| 178 | `gettid` | 无参数 | TID | Linux extension；musl pthread internals | **Complete**。单线程模型中 TID == TGID，但值来自 ThreadContext owner。 | `kernel/src/syscall/process.rs` |
| 214 | `brk` | `unsigned long new_brk` | 成功返回新 break；失败返回未改变旧 break，无负 errno | Linux legacy VM；musl compatibility path | **Complete**。越界/OOM 保持旧 break；页映射变化后同步跨 hart TLB。 | `kernel/src/syscall/memory.rs` |
| 221 | `execve` | `const char *path, char *const argv[], char *const envp[]`；raw NUL bytes | 成功不回旧映像；`E2BIG/EACCES/EFAULT/EIO/ELOOP/ENAMETOOLONG/ENOENT/ENOEXEC/ENOMEM/ENOTDIR` | POSIX `execve`；musl direct wrapper | **Partial**。已支持 relative/root path、non-UTF8 bytes、full read、execute bit、static ET_EXEC、BSS/W^X、Linux initial stack/auxv 和 failure rollback。缺失 symlink following、script interpreter、PIE/dynamic/TLS、完整 credentials、CLOEXEC、signal reset 和 sibling-thread handling；相关子系统当前均不存在。 | `kernel/src/syscall/process.rs`, `kernel/src/memory/mm.rs` |

汇总：8 `Complete`，4 `Partial`。

## 3. 第一阶段标准 ABI 缺口

下表是当前路线中的标准 Linux/riscv64 入口。它们都不在共享 ABI crate/dispatcher 中占位，所以当前结果为 `-ENOSYS`。

| 编号 | Linux 名称 | 状态 | 处理结论 |
|---:|---|---|---|
| 23 | `dup` | Removed | 无 fd entry/OFD 时不伪造共享 offset/status flags。 |
| 24 | `dup3` | Missing | 与 fd/OFD/CLOEXEC 竖切一起实现。 |
| 25 | `fcntl` | Removed | 已删除忽略 command 与 fd/OFD flag 分层的半实现。 |
| 29 | `ioctl` | Missing | 无标准 device file/ioctl UAPI，不用私有设备 syscall 替代。 |
| 34/35 | `mkdirat` / `unlinkat` | Missing | ext2 只读，无 dirfd/path mutation 闭环。 |
| 56 | `openat` | Missing | 需 fd entry + OFD + pathname/flags 唯一模型。 |
| 57 | `close` | Removed | 已删除不可达 fd table 上的表面 handler。 |
| 59 | `pipe2` | Not Planned | 无 fd/OFD/close/dup/poll/signal 闭环前不接入。 |
| 61 | `getdents64` | Missing | 无 directory OFD/offset，kernel inode lookup 不暴露为用户遍历 ABI。 |
| 62 | `lseek` | Removed | 无可共享 OFD offset。 |
| 63 | `read` | Removed | 已删除无事件唤醒、持续 runnable 的 SBI stdin 轮询。 |
| 65/66 | `readv` / `writev` | Missing | 无 fd/OFD，不用 kernel 聚合 buffer 伪造 atomicity。 |
| 79/80 | `newfstatat` / `fstat` | Missing | 无 Linux RV64 `struct stat` copyout 和用户 fd/dirfd。 |
| 96 | `set_tid_address` | Not Planned | 无 clone thread、clear-child-tid 和 futex wake。 |
| 98 | `futex` | Removed | 无 address-space key、lost-wakeup 状态机、timeout/signal 与 exit cleanup 前不恢复。 |
| 99 | `set_robust_list` | Not Planned | 无 futex owner-death/robust cleanup。 |
| 129 | `kill` | Removed | 已删除无 action/mask/frame 闭环的 handler。 |
| 130/131 | `tkill` / `tgkill` | Not Planned | 需完整 signal + thread group；不单独恢复过时 `tkill`。 |
| 134/135 | `rt_sigaction` / `rt_sigprocmask` | Not Planned | 不暴露部分 signal ABI。 |
| 139 | `rt_sigreturn` | Removed | 已删除私有 frame 和取指地址 0 fallback。 |
| 146 | `setuid` | Removed | 已删除只有 real/effective UID 的伪 credential state。 |
| 215 | `munmap` | Missing | 无 VMA/VM object 模型。 |
| 220 | `clone` | Not Planned | 无 parent/child/thread group、TLS、clear-tid、signal/futex 闭环。 |
| 222 | `mmap` | Missing | 已删除错误近似实现；未建 VMA/VM object/file mapping 前不恢复。 |
| 226 | `mprotect` | Missing | 无 VMA split/merge 和用户 W^X 变更契约。 |
| 260 | `wait4` | Not Planned | 无 child relation；exit 直接回收，不保留伪 zombie。 |
| 261 | `prlimit64` | Not Planned | 当前 init 启动不需 resource limits，不返回伪 infinity 表。 |
| 278 | `getrandom` | Missing | 无经证明 entropy source/CRNG，不以 RTC/timer 冒充随机。 |

更详细的参数、userspace 结构体、flags、errno、POSIX 和 musl 路径审计见 [Phase 11 记录](phase-11-syscall-abi.md)。其 `execve` 行是 Phase 12 修复前的历史状态；当前结论以本文为准。

## 4. musl 结论

当前 12 个入口和静态 initial stack 只足以支撑仓库自带最小 runtime。常规 musl 程序至少被下列缺口阻断：

1. `mmap/munmap/mprotect`；
2. `openat/read/close/fstat`；
3. TLS/`clone/futex/set_tid_address/set_robust_list`；
4. signal ABI；
5. `AT_RANDOM/getrandom`、HWCAP 和 dynamic interpreter/relocation。

因此不能将“编号与 musl header 一致”或“具有 Linux 格式 auxv”提升为 musl 兼容声明。
