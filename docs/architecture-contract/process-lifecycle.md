# 进程接口、等待与退休契约

## Interface

- 调度路径只使用 logical `CpuId`/`CpuSet`；不得把 hardware identity 保存为 affinity、processor 或 mask。
- Ready transition/retirement token 必须直接交给唯一 commit seam；不得缓存、遗忘或从全局 task 表重建 projection。
- 本地 Ready publication 只有在其 vruntime 严格早于 current 时才发布 reschedule；post-switch
  handoff 重新入队的高 vruntime outgoing 不得反向抢占刚选中的 successor。回归固定验证
  `1_000_000 > 10_000` 不抢占、`1_000 < 20_000` 抢占，相等/stale root 不产生 ping-pong。
  remote Ready sender 只发布 IPI wake edge，不读取 target policy 或强置 reschedule：idle target
  立即 drain inbound，busy target 在下一 tick 按 authoritative Ready projection 进入 CFS 选择。
- runnable successor 已存在时，scheduler 必须在一次 `task -> task` context switch 中转移
  `Processor.current`；只有没有 successor 且 outgoing 不能继续 Running 时才进入 idle。
  outgoing 是可继续的 Preempting/WakePending 且 runqueue 为空时直接恢复 current，不做自我切换。
- idle decision 必须在 local IRQ guard 内依次完成 deferred safe point、mailbox drain 与 runnable
  selection；无 successor 时 guard 继续覆盖 architecture guarded WFI。assembly 临时开中断，trap
  通过精确 WFI/resume PC 修复 enable-to-WFI 窗口，返回前再次关中断，guard 随后恢复旧状态。
  禁止先释放 guard 再执行裸 WFI，也禁止把周期 timer 当作一次性 IPI/deferred edge 的可靠兜底。
- boot CPU 保留 always-armed housekeeping/liveness tick；global deadline/timer queue 的状态 owner
  不变。非 boot CPU 在同一个 idle IRQ guard 内关闭 local timer source，只有选中 runnable task
  后才先写入未来 deadline、恢复 source，
  再切换到 task；remote Ready 的 IPI 必须独立于 timer source 唤醒 WFI。禁止让全部 idle CPU 保持
  周期 tick，也禁止关闭 boot tick 后用轮询或 runtime housekeeping source 迁移补偿。
- signal selection、permission、generation 与 job-control consequence 必须在 process-graph transaction 内线性化，锁外才执行 wake/notification。
- clone/fork/vfork child 必须在发布前从 calling task 的 live architecture context 取得完整 machine snapshot；AArch64 包含 q0-q31、FPCR 与 FPSR。新 task 初始 vector image 和 exec commit 后的 live vector file 必须为零，禁止跨进程映像泄漏；普通
  trap 不得承担该同步。
- exit/reparent/wait/TID selection 只能遍历 direct children、creator dependents 或 exact group
  members；禁止退回全 ProcessGraph 扫描。waiter storage 在锁外准备，event claim、signal
  复查与 waiter publication 必须在一次 graph transaction 内完成。正 PID 的 `wait4`
  selector 是 caller input，不是 parent-child index node；并发 waiter 已消费该 child 后，loser
  必须返回 `ECHILD`，不得把 exact PID 缺失误判为 graph corruption 并 panic。
- syscall 只能请求 task façade；不得访问 scheduler container、process graph lock 或 signal internal state。
- TaskMutex wait 不进入 signal-indexed registry；普通 lock 不接受 signal cancel，显式
  interruptible/killable acquisition 由同一 FIFO owner 取消 queued ticket 或归还已授予 handoff；owner unlock 或
  publication-window self-wake 与所选 signal policy 消费同一 membership 一次。wait node/Arc 在 owner spin
  gate 外预分配，final enqueue 与 `Held -> Handoff(ticket)` 在短锁内线性化，wake consequence
  必须在锁外执行。unlock 必须先退休 queue/publisher 持有的 wait-node owner，再发布独立
  completion token；否则 arming task 可在旧 Arc 释放前恢复，取得 guard 后无法回收并复用其
  预分配 waiter。

## Failure and cleanup

- `task::spawn_kernel_thread` 在 TID、kernel stack 或主体分配失败时返回 `OutOfMemory` 且不发布任何
  scheduler state。内核线程首次运行时显式打开本地中断：新执行体首次恢复时的中断状态取决于前一个
  outgoing task，缺失时主体会在屏蔽中断下运行并延迟 tick 与 I/O completion。

