# 用户态与 ABI 契约

## Owner

- `syscall-abi` 独占已接入 Linux 64-bit asm-generic number 与 RISC-V 扩展编号；dispatcher 独占 number-to-handler mapping。
- `syscall-abi::open` 独占 native open/fcntl flag 编码，在编译期选择 arm64 或
  asm-generic UAPI 的 DIRECTORY/NOFOLLOW/DIRECT/LARGEFILE 位。VFS/OFD 直接使用同一
  编码并保留实际 status；不能把 AArch64 musl 隐式加入的 LARGEFILE 当成 NOFOLLOW，
  也不屏蔽该位来绕过错误。guest 验证普通跟随、NOFOLLOW、目录约束及 O_PATH 链接 identity。
- 编译期选中的 `arch::user` backend 独占 raw syscall register codec、signal machine context、
  ELF machine/flags/HWCAP 与 architecture-private syscall number decode；generic syscall、process
  与 memory 不得解释这些 layout。decoder 通过编译期选中的普通后端函数调用，禁止 capability bool、
  单次使用的 trait 或零大小 dispatch type。
- syscall module 独占 raw UAPI codec、user-copy 和 errno translation；领域 module 独占行为与状态。
- `arch::user::AUDIT_ARCH` 独占目标 audit architecture 编码。dispatcher 在 architecture decode
  与任意 handler 前调用唯一 seccomp seam；不能让私有 syscall、未知 number 或 restart 绕过。
  TRAP 返回 `SyscallOutcome::PreserveRegisters`，trap 不写 x0/a0、不创建 restart record；
  SIGSYS 的 si_errno/code/call_addr/syscall/arch 由 typed siginfo codec 编码，PC 为 syscall 后地址。
  syscall 通过 `system::audit_architecture` 投影 identity，不直接依赖 architecture backend。
  trap 把已有 TCB Arc 转交 dispatcher，seccomp 消费后在任意 handler 前释放；disabled 路径
  不增加 current lookup、Arc clone 或 state lock。USER_NOTIF 仅带实际 queue 和 fixed seccomp_data；
  filtered task 自行等待通知，listener DeviceFile 拥有通知 ioctl codec，通用 syscall 不辨认 listener。
  FD/OFD/listener 完整准备在 graph publication 前，成功后才无分配发布 CLOEXEC descriptor。
  GET_NOTIF_SIZES 使用六字节固定 codec，无 allocation。
  仅实际 exec handler 成功归一成 PreserveRegisters；filter ERRNO|0 是普通 Return(0)，
  trap 必须写回零，不能从 syscall number/result 推断新映像已提交。
- `syscall::fs::statx` 独占 `struct statx` codec 与 flags/mask 校验；路径解析与 `stat`/`newfstatat` 共用 `resolve_stat_target`，
  时间精度与创建时间只来自 `InodeMetadata::fine_times`，filesystem 不记录时取 `Default`，不得由 syscall 层伪造。
- `syscall::unimplemented_report` 独占去重位图；dispatcher 的 ENOSYS 兜底分支对每个编号至多记一行 warn，编号名称由宿主按 ABI 表解析；不得逐调用打印。
- RISC-V base rt_sigframe 为 1088 bytes、16-byte 对齐；siginfo 后的 ucontext_t 为
  960 bytes，其 uc_mcontext 在 offset 176，因 FP/extension union 对齐保留八字节 padding。
  codec 与 musl consumer offsetof/size 静态断言及 SIGSYS context 修改/rt_sigreturn runtime
  必须一致，不保留原先省略 padding 的非标准 frame。
