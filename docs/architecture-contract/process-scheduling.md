# 进程与调度契约

- get_robust_list 的跨 Thread 查询先在 calling PID namespace 取得实际 live Thread Arc，再于 graph 外
  取得目标已有 Process credential_guard 的 killable guard，与 exec 的准备/commit 串行化。
  同组免检；跨组按 REAL UID/GID、permitted capability 子集、目标 mm dumpability/owner namespace
  授权。Filesystem 与 Real 是同一 credential policy 的输入口径，不复制授权实现。
  guard 内只读取 Thread 的真实 robust registration；copyout 在所有目标 guards 外执行，先 len 后 head。
  getter 不访问目标用户页，不持有旧 mm，也不发布新的 robust-list registry。
  查找也覆盖 initial PID namespace 的 kernel executor 和未 reap 的已退出 leader。kernel 凭据不可变、
  不执行 exec，授权后返回零 head。已退出 leader 的 query 在 graph 选择时 pin 原来的实际 credential
  inner handle 与 MM outer handle；exec 重指 Process aliases 不得让本次查询换目标。
  RetiredThreadInspection 只拥有这些真实 scalar handles 与 Process guard，不保活 TCB/mm/root，不复制 head。
  退出时同一 MmMembership 冻结 native user_dumpable 后按值移交 root；nondumpable 的 Retired 目标
  必须具备 initial UserNS 的 SYS_PTRACE，不能因为 MM 不存在而直接允许读取。
  benchmark 决策：新授权/退出/exec 为冷路径；mm_snapshot 保持同一 handle lock、一个 variant 检查
  与既有 Arc clone，没有新增热路径锁/atomic/分配，沿用已有 MM/credential 门禁，不增加宽阈值。

- `namespace` lower owner 不读取 Task/current，也不持用户地址：UserNamespace 拥有 ancestry/maps/
  setgroups，UtsNamespace 拥有 hostname/domainname，TimeNamespace 拥有 offsets/frozen tuple。mappings/names spin guard 只 clone immutable
  map引用、拷贝固定tuple或提交已准备 scalar/Arc，禁止分配/user-copy/credential回调；锁序 credential→map。
  Task namespace handle 只拥有 membership；credential namespace 不复制进入 membership tuple。
  membership 的原leaderalias与FS/files/MM同样在exit退休、nonleader exec重指；最终Drop锁外。

## Owner

- PIDFD binding 只持 immutable PidIdentity/source，不 pin TCB/MM/files。ProcessNode 与非 leader
  ThreadIndex 各以 Weak 指向本实际 PID 的 lazy readiness source；inode admission 决定并发打开的
 唯一 source，graph final recheck 后才发表。ThreadIndex 的 retiring_task 是同一 execution 的
  cleanup 移交，不是 running cache；actual MM/clearTid/vfork 清理完成后才退休 PID association。
  Process Retiring 不可 wait/poll；唯一 ThreadRetirement 的 outstanding 归零后发表 waitable exit、
  parent signal/waiter/autoreap。reaper 选择排除已领取 exit_in_progress 的 sibling。
  pidfs 的历史 exit record 在 association retirement 写一次，reap 后由 observer 保留；真实 poll
  每次查询 graph，不读取历史记录推断 alive。所有 source/TCB/registration 最终 Drop 在 graph 外。
  clone PIDFD 先复制/共享 child files，再准备原 caller fd reservation；严格 fd copyout 成功才发表
  child/PID/source，随后无分配发表 OFD 并 enqueue，失败不留 child 或可见 fd。
  getfd 取得目标已有 exec credential_guard 的 killable guard，以 REALCREDS 授权先于 PF_EXITING
  检查/文件查找；返回同一 OFD 的 CLOEXEC descriptor。native waitid 复用 child claim，P_PIDFD
  直接使用实际 PID identity；正常消费先于 user-copy，WNOWAIT 只释放观察 claim。
  rusage 与 cgroup namespace 未开放；NET 声明范围见 socket 契约。

- Process 的 leader_signal_mask 只别名原 Thread 的同一 Arc<Mutex<u64>>；zombie leader 的 PID
  group generation 不借 worker mask/UID/UserNS。exec 重指 survivor，原 scalar pin 在锁外释放；
  不 pin TCB、不缓存 mask 值。普通 PID/PGID 信号依原 leader 收费/投影；Thread PIDFD 的 group
  override 使用被引用 Thread 的实际 mask/credentials。锁外 preparation 后重新核对当前 PID
  association；exec promotion 换目标时重新准备并退回旧收费，不凭旧 TCB Arc 推断仍关联。
  shared pending 仍由唯一 signal_state 拥有；新 alias 不创建第二个 pending/generation owner。
  benchmark 决策：clone/exec alias allocation 为冷路径，集中验证还需测实际 group generation
  的 alias acquisition 与原 canonical Thread generation，不能以源码回归代替性能结果。

- UserNamespace 独占标准 ucounts limits 与 per-kuid 非零 counters；User/UTS/Mount/Time/PID/IPC 新 owner 逐级
  收费 current creator 及每个 ancestor owner，token 随实际 namespace final Drop 归还。任一
  ancestor/entry allocation 拒绝回滚已收费 prefix，返回 ENOSPC；对象分配失败返回 ENOMEM。
  初始 limits 从 composition root 的 managed RAM 与实际 stack bytes 按固定 fork_init 规则计算，
  新 user namespace 自有 limits 为 INT_MAX，仍受全部祖先限制。计数和判限同锁，AVL node 在锁外预备/退休；
  clone/setns 只共享既有 namespace，不重新收费。同一 `UserResourceCharge` 同时服务 inotify 实例/watch，实例固定原 real UID/UserNS，watch 沿原 instance owner 链收费。
  计数 entry 查找/插入为 O(log kuid)，删除零 entry 的 Drop 在锁外；最终 benchmark 对比现有/新 kuid 与 ancestor 深度。
