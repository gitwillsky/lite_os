# LiteOS Architecture Contract

本文只定义稳定的 module interface、依赖和状态 owner。当前功能事实属于 `architecture.md`，Linux ABI 状态属于 `syscall-support.md`。

## 1. Crate contract

- `bootloader` 是独立 M-mode domain，不依赖 kernel 或 userspace。
- `syscall-abi` 只保存 kernel dispatcher 接入的 Linux/riscv64 ABI 常量，不依赖实现 crate。
- 产品 userspace 只有固定上游 musl + BusyBox rootfs；禁止恢复 Rust user crate、自有 runtime/init 或第二条默认镜像路径。
- kernel 的 `main.rs` 是唯一 composition root；初始化顺序和 adapter 装配不得下沉到 driver、filesystem 或 task。

## 2. Kernel dependency contract

| Module | 允许依赖（机器读取） | 说明 |
|---|---|---|
| `arch` | `config`, `memory`, `sync` | architecture mechanism 不得消费上层领域状态 |
| `config` | 无 | 只保存无运行时依赖的常量 |
| `sync` | `arch` | 只依赖本地中断机制 |
| `memory` | `arch`, `config`, `id`, `random`, `sync` | 不感知 task、filesystem 或具体 driver policy |
| `drivers` | `arch`, `memory`, `sync` | 不感知 task、filesystem 或 syscall |
| `ipc` | `id`, `sync` | 只拥有 Pipe byte/endpoint，不感知 fd、task、socket 或 syscall；`id` 仅分配 anonymous inode identity |
| `socket` | `drivers`, `id`, `ipc`, `sync`, `timer` | 拥有 socket domain facade、AF_UNIX 与 AF_INET stack；`drivers` 只允许 network-device seam，`id` 仅分配 anonymous inode identity |
| `fs` | `drivers`, `ipc`, `memory`, `socket`, `sync`, `timer` | `drivers` 仅允许 `block` seam；socket 仅允许统一 OFD backend facade；`memory` 仅允许 shared-page seam |
| `task` | `arch`, `fs`, `ipc`, `memory`, `socket`, `sync`, `timer` | 不依赖具体 device 或 syscall/trap entry；只在 deferred context 推进 network stack |
| `trap` | `arch`, `drivers`, `memory`, `syscall`, `task`, `timer` | 只做入口、分类和事件投递 |
| `syscall` | `fs`, `ipc`, `memory`, `random`, `socket`, `system`, `task`, `timer` | 不得绕过 facade 接触 adapter/scheduler/page table |
| `random` | `drivers` | entropy facade；只消费 RNG device seam，不生成伪随机 fallback |
| `system` | `arch` | 只拥有 whole-system reset/shutdown/CAD policy |
| `timer` | `arch`, `config`, `drivers`, `sync` | RTC adapter 由 timer 唯一拥有 |
| `log` | `arch`, `sync` | 日志策略和输出在本 module 内闭合 |
| `id` | 无 | 纯 ID allocation mechanism |
| `lang_item` | `arch` | 只使用 architecture fail-stop mechanism |
| `main` | `arch`, `config`, `drivers`, `fs`, `id`, `ipc`, `lang_item`, `log`, `memory`, `random`, `socket`, `sync`, `syscall`, `system`, `task`, `timer`, `trap` | 唯一 composition root |

同一 module 内引用不构成跨 seam 依赖。`main.rs` 可以依赖所有 kernel module，但只能做装配、启动顺序和 fail-stop 策略。

## 3. State owners