- rt_sigreturn 的不可读 frame 与不支持的 extension 由现有 forced signal owner 投递
  SIGSEGV；RISC-V 来源是 SI_KERNEL，AArch64 携带 SP 并按 find_vma existence 选择
  SEGV_ACCERR/SEGV_MAPERR；禁止把 EFAULT 返回 restorer。SignalFrame 静态 backend
  独占 valid_user_address/fault_address 投影，AArch64 在 copyin 前拒绝未按 16 bytes 对齐的 SP。
  runtime 验证损坏 header、不可读/未对齐 SP、默认终止与 caught handler 的 code/address。
  memory 的 has_user_vma_at_or_after 只在 AArch64 坏帧错误路径投影既有 VMA，
  从 floor/iter_from 跳过 supervisor VMA，不 fault-in、不增加缓存；lookup waiter OOM 以 SIGKILL 终止。
  SignalFrameRestoreError 唯一携带 OOM 或失败阶段 fault address：早期 copy/alignment 使用
  frame SP，完整 copy 后 machine/extension failure 通过 restore_fault_address 使用 AArch
  saved SP。syscall 必须先区分 copyin OOM，释放 TCB Arc 后终止，不能误发 SIGSEGV。
- `arch::user::SignalActionBytes` 由 system 静态投影给 syscall，独占 rt_sigaction 的
  copyin/copyout 布局：AArch64 为 handler/flags/restorer/mask 共 32 bytes，RISC-V 无
  restorer 字段共 24 bytes。Process signal disposition 唯一保存 restorer；Thread context
  的 enter_signal_handler 接收 flags/restorer，由 AArch64 backend 按 SA_RESTORER 选择
  用户 restorer，否则使用内核 trampoline；RISC-V 始终使用 trampoline。
  该控制路径 codec 不分配、不增加锁，runtime 验证标准 mask、旧 action copyout canary
  与 AArch64 自定义 restorer，故不增加热路径 benchmark。
- SignalFrame::signal_mask 只在完整 copyin 后投影 saved mask；mask 在 machine restore 前
  发布，坏 extension 中 blocked SIGSEGV 因而按 Linux forced policy 恢复默认终止。
- `syscall::user_iovec::UserInputStaging` 独占 write/send copyin 的 initialized prefix；stack 与 heap storage 都以 `MaybeUninit<u8>` 准备，只有成功 user-copy 的 prefix 可投影为 backend `&[u8]`。
- task loader 独占 pathname/script rewrite；memory ELF loader 独占 ELF plan、mapping、initial stack
  与 rollback。Thread 的 fs context 唯一拥有 cwd/umask，CLONE_FS 共享，unshare
  可脱离；fork/vfork child 取得独立 context。Process 独占最终 main ELF opened-entry，
  exec 只替换此映像 identity，不改变共享/脱离的 cwd/umask。procfs 只投影 `/proc/<pid>/exe` magic link，不缓存第二份 pathname。
- `ProcessTable::TimerQueue` 独占 timerfd immutable clock、setting 与 active deadline membership；
  task-domain `TimerFd` backend 独占未读 expiration counter 和 notification endpoint。fs 只通过
  `TimerFdBackend` OFD seam 消费 counter/readiness，不反向依赖 task；deadline 到期在 timer lock
  外发布 readiness，最后一个 backend Arc 析构必须移除 record，禁止 per-tick 扫描全部 descriptor。
- userspace builder 独占 target-native compiler/linker/compiler runtime 与固定 package/key/cache 输入：
  compiler-rt freestanding supplement 只消费 Clang builtin headers（`-nostdlibinc`），不查 host
  libc include 路径；缺失时 Debian Clang 的 musl search order 会把 host glibc header 混入 target。
  两个架构都使用 Clang、固定 `rust-lld` 与固定 Rust bare-metal `compiler_builtins`（AArch64 为
  hard-float AAPCS64 `aarch64-unknown-none`，RISC-V 为 lp64d medany `riscv64gc-unknown-none-elf`），
  并合并同版本 LLVM compiler-rt 的 C ABI `__multc3` 为单一 runtime archive；softfloat builtins 只属于
  kernel，链接进 musl 会让 FP helper return ABI 与调用方分裂。裸机 `libgcc` 是 medlow 非 PIC
  代码，不能进入 `libc.so`。产品 userspace 每个架构只保留一条 runtime。
- Rust std builder 独占固定 rust-src `std/panic_abort` 与同 revision LLVM libunwind 的 source-list
  build；Cargo 最终链接由 build-std 的 `compiler_builtins` 独占，不能再追加 musl builder 的外部
  compiler runtime。最终 ELF 必须动态依赖唯一 musl `libc.so`，libunwind 只允许静态进入 consumer。
