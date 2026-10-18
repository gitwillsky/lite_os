# 进程与调度当前架构

## 当前设计

- Process 拥有 leader mm 的只读 handle alias、limits、执行映像与聚合 runtime；Thread 拥有独立 credentials、可共享的 fs context/file table、执行上下文、mask、pending signal、TLS 与调度 membership。
  raw UID/GID/groups syscall 只修改 calling Thread；musl 的线程同步 wrapper 通过标准 signal
  分别更新各 Thread。immutable sorted groups snapshot 不分配，permission 使用二分查询。
  同一 credentials owner 持有 FS UID/GID；pathname 权限使用 FS IDs，faccessat 使用 real IDs，
  proc status 第四列投影 FS IDs。setfsid 始终返回旧值，拒绝不改变身份；set[res]id 成功恢复 FS=E。
  每个实际 MM 独占 dumpability：VM 共享、fork 复制，prctl 只允许 0/1。身份提交在
  creation→graph→credentials 下先禁用 dumpability、清 calling Thread PDEATH，再发布凭据；
  proc 权限先捕获目标凭据再读实际 MM，并检查 caller FS IDs、目标 P⊆caller E 或 SYS_PTRACE。
  exec 新 MM 初始禁用，按旧 R/E identity 和实际 ELF/PT_INTERP 可读性设置 provisional 值，
  再由 credential security effects 决定是否禁用；不得以最终 IDs 再启用 dumpability。
  Process 保留原 leader credential owner 的只读 lifetime alias，leader 退出后 `/proc/PID`
  与 process-directed authorization 不改读任意 sibling；`/proc/PID/task/TID` 投影实际 Thread。
  Linux no_new_privs 由 calling Thread 独占，单向启用并沿 clone/fork/vfork/exec 继承；set-ID
  exec 通过 PreparedExecCredentials 在建栈前确定最终 IDs/capabilities/AT_SECURE/PDEATH，
  PONR只提交同一结果。Capabilities同credentials锁唯一持有；capget支持leader/zombie至reap，
  capset/prctl只改caller，generic authorization按具名cap与target sets判断，不以UID0直接旁路。
- namespace membership 的 outer handle 属于各 Thread，clone 共享 concrete namespace objects、
  exit 显式退休；Process 只读别名原 leader 的 handle，非 leader exec retarget，不能读任意 worker。
  User namespace 只属于同一 Credentials owner；UTS name tuple 属于 UtsNamespace，系统构建字段
  immutable。UID/GID maps 在 namespace owner 只发布一次，所有分配和排序先于短锁 publication；
  target namespace 的 capability 必须沿 immutable ancestry 授权，不把 child pE 用于全局设备/swap/reboot。
- TimeNamespace 统一拥有 monotonic/boottime offset 与进入后的冻结状态；每个 Thread 的 active
  time 和 time_for_children 归同一个 FsNamespaceState。unshare 只改后者，普通 fork/exec 进入它，
  共享 VM 的 clone 保留 active；setns 一次替换两者。clock_gettime、absolute sleep/timer、
  proc uptime/starttime 与 sysinfo 在操作边界投影 offset，host clocks/deadline queue 保持唯一 owner。
  原始 timer value 零才解除定时，已过期的非零 namespace timestamp 仍立即到期。
- 同一个 Thread security owner 持有不可撤销的 seccomp strict/filter policy；程序在发布前完整
  验证，immutable flat set 避免长链退出时递归析构。disabled syscall 使用原子 fast path，
  有 filter 时只在取得 immutable set snapshot 时短锁；不在锁内执行解释器或 fatal consequence。
  TSYNC、单线程安装和 clone policy inheritance 由同一 process graph 提交；全部校验后无失败发布，
  排除已开始退出的线程，NNP 单调合并，allocation 与旧 program final Drop 均在锁外。
- fs context 独占 root/cwd/umask，Thread clone 按 CLONE_FS 共享或复制，unshare(CLONE_FS)
  仅脱离 caller；独立 Process clone 按 CLONE_FS 选择共享/复制，exec 保留。执行映像 opened-entry 由 Process 独占，
  不随 fs context 脱离改变；旧 cwd/executable identity 在 owner 锁外退休。