- wait ID 由单一 AtomicU64 无锁签发；全部 source node 与 registration storage 在 shard
  lock 外准备。publication 先进入 `Arming`，readiness/backend/signal 复查只在 shard lock
  外执行；并发 source notification 把它变为 `Notified`，arm transaction 按 shard 编号升序
  锁定全部 exact nodes 后才发布 SchedulingState。winner 必须同步删除所有 source/deadline/
  task nodes，禁止 lazy stale membership 或恢复全局 queue。claimed wake 必须把自身 wait ID
  原样交给 SchedulingState 校验；signal wake 不得重读 task 的当前 membership，否则延迟完成
  会消费下一代 signal wait。`sigsuspend` 的匹配 signal 产生 `Woken`，另一可投递 signal 经
  通用 cancellation 产生 `Interrupted`；两者都保留 pending bit 给 trap delivery 并返回 `EINTR`，
  只有 timeout/OOM completion 属于不变量破坏。
- clone/fork/vfork 的 TCB、graph node 与 RLIMIT snapshot storage 在 `process_creation` 外准备；
  最终 guard 内只捕获已预留逐 Thread snapshot、复检 limit 并提交 graph；
  UID scalar transition 共用 process_creation，count 到 publication 期间身份不变，
  不再使用 representative UID 乘 thread_count。snapshot backing OOM
  属于 memory failure 并由 syscall 映射为 `ENOMEM`；只有 RLIMIT_NPROC/PID exhaustion
  映射为 `EAGAIN`。Thread clone 的 Linux
  best-effort TID stores 完成前 child 保持 `New`；pre-activation group stop 使用
  `Stopped(New)`，SIGCONT 只能恢复 `New`，最终才在 graph owner 下进入 Ready/Stopped。
  若期间已提交 group-exit，任何 scheduler state 都必须继承 kernel SIGKILL consequence，
  不得逃离既有 group exit。
- clone/fork/vfork 必须在 publication 前预留 process、Thread、parent-child、creator-child、
  TID 与 group membership 的全部节点；owner guard 内一次无失败 commit。任一 OOM 只 Drop
  未发布 token/TCB，不得留下半条 parent、creator、TID 或 group edge。
- exit 顺序保持 robust cleanup、context retirement、fs/files 退休、移交旧 mm/preparation、
  graph removal、旧 mm release；`UserExitMemory` token 必须消费，注销后不得重读 live mm。
  clear-child-tid 仅在旧 root 还有其他映射 users 时零写并尝试一次 wake；registration 始终
  退休。put_user 失败不抑制 readable key 的 wake；key fault 独立容忍。key 在 prepared MM
  guard 内解析，guard 释放后才实际 registry/scheduler wake。实际页 fault OOM 不等于
  cleanup metadata 等待分配失败，后者已由 constructor preparation 排除。
  exec/vfork/group-exit 必须明确 point of no return。
- Thread exit 先从 global TID index 移除并把 creator dependents 无分配迁移给同组 reaper
  Thread；最后一个 Thread 退出再把 direct children 无分配迁移给 init。pdeath count 为零时
  不得访问 child Thread collection；session foreground 与 orphan-group consequence 只冻结
  exact group members，并始终按 SIGHUP 后 SIGCONT 在 graph lock 外投递。
- 最后一个 global init Thread 退出必须在 graph mutation 前 fail-stop（Linux child-reaper
  lifetime）；不得先删除 init TID，再向不存在的 replacement 迁移 creator-child edge。
- consequence 可能 drop Arc、OFD、waiter 或发送 signal 时，必须在 owner lock 外执行。
- outgoing consequence 不得在 context save 前发布；否则 remote CPU 可恢复仍在执行的 kernel
  stack。context switch 前只发布 per-CPU pending token，restore 后才完成 wait/signal/stop 的
  exactly-once transition。IRQ restore token 若观察到不同 logical CPU 必须在恢复中断前 fail-stop。
- exit staged 的 parent/init child waiter 必须按来源各自 exactly once drain；跨来源 TID 没有排序契约，不得为合并它们扩大通用 ordered-storage interface。
- `FallibleMap::take_entry` 返回的未发布 token 必须是 `height = 1`、无 child/next 的规范叶节点；
  任何 graph/wait batch 迁移重新提交该 token 前不得携带原树高度，否则 AVL 旋转会基于失真元数据。