- `user/Cargo.toml` 与 `user/Cargo.lock` 是产品 Rust userspace 的唯一 workspace/依赖解析 owner；Cargo
  直接链接标准 Linux/musl PIE，禁止 staticlib 中间产物、手工二次链接或每应用 lockfile。
  `linux-uapi` 独占 raw musl FFI 与 Linux layout/constant；公开 UI SDK 的固定 ABI seam
  仅允许导入 liteos_ui_get_api，具体约束见 [UI 框架契约](ui-framework.md)。
  系统 GUI 正逐步迁移到共享控件运行库，尚未全部解除内部 Rust dylib 的构建集合约束。


- `network-drive` 独占每用户映射记录、credentials 文件与 OFD flock，资源管理器、桌面重连和命令行只委托它。
  `smbfs::State` 独占 SMB 会话、FUSE 节点 lookup 与打开句柄；传输失败撤销旧代，不自动重放修改。
  上游 helper 的提取与 setuid 权限属于 `scripts/fuse_runtime.py`/`base_userland`；普通用户认证不经过 root 服务。
  完整边界见 [网络驱动器](../architecture/network-drives.md)。

- 媒体播放：`mpv-client` 独占对上游 libmpv 的函数表签名、事件解码与软件渲染上下文，`linux-uapi::dynamic_library` 独占 `dlopen`（符号导入只在 linux-uapi，`mpv-client` 只声明函数指针类型与回调）；
  mpv 的唤醒/帧更新回调只向一个非阻塞 socket 写字节，所有 mpv 调用与渲染都在播放器 UI 线程。`media-player` 的 `Engine` 独占
  播放状态镜像，`View` 独占控件；视频帧经 Canvas `canvas_frame` 显示，没有私有显示协议。libmpv/FFmpeg 的 APK 提取与闭包由
  `scripts/mpv_runtime.py`/`apk_elf_closure.py` 拥有（AArch64；其他 target 不安装，播放器启动时报告缺库）。
  `dlopen` 的 unsafe 契约：符号只在 `Library` 存活期间有效，`Mpv` 以字段顺序保证先释放渲染上下文与核心、最后卸载库；
  缺少它，卸载后的回调会跳进已解除映射的代码。

## Interface

- 未接入 number 返回 `ENOSYS`；不得建立私有 number、错号转发、silent flag ignore 或 userspace compatibility shim。
- syscall matrix中的每个入口必须唯一归属一个领域文件，并明确 Complete/Partial、对象范围与已知缺口。
- Linux/AArch64 与 Linux/RISC-V register convention、signal frame、ELF/TLS 与 capability query 必须经静态 ABI backend；禁止 `dyn` dispatch、运行时 architecture 分支或 generic owner 依赖具体 layout。
- AArch64 ELF 必须是 `EM_AARCH64`（183），auxv HWCAP 只公布 FP 与 ASIMD；其静态 decoder
  不接纳编号 258，dispatcher 必须返回 `ENOSYS`。RISC-V decoder 唯一接纳该编号并投递既有
  `riscv_hwprobe` UAPI codec；禁止恢复 `SUPPORTS_*` flag 或 AArch64 hwprobe 假实现。
- ET_EXEC 无论是否携带 PT_INTERP 都以零 load bias 映射；PT_INTERP 必须是独立校验的 ET_DYN。
  把动态 ET_EXEC 误判为非法会让 musl loader 尚未执行就返回 `ENOEXEC`。
- AArch64 CPU 即使能 decode 未公布的 SVE/SME probe，也不得为其建立第二套 context state；Unknown、
  SVE-access 与 SME-access exception 必须统一强制投递 `SIGILL/ILL_ILLOPC`，使标准用户 signal
  handler 能恢复 feature probe。blocked/ignored consequence 仍按同步 fault policy 收敛为 default。
- AArch64 CPU-local initialization 必须把 `CNTKCTL_EL1` 精确设置为仅
  `EL0VCTEN`，并在既有 `SCTLR_EL1` 上设置 `UCT/UCI/DZE`，允许 EL0 读取 `CNTVCT_EL0` 与 cache
  geometry、执行当前地址空间指令发布和 cache-line zero；不得同时开放物理计数器、event stream
  或用户态 virtual/physical timer control。缺失这些权限时，V8 等标准 runtime 会在计时或
  `FlushICache` 陷入 unsupported system-register exception。