- PidNamespace 的唯一 spin State 持有 local-number→stable identity association、cyclic cursor、
  pid_max 与 Creating/Live/Dead lifecycle。None node 只是未发表 reservation，lookup 不可见；
  node 在锁外准备，失败 Drop 摘 node 后锁外 free。首个成功发表的 PID1 同锁成为 reaper，
  clone3 set_tid 从 leaf 向 ancestor 提供精确编号；每级验证实际 pid_max 与 owning UserNS 的
  SYS_ADMIN/CHECKPOINT_RESTORE，未指定的 ancestor 使用同一 cyclic owner。精确编号不移动
  cyclic cursor，已占用返回 EEXIST；无 reaper 时只允许 PID1。失败归还所有已准备编号。
  init 最后一个 Thread 退出禁用创建，commit 必须再次检查 Dead；historical observer 不决定编号占用。
  User ancestry、32 层与每个 ancestor quota 在创建时验证。pid_max 下限/初值按 actual possible CPUs
  对照基线 8/1024 per CPU，sysctl write 以 owning user SYS_ADMIN 或实际映射 root euid 授权。
  ancestor 只引用 parent，不 pin Task；member drain 沿实际 namespace index，禁止扫描不相关 process。
  Task identity/ABI 直接使用同一 PidNamespace association 与 immutable ladder。
- ProcessGraph.next_pid 只签发不复用的内部 identity key，以 usize checked_add 拒绝真正溢出，
  不再借 futex 30-bit mask 限制整个 kernel lifetime；用户 PID 上限归实际 namespace。
  robust futex 比较 local TID；proc inode 与 trap-context 地址都不再按内部 key 位移拼接。
- namespace::pid_identity 的不可变 PidIdentity 拥有 stable key 与 leaf→initial 编号 ladder，
  viewer level 直接索引并验证 actual namespace identity，热投影 O(1)且不分配。
  不可变 PidIdentity 可由 FS/TTY/socket 等 lower viewer pin，绝不反向导入 Task。
  PreparedPid/PidLinkSlot mutation seam 仅由 task graph construction/publication 使用，其他领域
  只消费 immutable identity/number_in/numbers_in；task::pid::ProcessId 只保留同一 TGID Arc，不复制 ladder。
  独立 graph PidRegistration 才决定 local association 占用，observer Arc 只保留历史编号；
  最后 registration 锁外 Drop 移除所有 association，再允许旧编号复用。
  PreparedPid 在 graph 外准备完整 ladder/reservation/link storage，commit 先整体复查 Dead；
  graph 同一 guard 排除 namespace init disable，成功后只发表已经预留的 scalar node。
  PidLinkSlot 的唯一未初始化 Box 仅在完整 write 后 assume_init；next 只由 retirement owner
  改写，RetiredPidRegistrations 在 IRQ guard 外迭代析构，避免深链递归与锁内 allocator。
  所有新增 link 分配属于冷 clone/group mutation，最终 benchmark 需测实际 owner；源码回归
  pid_identity 覆盖 ancestor OOM、dead ancestor commit 与 observer 不延迟编号回收。
- PGID 在实际 identity 生命周期内全局唯一，ProcessGraph.groups 的 AVL 直接按 PGID stable key 索引；
  SID 只在同一实际 group.sid registration 和 Process membership 中解释，不把 SID 再放入 map key。
  精确 kill PID 按 nodes 查找，group kill 按 group.members successor 推进；只有广播遍历可见进程。
  定向/group kill 对 zombie 仍校验原 leader 凭据，合法 signal 返回成功、不创建 pending。
- 空 PGID 不留历史 index。group retirement 先摘 orphan consequence 链，live leader 回收同一
  AVL node storage，旧 PGID/SID links 移给 RetiredPidRegistrations 锁外释放；leader 已 reap 时，
  现有 group node 移入 RetiredGroups.nodes，同样不分配。VacantEntry::into_slot 独占摘除 node，
  read 各 key/value 一次转交 caller，把相同 layout 的 Box 改为 MaybeUninit；topology edges
  必须为空，旧值不会重复 Drop，fill 重新完整初始化。新 group 只在实际需要时锁外准备两个
  link，重新进入 graph 后完整复查权限/目标；OOM 返回标准 ENOMEM，不改变 membership。
  clone namespace PID1 以自己的 identity 建 SID/PGID，普通 fork 继承 parent group。
- Namespace init 的 shutdown event 在新 init graph construction 前 fallibly 分配，
  ProcessNode.namespace_exit 独占 Arc；PidNamespace Dead lifecycle 只保存 PidExitWake 的 Weak，
  不保活 Task/waiter。所有 exit_in_progress 标记与最后 exiting init Thread 的 claim 同 graph
  guard 提交，不能靠早期 len==1 snapshot。shutdown 先 disable allocation，再设 SIGCHLD ignore，
  沿 namespace number index 请求既有 group-exit owner 以 SIGKILL 清退含 initiator 的实际成员；
  最后使用非阻塞既有 wait owner 消费旧 zombie，然后无分配、不可中断地等实际 association
  retirement（含外部 parent reap 与在途 reservation Drop）signal。自 pending→wait handshake
  防丢唤醒，numbers.len 是同一 index 的实际 node count，不另设活跃任务计数。
  orphan/creator reparent 按 actual immutable PID ancestry 选择最近 live namespace reaper，
  namespace init 自身退出转给 ancestor reaper；全局 init 仍在最后 Thread 退出时 fail-stop。
- Terminal 的 controlling SID/foreground PGID 改 pin actual immutable PidIdentity；TTY observer
  不增加 graph registration count，历史 identity 可在编号复用后继续被正确投影，不能由 raw key
  反查已不存在的 group。control replace/release/hangup 把旧 pins 交给 RetiredTerminalControl，
  在 terminal 与 graph guard 都释放后析构。Process 的 Option<TTY Arc> 是真实 controlling
  handle，初始无 TTY；setsid 清 handle，fork 只在 graph publication 捕获 creator 的当前值。
  TIOCSCTTY 是 graph→Terminal/Process 的同一事务；已拥有同 session 时不重置 foreground，
  接管仅 arg 的低 32 位为 1 且 initial SYS_ADMIN，只写 fd 同样须此全局 capability。
  steal/session exit/hangup 沿 PGID groups 的实际 SID/member index 清原 session 的 matching
  handles；target Terminal 有 caller pin，清 Arc 不可能触发最后 Drop/allocator，因此没有
  exit allocation 或另一份 session registry。PTY pins 和 control PID pins 的最终 Drop 在锁外。
  /dev/tty 与 proc tty_nr 只消费真实 Process handle，PIDNS fork 的 inherited tty 不被 SID
  projection 隐藏；没有 foreground PID 的真实 TTY 输出 tpgid=0，Process 无 TTY 才是 -1。
  task JobControl 冻结 graph SID/PGID pins，TIOCGPGRP 按 caller active PID namespace 投影，
  TIOCSPGRP 先解析同一 view 的 number，再验证同 session 实际 live group。