后台 progress work 的 `KernelDeadlineWait` 在 composition root 创建线程前预留 Arc 与唯一
AVL deadline node；timer arm/expiry/重复 wait 不分配。active node 仍属于 TimerQueue 的
唯一 deadline index，不建立第二队列；value 只保存 Weak，handle 独占 strong lifetime。
expiry 锁内摘除，锁外先归还 node、再 TaskEvent signal；TaskEvent 沿既有 scheduler
membership/arming 握手阻塞，无 signal cancellation 或用户 ABI。handle drop 撤销尚未到期
node；已经摘除但无法 upgrade 的 orphan 走 Silent。state 的 IrqMutex 防止同 CPU IRQ 重入，
只允许 state→timer index 锁序，timer index 内不得锁 state 或进入调度器。

Thread mm 与 scheduling owner 的 CPU 决策：同一 SchedulingEntity Arc 保留退出 leader 的
proc 视图，避免重复 scalar snapshot 或保留整个 TCB；每 Thread 一次冷 allocation 在 shared
VMA mapping 前预备，热路径只增加一次 pointer load，不 clone Arc。生产 Sched/Entity 的
host fixture 比较旧 inline 与新 Arc policy lock、readonly proc view 和生产 Thread mmget/drop；
每项 warmup 10k、5×100k median，IRQ/topology 为 host seam，
仅诊断 CPU 成本，不外推 native 调度/设备延迟；既有 context-switch/trap 成本围栏继续生效。

Capability 热路径决策：复用credential短锁，无新增授权锁/分配；生产getter host warmup100k、
5×1M median，覆盖 UID、capability 与 write kill-mask（root FSETID快速拒绝clear）。
仅诊断锁/branch成本，不外推native runtime；metadata prepared等待复用已有TaskMutex协议。

SignalHandlers query 的 host warm CPU benchmark：100k warmup、5×1M 中位数，
路径含 group 与 table 两个生产锁。此锁是跨 Process disposition serialization 的必要
owner；delivery 不分配、不 clone Arc，exec/fork copy 只在冷 preparation 分配。不是 native guest 性能结论。

- `Process` 的只读 projection seam 只对 task 领域开放；ProcessNode/proc/prlimit 共享同一 Arc，
  不经任意 worker 或保留退役 TCB。`process_file_descriptors` 从实际 leader handle 获取列表，
  exit_files 后为合法空目录；只有 graph identity 不存在时 upper ProcSource 返回 NotFound。
  zombie 的 user/PID namespace 链接由 cred/PID 的实际 owner 提供；nsproxy 已退休的其他类型
  保持 NotFound，FSCREDS 仍检查真实 Process。comm snapshot 两次短锁之间预留 Vec backing，
  复查容量后复制；exec 把旧 comm 移出 guard 再析构，fork 不再带 spin guard 分配。
  nice 的 group selector 使用无分配 NiceProcesses 枚举从真实 members index 迭代，USER selector
  仅扫描 calling PID view 的进程，requested UID 按 calling user namespace 映射。
- `fallible_tree::Iter` 是既有只读 iter/iter_from/iter_after 返回类型的具名 crate re-export，
  仅供 task nice selector 的静态领域枚举组合；生命周期继续禁止 AVL mutation，节点非 owning
  successor 与原 iterator proof 相同，没有新索引、trait object、unsafe 或分配。

- TTY open 的 O_NOCTTY/O_ACCMODE 由实际 OpenRequest 投影；PTS 文件壳分配成功后才调用
  JobControl::open_controlling。自动取得与 TIOCSCTTY 共享 graph→Terminal/Process publication，
  只在 session leader 无 TTY、目标无 owner 且 readable/global CAP_SYS_ADMIN 时取得。
  TIOCGPGRP/TIOCGSID 比较 caller 的实际 TTY，PTY master 查询豁免此比较；Terminal 在同一锁内
  投影 actual SID/PGID 到 caller PID namespace，无前台为 0，无 SID 为 ENOTTY。
  TIOCSPGRP 在 graph 锁内校验真实 TTY、SID 和 group membership，零/未知 PGID 为 ESRCH。
  TIOCNOTTY 对普通成员仅清自己的 Process handle，对 leader 释放控制并清同 session handles，
  HUP/CONT 在 graph guard 外向实际 foreground identity 投递。
  benchmark 决策：查询只增一次短 Terminal lock；自动取得/detach 不分配、沿实际 group members
  遍历，无第二份 membership 或映射。纳入集中 guest namespace/PTY 验证与已有 TTY fence。