- signal frame capture、SA_RESTART 与 sigreturn register restore 都通过 Thread context owner；frame
  copyout 成功前不得发布 handler registers，clone child 可取得一次完整 machine snapshot。
- `ContextOwner<UserContext>` 必须用两个短 transaction 调用静态 backend 的
  illegal-instruction seam：第一次只产生 typed probe，transaction 外完成可能阻塞的指令读取，
  第二次提交 retry/fault。RISC-V 可在精确 F/D/FP-CSR 且 `FS=Off` 时返回 retry；AArch64
  直接返回 typed fault，不得保留恒 false decoder/activation compatibility pipeline，也不得让
  context claim 跨越 AddressSpace lock。trap/task signal owner 必须把未被
  architecture seam 消费的非法指令发布为当前
  Thread 的 forced SIGILL generation；首个 fault siginfo 编码 `si_code=ILL_ILLOPC` 与
  `si_addr=PC`。caught+unblocked disposition 保持 handler；blocked 或 `SIG_IGN` 必须在同一
  generation 事务中恢复 `SIG_DFL` 并解除屏蔽，forced consequence 必须绕过 PID 1
  `SIGNAL_UNKILLABLE`。同号 standard signal 已 pending 时保留首个可见 siginfo，仅合并 forced
  consequence；缺失该合并会让同步 fault 返回同一 PC 无限 trap 或错误吞掉 capability probe。
- architecture breakpoint 必须发布当前 Thread 的 forced `SIGTRAP/TRAP_BRKPT` 与 fault PC；
  trap layer 不推进 architecture PC，也不得绕过已注册 signal handler 直接退出。
- 不可恢复的 user page fault 与 SIGILL 共享同一 forced fault policy：无 VMA 覆盖投递
  `SIGSEGV/SEGV_MAPERR`，VMA 拒绝本次访问投递 `SIGSEGV/SEGV_ACCERR`，file mapping 越过 EOF 或
  backing I/O 失败投递 `SIGBUS/BUS_ADRERR`；access fault 投递 `SIGSEGV/SEGV_ACCERR`，未支持的
  user exception 投递 `SIGILL/ILL_ILLTRP`，`si_addr` 均为 fault 地址。trap layer 不得直接终止进程，
  否则 guard page、JIT 与 fault-recovery handler 无法运行；唯一例外是物理页耗尽按 OOM 语义以
  SIGKILL 终止，不伪装为可捕获的地址错误。
- timerfd create/set/get/read 必须使用 Linux asm-generic 编号与 64-bit itimerspec/counter layout；
  timer replacement、deadline index 与 unread counter reset 构成同一串行 operation，readiness 必须进入
  既有 poll/epoll source。`TFD_TIMER_CANCEL_ON_SET` 在 realtime clock-set ABI 开放前明确返回 `EINVAL`。
- `ppoll` raw `pollfd` array 必须整批 copyin、解析并整批 copyout，不能按 fd 做 8-byte/2-byte
  微拷贝。DRM/evdev destructive event dequeue 必须先验证完整 batch，随后整批编码并一次 scatter；
  EFAULT 只允许保留此前完整 batch/vector 的 partial progress。
- pipe/socket/regular-file write 必须使用同一个 `UserInputStaging` seam；memory user-copy 直接初始化 `MaybeUninit<u8>` destination，不得为形成 `&mut [u8]` 预清零随后完整覆盖，也不得在各 syscall 保留 unsafe 转换分支。
- userspace application 不得依赖 LiteOS 私有 runtime、init、device protocol 或第二条 rootfs path。
- Rust application 必须使用标准 Linux/musl target；禁止 `os=none` custom target、预编译 bundled
  musl/CRT 或 LiteOS std fork。验证 fixture 只允许进入 disposable gate image，产品 rootfs 必须拒绝。