| 状态 | 唯一 owner |
|---|---|
| M-mode initialization、HSM、RFENCE | bootloader 对应 module |
| DTB board facts | immutable BoardInfo publication |
| hart possible/online/active、startup stack、同步 memory-barrier request/completion generation | HartTopology；每次调用的 generation 由 arch hart barrier mechanism 唯一分配 |
| per-hart current、runqueue、mailbox | task ProcessorTopology |
| task run state、generation、wait membership 与 wake result | SchedulingState |
| process address-space handle、cwd opened-entry、fd table、credentials/umask、resource limits、聚合 CPU runtime | Process；Thread 共享，fork 复制 limits 并重置 runtime，exec 保留；vfork child Process 初始持同一 AddressSpace Arc，exec 只替换 child handle；最后一个 Thread exit 立即取走 fd table |
| VMA 与 private expedited membarrier registration | AddressSpace；CLONE_VM/vfork 共享，fork/exec 新 owner 从未注册开始 |
| PID/TID allocation、parent edge、live thread collection、process-creation reservation、exec generation、group-exit status、job-control/orphan lifecycle、child event/waiter 与 ITIMER_REAL | TaskManager process graph；独立 creation lock 只覆盖 `RLIMIT_NPROC` 检查到 graph publication，防止并发超限；vfork child node 唯一持有 suspended parent Thread；timer softirq 只推进 expiration 并经统一 signal seam 发布 SIGALRM |
| deadline/futex/pipe/poll/signal/console wait registration、event filter、exclusive mode 及其 indexes | TaskManager 唯一 IndexedWaitQueue；futex index 只消费 memory 归一化的 private/shared key，requeue 原地迁移同一 registration；blocking pipe writer registration 保存本次原子写所需容量，poll 仍只观察通用 endpoint readiness；ppoll/epoll/socket blocking 共用 source indexes；socket 内部 edge notification 固定注册为 source-native read key，userspace event mask 只用于真实 data source 和后续 level recheck；source wake 唤醒全部普通 callback group（每个 epoll instance 一个 thread）及一个 exclusive group |
| signal disposition、process-directed shared pending set | Arc<Process> 的单一 ProcessSignalState lock |
| signal mask、thread-directed pending set、active frame | ThreadContext 与用户 RV64 rt_sigframe |
| interrupted syscall 的单次 replay record | ThreadContext；signal frame 保存最终 replay/EINTR 上下文 |
| OFD backend、opened-entry identity、offset/status flags、跨 fd-table descriptor 引用数 | OpenFileDescription；pathname-backed character/regular/directory fd 保留 VFS opened entry，anonymous pipe 使用 pipefs 语义；最后 descriptor close 触发 epoll interest cleanup |
| anonymous Pipe/AF_UNIX stream 共用的 byte ring、endpoint count、pipe `PIPE_BUF` atomicity、stream short-write policy 与内核 readiness notification token/generation | ipc::Pipe；mode 由调用 seam 明确选择，不复制到 fd table、socket state 或 wait registry；notification token 只合并待消费 edge，每次 signal 仍推进 generation 并通知 registry，registry owner lock 内先排空 token、再做 socket level recheck |
| AF_UNIX abstract namespace、endpoint state、listen/datagram queue | socket::UnixSocket；OFD 只持统一 Socket Arc，fork/dup 共享，具体 domain adapter 不穿透 fs seam |
| Ethernet interface IPv4 address/prefix/default route、ARP cache、UDP/TCP/raw ICMP socket set、peer、IP_PKTINFO、socket option、ephemeral port、TCP listener backlog 与 FIN/TIME_WAIT orphan | socket::NetworkStack；ioctl、packet dispatch、procfs 只读同一 owner 快照；accepted handle 只转移给新 Socket/OFD，不复制协议状态 |
| AF_PACKET binding、protocol 与有界 receive queue | socket::PacketRegistry；RX frame 在 smoltcp ingress 前只镜像一次，packet endpoint 与 L3 NetworkStack 不复制彼此状态 |
| VirtIO-net RX/TX queue、DMA buffers 与 packet/byte counters | VirtIONetworkDevice；hardirq 只确认并发布 network softirq，deferred context 才解析协议与唤醒 Pipe waiter |
| epoll `(fd, OFD)` interest、ET generation、MOD revision、delivery cursor、ONESHOT state、ctl notification 与无环嵌套图 | fs::Epoll；内部 notification Pipe 与 readiness generation 均接入 ppoll 的同一 source/wait seam |
| eventfd 64-bit counter、semaphore mode 与 read/write readiness generation | ipc::EventFd；OFD/fd table 只持 Arc，notification Pipe 只承载 edge，不复制 counter |
| VMA 区间、类型、权限、private dirty/discardable residency、anonymous shared backing 与 framed page lifetime | MemorySet 的有序 VMA 表；ELF/anonymous/brk/stack/file 页按 fault 驻留，anonymous shared backing 按索引唯一发布跨 fork frame，PageTable 只保存硬件 translation |
| physical frame lifetime | FrameTracker/frame allocator |
| process comm/创建时刻、mm argument range、thread runtime 与 run state | Process、MemorySet、SchedulingEntity；procfs 只读取快照或 mm 中的实时 argv bytes |
| termios、cooked input、controlling session、foreground process group | Terminal；TaskManager 只读取 job-control 判定结果，不复制 TTY 状态 |
| per-hart busy runtime | ProcessorTopology 对应 hart slot；procfs 不另建 CPU counter |
| 1/5/15 minute load average | TaskManager 的单一 fixed-point EWMA state |
| system information snapshot | task façade 只投影 allocator、process graph、load average 与 timer 的权威状态；procfs/sysinfo 不另建 counter |
| immutable system/build identity | system module；uname 只编码，不复制 hostname/release/machine state |
| realtime offset 与固定 UTC timezone policy | timer module；clock_gettime/gettimeofday 共用同一 realtime owner |
| root mount、source/filesystem association、boot-time mount table、mount enter/leave、pathname traversal 与 namespace mutation publication order | VFS；mutation lock 覆盖 adapter commit 到 opened-entry registry publication |
| opened-entry parent/name/deleted 关系与 live weak registry | VFS；cwd/OFD/procfs 只持 Arc 或读取投影，rename/unlink 只在 VFS 更新 |
| boot 内 anonymous Pipe/Socket object identity allocation | id module 分配机制；具体 object owner 持有，fd table/procfs 不复制 |
| inode/on-disk allocation、statfs capacity、JBD2 active transaction/recovery sequence 与 orphan chain | filesystem adapter mutation domain；ext2 统计、allocator、journal 与 orphan lifecycle 共用唯一 mutation 顺序 |
| VirtIO descriptor/DMA lifetime | VirtQueue/driver instance |
| entropy device 与请求串行化 | VirtIORngDevice；random facade 不缓存或派生第二份状态 |
| UART MMIO 与固定容量 RX ring | UART driver；hardirq 只填 ring，console waiter 只由 deferred softirq 消费 |
| interrupt registration/affinity | interrupt controller |
| syscall number | syscall-abi |
| mounted inode 的 BSD advisory flock state | VFS；key 为 filesystem+inode，holder 为 OFD identity |
| mounted inode 的 POSIX byte-range record-lock state | VFS；key 为 filesystem+inode，holder 为 Process TGID；TaskManager 只拥有 interruptible wait membership |
| user pointer与 errno translation | syscall module |