- PTY master hangup 通过 JobControl::hangup_terminal 先持 graph，再原子摘下 Terminal control
  与清除 old session 的 Process handles。不得先清 Terminal 再另取 graph，否则并发重新
  TIOCSCTTY 可能被旧 hangup 清除。signal 与旧 PID pins 退休均在 graph guard 外。

- `Thread.syscall_restart` 的同一 Mutex 持 `SyscallRestartState`，唯一拥有原 syscall PC/number/arguments、typed ERESTARTSYS/ERESTARTNOHAND/ERESTART_RESTARTBLOCK policy 与已导入的 Futex/Sleep semantic block。dispatcher 只向 trap 输出 policy，内部 sentinel 不向
  用户泄漏。STOP 保留 instruction/block 与 sigsuspend mask，继续运行后的最终信号选择才消费 instruction；捕获 handler 只在 ERESTARTSYS+SA_RESTART 时重放，计时 restart-block 始终 EINTR。无 handler 时 restart-block 改为标准编号 128 并使用
  原 PC，实际 syscall 消费冻结 address/value/bitset/private 与 host deadline，不重新读取用户 timeout 或增加相对 duration。relative nanosleep/clock_nanosleep 共用相同 block，absolute clock_nanosleep 沿 ERESTARTNOHAND 重放原绝对请求。rt_sigreturn、
  exec 与默认终止清理实际 owner，fork/clone 初始化空记录；没有新锁、queue、allocation 或 protocol shadow。规范为固定 Linux futex_wait_restart、hrtimer_nanosleep 与 RISC-V signal codec。host 直接加载生产记录；guest 覆盖 SA_RESTART 的 timed
  EINTR、修改 stopped waiter 的用户 timeout、stop 超期及 stop→捕获信号；既有 200ns benchmark 覆盖实际同锁 block 发布/resolve/消费成本，随统一 host/runtime 门禁执行。

- `TaskControlBlock::tid_identity` 的 crate projection 仅扩展给 IPC syscall 预备真实阻塞
  receiver：Linux pipelined_send 的 lrpid 是 task_pid，普通 msgrcv 则是 task_tgid。
  仍返回实际 immutable PID owner；不在 syscall 缓存编号或增加 PID registration。

- TaskEvent 的 TaskWaitKey 携带本次 optional absolute monotonic deadline；timed event 在唯一
  WaitRegistry 注册 actual TaskEvent source、Task index 和 Deadline index，复用原有
  prepare/publish/arm/claim 握手。IndexedWaitKind::TaskEvent 只保存同一 key，不复制事件完成值。
  completion 先于 publish 由 final recheck 消费，先于 finish_arming 则沿 exact source claim
  唤醒；expiry/event/signal 只 claim 一次并摘除全部索引，迟到 callback 不命中下一 ticket。
  TaskEvent 的 cancellation 在其 waiter guard 内清 completion，timeout/OOM 不消费将来的
  pending event。无 deadline 不登记 timer node；PONR mutex/event 的无分配保证仍适用。
  标准 semtimedop 共用 actual SemaphoreOperation，timeout copy 先于 nsops，范围校验在
  ops copy 后，巨大有效 duration 按 ktime 饱和；成功操作先于 timeout检查，超时为 EAGAIN。
  benchmark 决策：每次真实 timed wait 使用 existing registry 的三个 source nodes；
  TaskWaitKey 增加 optional deadline metadata，无新增全局时钟或轮询队列。纳入最终等待
  注册、semaphore syscall 与调度成本围栏，host 时钟/调度 adapter 仅提供输入环境。