- TimeNamespace 的唯一 spin State 持有 monotonic/boottime offset 与 frozen bit；initial owner
  由 composition root 在首个 Task 前显式安装且已冻结。offset 的批量验证先于同锁 publication，
  enter 与 update 同锁线性化，首个任务进入后返回 EACCES；不另设 timer clock 或 vDSO offset cache。
  Task FsNamespaceState 同时持有 active time 与 time_for_children：unshare 只替换后者，
  不共享 VM 的 fork 进入后者；CLONE_VM 保留 active，exec 在 PONR/sibling retirement 后进入后者，
  setns 同次替换两者。冻结时机沿固定 Linux copy_namespaces，失败 fork 可以冻结已准备的 owner。
  namespace::TimeOffsets 是一组 immutable operation scalars，TimerQueue 的 clock/host deadline
  仍是唯一 timer owner；timerfd/POSIX absolute replacement 按实际 record clock 选择转换，
  原始零决定 disarm，非零转换得到零必须 armed。relative duration、realtime、CPU clock 不变。
  clock_gettime 的 RAW/COARSE 使用 monotonic offset，uptime/sysinfo/proc starttime 使用 boottime。
  file::TimerFdBackend 只转交该 lower namespace snapshot，不保存 namespace/Thread 引用；
  热路径无分配；最终 architecture-bench 测真实 frozen snapshot/projection 与 absolute conversion。
- TimerQueue.TimerFile 的 generation 是 schedule 版本的唯一 owner；每次成功 replacement
  checked_add，wrap 前拒绝且无效果。软中断摘取携带历史版本，在 counter→queue 同锁序下
  复查真实 record，旧 event 不能写新 counter；backend 不镜像当前 generation。
  deadline node 分配在 counter/queue 锁外完成，版本与 deadline 同次发表；随后同 counter guard
  清旧计数/readiness。计划失效时先释放两锁再重试，禁止带 spin guard 分配或等任务。
  热 expiry 新增一次短 queue 复查，其成本在最终 timerfd guest 验证中观察；锁外 signal pipe
  可产生无数据的竞争唤醒，poll/read 仍只看实际 counter。host timer_file 用真实 Pipe/AVL/backend
  确定性覆盖 pop→rearm→deliver、disarm、周期计数、OOM 保留以及既有 deadline node 无分配重用。
  counter 的 spin guard 只由 syscall/任务 safe point 的 timer softirq 使用；hardirq 只投递
  deferred bit，kernel IRQ return 不执行 expiry，因此不会在持 counter 锁的内核栈上重入。
- user/time setns 的 single-threaded 检查不能只看 TGID Thread 数：先在 graph 外 pin actual MM，
  graph 内同时验证唯一 live Thread 和其他 Process 的 MM handle，禁止 strong_count 猜测 mm_users。
  scoped Task namespace_mm/shares_namespace_mm 仅供 process_table 此检查，graph 内借用且不析构 MM。
  graph→MM-handle 短锁，无 memory mutation/usercopy/调度；最后 MM pin 始终 graph 外退休。
- Process 独占共享资源与聚合 accounting；Thread 独占 execution/signal/scheduling context。
  legacy clone 与 clone3 只在 syscall 解码 raw 参数，随后共用 CloneParameters 和既有两种
  graph publication（独立 Process／同组 Thread）。clone3 的 64/80/88-byte 前缀零扩展，
  未知尾部固定小块检查为零，user-copy 与 stack range 验证早于 namespace/PID publication。
  两种 clone 在静态 arch UserContext::prepare_clone 消费可选 SP/TLS；未给 SETTLS 保留 caller
  TLS。Process CHILD_SETTID/CLEARTID 与 Thread 相同，但 child store 在实际 child MM、parent
  store 在 parent MM，各自用其可见 TID；best-effort fault 不回滚已创建实体。CLEAR_SIGHAND
  只清非 ignored disposition，不共享或覆盖 parent sighand。ABI 扩展不改变热 syscall 的
  lookup；冷 clone 引入的 PID 精确选择与 namespace/graph allocation 在最终 benchmark 检验。
  Thread 同时唯一拥有单向 `no_new_privileges`：初始 false，clone/fork/vfork 继承 caller 的值，
  exec 保留，其他 live sibling 不随 caller 的 PR_SET_NO_NEW_PRIVS 改变。exec set-ID credential
  transition 读取 calling Thread 的值，启用后忽略 setuid/setgid mode，并照常刷新 saved IDs。
- Thread 的独立 `MmHandle = Arc<Mutex<MmMembership>>` 唯一持有其映射 user；
  Live 拥有 root，Retired 只保存退出时的原生 user_dumpable。Process `leader_mm` 仅别名保活原 leader
  的同一 outer handle。exit retire 按值移交 root 后，外部
  TCB/Process/wake 引用不保活 VMA；clone VM 共享 root 但始终新建 outer handle，fork COW。
  usercopy、fault、futex、robust 与 trap token 读取 calling Thread mm；没有 Process 可变 mm 路径。
  原 leader Retired 时，proc Process 的 cmdline 空、stat/statm memory 零、status 无 Vm 字段；
  worker 仍使用自身 mm/FDSize。共同页统计从一个 active mm 扫描一次，不逐 Thread 扫描。
  Process state 在原 leader 退出后为 Z，nice/priority/CPU 读取同一 SchedulingEntity Arc
  的只读 leader alias；该 alias 不持有 TCB/mm/stack，也不维护 scalar 副本。exec 重定向
  mm/files/credentials/scheduler/I/O aliases 至 caller，并在锁外退休旧 aliases。
  ProcessNode 与 live Thread 同持实际 Process Arc，zombie 等待 reap 不保活 TCB 或已退休的
  MM/files/ns membership；所有 proc projection 由同一 Process owner 提供。原 leader I/O
  是实际 Thread counter Arc 的只读 alias，不保存副本。graph snapshot 包含 zombie，缺少 live
  leader 时由真实 leader owners 投影其 Z task entry，按挂载 PID view 排序。新增 counter Arc
  只在冷 Thread construction 分配且先于 trap VMA publication；snapshot/cold clone 成本需最终
  benchmark 与 guest 验证，目前没有执行或通过声明。
