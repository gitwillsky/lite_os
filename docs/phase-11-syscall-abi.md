# LiteOS Phase 11：Linux/riscv64 syscall ABI 矩阵

> 本文表格是 Phase 11 的历史快照；Pipe/readv、signal wait 与 TTY 后续实现状态以 `syscall-support.md` 及 Phase 24-26 文档为准。

> 审计日期：2026-07-11（Asia/Shanghai）  
> 代码基线：提交 `ad2b4a8`（Phase 0–10）  
> 权威基线：[standards-baseline.md](standards-baseline.md) 固定的 Linux `v7.1` / RISC-V UAPI、POSIX.1-2024 与 musl `v1.2.6`  
> 验证约束：不维护、不修正、不执行测试；只做静态 ABI 审计、构建和非测试 QEMU 启动观察。

## 1. 阶段结论

LiteOS 当前对 U-mode 只暴露 Linux/riscv64 的 `a7` syscall number、`a0..a5` 参数与 `a0` 返回约定。未识别编号统一返回 `-ENOSYS`，没有 LiteOS 私有 syscall number 或错号转发。

本阶段的有效 ABI 共 12 个入口：8 个 `Complete`、4 个 `Partial`（包括由 Phase 12 继续收口的 `execve`）。其中：

- 新增 `exit_group(94)`；在已声明的单进程单线程模型中，它与 `exit` 终止同一个唯一 thread group，语义完整。
- 新增 `getppid(173)`；当前唯一 PID 1 由 kernel 创建，没有 fork/clone 入口，因此其 PPID 始终为 0。
- 删除 `setuid(146)` 全链。旧实现只存 real/effective UID，无 saved-set-ID 和可证明的 credential transition，不再以 Linux 同名入口暴露。
- 修正 `nanosleep(NULL, ...)` 为 `-EFAULT`；不再把非法用户指针报为 `EINVAL`。
- trap 的 exec 成功分支使用共享 `SYSCALL_EXECVE` 常量，不再内嵌编号 `221`。

## 2. 状态定义

| 状态 | 本文含义 |
|---|---|
| Complete | 在当前明确的进程、fd 与设备模型内，编号、参数、返回值与可观察语义没有已知偏差。 |
| Partial | 入口可用，但表中必须列出精确缺失语义。 |
| Missing | 尚无正确实现；当前通过 dispatcher 返回 `-ENOSYS`。 |
| Not Planned | 当前收敛基线不计划接入；仍返回 `-ENOSYS`，不占用或转发该编号。 |
| Removed | 仓库曾有同名或同号的不完整入口，已整链删除；当前返回 `-ENOSYS`。 |

`Complete` 不是“Linux 兼容”总声明。例如 `exit_group` 在当前单线程模型内完整，但 LiteOS 仍不支持创建线程。

> 后续状态：本表是 Phase 11 的历史快照。Phase 22 已增加标准 `chdir(49)`、Process-owned cwd inode、relative lookup 与 VFS reverse `getcwd`；当前结论以 `syscall-support.md` 为准。

## 3. 当前有效入口