- `ProcessGraph.kernel_threads` 是实际无用户 Process 的 TCB admission index；唯一 PID registration
  仍由 KernelThread 持有。kernel publication 在同一 graph/PID 事务发布预留 node，真实 exit 先
  摘 node 并归还 PID，node/observer 析构不持 graph guard。系统 task 总数直接取两个执行 index
  的 len，禁止手动 counter 镜像；累计 processes 按 Linux total_forks 包括 thread/kernel creation。
  proc 精确 lookup/getdents/snapshot 同样读取此 actual kernel index；只 pin 请求的 kernel TCB，
  System 不分配或保活任何执行体。boot KernelThread 拥有真实 immutable root Credentials，
  user namespace/capability/proc owner 从同一对象投影，不创建用户 Process、mm 或 file table。
  actual TaskKind 产生 PF_KTHREAD/Kthread；comm/scheduler runtime 取真实执行体，PGID/SID 取
  boot creator 的唯一 idle PID0，缺失 mm/files/fs context 的 cmdline/statm/fd/exe/root/cwd
  为空。native TASK_IDLE 输出 I，真实不可中断 wait 输出 D；不以 absence of mm 推断 kind。
  execution_by_tid 仅为 proc 区分实际 user/kernel admission，不改变用户 Process graph。
  kill/tkill/tgkill 对实际 kernel index 执行同一 root credential/DAC 与 caller PID view 检查；
  boot workers 使用固定 Linux kthreadd ignore_signals 继承语义，不创建用户 pending、stop/exit
  state。kill(-1) 合并两个 actual ordered indexes，逐执行体成功/拒绝由原 SelectionResult 聚合；
  精确 lookup 仍 O(log N)。user wait 只选择其实际 children，kernel worker 由 boot idle 创建，
  没有用户 child relation，因此 waitpid 返回 ECHILD；真实主体退出仍消费同一 kernel admission。


- load average 采样实际 runnable admissions 加不可中断 Blocked admissions；后者由
  SchedulingState 独占的不可复制 BlockedLoad token 注册/自动归还，无 TCB 扫描或独立 wait-state
  镜像。只有 context 已完成切出才注册，early wake 不注册；跨 CPU wake、stop、exit 及最终
  owner 退休都消费同一 token。DriverIo/vfork/uninterruptible/killable wait 计 load，
  interruptible wait 与 kernel daemon 的 TASK_IDLE 不计；mount retirement、Unix collector 与 KernelDeadlineWait 的 daemon
  空闲等待显式选择 wait_idle，仍复用同一 event handshake。proc thread 的 D-state 来自同一
  run/wait owner 快照。新增 blocked register/retire 和 aggregate sample 的真实 CPU 测量
  blocked-load-bench 采用既有 200 ns policy，随最后的统一验证执行。

- Timer mutation executor 始终独占 prepared storage；final graph→timer commit 只借用 `&mut`，
  成功未使用的 placeholder、失败与 stale-plan retry 都在 guards 释放后退休/复用。实际已摘
  ITIMER_REAL/POSIX/timerfd/deadline AVL nodes 移入 RetiredTimers，exit/exec/delete/disarm/
  replace 的 final Drop 均在 graph、timer 与 timerfd counter guard 外；不先 `remove` 再
  只退休 value，否则 node allocation 已在锁内被释放。expiry 的 TimerExpirationRetirement
  仅持固定一份 deadline 和 optional dead timerfd node，不把四棵空树复制进 bounded batch。
  dead Weak kernel timer expiry 与 KernelTimer Drop 同样摘下实际 entry 后锁外退休。periodic
  expiry 重用原 node，one-shot/silent expiry 移交同一 owned entry，无分配 cleanup snapshot。
  新 borrowed-executor producer fixture 验证 unused node success、same-node retry、lifecycle
  failure 和 owned output；timerfd/KernelDeadlineWait fixtures 沿实际 metadata/node 路径覆盖。
  POSIX 的 PosixSchedule 是 Disarmed/Armed/Passive/Notification 的实际 owner；暂停通知
  直接拥有 deadline entry，expiry 不重新分配/周期重发。SIGEV_NONE 只按 clock lazy 投影，
  不插入 active index。posix_timer_lifetime fixture 原样加载 schedule/preparation/pending owner，
  覆盖长期阻塞 overrun、真实 node reuse、旧 sequence、ID 复用、收费及 ignored 恢复；外层
  graph/creator/clock 装配是显式输入 seam，完整协调由 musl guest 覆盖。