- FileTableOwner 的 TaskMutex 唯一保护 slot、CLOEXEC 与 SCM_RIGHTS publication。
  Thread 的独立 handle 按 CLONE_FILES 共享或复制；退出显式摘下 handle，Process 只别名保活
  leader handle，leader 退出后 procFD 不改读 sibling。每个 Thread 预置复用 waiter，
  查找与退出不在争用时临时分配；表内 allocation 不持 spin guard。
  unshare 的 fs/files 全部准备后才提交；exec 独占表保留 identity，共享表先复制再 CLOEXEC。
- SchedulingState 是 runnable/blocking/stopped membership 的唯一事实；Ready transition token 在同一 lock lifetime 内更新 per-CPU runqueue projection。
- 每个 Thread 独立持有 Live/Retired mm membership，Process 只别名原 leader handle；exit 移交 root 后
  TCB observer 不保活映射。root Arc 是 mmget user，Weak registries 不保活 VMA；最后一个
  user release 不写 clearTid，多个 users 时即使零写失败仍尝试 futex wake。所有 context、
  robust 与 clearTid MM 等待复用 constructor 预留 preparation，排除提交后的 waiter OOM。
  Retired 只保存原生 user_dumpable；缺失 MM 不能绕过 ptrace 授权，nondumpable 退休目标需 initial
  User namespace 的 SYS_PTRACE。get_robust_list 对 zombie/退出 leader 返回已清除的零 registration，
  查询时 pin 原 leader 的实际 credential/MM handles，随后 exec 重指 aliases 不改变已选目标。
  原 leader 退出后 Process proc state 为 Z、memory/FDSize 零；live worker 的 proc 使用自身
  mm/files，页统计只扫描 common mm 一次。原 leader scheduling Arc 只供 proc 读取，不留 TCB。
  唯一 survivor exec 在同一 graph transaction 取得 TGID、迁移 creator-child 索引且不生成
  父死亡事件，沿用原 leader birthdate；robust 先按旧 TID 清理，旧 clearTid 只在旧 mm 释放。
- `ProcessorTopology` 拥有 per-CPU current、runqueue、mailbox 与 load projection。远端 runnable 只经 logical target mailbox 和 platform IPI 交付。
- 任务被抢占后重新入队时，按各 CPU 的 ready+running 计数选核，负载并列时保留 last-CPU 亲和。为避免 N+1 个 CPU 密集任务在
  N 个 CPU 上被永久锁成 100% / 50% / 50%，每个 CPU 每发生 10 次抢占（约 100 ms），有一次在本 CPU 上还有别的可运行任务时
  放宽该亲和偏置，并列时轮转到另一个 CPU。被迁移的总是刚跑完一个时间片的任务。纯决策在 `scheduler::placement_policy`，
  `scheduler-unit` 单测；多 CPU 公平性由 `verify-runtime-scheduler` 裁决。
- 普通 yield/block 的 scheduler handoff 直接在 outgoing task 上选择 next Ready owner，并执行一次
  `task -> task` kernel context switch。被保存的 outgoing owner 与 IRQ restore consequence 暂存在
  per-CPU pending slot，由 next task 的 continuation 唯一提交；next 首次运行也先经过同一
  scheduler trampoline。idle 只在没有 runnable successor 且 outgoing 确实 Blocking/Stopped 时进入。
- runqueue 为空但 outgoing 仍为可继续的 Preempting/WakePending 时，scheduler 原地恢复
  Running/current，context switch 数为零。生产成本门禁以 1024 次 runnable handoff 约束
  kernel context switch 从旧双跳的 2048 次降为 1024 次，idle entry 从 1024 降为 0。
- scheduler idle 的 local IRQ guard 覆盖 deferred/mailbox/runqueue 复查与 guarded WFI；架构
  assembly 只为 WFI 临时开中断并在返回前再次关闭，trap 用精确 WFI/resume PC 关闭一次性
  wake edge 窗口，不依赖下一个周期 tick 兜底。