新增 global、Atomic、lock、cache 或 flag 必须在声明附近写 `OWNER:`，并说明缺失该 owner 会造成的具体状态分裂。

## 4. Source size contract

生产 Rust 源文件采用两级围栏：超过 600 行触发 architecture review notice，但不单独导致验证失败；超过 1200 行默认拒绝。reviewer 必须检查 owner、依赖方向、公开接口与真实领域 seam，选择拆成深 module，或在下表登记精确审查额度。登记是超过 1200 行的唯一例外入口，也可用于记录 601–1200 行文件的审查结论；必须同时给出状态 owner、不可立即拆分的原因与消除条件。每个登记额度就是该文件的硬上限，只能随重构下降，不得为功能开发上调；文件低于登记额度时 checker 强制同步下调。行数只是退化信号，禁止按行数机械切片或建立 pass-through module。

| Source | Reviewed max lines | Owner | Reason | Exit criterion |
|---|---:|---|---|---|
| `kernel/src/fs/ext2.rs` | 2288 | `fs::ext2` | ext2 inode、allocator 与 packed layout 仍共享同一 mutation domain；storage mutation 已下沉 | 提取不泄漏 packed layout 的 inode/allocator 深 module 后下调额度 |
| `kernel/src/task/task_manager.rs` | 744 | `task::TaskManager` | process graph 与非 futex wait orchestration 仍集中维护跨锁不变量；context switch、thread clone、futex、pipe wait、wait key/index、child wait/vfork lifecycle 与 deferred work storage 已下沉 | 按 process graph 与剩余 lifecycle 的真实 seam 继续分离后下调额度 |
| `kernel/src/memory/mm.rs` | 1087 | `memory::MemorySet` | 核心 VMA 表示、页表提交与 kernel/system mapping 仍共享底层 PageTable/frame 不变量；mapping request、mmap、user-copy、shared/private area 与 futex key lifecycle 已下沉 | 提取不泄漏 PageTable/frame 的 kernel mapping 与 VMA mutation 深 module 后下调额度 |
| `kernel/src/task/model.rs` | 612 | `task::Process/Thread` | process 与 thread 核心生命周期仍共处一文件；fd lifecycle 与 wait membership 已分别归入 file-descriptions/scheduling，其他 façade 已下沉 | 沿 Process/Thread 领域 seam 拆分且不扩大 scoped interface 后继续下调额度 |
| `kernel/src/fs/file.rs` | 613 | `fs::OpenFileDescription/FileDescriptorTable` | OFD kind dispatch 与 descriptor table 仍共享 close/dup/readiness 生命周期；terminal/proc projection 已下沉，当前再拆会建立 pass-through dispatch | 当 anonymous descriptor kind 可由统一 backend trait 封闭 readiness/stat/proc projection 时下沉并删除中央枚举分派 |