| 编号 / Linux 名称 | LiteOS handler / 代码位置 | 参数、userspace 结构与 flags | 返回值与 errno | POSIX 接口 / musl 路径 | 状态、已知差异与结论 |
|---|---|---|---|---|---|
| 17 `getcwd` | `sys_get_cwd` / `kernel/src/syscall/fs.rs` | `char *buf, size_t size`；无结构/flags | 成功返回含 NUL 的长度；`ERANGE/EFAULT`；内部无 current task 为 `ESRCH` | POSIX `getcwd()`；musl `getcwd` wrapper | **Complete**。当前没有 `chdir`，cwd 唯一可能值为 `/`；这是进程能力边界，不改变本入口的 copyout 契约。 |
| 64 `write` | `sys_write` / `kernel/src/syscall/fs.rs` | `unsigned fd, const void *buf, size_t count`；无结构/flags | 字节数或 `EBADF/EFAULT/EIO`；copyin/device 失败可返回 partial count | POSIX `write()`；musl 直接 syscall wrapper | **Partial**。只有 fd 1 -> SBI DBCN；无 fd table/OFD、普通文件、pipe 或 append/offset 语义。保留为 bootstrap console，其他 fd 明确 `EBADF`。 |
| 93 `exit` | `sys_exit` / `kernel/src/syscall/process.rs` | `int status`；无结构/flags | 不返回 | POSIX `_exit()`；musl `_Exit` fallback | **Complete**。当前一个 process 只有一个 thread；只终止 calling thread 即回收完整 process。无 `wait4` 消费者，不伪造 zombie。 |
| 94 `exit_group` | `sys_exit` / `kernel/src/syscall/process.rs` | `int status`；无结构/flags | 不返回 | Linux 扩展；musl `_Exit` 首选 | **Complete**。当前 thread group 恰好只有 calling thread；与 `exit` 共用唯一终止路径。 |
| 101 `nanosleep` | `sys_nanosleep` / `kernel/src/syscall/timer.rs` | `const struct timespec *req, struct timespec *rem`；RV64 两个 `i64`；无 flags | `0` 或 `EFAULT/EINVAL/EINTR` | POSIX `nanosleep()`；musl nanosleep wrapper | **Partial**。有 monotonic deadline wait queue，但当前无 signal interruption/restart 闭环，`rem` 只保留早醒分支；总时长超过 `u64` ns 返回 `EINVAL`。 |
| 113 `clock_gettime` | `sys_clock_gettime` / `kernel/src/syscall/timer.rs` | `clockid_t, struct timespec *`；RV64 两个 `i64`；无 flags | `0` 或 `EINVAL/EFAULT` | POSIX `clock_gettime()`；musl vDSO 失败后 syscall | **Partial**。只支持 `CLOCK_REALTIME(0)` 与 `CLOCK_MONOTONIC(1)`；CPU clock、boottime 等 Linux clock ID 返回 `EINVAL`。 |
| 124 `sched_yield` | `sys_sched_yield` / `kernel/src/syscall/process.rs` | 无参数/结构/flags | 成功返回 0 | POSIX `sched_yield()`；musl 直接 wrapper | **Complete**。当前 task 回到唯一 CFS runqueue，不存在装饰性第二调度器。 |
| 172 `getpid` | `sys_get_pid` / `kernel/src/syscall/process.rs` | 无参数/结构/flags | TGID；无 userspace errno | POSIX `getpid()`；musl 直接 wrapper | **Complete**。返回 Process 拥有的 TGID，不从 scheduler ID 推导。 |
| 173 `getppid` | `sys_get_ppid` / `kernel/src/syscall/process.rs` | 无参数/结构/flags | 0；无 userspace errno | POSIX `getppid()`；musl 直接 wrapper | **Complete**。当前唯一 Process 是 kernel 创建的 PID 1，无父 process。引入 fork/clone 时必须与 parent/child owner 一起扩展。 |
| 178 `gettid` | `sys_get_tid` / `kernel/src/syscall/process.rs` | 无参数/结构/flags | TID；无 userspace errno | Linux 扩展；musl pthread 内部路径 | **Complete**。单线程模型中 TID == TGID，值来自 ThreadContext owner。 |
| 214 `brk` | `sys_brk` / `kernel/src/syscall/memory.rs` | `unsigned long brk`；无结构/flags | 成功返回新 break；失败返回未改变的旧 break，不用负 errno | POSIX 已不要求 `brk()`；musl allocator/兼容 wrapper 可消费 | **Complete**。范围/OOM 失败先回滚页表再保持旧 break；页映射变化后跨 hart flush TLB。 |
| 221 `execve` | `sys_execve` / `kernel/src/syscall/process.rs`；loader 在 `kernel/src/task/loader.rs` | `const char *path, char *const argv[], char *const envp[]`；NUL 字节串；无 flags | 成功不回到旧映像；当前可返回 `EFAULT/ENAMETOOLONG/EINVAL/E2BIG/ENOENT/ENOMEM` | POSIX `execve()`；musl 直接 wrapper | **Partial**。只接受绝对路径和 UTF-8；loader 把 not-found/I/O/short-read 折叠为 `ENOENT`，坏 ELF 折叠为 `EINVAL`；无 auxv/TLS/CLOEXEC/signal reset/多线程 exec 语义。Phase 12 按字节串 ABI、ELF 错误与初始栈一次收口。 |

## 4. 标准最小闭环的缺失、删除与不计划入口

下表的“当前返回”都是 dispatcher 对未识别 number 的 `-ENOSYS`，不代表已进入该 syscall 后再返回的业务 errno。代码位置统一为 `kernel/src/syscall/mod.rs` 的 fallback；Removed 项的删除证据在对应 phase 文档。

