# Phase 22：BusyBox tracer bullet 与 cwd inode

## 目标

用固定、未修改的 BusyBox 1.37.0 `init + ash` 直接替换诊断镜像中的 `/bin/init`，以真实上游调用链确定首个 runtime blocker，并只实现对应的标准 Linux/riscv64 ABI。

## 首次启动证据

BusyBox 静态 `ET_EXEC` 已由现有 loader 进入 U-mode。最早可观察失败为：

```text
syscall: invalid syscall_id: 49
init: can't change directory to '/': Function not implemented
```

编号 49 是 Linux/riscv64 `chdir`。此前 cwd 字符串始终为 `/`，而所有 `AT_FDCWD` relative lookup 实际也从 VFS root 开始；直接增加字符串赋值会把错误模型扩展成第二条 namespace 状态。

## 终态模型

Process 只拥有一个 cwd directory inode。所有 relative `openat/mkdirat/unlinkat/newfstatat/renameat2/execve/chdir` 从该 inode 进入 VFS；`getcwd` 不读取 path cache，而是从 inode 沿 `..` 到 root，并在 parent directory entry 中反查当前名字。由此 rename 后的路径来自同一 namespace authority，fork child 取得 inode 引用后可以独立 chdir，Thread 继续共享 Process owner。

`chdir` 当前支持 raw absolute/relative path、`.`、`..` 与重复 `/`；不支持 credentials/execute permission、mount namespace 或 symlink following，因此在 syscall 矩阵中保持 Partial。

## Consumer 与后续 blocker

固定 musl consumer 创建 `/cwd`，执行 `chdir("/cwd") -> getcwd -> chdir("..") -> getcwd("/") -> rmdir`，随后继续通过原 pthread/signal/restart 完整路径。

同一个 BusyBox 产物在修复后打印：

```text
init started: BusyBox v1.37.0
```

下一组真实缺口是 `setsid(157)`、`rt_sigtimedwait(137)` 与 init 的 vfork 路径；`reboot(142)` 和 `ioctl(29)` 目前也返回 `ENOSYS`。后续必须按 session/process-group/TTY/signal-wait 的单一领域模型实现，不能以 BusyBox patch、伪成功 stub 或 inittab 规避长期语义。

> 后续状态：Phase 23 已加入 DTB UART IRQ、统一 Console wait、`writev(66)` 和真实 ash 输入/执行 gate；上述其余缺口继续保持明确未支持。

## 验证

- `cargo check/clippy` 覆盖 `syscall-abi/kernel/user`；
- 固定 musl consumer 冷启动并通过 cwd + pthread + signal restart 路径；
- 固定 BusyBox 诊断镜像越过原 chdir failure 并输出上游 init banner；
- architecture interface 记录新增 syscall、VFS reverse path 与 Process cwd interface。
