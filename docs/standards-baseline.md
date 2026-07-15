# LiteOS Phase 0：官方规范与上游源码基线

> 固定日期：2026-07-11（Asia/Shanghai）
> 范围：仅建立后续审计与实现所使用的规范/源码权威基线；不判断 LiteOS 当前实现是否合规。
> 来源策略：只使用标准组织发布物、上游官方仓库与项目官方站点。博客、百科、论坛、第三方 syscall 表不作为结论依据。

## 1. 固定基线总表

| 领域 | 固定版本或不可变 revision | 直接来源 | 在 LiteOS 中负责定义 |
|---|---|---|---|
| Linux/riscv64 ABI | Linux 主线正式版 `v7.1`；tag object `b3f94b2b3f3e51ab880a51fc6510e1dafba654ed`；peeled commit `8cd9520d35a6c38db6567e97dd93b1f11f185dc6` | [Linux 7.1 commit](https://github.com/torvalds/linux/commit/8cd9520d35a6c38db6567e97dd93b1f11f185dc6)、[kernel.org releases](https://www.kernel.org/) | syscall 编号、Linux/riscv64 寄存器契约、UAPI 类型/结构体/flags、返回值和 Linux errno 契约 |
| ext/JBD2 on-disk protocol | Linux `v7.1` 同一固定 commit；`Documentation/filesystems/ext4/journal.rst`、`include/linux/jbd2.h`、`fs/jbd2/{commit,recovery}.c` | [JBD2 documentation](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/Documentation/filesystems/ext4/journal.rst)、[fixed implementation](https://github.com/torvalds/linux/tree/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/jbd2) | journal superblock/header/tag/commit 的 big-endian layout、escape、flush/commit/replay 顺序与 ext orphan recovery |
| RISC-V ELF psABI | `1.1` pre-release，2026-07-01；tag `draft-20260701-e03d44ae2f0e1144f9498c2896b5ae25b0449398`；commit `e03d44ae2f0e1144f9498c2896b5ae25b0449398` | [固定源码](https://github.com/riscv-non-isa/riscv-elf-psabi-doc/tree/e03d44ae2f0e1144f9498c2896b5ae25b0449398)、[发布 HTML](https://riscv-non-isa.github.io/riscv-elf-psabi-doc/) | LP64/LP64D、过程调用、ELF、重定位、动态链接、TLS、DWARF；**不定义 syscall calling convention** |
| RISC-V Privileged Architecture | 官方发布包 `v20260120`；Machine ISA `1.13`、Supervisor ISA `1.13`，相关列出的扩展均为 ratified | [固定版本 HTML](https://docs.riscv.org/reference/isa/v20260120/priv/priv-index.html)、[固定版本 PDF](https://docs.riscv.org/reference/isa/v20260120/_attachments/riscv-privileged.pdf)、[版本清单](https://docs.riscv.org/reference/isa/v20260120/priv/priv-preface.html) | M/S/U 特权边界、CSR、trap/return、interrupt delegation、页表与 `SFENCE.VMA`、PMP 等硬件语义 |
| RISC-V SBI | SBI `v3.0`；tag/commit `c33ad9f414505806f084e8677e04d2744f76c8df` | [官方文档](https://docs.riscv.org/reference/sbi/intro.html)、[固定源码](https://github.com/riscv-non-isa/riscv-sbi-doc/tree/c33ad9f414505806f084e8677e04d2744f76c8df) | S-mode 与更高特权执行环境之间的调用编码、错误和各 SBI extension；不定义 U-mode Linux syscall |
| POSIX | POSIX.1-2024 / The Open Group Base Specifications Issue 8，publication id `9799919799`，2024 Edition | [Issue 8 在线规范](https://pubs.opengroup.org/onlinepubs/9799919799/)、[固定 2024 Edition 入口](https://pubs.opengroup.org/onlinepubs/9799919799.2024edition/)、[官方离线包入口](https://pubs.opengroup.org/onlinepubs/9799919799/download/index.html) | 用户可观察 API/utility 语义；不定义 Linux syscall 编号、寄存器 ABI 或 Linux 私有结构体 |
| VirtIO | Virtual I/O Device `1.4`, Committee Specification 01，2026-04-08；source tag/commit `917e900e0246b7fe21cdde795b0e566dd4f57d8d` | [OASIS CS01 HTML](https://docs.oasis-open.org/virtio/virtio/v1.4/cs01/virtio-v1.4-cs01.html)、[PDF](https://docs.oasis-open.org/virtio/virtio/v1.4/cs01/virtio-v1.4-cs01.pdf)、[固定源码](https://github.com/oasis-tcs/virtio-spec/tree/917e900e0246b7fe21cdde795b0e566dd4f57d8d) | transport、feature negotiation、device status、virtqueue、通知、reset、设备类型和 driver/device conformance clauses |
| Rust toolchain | `nightly-2026-07-12`；`rustc 1.99.0-nightly` commit `be8e82435eb04fbe75ed5286b52735366e160bed`；LLVM `22.1.8` | [固定 rustc commit](https://github.com/rust-lang/rust/commit/be8e82435eb04fbe75ed5286b52735366e160bed)、仓库 `rust-toolchain.toml` | kernel、bootloader、host tools 与 target `core/alloc` 的可复现编译器、组件和 lint 基线；禁止滚动 `nightly` |
| smoltcp | 官方 crate `0.13.1`；source commit `e347a1e2d3ac33c5ce2c0c114e24b85ae23c4897`；禁用 default features，仅启用 alloc/Ethernet/IPv4/UDP/TCP/Reno | [官方 release 文档](https://docs.rs/crate/smoltcp/0.13.1)、[固定源码](https://github.com/smoltcp-rs/smoltcp/tree/e347a1e2d3ac33c5ce2c0c114e24b85ae23c4897) | 唯一 NetworkStack 中 Ethernet/ARP/IPv4/UDP/TCP 的协议状态机；不定义 Linux socket ABI、ioctl 或 errno |
| musl | 官方稳定版 `v1.2.6`，2026-03-20；tag/commit `9fa28ece75d8a2191de7c5bb53bed224c5947417`；release tarball SHA-256 `d585fd3b613c66151fc3249e8ed44f77020cb5e6c1e635a616d3f9f82460512a` | [官方 release history](https://musl.libc.org/releases.html)、[固定源码树](https://git.musl-libc.org/cgit/musl/tree/?id=9fa28ece75d8a2191de7c5bb53bed224c5947417) | 一个真实 Linux/riscv64 libc consumer 对 syscall、ELF/TLS、线程、信号和 errno 的具体依赖；不是 Linux ABI 的制定者 |
| BusyBox | 官方 release `1.37.0`；release tarball SHA-256 `3311dff32e746499f4df0d5df04d7eb396382d7e108bb9250e7b519b837043a4` | [官方 release tarball](https://busybox.net/downloads/busybox-1.37.0.tar.bz2)、[官方源码目录](https://busybox.net/downloads/) | 固定的真实 `init + ash` 与基础 applet consumer；`udhcpc/wget` 只消费标准 packet/socket ABI，DNS 由固定 musl `getaddrinfo` probe 验证，不把 BusyBox 行为当作规范；独立 rootfs/UART gate 驱动缺口发现 |
| Display font | Spleen `2.2.0`；release tarball SHA-256 `ec42925c6b56d2138c862b2f97147c872e472f674bf03423417d827a08d69a89`；`spleen-16x32.psfu` SHA-256 `b3b6067d4c00c2e8acae1df68c04ab35d23b6bec47120cb29ffa7bc9b975baad`；BSD-2-Clause | [官方 release](https://github.com/fcambus/spleen/releases/tag/2.2.0)、[固定 tag](https://github.com/fcambus/spleen/tree/2.2.0) | 用户态显示 terminal 的唯一原生 16×32 PSF2 glyph source；必须逐像素渲染且保留上游许可证，不进入 kernel/DRM ABI 或状态 owner |
| apk-tools / Alpine keys | Alpine `v3.22` riscv64 main；`apk-tools-static 2.14.10-r0` SHA-256 `85419c4d80eceb12af9cc3be178dce3599ef04679c46eee25175b6673c14cd43`；`alpine-keys 2.5-r0` SHA-256 `ca4835c8907791ab172fc64e53a81ab4ed06ff21c493d2a7fe8f66a80e2ea200` | [Alpine v3.22 riscv64 main](https://dl-cdn.alpinelinux.org/alpine/v3.22/main/riscv64/) | target 内唯一 package database/signature/add/del/upgrade consumer；固定 package SHA-256 后才 extraction，本地 package 另用 runtime cache 内私钥签名；curl/SQLite/Git 的完整闭包身份固定在 `scripts/apk_apps_cache.py` |
| Alpine application consumers | curl `8.14.1-r2`、SQLite `3.49.2-r1`、Git `2.49.1-r0` 及其固定 SHA-256 闭包；只从 Alpine `v3.22/main/riscv64` 获取 | [Alpine v3.22 riscv64 main](https://dl-cdn.alpinelinux.org/alpine/v3.22/main/riscv64/) | 分别验证 TLS/HTTP 并发与 deadline、rollback/WAL/record-lock/掉电恢复、object/index/ref/worktree 与 HTTPS clone/fetch；它们是固定 userspace consumers，不制定 kernel ABI |
| OpenSSL / CA trust | OpenSSL `3.5.7` LTS；tag commit `8cf17aaeb4599f8af87fefd810b5b5fee90fe69e`；tarball SHA-256 `a8c0d28a529ca480f9f36cf5792e2cd21984552a3c8e4aa11a24aa31aeac98e8`；Alpine `ca-certificates-bundle 20260611-r0` SHA-256 `537dcb625ede1cb81e751dd92552b2715a35fdd72cdb43a965a055f14900d529` | [OpenSSL release](https://github.com/openssl/openssl/releases/tag/openssl-3.5.7)、[OpenSSL support policy](https://openssl-library.org/policies/releasestrat/)、[Alpine v3.22 riscv64 main](https://dl-cdn.alpinelinux.org/alpine/v3.22/main/riscv64/) | BusyBox `wget` 与 curl/Git HTTPS 的标准 userspace TLS；系统 trust bundle 只由官方 APK 拥有，必须启用 chain、time、hostname/IP verification，禁止 internal unverified TLS fallback 与发布镜像中的测试 CA |

选择 Linux `v7.1` 而不是 2026-07-11 当时的 `7.2-rc2`，是为了固定最近一个已完成的主线正式发布，而不是开发中的 release candidate。后续若升级任何一项基线，应以单独变更同时更新版本、commit、差异范围和受影响审计结论，不能静默跟随 `master`、`main` 或 `latest` URL。

## 2. Linux/riscv64 syscall 权威来源

### 2.1 编号表与生成链

Linux `v7.1` 中应按以下顺序读取 syscall 编号：

1. [`scripts/syscall.tbl`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/scripts/syscall.tbl) 是较新架构的生成输入；RV64 选择 `common`、`64`、`riscv` 以及 RISC-V 构建声明的附加 ABI 行。
2. [`arch/riscv/kernel/Makefile.syscalls`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/arch/riscv/kernel/Makefile.syscalls) 明确 RV64 额外选择 `riscv rlimit memfd_secret`。
3. [`arch/riscv/include/uapi/asm/unistd.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/arch/riscv/include/uapi/asm/unistd.h) 对 RV64 包含构建生成的 `asm/unistd_64.h`；它是用户态最终消费的架构 UAPI 入口。
4. [`include/uapi/asm-generic/unistd.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/asm-generic/unistd.h) 是同一固定 revision 的 asm-generic UAPI 表，可用于核对通用编号和 32/64 位选择，但不能脱离 RISC-V 的生成选择单独使用。
5. [`arch/riscv/kernel/syscall_table.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/arch/riscv/kernel/syscall_table.c) 使用生成的 `asm/syscall_table.h` 建立实际 dispatch table，可检查“编号存在”与“内核入口连接”的关系。

本基线下 `__NR_syscalls` 为 `472`，编号空间上界是 `471`；这不表示 0～471 每个编号都有可用实现。RISC-V 专用行是 `riscv_hwprobe = 258` 与 `riscv_flush_icache = 259`。禁止从旧版表、其他架构表、musl 的快照或网络整理表复制编号。

### 2.2 调用约定与 errno

[`arch/riscv/include/asm/syscall.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/arch/riscv/include/asm/syscall.h) 给出 Linux 入口侧的直接证据：

- `a7` 是 syscall number；
- 参数 0～5 分别来自原始 `a0`、`a1`～`a5`；
- 返回值写回 `a0`；
- 内核错误以 `a0` 中的负 errno 返回。

musl 的固定 [`arch/riscv64/syscall_arch.h`](https://git.musl-libc.org/cgit/musl/tree/arch/riscv64/syscall_arch.h?id=9fa28ece75d8a2191de7c5bb53bed224c5947417) 从调用侧交叉确认 `ecall`、`a7`、`a0`～`a5`；[`src/internal/syscall_ret.c`](https://git.musl-libc.org/cgit/musl/tree/src/internal/syscall_ret.c?id=9fa28ece75d8a2191de7c5bb53bed224c5947417) 将原始 `-1..-4095` 转换为 libc 的 `-1` 和线程局部 `errno`。因此内核入口不能直接返回“正 errno”，libc wrapper 的返回约定也不能被误当成裸 syscall 返回约定。

### 2.3 编号以外的 Linux ABI

每个 syscall 的验收不能停在“编号相同”。同一 Linux revision 中还必须逐项锁定：

- [`include/uapi/linux/`](https://github.com/torvalds/linux/tree/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/linux) 与 [`arch/riscv/include/uapi/`](https://github.com/torvalds/linux/tree/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/arch/riscv/include/uapi) 的用户可见类型、结构体、常量、位宽、对齐和 padding；
- [`include/linux/syscalls.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/linux/syscalls.h) 与实际实现的参数含义、符号性及返回类型；
- 对应 Linux 实现和官方 userspace API 文档中的 flags、错误分支、阻塞/重启、partial result、signal interaction 和并发语义。

Linux UAPI 是 LiteOS 对外 Linux/riscv64 ABI 的最高权威。POSIX 或 musl 与它看似冲突时，先确认比较的是否分别是裸 syscall、libc wrapper 和标准函数语义这三个不同层次。

script exec 额外固定同 revision 的 [`fs/binfmt_script.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/binfmt_script.c) 与 [`include/uapi/linux/binfmts.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/linux/binfmts.h)：前者定义 shebang space/tab、optional argument 与 argv rewrite，后者固定 256-byte `BINPRM_BUF_SIZE`；`fs/exec.c` 的 interpreter loop 定义最多 5 次 rewrite 后返回 `ELOOP`。

`sendfile` 额外固定同 revision 的 [`fs/read_write.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/read_write.c#L1304-L1433)：`do_sendfile` 与 RV64 选择的 64-bit entry 共同定义 input/output access、可空 signed 64-bit offset、MAX_RW_COUNT、partial result、OFD offset commit 与 copyout failure 顺序；LiteOS 当前精确声明 regular→regular scope，不把尚无 splice owner 的 socket/pipe 路径标成 complete。

`eventfd` 额外固定同 revision 的 [`fs/eventfd.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/eventfd.c)：`eventfd_read` 统一接收 `iov_iter`，只在总 capacity 小于 8 bytes 时返回 `EINVAL`，成功只消费并复制一个 `u64`；不得为 scalar `read` 另造“必须恰好 8 bytes”的分支。

Linux input ABI 额外固定同 revision 的 [`include/uapi/linux/input.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/linux/input.h)、[`drivers/input/evdev.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/drivers/input/evdev.c)、[`include/uapi/linux/virtio_input.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/linux/virtio_input.h) 与 [`drivers/virtio/virtio_input.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/drivers/virtio/virtio_input.c)：RV64 native `input_event` 是 16-byte timeval 加 type/code/value 的 24-byte layout；evdev 只在 `SYN_REPORT` 后发布完整 packet，丢弃空 report，并以 `SYN_DROPPED` 表达 ring overflow、clock change 与 state-copy failure。每个 open client 独立拥有 queue/clock，device 唯一拥有 live state 与 grab；variable ioctl 返回实际截断 byte count，fixed ioctl 返回零。VirtIO adapter 只提供 little-endian raw event/config，Linux input/evdev owner 才定义 timestamp、packet、minor 和 UAPI 语义。

`membarrier` 额外固定同 revision 的 [`include/uapi/linux/membarrier.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/linux/membarrier.h) 与 [`kernel/sched/membarrier.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/kernel/sched/membarrier.c)：前者定义 command/flag bit，后者定义 registration 的 mm owner、`EPERM/EINVAL`、syscall entry/exit full barrier 与同步 IPI completion 语义。固定 musl 的 [`src/linux/membarrier.c`](https://git.musl-libc.org/cgit/musl/tree/src/linux/membarrier.c?id=9fa28ece75d8a2191de7c5bb53bed224c5947417) 只作为首个 pthread 创建前注册和动态 TLS barrier 的 consumer 证据。

系统 load average 额外固定同 revision 的 [`include/linux/sched/loadavg.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/linux/sched/loadavg.h) 与 [`kernel/sched/loadavg.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/kernel/sched/loadavg.c)：前者固定 FSHIFT=11、5-second EXP constants 与 active 上升时的向上舍入，后者固定 missed periods 使用 binary exponentiation 的 `calc_load_n`，禁止逐周期无界补算或在读取 `/proc/loadavg`/`sysinfo` 时临时重采样。Linux active 包含 runnable 与 uninterruptible task；LiteOS 当前没有独立 uninterruptible run state，只能精确声明 runnable scope，不能把普通 interruptible wait 计入来伪造 D-state 语义。

procfs process/thread 投影额外固定同 revision 的 [`fs/proc/array.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/proc/array.c)、[`fs/proc/task_mmu.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/proc/task_mmu.c) 与 [`fs/proc/base.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/fs/proc/base.c)：`statm` 固定输出 size/resident/shared/text/0/data/0 七个页计数字段，其中 shared 采用 `MM_FILEPAGES + MM_SHMEMPAGES`、text 使用 exec 提交的主 ELF `start_code..end_code` page span，不从 executable VMA 反推；TGID directory 的 `task` 子目录只枚举该 thread group 的 live TID，TID stat 使用线程独立 identity、state、runtime、starttime 与 processor，而 mm、credentials、cmdline 等字段仍按所属 Process 共享 owner 投影。`/proc/<task>/io` 的字段、thread/group aggregation 与 storage-byte 口径固定同文件的 `proc_tgid_io_accounting`/`proc_tid_io_accounting` 路径及 [`include/linux/task_io_accounting.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/linux/task_io_accounting.h)。LiteOS 未实现独立 thread-name mutation ABI 前，thread `comm` 精确声明为 Process comm，而不是复制一份无法同步的名称状态。

TTY noncanonical read 固定同 revision 的 [`drivers/tty/n_tty.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/drivers/tty/n_tty.c) `n_tty_read`：分别实现 `MIN=0,TIME=0` poll、`MIN=0,TIME>0` 首字节 timeout、`MIN>0,TIME=0` minimum blocking 与 `MIN>0,TIME>0` inter-byte timeout，且 O_NONBLOCK、signal 与 partial result 不能旁路同一 cooked queue/wait owner。

I/O priority 固定同 revision 的 [`include/uapi/linux/ioprio.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/linux/ioprio.h) 与 [`block/ioprio.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/block/ioprio.c)：前者定义 class/data encoding 与 WHO selector，后者定义 task lookup、credentials、fork inheritance 和 get/set error semantics；LiteOS 当前只声明 WHO_PROCESS policy storage，不把尚无 block scheduler enforcement 的 class 值冒充调度效果。

alternate signal stack 额外固定同 revision 的 [`include/uapi/linux/signal.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/linux/signal.h)、[`include/linux/sched/signal.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/linux/sched/signal.h)、[`kernel/signal.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/kernel/signal.c) 与 [`arch/riscv/kernel/signal.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/arch/riscv/kernel/signal.c)：它们分别固定 `SS_*` UAPI、SP/range active projection、sigaltstack validation/autodisarm 与 RV64 frame placement/`ucontext.uc_stack` restore。musl v1.2.6 的 [`sigaltstack.c`](https://git.musl-libc.org/cgit/musl/tree/src/signal/sigaltstack.c?id=9fa28ece75d8a2191de7c5bb53bed224c5947417) 只作为 2048-byte minimum 与 wrapper 行为的 consumer 证据。

虚拟内存与资源限制额外固定同 revision 的 [`include/uapi/asm-generic/mman.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/asm-generic/mman.h)、[`include/uapi/asm-generic/mman-common.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/asm-generic/mman-common.h)、[`mm/madvise.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/mm/madvise.c)、[`include/uapi/asm-generic/resource.h`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/include/uapi/asm-generic/resource.h) 与 [`kernel/sys.c`](https://github.com/torvalds/linux/blob/8cd9520d35a6c38db6567e97dd93b1f11f185dc6/kernel/sys.c)：它们分别定义 mmap flags、prot/advice/rlimit 编号、VMA residency 行为、`rlimit64` 布局以及 `prlimit64` permission、soft/hard update 与 copyout 顺序。ELF/brk/stack/anonymous fault、COW 和 reclaim 则以同 revision 的 `fs/binfmt_elf.c`、`mm/mmap.c`、`mm/memory.c` 与 `mm/vmscan.c` owner/lifecycle 为实现语义基线。

## 3. RISC-V ELF psABI

固定 revision 的 [`riscv-cc.adoc`](https://github.com/riscv-non-isa/riscv-elf-psabi-doc/blob/e03d44ae2f0e1144f9498c2896b5ae25b0449398/riscv-cc.adoc) 明确：

- RV64G 的默认 ABI 推荐为 LP64D；LP64、LP64F、LP64D、LP64Q 已 ratified；
- 标准过程入口的栈指针按 128-bit 边界对齐，OS 在进入 signal handler 前必须恢复这一对齐；
- `gp`、`tp`、callee-saved 寄存器、参数/返回寄存器及 C 类型布局按 psABI 执行；
- “Calling Convention for System Calls” 明确写为不在本文范围内，应由 OS kernel ABI 或 SBI 定义。

因此 psABI 用于审计 ELF loader、初始栈、动态链接、TLS、重定位、signal frame 的过程调用环境和类型布局，但 syscall number/a7/a0～a5/negative errno 必须回到 Linux 固定 revision。ELF、TLS 与重定位以同 revision 的 [`riscv-elf.adoc`](https://github.com/riscv-non-isa/riscv-elf-psabi-doc/blob/e03d44ae2f0e1144f9498c2896b5ae25b0449398/riscv-elf.adoc) 为准。

该文档自称 `1.1` pre-release / Development state；其说明同时承诺已发布 ABI 不做破坏兼容的变化。为消除滚动 HTML 的漂移，本项目的审计引用必须使用上表的不可变 commit，发布 HTML 只作阅读入口。

## 4. RISC-V Privileged Architecture

固定 `v20260120` 官方发布物，而不是 `riscv-isa-manual` 滚动分支。其 preface 列出 Machine ISA 与 Supervisor ISA 均为 `1.13` 且 ratified。与 LiteOS 直接相关的审计入口至少包括：

- [Machine-Level ISA](https://docs.riscv.org/reference/isa/v20260120/priv/machine.html)：M-mode CSR、trap、delegation、PMP、返回路径；
- [Supervisor-Level ISA](https://docs.riscv.org/reference/isa/v20260120/priv/supervisor.html)：S-mode CSR、interrupt、exception、`sret`；
- [Supervisor virtual memory](https://docs.riscv.org/reference/isa/v20260120/priv/supervisor.html#sec:sv32)：`satp`、Sv39 页表遍历、PTE 权限/A/D、TLB 与 `SFENCE.VMA`；
- [官方版本 PDF](https://docs.riscv.org/reference/isa/v20260120/_attachments/riscv-privileged.pdf)：章节锚点变化时的固定整本依据。

原子指令与 RVWMO 属于同一 `v20260120` ISA 发布包的 Volume I，而不是 Privileged Volume II；涉及锁、页表发布、IPI、DMA 可见性时还必须同时查阅该固定发布包中的 [A extension](https://docs.riscv.org/reference/isa/v20260120/unpriv/a-st-ext.html) 与 [RVWMO](https://docs.riscv.org/reference/isa/v20260120/unpriv/rvwmo.html)，不能仅凭 Rust 原子 API 名称推断硬件顺序成立。

## 5. SBI

SBI `v3.0` 的固定 [`binary-encoding.adoc`](https://github.com/riscv-non-isa/riscv-sbi-doc/blob/c33ad9f414505806f084e8677e04d2744f76c8df/src/binary-encoding.adoc) 定义：

- `ecall` 在 supervisor 与 SEE 之间转移；SBI v0.2+ 用 `a7` 编码 EID、`a6` 编码 FID、`a0`～`a5` 传参；
- 返回 `struct sbiret` 语义：`a0 = error`、`a1 = value`，其他寄存器由 callee 保存；
- 不支持的 EID/FID 必须返回 `SBI_ERR_NOT_SUPPORTED`，错误时 `a1` 默认未指定；
- hart mask、共享物理内存、XLEN 与错误码必须按规范处理。

Base extension 必须实现，能力通过 [`sbi_probe_extension`](https://docs.riscv.org/reference/sbi/ext-base.html) 探测。LiteOS 当前目标相关的标准扩展应分别按固定源码审计：[TIME](https://github.com/riscv-non-isa/riscv-sbi-doc/blob/c33ad9f414505806f084e8677e04d2744f76c8df/src/ext-time.adoc)、[IPI](https://github.com/riscv-non-isa/riscv-sbi-doc/blob/c33ad9f414505806f084e8677e04d2744f76c8df/src/ext-ipi.adoc)、[RFENCE](https://github.com/riscv-non-isa/riscv-sbi-doc/blob/c33ad9f414505806f084e8677e04d2744f76c8df/src/ext-rfence.adoc)、[HSM](https://github.com/riscv-non-isa/riscv-sbi-doc/blob/c33ad9f414505806f084e8677e04d2744f76c8df/src/ext-hsm.adoc)、[SRST](https://github.com/riscv-non-isa/riscv-sbi-doc/blob/c33ad9f414505806f084e8677e04d2744f76c8df/src/ext-sys-reset.adoc) 和 [DBCN](https://github.com/riscv-non-isa/riscv-sbi-doc/blob/c33ad9f414505806f084e8677e04d2744f76c8df/src/ext-debug-console.adoc)。legacy extension 只能作为明确的旧平台兼容事实记录，不能与 v0.2+ EID/FID ABI 混写。

## 6. POSIX.1-2024 / Issue 8

POSIX 基线使用 publication id `9799919799` 的 Issue 8 固定 edition。后续每个用户可见接口应引用其具体页面（例如 [`read()`](https://pubs.opengroup.org/onlinepubs/9799919799/functions/read.html)）及相关 XBD 定义，而不是只写“POSIX compatible”。

适用边界：

- POSIX 决定函数/utility 的可观察语义、错误条件、同步和进程/文件行为；
- POSIX 不给出 Linux syscall number，不要求一个标准函数必须对应一个同名裸 syscall；
- Linux 特有的 `clone`、`futex`、`epoll`、`signalfd`、`io_uring` 等不由 POSIX 定义；
- musl 可在用户态组合多个 syscall、缓存状态或提供 fallback 来实现 POSIX 函数，因此不能把 musl wrapper 的内部调用路径提升为 POSIX 要求。

合规声明只能按已实现接口和选项逐项给出；Issue 8 作为语义目标，不等于 LiteOS 可以在未验证时宣称完整 POSIX.1-2024 conformance。

## 7. VirtIO

固定 OASIS VirtIO `1.4` CS01，而不是会继续移动的 “latest stage”。实现与审计必须使用规范中的 `MUST`/`SHOULD` driver requirements 和 device requirements，重点包括：

- [Basic Facilities](https://docs.oasis-open.org/virtio/virtio/v1.4/cs01/virtio-v1.4-cs01.html#x1-920002)：device status、feature negotiation、notification、reset、configuration generation；
- [Virtqueues](https://docs.oasis-open.org/virtio/virtio/v1.4/cs01/virtio-v1.4-cs01.html#x1-1510006)：split/packed ring、descriptor ownership、used length、barrier 与 notification suppression；
- [Device Initialization](https://docs.oasis-open.org/virtio/virtio/v1.4/cs01/virtio-v1.4-cs01.html#x1-2260001)：reset → ACKNOWLEDGE → DRIVER → features → FEATURES_OK → DRIVER_OK 的状态机；
- [Virtio over MMIO](https://docs.oasis-open.org/virtio/virtio/v1.4/cs01/virtio-v1.4-cs01.html#x1-3210002)：LiteOS/QEMU virt 平台的 transport 寄存器与中断确认；
- [Device Types](https://docs.oasis-open.org/virtio/virtio/v1.4/cs01/virtio-v1.4-cs01.html#x1-3570005)：block、console、GPU、input 等各设备专章。

其中 input device 必须分别审计 selector/subselector config transaction、little-endian `virtio_input_event`、eventq device-to-driver buffer ownership 与可选 statusq；未发布 statusq 时不得把 evdev write、LED、sound 或 force-feedback output 标成已支持。VirtIO capability bitmap 不替代 Linux input core 固有的 `EV_SYN`，两层能力必须在唯一 evdev owner 内合成。

规范明确机器可读的 normative artifacts 与 prose 不一致时前者优先；若后续实现使用这些 artifacts，也必须固定到同一 CS01 发布目录。设备“能工作”不能替代 feature、queue ownership、DMA 可见性、reset 和中断状态机的逐条合规证据。

## 8. musl 源码基线

musl `v1.2.6` 用于回答“标准 Linux/riscv64 libc 实际会向内核提出什么要求”，重点固定文件为：

- [`arch/riscv64/bits/syscall.h.in`](https://git.musl-libc.org/cgit/musl/tree/arch/riscv64/bits/syscall.h.in?id=9fa28ece75d8a2191de7c5bb53bed224c5947417)：musl release 内的 syscall number 快照；只用于 consumer 对照，编号仍以 Linux `v7.1` 为权威；
- [`arch/riscv64/syscall_arch.h`](https://git.musl-libc.org/cgit/musl/tree/arch/riscv64/syscall_arch.h?id=9fa28ece75d8a2191de7c5bb53bed224c5947417)：RISC-V 裸 syscall 寄存器与 `ecall`；
- [`src/internal/syscall_ret.c`](https://git.musl-libc.org/cgit/musl/tree/src/internal/syscall_ret.c?id=9fa28ece75d8a2191de7c5bb53bed224c5947417)：负 errno 到 libc `errno` 的转换；
- [`arch/riscv64/reloc.h`](https://git.musl-libc.org/cgit/musl/tree/arch/riscv64/reloc.h?id=9fa28ece75d8a2191de7c5bb53bed224c5947417)：RISC-V relocation、TLS 与 dynamic linker 架构契约；
- [`src/thread/`](https://git.musl-libc.org/cgit/musl/tree/src/thread?id=9fa28ece75d8a2191de7c5bb53bed224c5947417)、[`src/signal/`](https://git.musl-libc.org/cgit/musl/tree/src/signal?id=9fa28ece75d8a2191de7c5bb53bed224c5947417)、[`ldso/`](https://git.musl-libc.org/cgit/musl/tree/ldso?id=9fa28ece75d8a2191de7c5bb53bed224c5947417)：clone/futex/TID/robust list、signal、ELF/TLS/auxv 的实际组合需求。

官方 release history 当前同时警告 `v1.2.6` 受发布后披露的问题影响。这里固定它是为了可复现地审计 kernel ABI consumer，不代表建议把未打官方修复的 tarball 直接作为交付运行时；若仓库将来 vendor/build musl，应另行固定安全补丁后的 musl commit。

## 9. 冲突时的裁决顺序

1. 硬件 trap、CSR、页表、特权和内存顺序：RISC-V 固定 ISA 发布物。
2. S-mode 到 M-mode/SEE：固定 SBI；不得套用 Linux syscall 返回规则。
3. U-mode 到 LiteOS kernel 的 Linux/riscv64 ABI：固定 Linux 主线 UAPI 与 RISC-V arch source。
4. ELF、过程调用、TLS 与 relocation：固定 RISC-V psABI；syscall convention 除外。
5. 用户可观察的标准函数/进程/文件语义：POSIX Issue 8；若 Linux ABI 与 libc wrapper 分层实现该语义，分别审计各层。
6. musl：作为 consumer proof 和兼容性验收对象，不作为 Linux/POSIX/psABI 的替代规范。
7. BusyBox：作为标准 userspace consumer 驱动缺口发现，不得用 BusyBox patch 或 LiteOS 私有接口改变 kernel ABI。
8. 虚拟设备：固定 VirtIO CS01；Linux driver 行为只能作为实现参考，不能覆盖 VirtIO normative requirement。

## 10. 可复现性与更新规则

Git tag 的目标通过上游仓库 `git ls-remote` 固定；Linux annotated tag 同时记录 tag object 和 peeled commit，其他列出的 tag 为上述 commit。标准组织的不可变发布 URL 以版本/publication id 固定，不用滚动入口代替。

升级基线时必须至少检查：syscall 新增/改号（正常情况下既有 ABI 不应改号）、UAPI 结构体与 flags、psABI relocation/TLS、Privileged ISA 页表/trap/CSR、SBI extension、VirtIO feature/device requirements、POSIX corrigenda，以及 musl wrapper/fallback 和 BusyBox `init/ash/applet` 的新内核依赖。升级前后的审计结论应可追溯到两个不可变 revision。

本文件的建立未修改实现代码，未维护、修正或执行测试用例。