- ProcessNode.exec 唯一领取 exec preparation，竞争返回 EAGAIN；所有映像
  image preparation 成功后，graph 同锁进入 drain、发布强制退出。
  exec 与 TSYNC 的凭据/NNP preparation 共用 Process credential_guard TaskMutex；exec 在
  建 auxv 前持锁，TSYNC killable acquisition 使内部 SIGKILL 可解除等待，避免 drain 死锁。新 Thread
  publication 在同锁登记 Process 持续的在途 Thread count，activation 继承退出；旧线程在 trap return 按
  exec owner 分别 thread-only exit，不能把内部 SIGKILL 变成 group-exit 杀死 survivor。
  所有 graph removal 均取得 ThreadRetirement；完整 MM/clearTid 与原 vfork completion 后
  计数减一并唤醒唯一 exec waiter。Process 拥有持续计数，exec 不重置；包括 drain 开始前
  已摘下图但未清理的 sibling，不是 live thread count 的重复副本。
  claim RAII 在成功映像 commit 或准备失败后 graph 外退休。drain 后才复制最新 files/sighand，
  捕获最后 sibling 修改及晚期跨 Process SIGHAND 分享；此时 OOM 清理新映像后 fatal，不能
  返回旧 userspace。drain 的 TaskEvent 使用 Killable policy，只响应 SIGKILL；fatal 取消
  清理 claim/新映像后进入 group-exit，普通捕获信号不撤销 PONR。retirement tokens 独立保活
  Process lifecycle owner，claim 取消不丢掉已经摘图线程的 cleanup obligation。
- ExecutionContext 的 Mutex<Arc<PidIdentity>> 唯一拥有当前执行 identity pin，用户与内核线程使用相同类型；
  不镜像 raw TID。TGID 是 Process 或 KernelThread 的不可变 PidIdentity pin。所有 graph/scheduler lookup 使用
  stable key，Linux PID/TID 与 robust owner word 使用 actual namespace projection。
  非主线程 exec 在 PONR 前准备新的 registration storage，robust cleanup 先于 graph rename；
  同一 guard 替换 execution Arc 和 ThreadIndex 的 registration，并无分配重发布两个索引和
  creator-child parent_thread。旧 pin/link 在 graph guard 外析构，历史 observer 不阻止编号回收。
  pending pdeath cursor 重置，Thread starttime 在 key==TGID 时投影原 Process birthdate；
  graph→execution identity 是单一短锁序，hot key read 新增短锁，最终 benchmark 检查其成本。
- Credentials 同一短锁唯一持有 real/effective/saved/filesystem UID/GID/groups 和 `CapabilityState` 的 P/E/I/bounding/ambient/securebits。
  `thread_capabilities` 仅投影该 owner，不另设 root flag。具名 syscall/VFS privilege 读取 E；
  faccessat real-ID按 commoncap选择 P/空 E。scheduler 同 UID 还要求 target P⊆caller P，
  proc READ_FSCREDS 在同组以外统一校验 UID/GID tuple 和 target P⊆caller E；各自具名 cap 可豁免。
  effective pathname 使用 fsuid/fsgid，real-ID access 保留 real；proc第四列为FSIDs，
  READ_FSCREDS用caller FSIDs和target R/E/S。setfsid拒绝仍返回旧值，RES/ID成功总刷新FS=E；
  LSM_SETID_FS按固定CAP_FS_SET清/从P恢复E，普通RES policy独立。
  canonical UID/GID 保持全局 kernel IDs；syscall/proc/stat/peer credentials 按 caller/opener namespace
  映射，不可映射显示 overflow IDs。具名 capability 按 owning/target user namespace 授权，
  新 namespace 的能力不能获得 initial namespace 的 swap/reboot/device 权限。
- `CredentialTransition` 借creation→graph guard中的canonical ProcessNode，callback再取credential
  guard；candidate验证后先MM dumpability Release置0、PDEATH字段/count同graph清除，再publish。
  最终retired组/Arc在全部短锁外Drop，禁止旧递归/后补clear路径。query先target凭据再MM Acquire。
- `AddressSpace` 独占`Dumpability` atomic；VM共享、fork复制，prctl只0/1，当前suid_dumpable固定0。
  exec新mm先disabled，CLOEXEC后用旧R/E+main/PT_INTERP可读性 provisional，再credential effects
  E/FS/newP增长强制0并publish；不在commit后用最终IDs/AT_SECURE重新启用。
  AddressSpace 同时持有创建时的 immutable user_namespace：VM 共享，fork 继承旧 MM owner，
  exec 采用当前 credential namespace。proc 先检查目标 credentials，再独立检查 non-dumpable
  MM 的 SYS_PTRACE，不能用子 namespace 能力短路原 MM owner 的权限。
  capability subset 跨 namespace 按 ancestor/owner EUID 判定；进入 owned child 不清 PDEATH/
  dumpability，clone preparation 不提交 parent 的 security effects。
- `LeaderCredentialsHandle` 是 Arc<Mutex<Arc<Mutex<Credentials>>>>，Process/ProcessNode共用同一个
  可重定向 outer alias；survivor exec只换inner一次，zombie沿该alias读取到reap，不保活TCB/mm/files。
  capget在graph锁内仅复制caps scalar；reap用take_entry摘除node，graph guard外析构最后credential/group。