- `model::signal_dequeue` 是 sigwait/signalfd 的唯一 private/shared 消费 seam，供 task 协调层调用；128-byte 输出 codec 属于 signal_info。record/配额退休与 POSIX timer 重装在 shared/private guards 外执行；失效记录不能终止队列扫描。
- `SignalHandlers.wait_source` 拥有 file::SignalWaitSource Arc；CLONE_SIGHAND 共享源，不共享 Process pending。source 只保存全局可比较 generation，身份由实际 Arc 地址提供，epoll/wait snapshot pin 防止地址重用；不保存 Task、队列、mask 或
  disposition。signal publisher 在 Graph/pending guard 外推进 source、精确通知 epoll index，并从实际 SignalSource shard 唤醒直接 poll/read。进程通知只推进一次该源；mask 更新仅唤醒当前 sighand，与 Linux do_signalfd4 一致。
- FileReadinessContext 为静态泛型 seam；Task 读取实际 private/shared owner 并投影当前 sighand，file 不读取 current Task。ReadinessSource/PollWaitKey 新增 Signal identity，direct wait 的 guard pin 实际 Arc/generation，registration 与 final recheck 仍由唯
  一 wait registry 完成。deferred epoll callback 没有用户 reader，只保守标记候选，由实际 epoll_wait caller 复查；Signalfd source 在 ADD 固定，MOD/notify 不重绑，保持原生 fork caveat。
- benchmark 复用 realtime-signals-bench 的现有 2M 次/5 样本/200ns 阈值，新增实际 mask 与 native codec；guest anonymous-files 覆盖 dup、readv、copyout fault、reader identity、继承 epoll 和 ET。

PidNamespace::lookup_identity 向 SCM/pidfd 等 ABI caller 提供实际 PID observer；不是 Task 或 graph registration capability。Namespace association 的 Weak 与 immutable key 在同一 publication 生成，没有另建 PID identity registry；只存 Weak，避免 Namespace→
PidIdentity→Namespace cycle，map 摘除仍随最后 graph link。lookup 在 namespace guard 下 upgrade，同一 Arc 在 guard 外转交/退休；复用同号不改写旧对象。初始 PID1 node 先预留，PreparedPid::initial 完成真实对象和第一条 registration 后填充
它。Native Linux v7.1 kernel/pid.c 的 idr_replace/find_get_pid 为对照；actual host 覆盖 ancestor view、未发表不可见、仅 PGID/SID link 存活、lookup 零分配、最后 link 归还与旧 observer/同号新对象区分。新增 Weak/Arc 为冷构造与 observer 路径
，普通 key lookup 保持同一锁和索引查询。

Task::message_identity 从一个真实 credential guard 冻结 NamespaceAuthority 与 real/effective/saved UID/GID 两组三元组，kernel executor 投影其 immutable root credentials。MessageIdentity 只属于当前调用的不可变输入，constructor 限于 model owner；不会
创建 live credential mirror。SO_PEERCRED 已使用该同一 snapshot 的 effective IDs；SCM_CREDENTIALS 的 native ucred codec/映射/权限/PID observer preparation 已落源码与 host 回归，parse/transport/SCM 接收输出使用同一原生 codec 与 owner。
PID override 仅在 active PID namespace owning UserNS 的 SYS_ADMIN 权限下允许；UID/GID 接受 real/effective/saved，或 sender 当前 UserNS 的 SETUID/SETGID，缺失映射先于权限错误。源码、围栏与回归使用同一 ABI owner。

Futex source transaction：WaitRegistry 的同一 source hash 独占 task-context operation guards，不保存第二份 queue/value/readiness。WAIT 在 metadata preparation 后取得 source guard，发布 Arming 并复查 MM value/key，再归还
guard 后 arm；WAKE/REQUEUE/WAKE_OP 取得一或两个 source 的升序 guards，MM 二次确认 key 变化时完整重试。普通 requeue 不读取 private source，CMP_REQUEUE 才以 actual aligned load 比较；相同 key 仍统计实际候选 waiter。REQUEUEING/CLAIMING 在
prepare_arm 中等待结束，不触发 panic。所有 queue node 迁移/claimed staging 无分配，scheduler handoff 在 MM 和 operation guards 归还后进行。原各 registration 的 exact key list/源 shard/deadline/bitset 仍为唯一 owner，signal/timeout cancel 继续使用
同一 CAS phase。

