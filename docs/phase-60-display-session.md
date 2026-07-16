# Phase 60：display-session、标准 seat handoff 与首个 2D client

实现状态：broker、共享 libseat client lifecycle、terminal 迁移与首个 double-buffered 2D reference client 已落地；静态/构建门已闭环，运行时往返与故障注入证据仍按本阶段完成条件采集。

本阶段把当前“terminal 永久独占 DRM/input”深化为可证明恢复的单 seat capability domain。固定语义源为 Linux v7.1 AF_UNIX/SCM_RIGHTS/DRM/evdev、固定 upstream seatd/libseat wire revision 与 libdrm；不新增 LiteOS 私有 syscall、私有 session protocol 或第二套设备状态。

## 1. 用户态边界

- `/bin/display-session` 是独立、单线程、`no_std + alloc` Rust broker，唯一拥有 `seat0` active/pending transition；它只实现固定 seatd wire compatibility，clients 统一链接 pinned upstream `libseat.so` 的 seatd backend。
- broker 只授予/撤销 capability，不拥有 mode、framebuffer、mmap、connector topology、hotplug 或 pixel。terminal 与首个 2D client 统一经 pinned upstream `libdrm.so` 使用 legacy KMS；terminal 现有 raw DRM codec 在迁移提交时删除。
- 恰有一个 active session、至多一个 pending activation，不建立队列或 latest-wins。terminal 是永久 recovery target：inactive 时保留 process/model/PTTY 并有界解析输出，但关闭 DRM/evdev/GEM/framebuffer；重新 active 后 cold reacquire 并整帧重绘。
- 自动 activation 只接受当前 terminal foreground process group。broker 用 `SO_PEERCRED`、标准 proc/job-control 状态与固定 terminal identity 判定；background client 无法抢占。

## 2. 两阶段撤销与恢复域

1. broker 向 active client 发 disable，记录 monotonic generation 与 250 ms deadline；client ACK 只能匹配当前 generation。
2. deadline 前 ACK 后正常关闭 capability；deadline 到期则 broker 以标准 `DRM_IOCTL_DROP_MASTER` 与 `EVIOCREVOKE` 强制撤销。撤销、disconnect 与 recovery path 禁止分配。
3. client crash/协议错误只局部回退 terminal。broker panic、SIGKILL 或无法证明 revoke 完成属于系统恢复域，由 BusyBox init guard 触发 cold reboot；禁止伪装成可安全 respawn。

BusyBox init 独立 respawn terminal，broker 不 fork/exec/wait 它。broker 仅在 `euid=0、PPID=1、固定 comm` 同时成立时识别默认 terminal；terminal 缺失期间不激活 graphics client。

## 3. 必需 Linux ABI

- pathname AF_UNIX 必须具有真实 VFS socket inode、目录权限、unlink 后 inode lifetime 与 connect lookup；abstract namespace 保持原 owner，禁止 pathname registry 冒充 inode。
- `SO_PEERCRED` 返回连接建立时冻结的 peer identity。`SCM_RIGHTS` 对 stream/datagram/socketpair 通用，单条最多 `SCM_MAX_FD=253`，接收控制区不足按 Linux 关闭多余 fd 并置 `MSG_CTRUNC`。
- fd passing 允许 AF_UNIX socket 自身，因而 in-flight rights graph 必须可回收 cycle：普通非 socket rights 走 O(1) fast path；可能成环的 send、last-root close 或 pressure 同步执行 allocation-free iterative SCC，复杂度 O(V+E)。节点数由 per-real-UID `RLIMIT_NOFILE` inflight accounting 限制；一次 GC 后仍超限返回 `ETOOMANYREFS`，不设后台 GC worker。
- `/proc/<pid>/stat` 必须投影真实 `tty_nr/tpgid`；evdev 必须实现不可逆 `EVIOCREVOKE` 的 `ENODEV` 与 HUP+ERR wake 语义。kernel 不识别 seatd message，也不按设备类型过滤 SCM_RIGHTS。

## 4. 首个 consumer 与完成条件

- 首个 graphics consumer 是紧凑常驻 Rust 2D reference client：libseat+libdrm、两个 dumb buffer、page-flip event、damage-tracked software geometry/pointer、keyboard/mouse 与 resize/hotplug。idle 必须阻塞，render 上限 60 fps；不引入 toolkit、compositor、Mesa/GBM/EGL、字体、图片或 3D。
- broker/terminal/2D client 的持久容器都必须 fallible；revoke/exit/interrupt cleanup 不分配。`user/` 的 600 行 module 围栏与精确 source 清单覆盖新增 Rust crate。
- 完成必须证明：terminal↔2D 往返 activation、client crash、stale ACK、250 ms forced revoke、broker fatal cold reboot、SCM_RIGHTS cycle GC、fd/control truncation、background steal rejection，以及 idle 无轮询。不得用私有 ABI、兼容入口或双轨 DRM 实现通过。