- `PreparedExecCredentials` 在建栈前按 calling owner snapshot计算set-ID/IXGRP/旧组membership、
  NNP IDs降级、P/E/I/A/securebits与secureexec/PDEATH；初始栈消费同一最终IDs/AT_SECURE。
  PONR仅提交未发布结果、锁外退休旧Credentials，不根据mode或UID再次生成另一份身份。
  LoadedExecutable只捕获最终main ELF的实际path raw file-cap与metadata，不消费script/PT_INTERP
  属性；actual mount nosuid/membership/SB ancestry先限制file-cap，rootid匹配caller或ancestor UID0。
  wire保留64-bit属性，但commoncap exec只激活VALID_MASK；file P/I、bounding、ambient、legacy root
  与NNP全部由CapabilityState同一transition计算。effective文件不能获得全部file-permitted位时
  在PONR前返回EPERM且不改live凭据；file-cap存在时清ambient，setuid-root/non-root real UID
  不叠加legacy权限。初始root的full bounding保证bootstrap拒绝条件不可成立。
- 所有 Thread constructor 在共享 trap VMA mapping 前预留唯一 `memory_cleanup_wait`；
  context/robust cleanup 的 MM acquisition 均使用 `lock_prepared`，spin 锁外等待，取得 guard
  后归还 preparation；禁止同 mm 重入。退出 token 移交 preparation 与唯一旧 root user。
  exec release 后归还 preparation 供新 image 复用，不追加 PONR 后 metadata 分配。
- `Credentials` 由各 Thread 的独立 Arc/短锁唯一拥有；constructor 在 parent snapshot 锁外
  可失败准备 child owner。Process/graph 的 `leader_credentials` outer alias 仅保活原 leader 的同一 owner，
  不维护 credential 副本；exec 切换至 calling Thread owner，旧 handle 锁外退休。
  supplementary groups 为 immutable sorted Arc，允许重复；准备/排序/最终 Drop 在锁外，
  查询与 fork snapshot 只 clone 引用，permission 为 binary search。umask 属于 fs context。
  Thread 的单锁 fs handle 指向可共享 root/cwd/umask owner，读取锁序 handle→context；
  clone 按 CLONE_FS 共享或锁外可失败复制，unshare 的 replacement 与旧 handle 最终 Drop
  分离，OOM 不修改原 owner；独占时 unshare 无分配。退出在 graph 注销/clear_tid
  完成前 take 掉 caller handle，锁外退休，外部 TCB/wake Arc 不得延长 root/cwd lifetime。Process executable 独立于该 handle，
  防止 cwd 脱离导致同一 Process 的 `/proc/PID/exe` 读取不同映像。
  lookup_origin 在同一 context 锁内复制 root/cwd；chroot 仅替换 root、保留 cwd/dirfd，
  旧 root 锁外退休。退休 leader 的 exe/root/cwd 仍先按历史身份执行 FSCREDS 授权，
  未授权返回 EACCES，授权后因 fs/mm 已释放返回 ENOENT；不能借 worker 恢复 target。
  proc root/cwd 按准确 live TID 投影；main TGID 已退休时不能拿 worker 恢复 fs，
  task/TID/root/cwd 使用指定 Thread owner 与实际 credentials 授权。
  Thread signal 使用指定 Thread UID，Process signal/fd/exe 使用原 leader UID；same-thread-group
  signal 免 UID 检查，SIGCONT session 例外仍由 graph 判定。prlimit 非零 pid 指定 TID，
  仅 calling Thread 自身免身份检查，不能给异 UID sibling 全组豁免。
  signal 的 graph 内身份判断仅复制 UID scalar，不 clone/drop group 或 leader owner Arc。
- `FileTableOwner` 在稳定 Arc lifetime 内拥有 TaskMutex<FileDescriptorTable>；Thread
  的 FileTableHandle 是独立 Arc<Mutex<Option<Arc<FileTableOwner>>>>，Process leader_files
  仅别名保活原 leader 的同一 handle，退出 None 不持有任何 fd。clone/fork 按 CLONE_FILES
  共享或复制内容，handle 始终独立。exec 仅复制 shared owner，独占 owner 的 POSIX 锁保持；
  fs/files unshare 全部预备成功后无失败提交，任何 staging OOM 均保留两份原 handle。
  所有表操作在释放 handle spin guard 后取 TaskMutex；caller 的预置 file_wait preparation
  在取得 guard 后归还，fd lookup/cleanup 不因 waiter OOM 返回错误。guard 内禁止再次取
  同一表、FD/OFD 最终 Drop 或 close consequence；两 FD epoll publication 保留原原子语义。
  封闭的 constructor/rollback/exit/exec 均在 table/handle guard 外退休 old owner；外部 TCB
  observer 不延长 fd lifetime。procFD/FDSize 读取原 leader live handle，无表时 None/0。
  proc statistics 的 comm guard 必须在可能等待 table TaskMutex 前释放。
- 独立 Process 的 fork/VM/FS/FILES/VFORK 组合共用唯一 constructor 与 graph publication；
  VFORK 仅决定 caller 完成等待，资源共享分别按标准 bits 选择。VM 且非 VFORK 的
  child 清除 alternate stack；VFORK 保留。禁止另建 namespace-only clone/旧 fork publication。
  context_storage 唯一分配/解析 trap backing：临时 VMA 先准备 retirement waiter，再在同一
  mm guard 内完成 mapping 与 ContextOwner bind；不得在 mapping 成功后再分配 binding/waiter。
  exec 的新 mm 已注册 mapping/reclaimer，PONR 前准备 Arc 保活的 canonical pointer token，
  commit 只应用已准备 pointer；旧 mm 可被 CLONE_VM parent/其他 Process 占用，临时 VMA
  清理必须消费 constructor 预置 waiter，禁止依据 child single-thread 推导 mm 独占。
- `ThreadSecurityOwner` 同锁拥有 NNP/seccomp mode；既有 process graph 独占 strict/filter、TSYNC
  与 clone/fork/vfork policy inheritance 的提交顺序。New child 在 graph publication 前保持 disabled，
  publication 在同一锁内从真实 creator 重取 state；没有准备期继承后人工追赶的第二条路径。
  filter set 是 immutable flat program Arc array；锁外准备并复查 old set identity，远端 TSYNC
  推进后重新追加。每个新 Program Arc 只作为叶成功提交一次，leaf identity 唯一确定 canonical
  ancestry，sibling 最后一个 program identity 允许常数时间判断 prefix。TSYNC 校验全部 targets
  和退役 Vec 容量后才发布；NNP 只单调 OR，不覆盖 target 并发 prctl；旧 owners 在 graph 和
  process_creation 锁外释放。graph ThreadIndex 的 exit_in_progress 表示不可返回退出已开始但
  cleanup 仍可阻塞；TSYNC 跳过，scheduler current/member/thread_count 保留至正常摘除。
  `active` 只投影 disabled fast path，mode 发布后 Release=true；evaluation 只短锁 clone set，
  程序/信号/日志/退出均在锁外，不把 Arc 留在 non-returning exit stack，Drop 不递归释放 ancestor。