- CPU0 持续提供 100Hz housekeeping/liveness tick；global timer queue 仍由 ProcessTable 唯一拥有。
  其他 CPU 进入 idle 时屏蔽本地 tick，由 IPI/device edge 唤醒，选中 task 后在 context switch
  前恢复新 deadline。空闲成本因此不随
  vCPU 数乘以 tick 频率增长，busy CPU 仍保留固定时间片抢占。
- ProcessTable process graph 拥有 PID/TID、parent/child、creator Thread、process group/session、
  wait event、timer index 与 process lifecycle transaction；内部维护 direct-child、global TID、
  creator-dependent 与 `(SID,PGID)` exact-membership indexes，使 exit/wait/signal lookup 只触达
  受影响集合。
- `WaitRegistry` 统一拥有 futex、deadline、pipe、poll、signal 和 socket wait registration；
  16 个 source shard 允许无共同 source 的 publication/wake 并行。multi-source wait 仍只有一个
  registration，`Arming/Notified/Armed/Claimed` 状态封闭锁外 readiness 复查与 exactly-once
  completion；发布 membership 前完成全部 fallible allocation。所有 claimed completion（包括
  signal）携带原 registration ID 进入 SchedulingState，延迟的旧代 wake 只能返回 stale。
  `sigsuspend` 同时接受匹配 signal 的 `Woken` 与通用可投递 signal cancellation 的
  `Interrupted`，pending signal 仍由 trap-return 唯一消费。
- process graph 与 wait batch 通过 `FallibleMap::take_entry/commit_vacant` 无分配迁移节点；
  取出的 token 会归一为无链接、叶高度节点，再由目标树重新计算 AVL 高度。
- disposition 表由 SignalHandlers 唯一拥有，Process 持可退休共享 handle；CLONE_SIGHAND
  跨 Process 共享表，各自 pending/mask 独立。exec 只预分配目标表、提交时复制最新共享表并清 caught handler 与所有
  flags/mask/restorer；最后 Thread exit 显式退休。忽略 transition 清同组 pending，one-shot
  只在 locked selection 清 handler，用户 frame 写入不持 disposition lock。blocked ignored
  arrival 可供 sigwait；SIGCHLD 显式忽略/NOCLDWAIT 自动回收，NOCLDSTOP 保留 wait event。
  wait 与自动回收共享 graph retirement，并清 staged consequence 链，最终资源锁外析构。
  active wait claim 延后已决定的自动回收到 copyout 成功/失败事务结束；SIGCHLD generation
  使用 child 的 creator/reaper mask，pending 在
  parent disposition 同锁事务发布。vfork completion 只属于原始 child Thread，退出先移出
  token、旧 MM 清理后 wake，不受 Process autoreap 或 sibling exit 影响。
- signal generation、pending、delivery 与 syscall replay 分层但不复制状态；AArch64 live
  FP/NEON image 只在 task switch、signal capture/restore、clone inheritance 与 exec reset
  的固定边界转移，普通 trap 不复制 q0-q31。exit、exec、vfork、robust-list 和 group-exit
  均有明确 point of no return 与清理顺序。
  Thread 退出在 graph 注销前退休 user context 并移交旧 AddressSpace，随后 mm release
  使用旧 root；最后一个 sibling exec 改换自身 mm handle 不影响退出 token。

- 内核线程与用户 Thread 共用调度器、kernel context 切换、DriverIo/`TaskMutex`/`TaskEvent` 阻塞与
  deadline 睡眠；它们没有 Process，不出现在 process graph。主体返回即终止：关本地中断、撤销调度
  ownership、交给 deferred reap 后切到 idle。boot 装配 memory-reclaim 与 unix-rights-gc 两个后台
  worker；空闲 deadline/event 使用 TASK_IDLE，不计入 load。

## Known limits