mandatory robust/clear-child-tid wake 使用 Thread 已预置的同一 TaskMutexWaitPreparation，先解析旧 MM key、归还 MM guard，再复用该 preparation 取得 operation guard；退出不新增 fallible allocation。WAKE_OP 在实际 U|W word 上 RMW，再按返回的旧
signed 值决定第二队列；unsupported comparison 在更新之后返回 ENOSYS，unsupported operation 不更新。user page/word 的 unsafe 归 memory，raw FUTEX_OP codec 归 syscall，task 不重建 mapping identity。新 TaskMutex guards 可跨 MM fault-in 的睡眠边界；
任何 registry spin/IRQ guard 不跨 user access 或调度。

benchmark 决策：增加 actual 一/双 source empty-wake transaction，复用既有 2M 次、5 样本和 200ns 门禁；host fixture 直接载入生产 WaitRegistry、node requeue、TaskMutex 与 atomic leaf，仅 MM key、scheduler handoff/本地 IRQ 为边界。guest 覆盖不同/
同 key 双队列、false comparison、legacy zero wake、共享页并发 RMW、只读 fault、符号 operand/shift/unsupported 错误优先级。源码围栏与回归已编写；PI/PI requeue 与完整 timed restart-block 暂不扩展；不支持的操作明确返回标准错误。

robust-list 清理按原生 link bit 0 解码 PI 属性并剥离真实 node 地址，pending 对比使用剥离地址，下一 link 在实际 MM guard 内预取。word 初始读取使用 actual aligned AtomicU32 load，owner-death CAS 共用同一个已准备的 UserAtomicU32 token；
PI tag 不能消费普通 FUTEX_WAKE 队列，普通 pending-unlock 的 owner 0 只执行 wake、不伪造 OWNER_DIED。guest 用实际共享用户链表的 PI-tag node 后接普通 node，并等待普通实际 futex queue，覆盖遍历不被 tag 截断。PI state handoff 尚未实
现。

clone/wait relation 新源码：ProcessNode.exit_signal 属于实际 group-leader completion，parent_exec_generation 冻结 birth relation 的 real parent epoch；exec_generation 是同一 Process 的实际 exec 次数，非零同时禁止 parent setpgid，不保留旧
has_execed 镜像。CLONE_PARENT 在唯一 graph publication 中继承 caller 的 real parent/parent_thread/exit_signal/parent epoch；资源、安全、terminal、SID/PGID 仍取 caller，namespace init 不能创建 sibling。自身 exec 与跨 Process adoption 重置 SIGCHLD；父
exec 只影响 notification signal，__WCLONE 的分类仍读取原 exit_signal。PreparedChildNotifications 在 graph 外准备实际 recipient+signal 的独立收费节点，最终同时复核 relation 与 signal；zero 不准备/发送，普通 SIGCHLD policy 不决定 clone child
autoreap。ChildWaitFilter 在实际 child relation 上选择 Ordinary/Clone/All 和 calling creator，忽略不匹配者时不计作可等待 child；唯一 claim/copyout/reap transaction 不变。namespace init shutdown 使用 All 回收原生全部 child。bench 决策：冷
clone/exit 与既有 wait selection 增加 scalar 比较，无新增锁/分配/缓存；沿用 process graph owner 门禁，actual host pending/exec owner、native guest class/creator/PARENT/exec/orphan 回归已补源码。


KernelThread 的 actual nsproxy 源码已统一：boot constructor 与初始用户 Thread
共用 initial_membership 的 concrete owner 捕获，KernelThread.namespaces 是唯一可退休
Mutex<Option<NamespaceMembership>>，不创建用户 Process/MM/fs/files。proc/PIDFD 的
with_proc_namespaces 只在实际 execution variant 的资源锁下提升被选 namespace Arc，
不查询 initial owner 代替 target；真正退出在 guard 外释放 namespace tuple，随后退休 PID。
PID/user proc links仍由其真实 PID/cred对象提供；PIDFD namespace ioctl 必须在同一 membership
transaction 中确认 nsproxy存活并捕获具体引用，不能检查后再次查询已退休 target。
新增 Mutex 属冷 constructor/proc observer/exit 路径，没有 scheduler/trap 热路径成本；
production KernelThread 退出与历史 observer 回归及拒绝不退休/伪造initial的围栏纳入集中验证。