- seccomp 的 flat set 保存实际 immutable `InstalledFilter`（Program、flags、可选 NotificationQueue）；
  queue 的 task users 仅由真实 Thread mode publication/inheritance/exit 改变，不由临时 Arc refs 推导。
  graph 内先 attach 新 membership，旧 `RetiredSecurity` 锁外 detach；exit 显式退休，保留 TCB 或
  prepare/evaluate snapshot 不延迟 HUP。filter/queue/request 均不得强持 TCB 或 file-table，防止反向环。
  Request 只 pin 实际不可变 TID identity，不保留稳定 key 的 ABI 副本；RECV 从 UserMemory
  读取 calling PID namespace 后投影，listener 跨 view 传递或 receiver setns 不改变生成时 identity。
  TSYNC 失败在 graph guard 内投影到 caller view，绝不把稳定 graph key 返回用户。
  Queue index 与 Request phase/active 各只有一个 owner，摘除以 queue→request 同锁事务先失效再摘索引。
  poll 可在 epoll spin guard 内调用，故其内只有短 spin transaction；allocation、user-copy、wake、
  fd receive/replace 与最终资源 Drop 均在这些 guard 外。RECV失败重回 Init，已取消请求不复活；
  Request 的 installing pin 保留从 pending 移出的实际 command，安装结果与已答复请求失效
  在 queue→request 同事务提交。RECV 在置 Sent 前准备自己的 recovery waiter；失败 copyout
  遇正在安装的 command 时，以 command 同锁 Done/登记握手等待，结束后重查 active。
  每个 receiver 独占 TaskEvent，command 只拥有侵入式等待链；complete 先摘 next 再锁外
  signal 并迭代释放，既不共享单等待者事件，也不在 copyout 失败后分配或递归 Drop。
  SEND 只接受 Sent，CONTINUE 不重新评估 policy；WAIT_KILLABLE_RECV 在 Sent 后切换实际等待策略。
  ADDFD 的实际 OFD 经 Pending→Processing→Done 交给目标线程；Processing 不允许 ioctl 提早返回，
  完成前观察者的实际 OFD pin 阻止 listener final Drop。原子 SEND 以 typed AddFdReplyPending
  占据 reply，实际安装后才发布 Replied，失败可回 Sent；其他普通注入不能消费这个预占；
  Pending SEND 的中断取消按固定 Linux 保留已占据零值 reply。其余取消 command 以 ESRCH 完成。
  `NOTIF_SET_FLAGS` 的同步 wake flag 是唯一局部性 policy；TaskEvent/Pipe 传递至同一 scheduler
  wait transition，在 active/affinity 允许时优先当前 CPU，不另建 Ready state 或 wake registry。
  LOG 使用选中 filter/action 在锁外投递；SPEC_ALLOW 遵从当前无 per-task architecture override 的
  generic Linux weak hook profile，不声称提供硬件 mitigation。通知 transaction 为冷 syscall 路径，
  不外推为实时上界；既有 disabled/单 filter/八 filter evaluation benchmark 继续约束未通知热路径。
- `ProcessSignalState` 独占 group pending 与可退休的 `Option<Arc<Mutex<SignalHandlers>>>`；
  disposition 只属于 SignalHandlers，CLONE_SIGHAND 跨 Process 共享表且要求 CLONE_VM，
  不共享 pending/mask。普通 fork 锁外可失败复制；Thread group 始终同一 Process handle。
  锁序 graph（冻结成员时）→mask（需要时）→group state→handler table→Thread pending；
  handler guard 内不得进入 graph、MM、allocator 或其他 group state。SIG_IGN/default-ignore
  transition 同锁清掉 caller group 全部 pending；不清跨 Process 共享者的 pending。
  SA_RESETHAND 在选择时只清 handler，不能在 frame user-copy 后延迟重置。
  exec 在 PONR 前只预分配目标表，提交时同锁复制最新共享表再保留 IGN handler、清空 flags/mask/restorer；
  pending 保留。最后 Thread exit 在 graph 外 take/drop 表；TCB observer 不保活退休资源。
  generation 对 blocked ignored signal 保留 pending 供 sigwait；mask 与 disposition 同事务读取。
  init exemption 在同事务判定：全局 init 拒绝外部 SIGKILL/SIGSTOP；namespace init 只接受
  祖先 PID namespace 或内核强制生成的 SIGKILL/SIGSTOP，其余异步默认 disposition 仍忽略。
  内部 group-exit SIGKILL 单独无条件发布且无第二份状态。
  admission 清除未知/SA_UNSUPPORTED flags，RESTORER 能力由静态 arch codec 提供。
  SIGCHLD 显式 IGN/NOCLDWAIT 自动回收，DFL-ignore 保留 zombie；NOCLDSTOP 只抑制 job
  SIGCHLD，不抑制 wait-event wake。wait/exit/orphan autoreap 共用 RetiredProcesses：同锁摘下
  graph、parent/creator/group membership 与 staged consequence 链；existing node storage 移交
  retirement owner、不分配，全部资源在 IRQ graph guard 外析构。active stopped/continued claim
  跨 exit 时，ProcessState::Exited 的 ExitRetirement::AfterClaim 唯一保存自动回收决定；
  claim consume/release 结束时退休，不重新读取已经可能变化的 parent disposition。
  exit/job/adoption 以 child canonical real-parent/reaper Thread 的 mask 生成实际 child signal，
  policy 与 group pending publication 在同一 parent handler 事务完成；
  ParentSignalWake 只保活调度 target、锁外唤醒，不得重新生成信号。vfork completion 归原始
  child ThreadIndex（sibling 不继承），exit 在 TID index 摘除前移出 VforkCompletion token，
  旧 MM/context 清理完成后才锁外 wake；autoreap 不得吞掉该 owner。