- scheduler 当前提供 Linux `SCHED_OTHER`/nice 语义子集，不包含实时调度 class。
- 负载均衡只有抢占时的重新选核：没有跨 CPU 拉取或推送 Ready 任务，空闲 CPU 不会主动从别的 runqueue 取任务。
  Ready 任务只有在所在 CPU 的 current 被抢占或阻塞时才有机会迁走，内核态长时间不返回用户态的 current 会延迟这一点。
  迁移不重新归一化 vruntime，沿用既有的跨核放置语义；公平性只验证了等权重的 CPU 密集任务，nice 混合负载未验证。
- futex PI、PI requeue 与完整 clone flags 尚未开放；WAKE_OP 使用实际原子更新与双 source transaction。
- 实时信号已接入实际 FIFO/recipient rlimit token、标准 rt_sigqueueinfo/tgsigqueueinfo；
  handler 与 sigwait 共享唯一 pending owner；普通也用实际 charged record，SIGKILL/低内存
  bit fallback 不保留第二份 payload 表，SIGCHLD 在 graph 外准备并复查 creator/reaper。
  POSIX timer 创建时预留同一实际 signal record/原 creator quota，
  周期通知在消费后重装并更新 overrun；重设/删除按实际 Arc 与 sequence 拒绝旧通知。
  ignored 周期通知停放到 disposition 恢复，exec 清旧 SI_TIMER，SIGEV_NONE 不进入 active index。
  CPU clocks timer 及其他 notification mode 未开放。

- 系统任务总数由实际 user TID index 与 kernel execution index 的长度之和投影，不计 idle、
  不扫描 TCB、不维护手动增减镜像。kernel TCB node 在 graph guard 外准备，与实际 PID 同次
  publication；主体返回后先摘 execution node 并归还 PID，再退休 scheduler current，历史
  observer 不延长系统 task admission。`/proc/stat` processes 的实际 fork 累计也包含 kernel
  creation 与 `CLONE_THREAD`，只在成功 publication 时增加。kernel proc/kill 投影读取实际
  execution index 与 immutable root credentials；boot workers 忽略信号，无用户 wait child relation。
  proc flags/Kthread 来自 TaskKind，空 mm/files 与 idle state 来自实际资源/wait owner。

- load average 采样实际 runnable admissions 加不可中断 Blocked admissions；后者由
  SchedulingState 独占的不可复制 BlockedLoad token 注册/自动归还，无 TCB 扫描或独立 wait-state
  镜像。只有 context 已完成切出才注册，early wake 不注册；跨 CPU wake、stop、exit 及最终
  owner 退休都消费同一 token。DriverIo/vfork/uninterruptible/killable wait 计 load，
  interruptible wait 与 kernel daemon 的 TASK_IDLE 不计；mount retirement 与 Unix collector
  空闲等待显式选择 wait_idle，仍复用同一 event handshake。proc thread 的 D-state 来自同一
  run/wait owner 快照。新增 blocked register/retire 和 aggregate sample 的真实 CPU 测量
  blocked-load-bench 采用既有 200 ns policy，随最后的统一验证执行。

`signalfd4` 的 OFD 只保存原生消费 mask，dup/fork/SCM_RIGHTS 共享同一 AtomicU64。read/readv 通过 `Task::read_signal_notification` 消费实际 reader 的 private/shared 队列，和 sigwait 使用同一 `signal_dequeue`；已失效 timer 记录继续跳过，周期
timer 在实际 dequeue 时重装。输出由 signal_info owner 编码标准 128-byte signalfd_siginfo，copyout fault 消费当前信号，只报告此前完整记录。sighand 的 SignalWaitSource 只有稳定身份与 generation，不复制 pending。实际 owner 与 native guest 回归随统一
门禁执行。

PID namespace 的实际 numbered association 同时保留 immutable PidKey 索引投影与同一 PidIdentity 的 Weak。lookup_identity 可直接取得 native PID observer，包括仅由 PGID/SID links 持有的编号，无需查找 live TCB 或扫描 graph；association 仍只由实
际 graph registration 控制占用，observer 不延迟编号回收。PreparedPid publication 向每个已预留 node 提交自己的实际 identity，并验证相同 ladder/key；boot PID1 在完整构造后无分配填充预留 node。