- 应用优先使用 `std`；稳定 `std` 缺失的 Linux 专有机制只能通过
  `linux-uapi::{alsa,drm,input,pty,process,shared_memory,unix}` 的安全 typed interface。
  shared-memory 的 memfd creation 通过标准 libc `memfd_create` 选择目标 syscall number，
  不在通用 library 固定 asm-generic 编号；该编号在 x86 Linux host 对应另一个 syscall。
  `linux-uapi::filesystem` 拥有 LP64 statvfs codec、容量换算与标准 renameat2(RENAME_NOREPLACE)；
  属性页只消费 typed capacity，资源管理器的改名/移动/还原用原子不覆盖接口，不能用 exists+rename 模拟；
  `linux-uapi::time` 只拥有 timespec 与时钟设置；calendar 拥有 TZif 解析、日期投影、持久化和重载。
  已解析时区由读写锁保活，重载不修改进程环境，支持 UI 与后台线程同时查询；TZif 支持范围见 UI 架构。
  fd ancillary mechanism 委托 `linux-uapi::unix`。raw syscall、应用私有 ABI、裸 fd/device owner 和并行兼容路径均禁止。
- 标准 `Command` spawn 的 AF_UNIX `SOCK_SEQPACKET|SOCK_CLOEXEC` socketpair 是 exec error
  publication owner；kernel 必须保留消息边界、peer-close EOF/hangup 与 `SO_TYPE=5`。只开放
  socketpair，seqpacket bind/listen/connect 仍明确返回不支持，不能在应用退回多线程不安全的 raw fork。
- 普通 `fork` 从多线程 parent 只复制 calling Thread，child 以独立 process owner、COW AddressSpace
  与调用时的 fd/credential/signal-action snapshot 发布；parent sibling 不进入 child graph。
  child 在 exec 前只能依赖 POSIX async-signal-safe 路径，kernel 不复制用户态锁状态。
- 多线程客户端若需要独立 PTY session，必须通过 `SessionChild` 的固定 `/bin/session-launch` 单轨启动 session child。
  owned group termination 必须在 wait 前冻结 kill errno，以 `ESRCH` 判断 group 已消失，
  不使用 host-dependent `ErrorKind::NotFound`；无论 group 是否已消失都必须回收 leader。
  parent 的标准 `Command` 不得安装 `pre_exec`；单线程 trampoline 依次设置 parent-death signal、
  复检 parent identity、`setsid`，PTY 模式再执行 `TIOCSCTTY`，最后同 PID `exec`。
  PTY 目标 stderr 接 slave，其他 profile 接 console，均与 exec-status 分离。
  parent 独占 exec-status 读端，trampoline
  独占 CLOEXEC 写端；固定 frame 发布所有确定性 setup/exec 错误，空 EOF 表示没有这类已发布错误，
  不虚报异步致死与成功 exec 后立即退出可区分。错误、非空截断或非法 frame 必须 kill/wait 回收；
  禁止普通 spawn、raw fork 或重试兼容路径。
- Wayland 外壳继承的连接经 `linux-uapi::process` 传递：发送端消费 `OwnedFd`，标准 I/O 装配失败时由 `Command` 回收；
  接收端用 `F_DUPFD_CLOEXEC` 取得独立所有权，并给原 fd 设置 CLOEXEC，不接管可能被标准输入等对象借用的 fd。
  接收不修改全局环境；之后启动普通客户端须显式 `env_remove("WAYLAND_SOCKET")`，避免它使用 exec 已关闭的继承编号。
- `classic-ui::Display` 独占客户端 Wayland 连接、对象编号、configure、shm 与输入路由：仅在 delete_id 后复用编号，
  xdg_toplevel.configure 先暂存，到 xdg_surface.configure 时 ack 并一次性发布；首轮 configure 前不能提交 buffer。
  每个窗口最多一条未完成 frame callback，`ready_to_draw` 是该状态的只读投影；收到 buffer.release 后销毁该 buffer，
  在此之前绝不修改提交的 memfd。窗口销毁撤销焦点、重复键和已排队语义事件；连接出错由应用退出/drop 一次性回收 fd。
  Transport drop 对独占协议连接执行 shutdown，因此继承的 stdin 别名仍打开时 server 也能立即观察 EOF；不关闭外部借用的 fd。