| 编号 / Linux 名称 | 参数、userspace 结构与 flags | 标准返回/errno 契约 | POSIX 对应 / musl 路径 | 状态 | 已知差异与处理结论 |
|---|---|---|---|---|---|
| 23 `dup` | `oldfd`；无结构/flags | 新 fd；`EBADF/EMFILE` | POSIX `dup()`；musl wrapper | Removed | Phase 8 删除；无 fd entry/OFD 时不伪造共享 offset/status flags。 |
| 24 `dup3` | `oldfd,newfd,O_CLOEXEC` | 新 fd；`EBADF/EBUSY/EINVAL/EMFILE` | Linux 扩展；musl `dup3`/`dup2` 路径 | Missing | 与 fd/OFD/CLOEXEC 竖切一起实现。 |
| 25 `fcntl` | `fd,cmd,arg`；`F_*` 命令及 `flock` | 依 cmd 返回；`EBADF/EINVAL/...` | POSIX `fcntl()`；musl 直接 wrapper | Removed | Phase 8 删除忽略 command/OFD 分层的半实现。 |
| 29 `ioctl` | `fd,request,arg`；request-specific UAPI | request-specific；`EBADF/ENOTTY/EFAULT/...` | POSIX `ioctl()` 选项；musl tty/fd 路径 | Missing | 没有标准设备文件或 ioctl UAPI，不用私有设备 syscall 替代。 |
| 34 `mkdirat` | `dirfd,path,mode`；`mode_t` | 0；`EACCES/EEXIST/ENOENT/ENOTDIR/EROFS/...` | POSIX `mkdirat()`；musl wrapper | Missing | ext2 只读，无用户 fd/path-resolution 闭环。 |
| 35 `unlinkat` | `dirfd,path,AT_REMOVEDIR` | 0；`EACCES/ENOENT/ENOTDIR/EROFS/...` | POSIX `unlinkat()`；musl wrapper | Missing | ext2 只读，无 dirfd 语义。 |
| 56 `openat` | `dirfd,path,flags,mode`；`O_*` | fd；`EACCES/EEXIST/ENOENT/ENOTDIR/...` | POSIX `openat()`；musl `open/openat` 路径 | Missing | 必须先建 fd entry + OFD + pathname/flags 唯一模型。 |
| 57 `close` | `fd` | 0；`EBADF/EINTR/EIO` | POSIX `close()`；musl wrapper | Removed | Phase 8 删除不可达 fd table 上的表面 handler。 |
| 59 `pipe2` | `int pipefd[2], O_CLOEXEC|O_NONBLOCK` | 0；`EFAULT/EMFILE/ENFILE/EINVAL` | POSIX `pipe()` 由 musl 包装 Linux `pipe2` | Not Planned | Phase 9 结论：无 fd/OFD/close/dup/poll/signal 闭环前不接入。 |
| 61 `getdents64` | `fd, struct linux_dirent64 *, count` | 字节数；`EBADF/EFAULT/EINVAL/ENOTDIR` | musl `readdir` 内部路径 | Missing | 无 directory OFD/offset；kernel-only inode lookup 不暴露为用户遍历 ABI。 |
| 62 `lseek` | `fd,off_t,whence` | 新 offset；`EBADF/EINVAL/EOVERFLOW/ESPIPE` | POSIX `lseek()`；musl wrapper | Removed | Phase 8 删除；无可共享 OFD offset。 |
| 63 `read` | `fd,void *,count` | 字节数；`EBADF/EFAULT/EINTR/EIO/...` | POSIX `read()`；musl wrapper | Removed | Phase 8 删除轮询 SBI 且始终 runnable 的 stdin 实现。 |
| 65 `readv` | `fd,const struct iovec *,iovcnt` | 字节数；`EBADF/EFAULT/EINVAL/...` | POSIX `readv()`；musl wrapper | Missing | 无 fd/OFD，也不以内核临时聚合缓冲伪造 atomicity。 |
| 66 `writev` | `fd,const struct iovec *,iovcnt` | 字节数；`EBADF/EFAULT/EINVAL/EPIPE/...` | POSIX `writev()`；musl stdio 可消费 | Missing | bootstrap console 只保留 `write(1)`，未提升为通用 vectored I/O。 |
| 79 `newfstatat` | `dirfd,path,struct stat *,flags`；`AT_*` | 0；`EACCES/EBADF/EFAULT/ELOOP/...` | POSIX `fstatat()`；musl `stat/lstat/fstatat` 路径 | Missing | 无稳定 Linux RV64 `struct stat` UAPI 和 dirfd/symlink resolution。 |
| 80 `fstat` | `fd,struct stat *` | 0；`EBADF/EFAULT/EOVERFLOW` | POSIX `fstat()`；musl wrapper | Missing | 无用户 fd 和 Linux RV64 `struct stat` copyout。 |
| 96 `set_tid_address` | `int *tidptr` | TID；通常无 errno | Linux thread runtime；musl pthread startup/exit | Not Planned | 当前无 clone thread、clear-child-tid 和 futex wake 语义。 |
| 98 `futex` | `uaddr,op,val,timeout/uaddr2,val3`；`FUTEX_*`, `timespec` | 依 op；`EAGAIN/EFAULT/EINTR/EINVAL/ETIMEDOUT/...` | Linux thread primitive；musl locks/condvars | Removed | Phase 7/9 删除非闭环设计；未来必须有 address-space key、lost-wakeup 状态机和退出清理。 |
| 99 `set_robust_list` | `struct robust_list_head *,size_t` | 0；`EINVAL` | Linux thread runtime；musl pthread startup | Not Planned | 无 futex owner-death/robust cleanup 模型。 |
| 129 `kill` | `pid,sig` | 0；`EINVAL/EPERM/ESRCH` | POSIX `kill()`；musl wrapper | Removed | Phase 7 删除无 action/mask/frame 闭环的 handler。 |
| 130 `tkill` | `tid,sig` | 0；`EINVAL/EPERM/ESRCH` | 过时 Linux thread signal；musl 优先 `tgkill` | Not Planned | 不单独恢复过时入口。 |
| 131 `tgkill` | `tgid,tid,sig` | 0；`EINVAL/EPERM/ESRCH` | Linux thread signal；musl pthread kill/raise | Not Planned | 需与完整 signal disposition/mask/pending/frame 和 thread group 一起实现。 |
| 134 `rt_sigaction` | `sig,act,oldact,sigsetsize`；`struct sigaction` | 0；`EFAULT/EINVAL` | POSIX `sigaction()`；musl wrapper | Not Planned | Phase 7 明确不暴露部分 signal ABI。 |
| 135 `rt_sigprocmask` | `how,set,oldset,sigsetsize`；kernel sigset layout | 0；`EFAULT/EINVAL` | POSIX `pthread_sigmask/sigprocmask`；musl wrapper | Not Planned | 无 thread signal state，不接受后忽略 mask。 |
| 139 `rt_sigreturn` | 无显式参数；riscv64 rt signal frame | 恢复 context；错误 frame 通常致命 | POSIX handler return 由 musl trampoline 触发 | Removed | Phase 7 删除私有 frame 和取指地址 0 fallback。 |
| 146 `setuid` | `uid_t uid` | 0；`EAGAIN/EINVAL/EPERM` | POSIX `setuid()`；musl wrapper | Removed | Phase 11 删除只有 real/effective UID 的伪 credential 状态；无 saved-set-ID 前不恢复。 |
| 215 `munmap` | `addr,length` | 0；`EINVAL` | POSIX `munmap()`；musl allocator/dl | Missing | 当前只有 ELF/stack/brk 固定 area，无 VMA/VM object ABI。 |
| 220 `clone` | `flags,stack,parent_tid,tls,child_tid`；`CLONE_*` | child TID/PID；`EAGAIN/EINVAL/ENOMEM/...` | Linux process/thread primitive；musl pthread/fork 路径 | Not Planned | 当前明确一 Process 一 Thread；无 parent/child、TLS、clear-tid、signal/futex 闭环。 |
| 222 `mmap` | `addr,len,prot,flags,fd,offset`；`PROT_*`,`MAP_*` | 地址；`EACCES/EBADF/EINVAL/ENOMEM/...` | POSIX `mmap()`；musl allocator/dl | Missing | Phase 4 删除同名近似实现；未建立 VMA/VM object/file mapping 模型前不暴露。 |
| 226 `mprotect` | `addr,len,prot`；`PROT_*` | 0；`EACCES/EINVAL/ENOMEM` | POSIX `mprotect()`；musl dl/allocator 可消费 | Missing | 无 VMA split/merge 与 W^X 用户 ABI。 |
| 260 `wait4` | `pid,status,options,struct rusage *`；`W*` | child PID/0；`ECHILD/EINTR/EINVAL` | POSIX `waitpid()` 由 musl 组合 | Not Planned | 无 child relation；exit 直接回收，不保留伪 zombie。 |
| 261 `prlimit64` | `pid,resource,new,old`；`struct rlimit64` | 0；`EFAULT/EINVAL/EPERM/ESRCH` | Linux 扩展；musl `getrlimit/setrlimit` 路径 | Not Planned | 当前最小 init 启动不需要 resource limits；不返回伪造 infinity 表。 |
| 278 `getrandom` | `buf,len,GRND_*` | 字节数；`EAGAIN/EFAULT/EINTR/EINVAL` | Linux 扩展；musl entropy/stack-protector 路径 | Missing | 无经证明的 entropy source/CRNG；不以 timer/RTC 冒充随机数。 |

