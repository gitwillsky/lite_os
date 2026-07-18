# Process、credential 与 identity syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 93 | `exit` | Complete | Thread exit、robust cleanup 与 clear-child-tid |
| 94 | `exit_group` | Complete | group status 唯一提交与 sibling 退出 |
| 96 | `set_tid_address` | Complete | calling Thread clear-child-tid |
| 144 | `setgid` | Partial | 当前 credential model 的标准 permission 范围 |
| 146 | `setuid` | Partial | 当前 credential model 的标准 permission 范围 |
| 147 | `setresuid` | Partial | real/effective/saved UID 与 privilege drop |
| 148 | `getresuid` | Complete | 三 UID copyout |
| 149 | `setresgid` | Partial | real/effective/saved GID 与 privilege drop |
| 150 | `getresgid` | Complete | 三 GID copyout |
| 154 | `setpgid` | Partial | 当前 process graph、exec 与 session 约束 |
| 155 | `getpgid` | Complete | live process 查询 |
| 156 | `getsid` | Complete | live process 查询 |
| 157 | `setsid` | Complete | session/process-group transaction |
| 158 | `getgroups` | Complete | supplementary group snapshot |
| 159 | `setgroups` | Complete | privileged immutable group publication |
| 167 | `prctl` | Partial | parent-death signal 与已声明 options |
| 172 | `getpid` | Complete | TGID |
| 173 | `getppid` | Complete | process graph parent |
| 174 | `getuid` | Complete | real UID |
| 175 | `geteuid` | Complete | effective UID |
| 176 | `getgid` | Complete | real GID |
| 177 | `getegid` | Complete | effective GID |
| 178 | `gettid` | Complete | Thread ID |
| 220 | `clone` | Partial | fork/thread/vfork 已声明 flags；SETTID 为 Linux best-effort store，fault 不回滚 child；其余返回标准错误 |
| 221 | `execve` | Partial | ELF64/script、dynamic musl 与 single-thread commit |
| 260 | `wait4` | Partial | exit/stop/continue event 与 rusage 子集 |
| 261 | `prlimit64` | Partial | 已声明 resources、permission 与 copyout ordering |

## 已知缺口

普通多线程 Process 的全部 fork/exec 组合、完整 clone namespace/ptrace flags 与任意 process capability model 尚未开放。
