# 内核运行时回归覆盖

通用后台压力回收由 musl consumer 实际制造低水位：清洁文件 cache 与不可回收匿名页共同施压，
`pgscan_kswapd`/`pgsteal_kswapd` 增长且 `allocstall` 不增长；随后完整校验匿名页与文件内容。
生产 policy 的 host 测试验证 hysteresis、64 页 batch、精确补齐和无进展退避；统计分类测试
直接执行唯一 registry，不能将这些有界策略检查外推为 swap 或完整 Linux kswapd 支持。

musl 另以标准 swapon/swapoff 启动第二块 64MiB SWAPSPACE2 盘，双架构各执行 1/2 CPU
冷启动，实际施压换出并逐页验证 canary，检查权限、重复 activation、raw writer/mount 排斥、
proc/sysinfo、drain 与立即重新启用。QEMU 的两个 virt machine DTB transport 顺序不同，
命令声明顺序分别保证 root=vda、scratch=vdb；全部四次启动成功后才发布该 runtime stamp。
同一 pressure consumer 依次覆盖 block/file swap；swapfile 另裁决失败激活后的 fork、旧共享映射 SIGBUS、
PROT_NONE→WRITE、private COW、只读 fd VM_MAYWRITE、MAP_FIXED 拒绝不改旧 mapping、hardlink
排他、readonly/remount 与成功 close。共享 COW、共享匿名、memfd 与 tmpfs 另建立8MiB
种子并 fork 休眠子进程，要求其自身 VmRSS 至少下降2MiB；父进程恢复全部 canary 后，
子进程的 RSS 仍保持独立，随后验证共享写入/COW隔离、mprotect与文件截断尾页。
共享页 consumer 纳入双 backend 各 1/2 CPU TCG 的 musl 门禁。双 CPU 时父/子分别
固定在首/末可用 CPU；父恢复种子后 SwapCached 必须至少覆盖种子大小减子当前 RSS，
而子 RSS 至少下降 2MiB。RSS 同时包含代码和栈，不能将其全部下降误认为种子页数。
完整产品验证入口与执行环境限制见 [build-and-verify](build-and-verify.md)。

`kernel_deadline_wait` 在启动时准备 timer storage 后拒绝当前线程的一切 allocation，执行生产
KernelDeadlineWait、TaskEvent、WaitCompletion 与 AVL；100 次等待必须使用同一 node，
同时用 Vec reserve 控制确认故障注入有效。scheduler target 的 host stub 固定到期发生于
arming，仅证明该交错和无分配；真实 sleeping、timer delivery 与多 CPU 调度由上述 guest
压力 consumer 验证，不声称 host 穷尽 Drop 或所有唤醒交错。标准 timer deadline node 额外
携带一个 pointer-sized Option<Weak>（None）；有界 timer benchmark/expiry batch 保持原门禁，
不为后台能力放宽时序阈值或增加每轮 allocation。

Swap extent 二分定位与 writer event 无 waiter signal 的 CPU 诊断：
`cargo test -p kernel-unit --release --lib production_swap_extent_and_writer_retirement_cpu_cost -- --ignored --nocapture`。
固定10万次 warmup、5次 median，分别测1/512 extent；仅生产 lookup 与 TaskEvent signal 路径，
不把 host CPU 数值外推为 VirtIO 延迟、scheduler wake 或竞争 waiter 成本，不改变门槛。

Swapoff consumer 的第五种共享 COW 场景，在休眠 child 自身 RSS 已下降至少2MiB后，
由另一进程反复发送 SIGUSR1。要求真实 swapoff 返回 EINTR、仍然激活的 area 让
swapon 返回 EBUSY、停止信号源后重试 drain 成功，并重新启用、校验全部 canary。
单/双 CPU、双 backend 均执行该 consumer，完整 musl 门禁通过后才发布产品 stamp。
TaskEvent 的取消协议 host fixture 另外覆盖同地址重建 event 的迟到旧 key、signal/event
同时完成、非可中断等待、空 area/最终槽已释放优先成功。调度和 signal source 为显式
host seam，实际 TCB 的信号入口由 musl consumer 验证。
新增 event 全局唯一 ticket 的性能决策：Atomic 只在阻塞登记时领取，不进入 normal
syscall/trap、page resident pin 或无 waiter 的 event signal；不开宿主时序阈值。
预置 KernelDeadlineWait 的无分配回归及实际双 CPU signal/drain consumer 必须保持通过。

