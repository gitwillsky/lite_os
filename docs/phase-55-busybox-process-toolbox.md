# Phase 55：BusyBox 进程发现与生命周期工具箱

本阶段在唯一动态 BusyBox 1.37.0 rootfs 上开放 `pidof/pgrep/pkill/killall/timeout/nohup/watch`，形成“发现进程、筛选、发 signal、限时运行、忽略 hangup、周期观察”的 shell 运维闭环。所有 applet 仍是 `/bin/init` 同一 inode 的 hardlink，不引入私有进程 API、BusyBox patch 或第二套状态。

## 实现边界

- procfs 新增动态 `/proc/self` 与 live `/proc/<pid>/{status,comm,cmdline}`；原有 `stat` 保持同一 Process snapshot 来源。
- `cmdline` 从 MemorySet 的 Linux mm argument range 读取 NUL 分隔的实时用户栈 bytes；fork/vfork 继承该 range，exec transaction 随新初始栈原子替换。读取失败返回空内容，不以 `comm` 伪造 argv。
- `status` 的 identity、groups、FDSize、memory、process-group/session 与 thread count 分别来自 credentials、fd table、MemorySet 和 process graph 的唯一 owner。
- 当前未提供 `/proc/<pid>/exe`、task thread directories 或完整 Linux status 字段；进程工具在已声明 process 模型中通过标准 `stat/cmdline` 完成发现。

## 运行验收事实

- guest 并发启动同名进程，核对 `status/comm/cmdline/self`，并由 `pidof`、name/full-command `pgrep` 找到真实 PID。
- `pkill -P` 与 `killall` 发送 SIGTERM，ash `wait` 观察 143，进程退出后对应 proc directory 消失。
- `timeout` 终止超时 consumer；`nohup` 安装 SIGHUP ignore 后在显式 HUP 下继续到正常退出。
- `watch` 至少完成两次周期刷新，SIGINT 后由 `wait` 观察 130，terminal echo 保持正常。
- 完整 `python3 scripts/verify_busybox.py --image fs.img` 冷启动门通过，并继续执行既有全部 BusyBox 功能门。
