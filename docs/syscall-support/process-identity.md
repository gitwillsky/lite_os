# Process、credential 与 identity syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 90 | `capget` | Partial | v1/v2/v3 标准 codec、任意 live TID/原 leader/zombie；实际 user namespace owner |
| 91 | `capset` | Partial | calling Thread P/E/I 与 bounding/ambient 约束、原子失败；实际 user namespace owner |
| 93 | `exit` | Complete | Thread exit、robust cleanup 与 clear-child-tid |
| 94 | `exit_group` | Complete | group status 唯一提交与 sibling 退出 |
| 95 | `waitid` | Partial | native ALL/PID/PGID/PIDFD、exit/stop/continue、NOWAIT/NOHANG/clone class；复用唯一 claim，消费先于 copyout；rusage 为 child 含其已回收后代的使用量 |
| 96 | `set_tid_address` | Complete | calling Thread clear-child-tid |
| 97 | `unshare` | Partial | 零、CLONE_FS/CLONE_FILES/CLONE_NEWUSER/CLONE_NEWUTS/CLONE_NEWNS/CLONE_NEWTIME；全部锁外准备、复合 OOM 保留、calling Thread 脱离；NEWTIME 只改 for_children |
| 144 | `setgid` | Partial | 当前 credential model 的标准 permission 范围 |
| 146 | `setuid` | Partial | 当前 credential model 的标准 permission 范围 |
| 147 | `setresuid` | Partial | real/effective/saved UID 与 privilege drop |
| 148 | `getresuid` | Complete | 三 UID copyout |
| 149 | `setresgid` | Partial | real/effective/saved GID 与 privilege drop |
| 150 | `getresgid` | Complete | 三 GID copyout |
| 151 | `setfsuid` | Partial | filesystem UID、旧值返回与 commoncap FS-set transition；按实际 user namespace 映射 |
| 152 | `setfsgid` | Partial | filesystem GID、旧值返回；按实际 user namespace 映射 |
| 154 | `setpgid` | Partial | 当前 process graph、exec 与 session 约束 |
| 155 | `getpgid` | Complete | live process 查询 |
| 156 | `getsid` | Complete | live process 查询 |
| 157 | `setsid` | Complete | session/process-group transaction |
| 158 | `getgroups` | Complete | calling Thread immutable sorted group snapshot；size=0 不分配 |
| 159 | `setgroups` | Complete | calling Thread privileged immutable group publication；逐元素错误顺序 |
| 167 | `prctl` | Partial | `PR_SET_NAME`/`PR_GET_NAME`（calling Thread 独立 comm）、parent-death signal、共享 MM dumpability 0/1、NNP/seccomp、keepcaps/securebits/bounding/ambient capability operations |
| 172 | `getpid` | Complete | TGID |
| 173 | `getppid` | Complete | process graph parent |
| 174 | `getuid` | Complete | real UID |
| 175 | `geteuid` | Complete | effective UID |
| 176 | `getgid` | Complete | real GID |
| 177 | `getegid` | Complete | effective GID |
| 178 | `gettid` | Complete | Thread ID |
| 220 | `clone` | Partial | 独立 Process 的 0..64 exit signal、PARENT+VM/FS/FILES/SIGHAND/VFORK 组合，共用单一 publication；PIDFD/THREAD PIDFD 严格 fd copyout，legacy PIDFD 与 PARENT_SETTID 拒绝共用输出；SETTID 为 Linux best-effort store |
| 221 | `execve` | Partial | ELF64/script、dynamic musl、完整 sibling retirement 后唯一 survivor commit；非 leader 取得 TGID/原 leader birthdate，creator-child relation 不生成虚假父死亡事件 |
| 260 | `wait4` | Partial | exit/stop/continue、__WCLONE/__WALL/__WNOTHREAD；native int pid/options，消费先于 copyout；rusage 当前须为空 |
| 261 | `prlimit64` | Partial | 已声明 resources、permission 与 copyout ordering |
| 268 | `setns` | Partial | nsfs FD 的 user/UTS/mount/time owner；type=0 或匹配 CLONE_NEW*，user 单线程/FS 独占、mount 的 root/cwd/membership 同次提交、time 要求无其他 Thread/MM user 并替换 active/for_children，target-scoped 授权 |
| 277 | `seccomp` | Partial | calling Thread strict、validated classic BPF、action/sizes queries、TSYNC、USER_NOTIF listener |
| 424 | `pidfd_send_signal` | Partial | 实际 pidfs 或 proc TGID directory fops、SELF_THREAD/SELF_GROUP、THREAD/TGID/PGID scope、native DAC/PID namespace/si_code；O_PATH 拒绝 |
| 434 | `pidfd_open` | Partial | native NONBLOCK/THREAD、CLOEXEC/真实 pidfs inode、实际 association poll/epoll 与 PID reuse；GETVERSION/GET_INFO/现有 namespace ioctl；已接入 NET；cgroup namespace 未支持 |
| 435 | `clone3` | Partial | 标准 64/80/88-byte 前缀、零 future tail、stack range、TLS/TID stores、CLEAR_SIGHAND、已实现 namespace 与 set_tid 精确编号；PARENT/PIDFD/0..64 exit signal 已补源码；cgroup 未支持，且不在本轮新增范围 |
| 438 | `pidfd_getfd` | Partial | flags=0、精确 pidfs fops、目标 exec guard/ATTACH_REALCREDS、授权后 PF_EXITING/target fd 检查、同一 OFD+CLOEXEC |