- `classic-ui::Decoration` 只消费语义事件，按钮命中与绘制共用 `window` 的几何事实；窗口模式来自 compositor configure；普通固定窗口可按内容选择首选尺寸并更新标准 min/max 约束，当前尺寸仍由 Display 唯一持有。
- 应用的待重绘失效由该窗口的事件循环持有，必须跨越在途 frame 保留；只有 `present` 成功后清除，
  不能在每轮事件开始时重置或依赖定时采样补画。`FramePresented` 解除提交背压，本身不代替尚未消费的失效。
- `Painter` 的有界文字 API 必须同时约束排版宽度和实际字形落笔；省略号、粗体与禁用浮雕也不能越界。段落复用 `typeface::wrap`，返回完整所需高度但只绘制指定矩形，结束后恢复调用者裁剪区。
- `ColumnHeader` 独占逻辑列宽和拖动状态；`paint` 在真实视口与每列逻辑矩形的交集内绘制，命中只接受视口内坐标。任务管理器、资源管理器共用该边界，不截短逻辑列宽或各自补裁切。
- 控制面板的属性页内容矩形由同一函数供绘制与输入使用，宿主统一裁剪页内绘制；列表与滚动条消费剩余高度，滚动和命中使用实际视口高度，不以固定可见行数替代。
- `MemFd::write_segments` 借用已初始化、无 padding 的 u32 字节视图，先校验全部段都在 extent 内再按原 native-endian 布局写入，
  零额外整帧字节数组；所有段共用一次 fd 复制，视图不逃逸，像素数据不可变直到写入结束，frame 发布仍由 `Display` 独占。
- `Edit` 独占文本、UTF-8 光标与选择锚点；`FocusChain` 独占控件槽位的 Tab 顺序。Painter 只读这些状态，不拥有第二份可写控件模型。
- `classic-ui` 的 Window 角色为互斥的 Toplevel/Layer；只有 Toplevel 持有待确认的 xdg configure，Layer 在自己的 configure/ack 时直接发布当前几何。
  两种角色共用帧、输入和 fd 生命周期。Layer unmap 清除旧配置、重复键与排队语义事件，configure_layer 后必须等待新的 configure 才能提交。
- `classic-ui::delegate_shell` 通过标准 security-context-v1 注册独立 listener；0600 pathname 在预连接后立即 unlink，
  子进程只接收连接，父会话持有 ShellLease，drop 只撤销未来 accept，不终止已经连接的进程。
  `linux-uapi::process::spawn_with_wayland_socket` 的可选 Identity 使用标准 Command gid/uid，清空继承补充组；不使用 pre_exec，也不承担认证判断。
  它只用于初始 logon/机制探针；实际用户桌面由 SessionChild 的 Wayland profile 启动，执行账户补充组与清洁环境契约。
- `keyboard::xkb` 是 modifier masks 与 US keymap 的唯一事实 owner，客户端在接收 keymap 时验证支持范围后再解释按键。
  重复键只使用服务端 repeat_info；release、键盘 leave、能力撤销或窗口销毁清除重复，重复事件使用当前 modifiers，不延续已失焦窗口的按键。
- APK 只接受所选 architecture repository 的固定摘要与精确 `.PKGINFO`。只有 `ca-certificates-bundle`、`git-init-template` 与 `ncurses-terminfo-base` 三个固定数据包预期 `noarch`；其余包必须精确匹配目标架构，禁止 blanket `noarch` 放宽。
- `/etc/apk/repositories` 必须精确启用固定 Alpine stable branch 的 `main` 与 `community`；
  `main` 缺失会破坏基础包解析，`community` 缺失会让 npm 等标准拆分包不可见，禁止加入
  edge/testing 或其他 branch 形成未固定 ABI 回退。
- 产品 rootfs 必须把同一个 BusyBox `env` inode 发布到 `/bin/env` 与 `/usr/bin/env`；缺失后者会让
  npm/pnpm 等已成功安装的 `#!/usr/bin/env node` 入口在 exec 时错误返回 `ENOENT`。