`verify-architecture-benchmark` 同时运行原 architecture-bench 与实际 SCM owner 的
unix-rights-bench、inotify-bench、blocked-load-bench、shared-page-stats-bench 与 realtime-signals-bench。共用原 2,000,000 次、5 个样本、200 ns hot-path policy；新增
SCM exact-UID quota rejection 适用同一阈值。实际 cold preparation/publication/retirement
与 16/256 node Tarjan pass 单独输出 ns/op，用于评估分配、AVL 与 O(v+e) collector 成本，
不把这些冷 transaction 伪装成纯 policy lookup，也不放宽任何原门禁。

`inotify-bench` 原样加载 production queue/codec/UserNS ucounts。标准栈 codec、8192 条队列的 FIONREAD
aggregate、可复用 overflow publish/pop 采用同一 200 ns 热路径阈值；含 allocation 的普通 event node
publish/pop（0/8192 resident）及 1024 UID 条目下既有/新 UID admission/retirement 只报告 cold-path
CPU 成本，随 architecture benchmark 统一执行；结果不表示用户缺页或 scheduler wake 延迟。

architecture-bench 还原样装配 IPC namespace IDs/metadata 与 proc IpcReadCursor，报告16/1024个对象的
完整 native64 行格式化/连续读取成本；固定512字节记录不分配输出 Vec，标准 UID/PID view 取真实
namespace owner。该诊断耗时不适用纯200ns policy gate；分段读取和EFAULT suffix另由实际 per-open fixture验证。

- `blocked-load-bench` 在实际 1024 个 blocked tokens 下测量 register/retire 与 aggregate read，
  沿用 `cost_policy` 的次数、样本与 200 ns 热路径预算；纳入 architecture benchmark。
  TaskWait mode、early wake、跨 CPU ready、stop/resume 和 exit 的 owner 用例随 kernel-unit 执行。

- `shared-page-stats-bench` 原样加载 production 共享页账本，对 1/1024/1048576 个 membership
  测量 O(1) 一致快照、同锁 swap/fault 状态变更及 admission/retirement，沿用同一 200ns policy。
  actual backing/page fixture 覆盖并发 fault 胜出、物理分配失败、读写 IO 失败和准备期间未提交，
  以及 backing/observer 最后退休；随统一单元测试与 architecture benchmark 执行。

- `realtime-signals-bench` 加载实际 native record/UserNS rlimit owner，对 1/1024 个 RT records
  测量批量 publication/dequeue。使用原 2,000,000 次、5 样本、200 ns policy；record allocation、
  quota preparation 和最终退款在计时区外执行，计时只包含无分配 pending owner 热路径。
  每个 preparation 必须成功，不能用 bit-only OOM fallback 冒充 FIFO benchmark。消费保存
  实际 TakenSignal，计时完成后再 Drop，源和 guest 验证资源的真实退款/退出 lifetime。
  同一 binary 另测 ordinary/RT POSIX notification 的实际预留 Box checkout/publication/
  dequeue/return 全部 roundtrip，无 allocator/ucounts preparation；每 roundtrip 仍采用既有
  200 ns policy，不调整阈值。posix_timer_lifetime/signal_info 实际 owner 回归覆盖 creator
  creation quota（含 silent timer）、降限后到期、长期阻塞 overrun、同 node 重用、ignored 恢复、
  旧 sequence/ID 复用、queued charge 与 exec SI_TIMER 退休。musl signal-frame 原 fixture
  增加真实标准 syscall 的同组/Thread timer、两个普通同号 timer、SIG_IGN 与删除后额度用例；
  namespace-user 的 PIDNS init 使用 namespace 可见 TID 1 的实际 SIGEV_THREAD_ID timer，
  覆盖可见编号到实际 identity 的转换。
  普通 signal 实际 charge 与 RT/timer 共用额度；producer/native guest 覆盖 ordinary
  first-source coalescing、nonnegative 越限仍收费、负 code 低内存 SI_USER fallback，以及
  ordinary 与同号多个 timer 的真实 FIFO。同一 benchmark 增加已有 ordinary record 的
  实际 coalescing，unused reservation 在计时外退款，原 200 ns threshold 保持。
  pending fee/creator preparation 属于 graph 外冷事务，不改变 publication/dequeue 门禁。
  Pending owner 不再内嵌 65 个 56-byte payload，
  只有 actual records 与 force-only fallback，LP64 owner 的 compile-time 上界为 1152 bytes。


- Timer record/deadline retirement 的改动不新增 wall-clock benchmark：owner critical section
  的 node 摘取仍用实际 AVL；final allocator/Weak Drop 移到锁外。borrowed transaction fixture
  直接观察真实 VacantEntry payload Drop 时两个 owner guards 均可取得；stale-plan 复用必须
  保持 allocation identity，lifecycle failure 不能发布。expiry 的固定 retirement 字段保持
  原 bounded batch，沿用既有 timer transaction/period/source/runtime gates。