## 已知缺口

`prctl` option 按 Linux `int` 解码低 32 位，其余参数为完整 unsigned long。
`PR_SET_NO_NEW_PRIVS` 只接受 arg2=1、arg3/4/5=0；`PR_GET_NO_NEW_PRIVS` 要求全部参数为零，
直接返回 calling Thread 的 0/1。该 bit 不可清除，clone/fork/vfork 继承、exec 保留，并抑制
setuid/setgid executable 提权。P/E/I、bounding/ambient 与 securebits 已按固定 commoncap 实现；
file capability/xattr 支持范围见 filesystem 矩阵，
不能将该选项描述为完整 Linux sandbox。

PR_GET/SET_DUMPABLE读写共享MM，SET只允许0/1，suid_dumpable固定0；身份E/FS/newP变化
先禁用dumpability并清PDEATH，再发布凭据。exec遵循旧R/E provisional与credential effects，
ELF/PT_INTERP不可读保持disabled；proc同组免检，非dumpable必须另检查MM创建namespace的SYS_PTRACE。

raw credential syscall 只更新 calling Thread，clone/fork 继承其 UID/GID/groups；
libc 线程同步通过各 Thread 的标准 syscall 完成。Process `/proc` 投影保留原 leader identity，
Thread status 使用自身 identity；RLIMIT_NPROC 按全部 live Thread 的实际 real UID 计数。
Thread exit/exec release 始终退休旧 clearTid；仅旧 mm_users>1 时尝试零写和 wake，零写
fault 不抑制 readable futex key 的 wake。外部 TCB 不计作 mm user；原 leader exit 后
Process proc memory/FDSize 为零、state 为 Z，live worker proc 保留自身字段。

PID/IPC/NET namespace 接入实际 owner；NET 支持独立 lo 与已声明的 socket/proc 范围，不支持设备迁移。ptrace clone flags 未开放；IPv6、多网卡不在本轮范围。
capability 初始为 UID0 的全集，UID0 不直接绕过具名权限；capset只更新 calling Thread，
capget支持任意TID和到reap为止的原leader/zombie。exec建栈前一次准备最终IDs/caps、
AT_UID/EUID/GID/EGID/SECURE与PDEATH consequence，PONR只提交该结果。SGID需IXGRP，
ambient按旧supplementary groups判断，NNP候选增长可能降级effective IDs到real IDs。

seccomp strict 只允许 read/write/exit/rt_sigreturn，违例以 SIGKILL 终止 calling Thread。
filter 支持固定 Linux seccomp classic BPF whitelist、最多 4096 条程序与 32768 validated cBPF
路径成本（每个 ancestor 加四）。不可撤销，fork/clone/vfork 继承、exec 保留，全部通用与架构
syscall 在 handler 前经过同一 filter。授权为 no_new_privs 或当前 CAP_SYS_ADMIN；
capability 按实际 user namespace 授权；完整 namespace sandbox 仍有上述缺口。支持 KILL_PROCESS/KILL_THREAD/TRAP/ERRNO/
ALLOW/LOG/USER_NOTIF；TRACE 没有 ptracer、USER_NOTIF 没有 listener 时按 Linux 返回 ENOSYS。
NEW_LISTENER 返回 CLOEXEC/O_RDWR 匿名 fd；GET_NOTIF_SIZES、RECV/SEND/CONTINUE/ID_VALID、
extensible ADDFD（SETFD/SEND）与 scalar NOTIF_SET_FLAGS 按固定 Linux codec。真实 fd 注入由目标线程
完成，listener 关闭使未答复 syscall 返回 ENOSYS；取消后的 ID 无效。WAIT_KILLABLE_RECV 在接收后
只被致命信号中断；LOG flag 和 RET_LOG 遵从 kernel log level。同步 wake flag 以 affinity/active
允许的当前 CPU 作为 placement 提示。SPEC_ALLOW 可接受；当前静态 architecture profile 没有
seccomp-specific per-task mitigation override，不声明新增硬件 mitigation。TSYNC 校验 disabled 或 calling policy ancestor 的全部未退出 siblings 后一次提交；
不兼容返回 target TID，TSYNC_ESRCH 改为 ESRCH，失败不改变任何 policy。同步传播 caller NNP，
group exit 已提交时校验后只更新 caller。TSYNC+NEW_LISTENER 必须同时给 TSYNC_ESRCH，
WAIT_KILLABLE_RECV 必须同时给 NEW_LISTENER；已附着 listener 的 ancestry 拒绝第二 listener。
通知请求 pin 真实 TID identity，接收时按 calling PID namespace 输出编号；listener 跨 namespace 传递不冻结接收 view。TSYNC 失败编号同样按 caller view 投影。