- 产品 login profile 必须使用标准 PATH 并包含
  `/usr/local/bin`；npm global prefix 和 Agent 开发环境发布的 command 位于该目录，任一
  owner 缺失时包已成功安装但对应交互 shell 仍会错误报告 command not found。增量用户态同步
  必须拥有 `/etc/profile`，否则旧持久镜像不会随产品契约修复。
- 产品 BusyBox 必须发布 `add-shell` 与 `remove-shell` applet；缺失时 Bash 等 stable APK 的
  maintainer script 无法通过标准 owner 更新 `/etc/shells`，安装只能留下部分配置。
- Codex/Claude 只属于 AArch64 持久开发实例 owner：固定 Node/npm APK closure 先离线安装，
  Guest npm 再从 host 按 registry SRI 校验并固定的 cache 把两个官方 package 全局安装到
  `/usr/local`。该 owner 必须验证 npm package identity、平台 optional package、APK metadata、
  package database、唯一 command 入口与真实 CLI 启动；不得污染产品 rootfs、加入滚动
  repository、保留 raw binary/Claude APK 第二路径，或把 namespace sandbox 宣称为已支持。

## Failure and cleanup

- exec 在 point of no return 前完成 source、ELF、stack 与 owner allocation；失败保持旧 image，提交后只允许新 image 或进程退出。
- ABI publication 遵循各 native owner：普通 open、clone PIDFD 等严格输出在 publish 前完成；
  `recvmsg` 的 SCM_RIGHTS 每个 FD 编号 copyout 成功后立即安装，后续 control/name/msghdr fault
  保留已安装 prefix，只撤销尚未发布的当前 reservation。不得以整批 rollback 改写 native 行为。
- copyin fault 只允许发布已完成的 initialized prefix；atomic socket message 丢弃该 prefix，regular write 可按既有 partial-write policy 提交它，任何路径都不得读取未发布 suffix。
- compiler、linker、compiler runtime、ELF machine 或 APK name/version/arch/摘要不匹配时，必须在 sysroot、rootfs 或 cache generation 发布前 fail-stop；临时下载和未发布 generation 必须清理，其他架构 cache 不得作为回退。
- rust-src、LLVM libunwind input、build-std feature、target linker 或唯一动态 dependency 不匹配时，
  必须在 std consumer generation 发布前 fail-stop；未完成的 object/Cargo target directory 必须清理。
- Linux `clone` 的 `CLONE_PARENT_SETTID`/`CLONE_CHILD_SETTID` 是例外的 best-effort store：
  Thread identity 先发布但保持 `New`，store fault 不回滚、不改成功返回；全部 store 尝试
  完成后才按 process job-control 原子转为 Ready/Stopped。并发 `exit_group` 已提交时，新
  child 在变为 Ready 前继承 kernel SIGKILL，parent-visible exit status 仍由首次
  group-exit owner 决定。

- `syscall_abi::capability` 独占41个固定Linux capability编号及v1/v2/v3的12/24-byte数据codec，
  decode屏蔽未定义高位。capget NULL data只读version、unknown version写回V3后成功；
  capset先version/PID权限后data copy，foreign target EPERM不变更任何credential。
  InitialStackCredentials只投影已准备的UID/EUID/GID/EGID/secureexec；memory不重算权限。
- `syscall_abi::file_capability`只编解码标准security.capability的12/20/24-byte little-endian
  revision1/2/3。disk reader按固定commoncap保留未知非revision标志和全部64-bit capability bits；
  setxattr header只接受revision2/3与EFFECTIVE标志，两者不能共用更严格的read validator。
  encode拒绝不能由较旧revision表示的rootid/high words；不静默丢权限数据。inode xattr、
  rootid的superblock/user namespace映射、授权与exec credential转移仍归各实际kernel owner。
  该codec固定最多24字节、无分配/锁；用固定disk vectors、截断/扩展/旧格式损失用例验证，
  不新增wall-time benchmark。

## 本机登录与桌面会话