- `signal_info::PendingSignal` 独占 typed identity/fault/timer/seccomp/user-native payload 与 forced consequence；
  identity 的 si_pid/si_uid 在 generation 时按准确接收方 PID/user namespace 固定，
  SIGCHLD 使用原 leader real UID；parent-death 保存同一 typed 来源，不在 drain 时重读身份。
  每个 pending payload 为 56 bytes；user-native 保留 LP64 的 48-byte kernel_siginfo 前缀，
  不把 80-byte expansion 放进每个 slot。SignalForce 的 None/Namespace/Synchronous 是唯一投递
  consequence，coalescing 单调合并其强度并保留首个 siginfo；不能按首个 payload 反推后续
  同步 fault，否则同号 SI_USER 先到达时 namespace init 会错误忽略 fault。
  SIGSYS 和其他同步 fault 共用 disposition/mask 的原子规范化，不伪装成地址 fault。
- `signal_pending::PendingSignals` 是每个 Thread/Process 的唯一 pending owner。1..31 普通信号
  coalesce 首个 payload/forced consequence；32..64 实时信号各有实际 Box FIFO，bit 只投影是否
  可消费，不保存另一份实时 payload/事件数。普通与实时 payload 都只在实际 SignalRecord 中，
  不再有固定历史 payload 数组；bit-only fast/OOM path 的 SignalFallback 只持 force consequence，
  encode 统一为 SI_USER 零来源。LP64 PendingSignals owner 有界为 1152 bytes。
  PreparedSignalRecord 在 graph 外分配实际 record，
  以 recipient real UID/UserNS 和 RLIMIT_SIGPENDING 收费；同一 RlimitCharge 也服务 SHM_LOCK，
  typed resource key 区分 locked pages 与 pending records，没有第二份 locked-memory ledger。
  普通 sigqueue 也占同一 native quota；nonnegative/special source 按原生规则越过 soft/ancestor
  max 但仍计费、仍受 LONG_MAX 溢出保护。SIGKILL 和 kernel ignore policy 使用无 record fast path；
  普通负 code OOM/限额耗尽保留零来源 pending bit，RT strict 仍 EAGAIN。普通 coalescing 保留首个
  实际 record，unused reservation 在 graph 外退款，与同号 POSIX records 保持原生 FIFO。
  child signal 在 graph 外准备准确 parent/creator/adoption recipient；相同 recipient+signal 只保留一个
  reservation。锁回后校验原 child Process Arc、creator/reaper identity 与实际 signal，改边或 parent
  exec 改变 notification signal 则退款并重试，
  不让复用 PID 的新 Process 接收旧事件。退出/收养/job-control 最终 notify/autoreap 与 pending
  仍同一 disposition/graph 事务；PreparedChildNotifications 先声明、最终 guard 后声明，unused
  storage 晚于 guard Drop。synchronous fault 同样在 mask/table guard 外准备真实收费节点。
  NEWUSER 在同一 Process limits guard 捕获 MEMLOCK/SIGPENDING soft maxima；原收费者及祖先
  token 随实际 record lifetime 退款，后续 setuid/setns/降限或 observer pin 不改收费起点。
  同号实时 FIFO 与低编号选择为 O(1)；thread-private 优先于 shared，复用既有 handler/sigwait。
  tail 只借用独占链的最后 Box，Send unsafe 不创建共享 mutable 引用；push/pop/splice 无分配，
  TakenSignal/RetiredSignals 移出 pending/graph guard 后 Drop，长链逐个取 next，禁止递归析构。
  kill/SEND_SIG_PRIV 的 RT OOM 保留一个 SI_USER 零来源 bit；tkill、用户 queue 和 timer info 的
  独立 record 不可丢失，失败 EAGAIN；blocked ignored generation 保留给 sigwait，SIG_IGN
  transition 退休全部同号记录。Thread graph removal 和最后 Group exit 显式消费 pending，
  TCB/proc observer 不能延长收费。representative 在准备期间退出时只在同一 actual Process
  重选；广播仍按 actual group/index 推进，不能因一个 stale Thread 提前截断全部 recipients。
  标准 rt_sigqueueinfo/tgsigqueueinfo 共用 SignalOrigin authority；用户 payload 始终持 caller，
  不能走 Kernel DAC 豁免。正 code/SI_TKILL 仅允许 current TID，用户 si_pid 保留历史输入，
  si_uid 按 recipient UserNS 翻译；ancestor sender 隐藏 pid。仅未知 layout 验证 expansion 全零。
  POSIX timers 也使用同一 SignalRecord FIFO；预分配 timer 即使使用普通 signal，也保留各 timer
  的独立通知。SignalRecordSource 分别持 dynamic recipient charge 或 queued timer Arc；空闲
  timer Box 的 Timer(None) 不持 owner，避免 owner→slot→Box→owner 自环。PreallocatedSignal
  在 timer_create（含 SIGEV_NONE）一次收取 creator real UID/UserNS/RLIMIT_SIGPENDING，并预留
  Box/Arc；slot mutex 只转移 Box/queued sequence，不分配、不进入 graph/pending/timer owner。
  registry 删除后 queued record 保留原收费直到实际消费/丢弃；普通 TCB/proc observer 不持此 owner。
  TakenSignal.resolve 先将真实 Box 归还，再交 coordinator 验证 registry Arc 和 sequence；
  周期通知到期进入暂停状态，dequeue 根据全部 missed periods 更新最终 overrun，并复用原
  deadline node 重装。普通信号仍在同锁选择 handler；timer notification 先验证有效性，才清
  SA_RESETHAND，旧/已删除/同 ID 新 lifetime 的通知不能修改 disposition 或重装 timer。
  已进入 pending 的通知经统一 dequeue 验证/重装后才选择 timer disposition，与 Linux
  dequeue_signal 一致；unblocked ignored generation 停放原通知。SIG_IGN 与 stop/continue
  冲突清队列在 graph 下停放并归还 live 周期 Box，归还先于 graph unlock，恢复 action 后沿
  实际 process timer 范围重发原通知。其他退休节点保持锁外 Drop。exec 显式摘 SI_TIMER records，
  保留同号其他来源，避免 blocked 的旧 timer 无限保留 quota。thread-directed timer 转换
  creator PID namespace 中可见 TID，持实际 PidIdentity 并在创建/发表复查，数字复用不能改目标。
  producer fixture、guest 与 realtime-signals-bench 随统一门禁执行；普通/实时 native
  ucounts 和 POSIX lifetime 共用实际 owner，接口、并发 producer 与运行时行为均需验证，
  仅声明 ABI 矩阵已开放的 signal 能力。
