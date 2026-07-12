# Phase 37：TTY 后台访问 job control

## 目标

按固定 Linux v7.1 `tty_jobctrl.c` 与 `n_tty.c` 语义补齐 controlling TTY 后台访问：read 使用 SIGTTIN，启用 `TOSTOP` 的 write 与 TTY state change 使用 SIGTTOU。实现不得复制 Terminal/process/signal 状态，也不得为 BusyBox 建立旁路。

## 唯一 seam

职责拆成三个 owner，但只有一条调用链：

1. Terminal 根据 controlling session、foreground process group 与 `TOSTOP` 返回所需 signal；
2. TaskManager process graph 判断 caller process group 是否 orphaned，并读取当前 Thread mask 与 Process disposition；
3. syscall 只把领域结果翻译成 `EIO` 或内部 restart sentinel。

后台 SIGTTIN 被 blocked/ignored 时返回 `EIO`；后台 SIGTTOU 被 blocked/ignored 时允许操作；orphaned process group 两者均返回 `EIO`。其他情况向 caller 的完整 process group 发布 kernel signal。默认 stop action 在停止前恢复原始 ecall context，SIGCONT 后重放 syscall；caught signal 仍由 `SA_RESTART` 决定重放或 `EINTR`。

`/dev/console` read/write 保持 Linux redirected-console 豁免；job-control enforcement 作用于 caller controlling session 的 `/dev/tty`。`TCSETS*` 与 `TIOCSPGRP` 无 redirected-console 豁免，始终执行 SIGTTOU state-change check。

## 架构收敛

- Terminal 从 `fs/file.rs` 提取为独立深模块，原文件从 648 行降至 318 行并退出例外表；
- signal frame/delivery 从 `task/model.rs` 下沉到既有 signal module，原文件从 1326 行降至 1126 行；
- read/write/readv/writev 从 `syscall/fs.rs` 提取为 fd-I/O 深模块，原文件从 1124 行降至 645 行；
- TaskManager 的 process exit status 下沉到其 owner module，主文件从 1483 行降至 1471 行。

## 验证

固定动态 BusyBox 增加官方 `stty` applet，并在真实 interactive ash 中验证：

- 后台 child 从 `/dev/tty` read 后进入 Stopped，`fg`/SIGCONT 后 syscall 重放并读取 UART 输入；
- ignored SIGTTIN 的后台 read 返回 `EIO`，不会停止；
- `stty tostop` 后后台 child 向 `/dev/tty` write 进入 Stopped，`fg` 后重放并写出；
- ignored SIGTTOU 的后台 write 直接成功。

UART interaction gate 使用单调 output cursor；每个 prompt/Stopped marker 只能匹配上一次交互之后的新输出，避免历史重复文本提前注入 Ctrl-C、fg 或后续命令。

`make verify` 仍是唯一强制入口，覆盖架构围栏、Clippy、构建、1/3/8 hart、musl 与动态 BusyBox 冷启动。