- `logon::account` 是 passwd/shadow/group 解析、锁定/到期策略、用户主目录与补充组初始化的唯一 owner。
  散列验证仅用固定 musl `crypt_r`，接受 SHA-256/512 crypt；空 shadow 字段只接受空输入，拒绝锁定值与需要强制改密的账户；不使用全局 crypt 存储。
  `credentials` 独占 FFI 布局与 volatile 擦除；密码不会进入日志，提交、失败、取消和页面切换清空整个编辑分配（包括删除后的尾部）。
  Purpose 仅区分登录与改密策略：非 UID 0 的改密检查最短间隔，正常登录不受最短改密间隔影响。
- `SessionCommand::user` 将认证结果作为数据消费，不自行认证。单线程 `session-launch` 先打开诊断 console，
  再依次 setgroups/setgid/setuid，最后设置 parent-death、复检 parent、setsid 并 exec；父进程不安装 pre_exec。
  组列表按 musl getgrouplist 包含主组并去重。任一步失败通过原有 exec-status 发布，父进程 kill/wait 回收。
- 子会话使用清洁 PATH/HOME/USER/LOGNAME/XDG_RUNTIME_DIR/WAYLAND_DISPLAY；运行时目录为用户拥有的 0700 `/run/user/<uid>`。
  ShellLease、SessionChild 与监督 socket 都由同一个 UserSession 拥有；注销先请求安全覆盖，再 kill 拥有的进程组并 wait，最后撤销租约。
  leader 已退出时仍清理后台组成员；terminated 仅在 group kill/wait 完成后设置，阻止析构再次 signal 已可复用的编号。主动 setsid/改变组的进程不属于此进程组监督；当前没有 cgroup/logind 会话资源域。
- 标准 stdin 传递 Wayland，stdout 装配为独占 AF_UNIX socketpair，供同一会话应用内部监督使用：
  desktop→logon 单字节 1=首帧完成、2=锁定、4=关机页；logon→desktop 5=任务窗口。
  只有背景与任务栏都收到 frame callback 后可发 1；重复 ready、未知消息、提前 EOF 和启动超时均失败并回收会话。
  该通道不接受密码、路径或身份，不能委派新能力；应用启动移除继承环境并重装标准 I/O，不能继承它。
- `desktop` 拥有图标、任务栏、菜单、运行与应用任务窗口；窗口状态只读 foreign-toplevel 的 done 快照，操作等后续事件确认。
  publish revision 只用于合并重绘，pending 属性不对界面可见，closed 立即删除句柄；服务端对象不能进入客户端编号复用队列。
  待绘制窗口集合只记录失效，不复制窗口配置，避免一个窗口等待 frame 时反复重绘其他窗口。
- 系统图像为带显式 RGBA masks 的标准 BITMAPV4HEADER BMP；只在启动加载，限制 2048²，像素转为预乘 ARGB 后复用。
  新数据与 Vec 分配不进入内核，不增加锁；成本决策见桌面验证文档。
- 默认 root 的 shadow 密码字段为空、nobody 保持锁定，文件 mode 0600；BusyBox passwd/adduser/addgroup 拥有账户文件写入。
  增量同步在 shadow 缺失时创建默认，并仅迁移完整匹配的旧出厂锁定模板；已有密码与自定义账户不被覆盖；账户工具复用同一个固定 BusyBox ELF。

- `alsa::control::MasterControl` 独占 card0 control fd，INFO 验证固定 stereo Master 的类型与范围，READ 返回只读快照，WRITE 后重新读取实际量化值。
  Linux 7.1 LP64 的 ID/INFO/VALUE 布局与 ioctl 编号只在 linux-uapi 中编码；两个 control 分别写入，不伪装成原子事务。
  GUI 不代理或扩大设备权限，默认 0600 节点只有有权账户可用；无权限/无设备显示禁用状态。
- desktop 的 Volume 是任务栏与音量窗口共用的唯一 UI 投影，不复制第二份 mixer owner；内核仍拥有真实 gain/switch。
  外部变化最多每 100ms 采样，防止指针频率放大 ioctl；slider gesture 仅在按住至 release/leave/focus-loss/close 期间有效，避免悬停修改音量。
- 解锁只允许认证后的 UID 等于会话 owner 或为本机 UID 0；管理员解锁不替换既有 SessionUser、进程身份或设备权限。