## 5. musl 最小路径结论

当前入口足以支撑仓库自带的静态最小 init，但不足以声称“可运行 musl 常规程序”。主要阻断项不是 syscall number，而是：

1. 无 `mmap/munmap/mprotect`，不能支撑 musl allocator、TLS 与 loader 的常规 VM 路径。
2. 无 `openat/close/read/fstat`，不能支撑标准文件 I/O 和动态 loader。
3. 无 signal/futex/clone 闭环，不能支撑 pthread 或 POSIX signal。
4. 当前 ELF 初始栈无 auxv，`execve` 仍有字节串和 errno 偏差。

因此 Phase 12 只验证正确的静态 ELF + 最小 userspace，不通过 libc fallback、私有 auxv 或内核专用 wrapper 绕开上述缺失。

## 6. ABI 不变量与心智验收

1. U-mode `ecall` 的 number 只从 `a7` 取得，最多 6 个参数只从 `a0..a5` 取得，返回值只写 `a0`。
2. trap 在 dispatch 前把旧映像 `sepc` 前移 4；`execve` 成功后不向新 TrapContext 写入返回值，因此新映像从 ELF entry 开始。
3. `exit`/`exit_group` 不返回，不在已释放的 task kernel stack 上保留 `Arc` owner。
4. `brk` 查询不修改映射；扩展或收缩失败时返回旧 break，不返回 `-ENOMEM`。
5. `write(1, NULL, 0)` 返回 0 且不解引用指针；`write(fd != 1, ..., 0)` 先校验 fd 并返回 `EBADF`。
6. `nanosleep(NULL, rem)` 返回 `EFAULT`；负秒或 `tv_nsec >= 1_000_000_000` 返回 `EINVAL`。
7. 所有 Missing/Not Planned/Removed number 不在共享 ABI crate 或 dispatcher 中占位，用户调用时一律得到 `-ENOSYS`。