User/UTS namespace 使用标准 clone/unshare/setns 与 `/proc/PID[/task/TID]/ns/{user,uts}`；
nsfs ioctl 提供 owning user/parent/type/owner UID/namespace ID，seek/mmap 不按 REG inode 放行。
UID/GID maps 由 target owner 一次发布，per-open 固定 opener credentials 与 target namespace；
setgroups deny 单向，root 映射须满足 parent SETFCAP 条件。namespace 深度、范围/重叠/非法ID、
caller/chroot/FS共享约束按固定 Linux 基线检查。User/UTS 资源按创建者 kuid 与 user namespace
祖先逐级收费，实际 final Drop 归还；标准 `/proc/sys/user/max_user_namespaces` 与
`max_uts_namespaces`、`max_mnt_namespaces`、`max_time_namespaces` 与 `max_pid_namespaces` 控制实际 owner，
子 namespace 的能力不能修改祖先配额。Time namespace 提供 ns/time 与 ns/time_for_children；
timens_offsets 每次 IO 捕获 target 的 for_children，写入使用 opener f_cred 的 SYS_TIME，
支持两行 monotonic/boottime 或数字 clock，标准范围/冻结错误，第三行保留为未消费 suffix。
普通 fork 与 exec 进入 for_children，CLONE_VM 保留 active，setns 同次替换两者；
legacy clone 的低 byte 仍为 exit signal，创建新的 time namespace 使用 unshare。
PID namespace 的 clone/unshare/setns、祖先编号 reservation、init shutdown/reparent 与
signal/wait/TTY 使用实际 namespace 投影；proc mount 固定实际 PID view，stat/status/NS* 使用真实 ladder，
kernel/pid_max 控制实际 namespace 的 allocation limit。seccomp 通知、SO_PEERCRED、
SCM_CREDENTIALS、capget/capset、prlimit 与已声明 PID selector 使用实际 namespace 投影。
clone3 的基本参数、set_tid 与已声明 flags 使用同一 clone owner；完整 proc 范围见进程契约。
IPC semaphore/shared memory 使用实际 IPC owner；文件系统配额不在本轮新增范围。


- CLONE_NEWIPC 为实际 IpcNamespace，clone/unshare 默认共享或新建标准空 message registry；
  NEWIPC|SYSVSEM clone 拒绝 EINVAL。setns 同时要求当前 UserNS 和目标 owning UserNS 的
  SYS_ADMIN，最后旧 membership/namespace Drop 在 tuple guard 外。/proc/ns/ipc 与 user
  max_ipc_namespaces 只投影/收费实际对象，不把 Pipe、eventfd 或已有 fd 归入 IPC namespace。
  semaphore/shared memory、SEM_UNDO 已补源码；POSIX mqueue 不在本轮新增范围。

clone/wait relation：ProcessNode 唯一拥有 exit_signal 与 real-parent exec generation 快照；CLONE_PARENT 最终同锁选择实际 parent Thread，但 security、session/group 与 terminal 仍来自 caller。零信号 child 保留 zombie，非
SIGCHLD 不服从 SIGCHLD IGN/NOCLDWAIT autoreap。__WALL 覆盖 __WCLONE，__WNOTHREAD 使用同一 parent_thread；wait4 的 pid/options 按 native 32 位解码。子进程 exec 或跨 Process orphan adoption 将 exit_signal 设为 SIGCHLD；父 exec 只把退出通知强制为
SIGCHLD，保留原 child wait class。


CLONE_NEWNET源码已接clone/clone3/unshare/setns：创建独立实际protocol/ports及DOWN
无地址lo，UP发表127/8；NEWUSER|NEWNET用新owning UserNS准备，权限失败或OOM不替换旧tuple。
/proc/*/ns/net、NS_GET_NSTYPE/USERNS及PIDFD_GET_NET_NAMESPACE共享同一实际对象；setns
要求calling和target owning UserNS的SYS_ADMIN，旧socket/proc fd捕获域保持。标准guest及
actual factory/OOM/quota/pin/loopback回归已写，集中阶段执行；完整容器网络扩展不在本轮新增范围。

`PR_SET_NAME`/`PR_GET_NAME` 的名称最长 15 字节并以 NUL 填充；`/proc/<tgid>/task/<tid>` 的 comm、stat、status
读取指定线程的名称，顶层 `/proc/<tgid>` 读取 leader 名称。fork/clone 按值继承 caller 名称，exec 更新 survivor 并重指 leader 别名。