signalfd 的 anonymous-files 包含真实 reader/dup mask/readv/EFAULT、fork 的 epoll ADD-time source 以及 ET 重投；signal_handlers/signal_info 的 host 回归加载实际消费 owner/native codec，realtime-signals-bench 增加
标准 128-byte codec 与 mask，沿用原阈值。

回归覆盖：unix_rights_graph 加载真实 stream/barrier/rights/GC owners；namespace-user 对 stream/datagram/seqpacket 各重复 peek 同一消息并安装独立 fd，最终消费；unix-rights-bench 追加实际 chunk FIFO 热
路径及标准 SCM copy 的冷成本。

AF_UNIX autobind 的回归包含 actual namespace host fixture 的 OOM/碰撞复用/重复绑定零分配/closed observer 拒绝/类型隔离，以及 namespace-user guest 的三种 socket type、原生 8-byte sockaddr、显式碰撞、同名不同类型、关闭后重绑
和 datagram 投递。绑定为冷路径，本次不新增热路径 benchmark；已有 socket/SCM benchmark 阈值保持。

Socket timeout 的回归包括 actual SocketTimeouts/native codec host fixture（非法 usec、负秒与零的不同等待语义、tick 上取整、饱和、dup 共享/已冻结 deadline 独立）、namespace-user guest 的 scalar/vector/message recv/send、stream 和
datagram 背压、accept、OLD/NEW option、短/零/负 getter 长度与 UNIX accepted 默认 policy。unix-rights-bench 增加实际 policy lock/deadline snapshot，沿用 2M 次、5 采样与 200 ns 既有阈值。

SocketCapacityWait 与实际 unix::capacity owner 新增 host fixture，覆盖 full listener → accept → ready、关闭后排空通知仍可重新检查、packet 终态；ConnectionBacklog 回归维护 native backlog + 1 和并发 reservation/FIFO。namespace-user 增加 listen(0)
、非阻塞满队列、SO_SNDTIMEO、同一 client 重试、accept 唤醒阻塞 connect 与 listener 关闭返回 ECONNREFUSED。capacity 为既有通知和冷连接路径，不另设性能阈值；

UNIX address codec 的最后 guest 验证还包含非 NUL 结尾的全部 108-byte pathname：绑定、完整/短 getsockname 的 111-byte actual length、末尾 NUL/canary、连接与 accepted identity。编码仍使用定长 stack storage，无新 allocation 或热路径算法，沿
用已有 socket benchmark。

PID identity observer 的最后 host 验证直接加载实际 PidNamespace/PreparedPid/graph registration：namespace/ancestor 返回相同 Arc、未发表不可见、PGID/SID link 保持 association、lookup 无分配、保留旧 observer 仍可立即复用相同编号。原 PID
namespace allocator 回归已改为保留真实 PidRegistration 输入，取消只发表 key 的合成占用。

SCM_CREDENTIALS preparation 的 actual host fixture 已加入：调用方只提供不可变 owner 输入，实际 codec/权限计算/PID registry 来自生产源码。覆盖全部 real/effective/saved ID、错误 ucred 长度、映射优先级、CAP_SYS_ADMIN 的 PID owning UserNS 限
制、仅 PGID/SID link 存活、保留旧 observer 仍 ESRCH、负/零/不存在 PID。SCM parse/transport/接收已接通源码；actual frame host 回归和 guest 三种类型的旧消息、explicit claim、PID/IDs 权限、partial controls、creds+rights/MSG_PEEK、零长度
recvmsg 以及 real/saved IDs 均纳入统一 host 与 musl guest 门禁。

unix-rights-bench 还加入实际 PID namespace observer lookup 与 MessageIdentity claim validation，均沿用现有 2M 次/5 采样/200 ns 门禁。前者直接访问实际有序 association，后者使用不可变调用输入并执行生产映射/capability 检查；不把 fixture
producer 当作完整 TCB credential snapshot 的性能结论。

SCM 凭据 hot benchmark 使用真实 PID lookup/claim codec 与 MessageFrame peek、相邻 coalescing/prefix consume，沿用 unix-rights-bench 的 2M 次/5采样/200 ns既有阈值；cold stream construction + first frame admission 独立报告。所有 case 随统一
architecture benchmark 执行。