## 7. Phase 12 输入

1. 把 `execve` 的 path/argv/envp 改为 Linux NUL-terminated byte strings，去掉 UTF-8 前提。
2. 使 VFS/loader 保留 `ENOENT/EIO/ENOEXEC` 等可观察失败类别，且在提交新 AddressSpace 前失败原子。
3. 审计静态 ELF 的 program header、BSS、alignment、entry、权限和栈 16-byte alignment。
4. 产生 Linux/riscv64 initial stack：`argc/argv/envp/auxv`，至少有 `AT_PAGESZ/AT_PHDR/AT_PHENT/AT_PHNUM/AT_ENTRY`。
5. 只保留验证上述标准路径的 `_start`、init、syscall wrapper、panic 和 bootstrap output。

## 8. 验证结果

- `git diff --check` 与 `cargo check --workspace` 通过；kernel warning 从 Phase 10 的 132 降为 9，主要来自删除未被任何 handler 使用的全量 errno 常量表。
- `make build-user`、`make build-kernel`、`make build-bootloader` 全部通过。
- `python3 create_fs.py create` 成功生成 128 MiB、4 KiB block 的 ext2 镜像，且只写入 `/bin/init`。
- 两轮 QEMU `virt -smp 8` 冷启动分别由 boot hart 7 和 5 开始；8 个 hart 全部上线，RTC、VirtIO block、ext2 mount 与 init 入队成功，观察窗口内无 panic/fault。
- `cargo fmt --all -- --check` 仍只因本阶段未修改的 `kernel/src/arch/mod.rs` 与 `kernel/src/arch/riscv64/mod.rs` 导出排序失败；未为格式检查扩大 diff。
- 静态检索确认不存在 `SYSCALL_SETUID`、`sys_setuid`、credential 伪状态、exec number `221` 的 trap 硬编码或 LiteOS 私有 syscall number。
- 按仓库规则未执行、维护或修正测试用例。