## 5. Interface and capability contract

- kernel 与 bootloader 是 binary crate，跨 module interface 使用最窄的 `pub(super)`、`pub(in …)` 或 `pub(crate)`；不得使用裸 `pub` 伪造外部 interface。
- `processor::job_control::request_reschedule_on` 只向 parent scheduler 的 Ready delivery 开放；它统一发布 per-hart reschedule，并在远端复用同一次 SBI IPI 唤醒 mailbox，其他 module 不得直接调用。
- 默认 private；Rust AST 围栏解析所有 scoped visibility declaration、字段、方法、trait item 与 enum variant，连同可见域由 `architecture-interface.txt` 完整记录。
- `task_manager::wait_registry` 的 scoped interface 只允许 parent orchestration 与 sibling signal cancellation 使用；它唯一执行 membership/index 的 insert/remove/take，caller 不直接修改 `entries` 或 source index。
- `ipc::PipeWaitCondition` 只允许 syscall I/O 构造、`task_manager::pipe_wait` 消费；它把 `PIPE_BUF` 原子写的容量条件带入唯一 wait registration，poll key 不得复制这项 blocking-write policy。
- `ipc::PipeEnd::{signal_readiness,drain_readiness}` 只允许 epoll、eventfd 与 socket adapter 的内部 notification Pipe 使用；anonymous Pipe 和 AF_UNIX stream data Pipe 禁止调用。eventfd 的 Pipe 只发布 read/write edge，counter 仍由 `ipc::EventFd` 单锁唯一拥有；零值 write 不得制造虚假 readable edge。`socket::SocketWaitSource` 是 socket façade 到 syscall poll 的唯一 wait-source seam：`Notification` 固定映射 source-native read key，`Data` 只投影 AF_UNIX stream endpoint；backend 的 `wait_sources/consume_wait_notifications` 只向 parent socket module 开放。syscall blocking I/O 只经 `poll::wait_for_ofd` 执行 token drain、registry publication 与 level recheck，不得复制该序列。若后续用独立非 Pipe notification primitive 取代 token，该 primitive 必须接管同一 generation/notifier owner，并删除这些 Pipe 专用 scoped interface。
- `socket::UnixSocket` 只在 state lock 内选择/转移 endpoint；Pipe I/O、endpoint Drop 与 notification 必须在释放 state lock 后执行。AF_UNIX stream 必须使用 ipc owner 的 short-write mode，禁止复制 pipe atomicity 或在 socket 层维护第二份容量状态。
- filesystem 只能看到 `drivers::block` seam，不得看到 VirtIO adapter。
- ext2 只提供 persistent root；`/tmp`、`/root`、passwd/group 与 `/dev`、`/proc` mountpoint 都由唯一 rootfs builder 固化，禁止 kernel/syscall/applet 按路径补造。rootfs builder 只能把完整 tree 转成一个 signed `liteos-base` package，再由 target `apk.static` 创建最终 installed DB；官方 `ca-certificates-bundle` APK 唯一拥有系统 trust bundle，host 不得覆盖其路径。host 不得直接解包最终 runtime、伪造数据库或保留 package 外的同名文件 owner。本地 private key 只允许位于 ignored `target/apk-runtime`，镜像只安装 public trust root。运行时 devfs/procfs 只经 VFS mount table发布；`/dev/fd` 与 stdio aliases 只由 devfs 指向 `/proc/self/fd`，禁止写入会被 mount 遮蔽的 ext2 节点。VFS 唯一保留 mount source 到 filesystem adapter 的关联，并向 procfs 发布 `/proc/mounts`；procfs 通过 `ProcSource` 反转依赖消费 task/memory/fd 快照，禁止 fs 反向依赖 task、syscall pathname 特判或伪 regular-file 节点。
- cwd、directory fd 与 pathname-backed OFD 必须持有同一 VFS `OpenedFile` seam；VFS weak registry 唯一提交 rename/unlink 对 parent/name/deleted 的更新。禁止缓存绝对路径、由 inode 扫描猜测 hardlink 名称，或把 procfs ` (deleted)` target 当普通 pathname 重新解析。`/proc/<pid>/fd` magic link 可跟随 live opened entry，Pipe/Socket target 与 `fstat` 只投影其 object owner identity，eventpoll 使用标准 anon-inode label。
- `/proc/<pid>/fd` 只允许同 TGID、effective root，或 caller effective UID 与目标 real/effective/saved UID 全部相同的访问；credential 判定留在 task façade，procfs 不复制 Process credential state。未建模 fsuid/capability/dumpable 前不得扩大该边界。
- `statfs/fstatfs` 只经 VFS/OFD seam 选择 filesystem；ext2 从同一 mutation domain 投影 superblock 容量，procfs/devfs/anonymous pipe 使用 Linux simple-statfs 形状。禁止 syscall 识别具体 adapter、按 filesystem id 复制统计或伪造可分配容量。
- VFS 只决定 pathname、mount 与 cross-filesystem policy；ext2 的 create/link/unlink/rename、allocator、JBD2 write-set/commit/checkpoint 与 orphan recovery 必须在同一 mutation domain。禁止 syscall/VFS 复制 link count、journal 状态或用写序调整冒充跨块原子性。
- live mount root/mountpoint 不得被 unlink 或作为 rename source/replace target；VFS 在进入 adapter mutation 前返回 `EBUSY`，防止 mount table 与 opened-entry namespace 分裂。
- VFS permission evaluator 只消费 Process 发布的 immutable identity snapshot，唯一决定 traversal、inode rwx、parent mutation、sticky directory、protected hardlink 与 setgid-directory inheritance；syscall 和 filesystem adapter 不得复制权限 policy。ext2 只持久化 VFS 已决定的 mode/UID/GID/ctime。
- regular-file `fallocate` 与 write/append/truncate/mmap writeback 必须共用 page-cache operation lock；ext2 只在同一 mutation/JBD2 owner 内分配 hole、更新 block pointers/i_blocks/i_size。大 range 允许拆成有界且各自完整提交的 transaction，禁止 syscall 写零覆盖已有内容、伪造成功或维护第二份 allocation map。
- BSD `flock` state 只由 VFS 以 mounted inode identity 持有，OFD pointer 只作为 open-file-description lifetime identity；Task wait registry 只拥有 interruptible membership，VFS notifier 只投递发生变化的 key。唯一 VFS lock-table mutex 覆盖 shared/exclusive 转换，释放后才调用 notifier；缺失该锁会让两个 exclusive holder 同时成功，反向持锁或在锁内通知会与 wait publication 死锁。最后 descriptor close 必须经 OFD descriptor_refs 释放，禁止按 Process/fd 复制 lock state或让 ext2 维护第二套 advisory lock。
- POSIX record lock 只由 VFS 以 mounted inode identity + Process TGID 持有；同 owner 的相交 range 在一次锁内拆分、替换、排序并合并。`F_SETLKW` 只把 membership 交给统一 indexed wait owner，VFS 锁外通知；任一该 inode descriptor close、CLOEXEC close 或 Process exit 必须释放 Linux process-associated locks，禁止按 fd/OFD/ext2 复制 lock table。
- Process exit 只负责关闭本 Process 的 descriptor/OFD lifecycle；禁止在 exit/close cleanup 隐式执行全局 filesystem sync。durability 只能经 ext2 journal/writeback owner 与显式 `fsync/sync` seam 提交，否则任意短命进程都能阻塞全系统 I/O，并把 close 语义错误扩大为全局持久化屏障。
- procfs 与 `sysinfo` 必须消费 task façade 的同一采集边界；syscall 只编码 Linux UAPI，禁止解析 `/proc` 文本、复制统计状态或在 ABI 层维护第二套 uptime/load/memory/task counter。
- `uname` 只投影 system module 的 immutable identity；`riscv_hwprobe` 只通过 system façade 投影 DTB/HartTopology 的平台事实；`gettimeofday` 与 `clock_gettime(CLOCK_REALTIME)` 只投影 timer realtime owner。禁止 syscall module 维护 hostname、ISA、hart mask、release、timezone 或第二份 wallclock offset。
- MMIO/volatile 只存在于 arch/driver HAL；user pointer 只通过 AddressSpace copy；磁盘 packed layout 只存在于 filesystem adapter。
- syscall memory handler 只解析 Linux flags/prot/errno；TaskControlBlock/AddressSpace 只持锁转发；VMA 选址、冲突、split/merge、frame rollback 与 PTE 提交只存在于 MemorySet。
- Process resource-limit owner 只在 task layer 执行权限、fork/exec lifecycle 与 CPU/NPROC policy；syscall 只编解码 `rlimit64`，memory/fs 只消费数值上限，不得反向读取 Process。
- frame allocator 的唯一慢路径通过 weak reclaimer seam 同步回收 `MADV_FREE`、clean private file/ELF 与无外部引用的 clean page-cache 页；private/shared dirty 页无 writeback 证明时禁止回收。kernel heap 只从 frame allocator 转移页，不维护第二份物理容量 owner。
- futex private key 由 AddressSpace identity + uaddr 构成；anonymous/file shared key 只能由 MemorySet 从 backing identity + offset 归一化。IndexedWaitQueue 锁在 AddressSpace 锁外覆盖 key/value 比较、membership 发布、bitset wake 与 requeue，禁止 syscall、Process 或 scheduler 复制映射判断。
- task loader 是 pathname、Linux script rewrite 与 inode 到 `ExecutableSource` adapter 的唯一 owner；memory 只消费最终 ELF 随机读 seam，并唯一拥有 ELF 解析计划、PT_LOAD 映射、initial stack 与失败回滚。禁止恢复完整文件 `Vec`、filesystem 到 memory 的具体类型泄漏或第二套 script/ELF loader。
- thread-directed signal 发布到 Thread pending；kill/TTY/SIGCHLD 发布到 ProcessSignalState shared pending。两者经同一 delivery/wait seam 先发布 pending bit，再从 wait 的唯一 owner 注销 membership；blocking path 必须在 owner lock 内复查两类 pending，禁止 signal-before-enqueue lost wakeup。
- stop/continue 冲突消除必须与 signal queue generation 同锁；Process graph 唯一提交 group-stop/continue 与 parent event，scheduler 只维护每个 Thread 的 `StopPending/Stopped` membership。禁止用第二套 stopped flag 与 run state 人工同步。
- Process graph 在 exec point-of-no-return 唯一发布 `has_execed`，parent-side `setpgid` 必须在同一 graph lock 下得到成功或 `EACCES`。Process 退出/重父子关系前后只在该 owner 内计算 orphan transition，并冻结当时的 target TGID；锁外仅经统一 signal seam 发布全组 SIGHUP 后 SIGCONT。禁止在 syscall、Terminal 或 scheduler 复制 parent/SID/PGID/orphan 状态。
- global init 的 unkillable policy 位于统一 signal generation/delivery：默认 disposition signal 被丢弃，显式 handler 和 blocked pending 仍遵循 Linux 语义，强制同步 fault 不经该豁免。默认 SIGTSTP/SIGTTIN/SIGTTOU 在 delivery 时必须复查 orphan group，SIGSTOP 不豁免。
- Terminal 唯一判断 controlling session、foreground group 与 `TOSTOP`；TaskManager 唯一判断 caller process group、orphan 状态及 signal mask/disposition。后台访问只经该 seam 产生 SIGTTIN/SIGTTOU、EIO 或 syscall restart，禁止 syscall pathname/device 特判复制 job-control policy。
- session leader 退出使用固定 graph -> Terminal 锁序原子取走 controlling session/foreground PGID，并在 graph lock 内冻结 foreground target；SIGHUP 必须在锁外经统一 signal seam 发布，禁止按可能已复用的 PGID 重新选择目标。
- UART hardirq 不调度、不分配，只清空设备 FIFO并发布 console softirq；console read 在统一 indexed wait owner 内复查 RX ring，deferred consumer 才移除 membership 并 wake task。
- syscall handler 只能向 dispatcher 返回内部 restart 结果；trap layer 将其暂存为 `EINTR` 并把原 `a0..a5/a7/ecall PC` 交给当前 Thread。实际交付的 handler disposition 含 `SA_RESTART` 时才把 replay context 写入 signal frame，否则 frame 保留 `EINTR`；内部结果不得进入 U-mode。
- Thread exit 发布顺序固定为 robust cleanup -> process graph removal -> clear-child-tid/futex wake；join completion 不得早于 Thread owner 注销。
- vfork caller 使用独立 `WaitMembership::Vfork(child)` 且不可被 signal 提前解除；child Process 与 parent 精确共享同一 AddressSpace Arc，并使用按 TID 分配的 supervisor trap page。只有 exec 原子替换 child handle并删除临时页，或 exit 先删除临时页，才能消费 child node 的唯一 caller waiter；不得挂起整个 parent Process。
- child exit/stop/continue event 必须在 process graph 锁内 claim；status copyout 成功后才消费，失败必须 release。多个 parent Thread 可分别等待，但同一 event 不得被重复返回或回收。
- private expedited membarrier registration 只存在于 AddressSpace；syscall 只能通过 task façade 请求 arch 对所有 active DTB hart 做同步 full fence。不得用本地 fence、RFENCE、吞 `ENOSYS` 或 musl fallback 冒充跨 hart memory ordering。
- terminal exit 必须先在可返回的 prepare frame 完成副作用并释放全部 task Arc，再以只含 raw context 的凭据切到 idle，由 per-hart deferred-reap slot 保活并析构 TCB；禁止从持有 task Arc 的 Rust frame 永久切走。
- user trap return 的 noreturn trampoline 前必须显式释放当前 TCB Arc；该 Rust frame 不会展开，依赖作用域析构会让每次 syscall 永久增加一个 kernel-stack 自引用。
- 首个 `exit_group` 或默认致命 signal 在 process graph 唯一提交 group-exit status；所有 sibling 只经 signal/wait/scheduler seam 回到自身内核栈退出，最后一个 Thread 才发布 zombie 与 SIGCHLD。禁止远程释放运行栈、重复保存 status 或把 signal death 改写成 shell exit code。
- raw CSR、DMA、page-table pointer、trap context 和 packed disk unsafe 必须有局部 `SAFETY:` 证明。
- 禁止 `static mut`、私有 syscall、固定 hart 容量、console syscall 旁路、deprecated/feature-flag 双轨。
- 禁止 `common/utils/helpers/misc/manager/base/shared/core` 等无领域含义的目录。
- `user/` 顶层只允许固定 BusyBox config、passwd/group identity、inittab/network service/udhcpc lease script、musl consumer 与 dynamic-loader C probe；围栏禁止 Rust user crate/source/linker、`build-user` 和旧 init artifact。

## 6. Change contract

修改前必须确定所属 module、状态 owner、interface 变化、依赖变化以及 error/exit/interrupt cleanup。依赖采用正向 allowlist，未列出的跨 module 依赖一律失败。扩大 interface 或新增 global state 必须更新对应权威契约；不得修改围栏来掩盖实现问题。只有其他架构规则全部通过时，`cargo run -p architecture-check -- --write-interface` 才能更新 interface contract。

唯一强制验证入口是 `make verify`。