完整 unix_endpoint host fixture 直接加载生产 AF_UNIX 全部 owners，补充 connect factory/backlog 失败回滚、accept 前 SeqPacket 排队、半关闭/RESET/错误顺序、SO_ERROR、datagram peer 更换/断开、pathname actual inode pin/child/closed observer。
namespace-user 补充对应 native ABI、POLLRDHUP 与 pathname unlink/recreate；anonymous-files 加入三种最小记录匿名文件 read(NULL,0) 的 EINVAL。统一验证时同时覆盖三 source 的 persistent epoll membership 与原有
stream/epoll 热路径阈值，不降低门禁。

unix-packet-bench 加载完整 actual Unix endpoint/queue/ledger owners，actual length validation 与完整 packet poll 沿用统一 2M 次/5样本/200ns 门禁；逐消息 native allocation/publication/consume/refund 20k 次独立报告冷成本。make verify 的既有 benchmark
workflow 已加入该 executable。namespace-user 新增互连 Datagram/SeqPacket >11条、100000-byte payload/两次peek/一次consume/canary/POLLOUT 退费行为。

回归覆盖：namespace-user 覆盖三个 type 的 SO_SNDBUF/RCVBUF/dup/短 getter、packet sndbuf-32、stream peek/partial退款、sendmsg name/control/SeqPacket error 优先级、零长度 peer half/full-close，以及 write/send 共用 blocking
suffix 和 timeout prefix。anonymous-files 覆盖 pipe/inotify/scalar minimum record 与原生 readv/writev empty iterator 区别。send_staging_cost 仍要求初始化成本为零，并验证 fs route 到唯一 socket engine，无较宽阈值。

- full/fast host scope 首先执行 `scripts/verify_smoltcp.py`，核对固定 upstream inventory、
  允许变化的实际 transport producer 与全部 consumer 的唯一 path/feature 配置；不会生成
  或覆盖源码。architecture source fence 与 actual wire host 回归随后检查 native admission、
  解绑、shutdown、source freeze；vendor Rust formatting 在集中验证阶段检查所有实际修改文件。

BusyBox 挂载 gate 验证完整 relatime/data=ordered 展示、同一 ext4 volume 的 mount view
共享 inode/数据、cwd 与真实 writer 的卸载拒绝，以及 umount 根的 superblock-only RO。
后者必须保留 mountinfo 的 per-view rw，拒绝真实 truncate 写入并保持原 canary，再由
remount,rw 恢复。不能用“根卸载始终失败”替代标准语义；marker 只在全部条件成功后生成。

BusyBox cache 退休 gate 保留原 4MiB 删除回收/打开持有及 3000KiB/256KiB 阈值，额外验证
O_RDWR unlink 后的 8-byte canary、最后 fd 关闭回收、rename overwrite 的缓存 victim 回收。
这些测试执行真实 page-cache、dentry、inode、OFD 与 ext4 allocation owner；不依赖 sync 才释放。
串口输入仅对 printable glyph 等待回显，最终 LF 作为提交 delimiter；BusyBox lineedit 的
80-column 边界不保证 LF 回显，命令执行仍由真实结果 marker、deadline 和禁止项裁决。

动态链接 UNIX/epoll fixture 验证 64KiB datagram 的完整 payload，并从实际 SO_SNDBUF
取得 packet 超限边界，write/sendto/sendmsg 均须返回 EMSGSIZE 且不入队。小发送缓冲区
使 backpressure 有界；POLLOUT/epoll 在实际退款越过 quarter-watermark 后恢复，阻塞发送
在已知旧 packet 全部退还后完成，不假定单条读取就满足原生 socket 写唤醒条件。

串口门禁分别记录 readiness 消费位置和最近 shell command 提交位置，位置均为去 ANSI
后的字符偏移。后台事件晚于 prompt 时使用同一真实 prompt，不重复空等；旧 prompt
不能满足下一条命令，超时/EOF 明确失败。实际 pipe 回归覆盖该输出顺序与缺失 prompt。

作业控制 probe 在实际 foreground group 确认后输出首个 ready；SIGCONT 交付后仅在
tcgetpgrp 再次等于自身 group 时输出 fg ready。Ctrl-Z/C 不以 parent 在 fork/fg 前的
echo 作为时机，不依赖信号次数或固定延迟；SIGTSTP/SIGINT 使用实际继承 disposition。

每次交互由 caller 显式构造 ShellCommand 或 TerminalInput，boot 在启动 QEMU 前拒绝
未分类 bytes。`:wq` 与 `dd` 的带换行输入仍为 TerminalInput；不按换行推测 shell，
无超时降级或旧 raw-bytes 兼容入口。真实终端输入与非法调用回归覆盖该分类。