- SchedulingState 独占 run membership；ProcessorTopology 独占 per-CPU runqueue/current/mailbox projection。
- `scheduler::placement_policy` 独占选核的纯决策与轮转节拍；`PerCpuProcessor::preemptions_since_rotation` 只由所属 CPU 读写。
  缺失轮转时，N+1 个任务在 N 个 CPU 上的分布会因并列保留 last-CPU 而永久锁死。轮转只发生在被抢占任务重新入队的选核，
  且只在本 CPU 还有别的可运行任务时放宽；不新增锁、分配或跨 CPU 访问 runqueue。
- `processor::runnable_task_count` 只按 possible logical CPU 扫描现有 Ready/Running
  membership；proc 全局统计和到期 load-average 共用该实际 producer，不获取任何 TCB 或
  ProcessGraph，也不按物理 runqueue/mailbox 的 stale token 计数。跨 CPU 瞬时采样与 Linux
  nr_running 一样允许转换期间短暂误差，不能新增另一组 runnable counter。
  benchmark 决策：冷 proc 与五秒采样由 O(global Tasks) 降为 O(CPU)，零分配/锁；非到期
  timer 仍只读原 cadence atomic。load-average 加入实际 TASK_UNINTERRUPTIBLE memberships 的
  O(1) aggregate，见同一 SchedulingState BlockedLoad owner；不能把所有睡眠任务混入负载。
- ProcessorTopology 的 per-CPU pending handoff slot 独占已经保存 context、尚未提交
  Ready/Blocked/Stopped consequence 的 outgoing owner。slot 同时携带原 logical CPU 的 IRQ
  restore token；next task 或 idle continuation 必须恰好消费一次，禁止从 task identity 重建。
- ProcessTable process graph 独占 identity、parent/child、creator-Thread、group/session、exit/wait
  与 process timer relation；`parent.children`、global `TID -> TGID`、creator children 与
  `(SID, PGID) -> members` 是同一 graph owner 的 projection，不得复制成第二套 lifecycle state。
- `TaskControlBlock` 由共有 `ExecutionContext`（TID、kernel stack、kernel context）与
  `TaskKind::{User, Kernel}` 组成。只有 `User` 拥有 Process 与 user-mode thread state；内核线程只运行
  `run_kernel_thread` 主体，从不进入 user trap、syscall、signal delivery 或 process lifecycle，因此
  `process()`/`thread()` 对内核线程不可达。`kind` 必须先于 `execution` 声明，使 user context binding
  先于 kernel stack backing 析构。
- 内核线程稳定 key 与用户共用 graph allocator，实际编号共用 initial PID namespace cyclic
  allocator；它没有用户 Process，actual kernel execution index 供 proc/kill 精确选择；没有用户
  wait child 关系、CPU rlimit 与用户地址空间，boot worker 信号 disposition 全部忽略；
  `tgid()` 返回自身 key。KernelThread 独占
  actual PID registration，主体返回后、撤销 current 前显式归还编号；TCB/IPC observer 只 pin
  历史 identity。共享内存最终 mmput 来自后台 reclaim 时，VMA close 也使用真实执行者 PID，
  不会访问用户 Process 或把实际 kernel Thread 伪装成 PID0。初始化失败退还未发表 reservation。
  namespace boot 同时构造标准 init_struct_pid 的唯一 PID0 pin；Processor current 为 None 时
  是真实 boot/idle 执行体，资源 retirement 使用该对象，而非临时造 identity。PID0 没有 user
  association/registration，不能被 cyclic allocator 分配或进入用户 graph。
  benchmark 决策：已有用户 hot identity 读取不新增分支或分配；只有冷内核线程创建/退出
  增加相同 PID allocator transaction，纳入最终 PID/资源退休回归。
- `WaitRegistry` 独占全部 wait registration 与 source index；固定 16 个 shard 只按稳定
  source identity 路由，registration 的 exact key list 是跨 shard claim/cancel 的唯一反向
  metadata。pending 分别由 Process/Thread 独占；disposition 只属于可跨 Process 共享的表。
- `sync::TaskMutex` 独占 task-only blocking owner 的 FIFO ticket、wait chain 与 handoff；
  `sync::TaskEvent` 独占单等待者自动复位事件的 pending/waiter/completion。两者共用
  `sync::task_wait` 的唯一 scheduler adapter：task domain 只实现 opaque `TaskWaitTarget`，把完整
  `(owner address, ticket)` 投影为唯一 `WaitMembership::TaskWait`。`TaskEvent` 先登记 waiter 再复查
  pending，signal 先发布 pending 再取走 waiter，任一交错都不丢失唤醒。event 的 signal/cancel
  必须在同一 waiter lock 内 complete，锁外才 exact wake，不能让迟到 completion 污染新等待。
  TaskWaitMode 明确区分普通、可中断与仅致命信号中断；event 与 mutex 复用相同
  signal predicate 和 exact membership，DMA 与普通 mutex lock 永不中断。
  scheduler membership 发布后再复查 signal，signal/event 都只消费同一 membership 一次；
  中断后已提取的旧 wake 可合法失败，但不能命中新等待。event ticket 从唯一全局
  EVENT_TICKETS 领取，跨同地址对象重建也不复用，不分配、不承载内容 ordering；否则
  仅保活 TCB 的迟到信号会形成生命周期 ABA。此 Atomic 只在阻塞登记时使用。
  scheduler adapter 在 processor topology 后只安装一次；缺失、重复安装、ticket 耗尽或
  非可中断 key 的 exact wake mismatch 必须 fail-stop，不能退回 runnable polling。


补充契约见 [进程接口、等待与退休契约](process-lifecycle.md)。
