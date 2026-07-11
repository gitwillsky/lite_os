# Phase 25：Terminal、session 与 foreground signal

## 目标

把 Phase 23 的 raw console OFD 原地深化为一个 Linux TTY domain：BusyBox init 能创建 session 并取得 controlling terminal，ash 能建立 foreground process group，UART 输入由 termios line discipline 处理，Ctrl-C 只投递给 foreground group。不得保留 raw-console syscall 旁路或 BusyBox 专用 ioctl 成功桩。

## 所有权

- process graph 唯一拥有每个 TGID 的 SID/PGID，fork 继承，`setsid/setpgid` 在同一 graph lock 内校验和提交；
- 一个 `Terminal` 同时拥有 platform Console、kernel termios、window size、fixed cooked queue、controlling SID 与 foreground PGID；fd 0/1/2 只是引用该对象的普通 OFD；
- UART driver 只拥有 hardirq raw ring。deferred console softirq 调用 Terminal line discipline，再通过 process graph 定位 foreground group；不存在第二份 input queue 或 task-table polling。

session leader 最后一个 Thread 退出时释放 Terminal 的 controlling-session relation，使 BusyBox init 后续 respawn 可以建立新的 session。process exit、wait4 与 SIGCHLD 仍沿原唯一 lifecycle。

## ABI

新增 Linux/riscv64 `ioctl(29)`、`setpgid(154)`、`getpgid(155)`、`getsid(156)`、`setsid(157)`。TTY ioctl 当前支持 `TCGETS/TCSETS/TCSETSW/TCSETSF`、`TIOCSCTTY`、`TIOCGPGRP/TIOCSPGRP`、`TIOCGWINSZ/TIOCSWINSZ` 和 `TIOCGSID`；非 Terminal fd 或未知 request 返回 `ENOTTY`。

line discipline 支持 CR/NL input mapping、OPOST/ONLCR、canonical line、echo、erase、kill、EOF，以及 ISIG 的 VINTR/VQUIT/VSUSP。signal 使用 `SI_KERNEL` 并向 foreground process group 的每个 live Process 投递一次；显式 ignore/mask、wait cancellation 与 frame delivery继续复用单一 pending-signal owner。

当前明确不支持完整 VMIN/VTIME、TCSETSW drain、TCSETSF flush、TIOCNOTTY、background read/write 的 SIGTTIN/SIGTTOU enforcement，以及 stopped/continued scheduler/wait4 lifecycle，因此相关 syscall 保持 Partial。

## 验收

固定 musl consumer 验证 `TCGETS/TCSETS/TIOCGWINSZ`，child `setsid → TIOCSCTTY → tcgetpgrp`，并证明 session leader 的 `setpgid` 返回 `EPERM`。固定 BusyBox gate 禁止 syscall 29/137/154-157 缺失，启动 init+ash 后执行算术命令，再让 ash 进入 foreground infinite loop；注入 VINTR 后必须继续执行并输出 `LITEOS_TTY_CTRL_C_OK`。全量 `make verify` 继续覆盖 1/3/8 hart、架构围栏和两个真实上游 consumer。
