# System V IPC 对象与资源契约

- IpcNamespace 独占 actual System V message registry（slot/sequence IDs、key index 与 msgmax/msgmnb/
  msgmni），UserResourceCharge 按 owning UserNS/kuid ancestry 收费；NEWIPC 新建标准空初态，clone
  默认共享同一 Arc。Task 的 fs/namespace tuple 只保存 membership，setns 校验两侧 owning UserNS
  SYS_ADMIN 后一次替换；nsfs/proc pin实际 NamespaceOwner，quota 只在最后 namespace Drop 归还。
- MessageQueue 单锁拥有 permission、按 enqueue sequence 排序的 message nodes、bytes/qbytes、
  发送/接收时间与历史 PID pins。IpcAccess 是同一 credential lock 的不可变输入；UID/control 使用 EUID，group DAC
  使用 fsgid + supplementary groups（Linux in_group_p），creator GID 使用 EGID；
  IPC_OWNER 覆盖目标 UserNS DAC，control 要求对象 owner/creator 或目标 SYS_ADMIN，增加 qbytes
  超 namespace msgmnb 必须 initial CAP_SYS_RESOURCE。不按 UID0 绕过 capability namespace。
- Arc/message/AVL node 与阻塞 event 在锁外准备；容量检查与 waiter admission 同一 queue lock，
  直接交付/dequeue/SET/RMID 摘下对应 actual admission 后锁外 signal。每个 waiter 独占一个
  实际 TaskEvent，Drop 精确取消递增 admission key。RMID 同时撤 key/slot 并发布 removed，messages/PID pins
  与 waiters 在 registry/queue guards 外退休；阻塞 caller 为 EIDRM，后续旧 ID lookup 为 EINVAL。
  最后 namespace Drop 摘除真实队列内容，即使只读 queue observer 仍有 Arc 也不能继续操作。
- IPC namespace sysctl 的普通 0644 权限按 owning UserNS 映射 root EUID，无 SYS_ADMIN DAC
  豁免；message min/max 是 Linux ipc_sysctl.c 的 0..INT_MAX / 0..IPCMNI。IPC_INFO/MSG_INFO 沿实际
  registry/queue owner 聚合，不添加全局 byte/header mirror。ABI 来源固定 Linux v7.1 ipc/msg.c、
  ipc/ipc_sysctl.c 与 include/uapi/asm-generic/{ipcbuf,msgbuf}.h。
- benchmark 决策：key/ID/queue admission 为 AVL O(log n)，负类型选择必须扫描真实 message
  types，消息 payload 与 node 准备不持 spin guard；阻塞/删除按实际 waiter 数唤醒。集中执行
  host owner、guest IPC namespace/codec 和原性能围栏，不新增阈值例外。

- Queue waiters 以唯一递增 admission key 排序；预备 token/Arc 在 queue guard 外分配，
  queue 条件检查与 commit 同锁，不用 event 地址模拟 FIFO。send 按实际 receive admission
  直接交付首个匹配消息，容量不足的接收者获得 E2BIG 后继续寻找下一人；直接交付不入队，
  不增加 qnum/cbytes，记录 rtime 与 Linux task_pid(receiver)。普通 dequeue 记录 task_tgid。
  完成值只属于实际 waiter，不另建 payload/状态缓存。finish_wait 先持 queue 锁撤销 admission
  再取完成值，防止信号检查后迟到交付丢失消息；已经交付胜过信号和随后 RMID。queue→
  completion 为唯一锁序；event signal、node/PID/message 退休在 queue lock 外进行。
  dequeue 仅唤醒发送者，到达仅唤醒匹配接收者；metadata change/RMID 才唤醒全体重新检查。
  benchmark 决策：合并 event 到 waiter Arc 省一次 cold allocation；热路径有序 admission
  与直接交付避免全部 receiver 的无效 scheduler wake，纳入最终 IPC/调度 guest 成本验证。

- `ipc::ids::IpcIds<T>` 是 System V 各对象类型使用的唯一 ID/key 算法；每种类型保留独立
  registry 与实际对象内容。PreparedIpcObject 在 registry guard 外预留 Arc 与双索引节点，
  guard 内只选 slot/generation、初始化完整对象并无失败 commit。RetiredIpcObject 负责
  锁外 node/object 退休，不另存 ipc_perm 或内容。native 31-bit ID 按标准 16-bit seq 环回绕；
  raw ID 只解析当前实际对象，历史 observer Arc 始终指向原实际对象。Queue 与 semaphore 通过 actual IpcObject
  投影不可变 id 与实际 ipc_perm.key；shared-memory RMID 在同一 registry 事务撤销 key、将 key 置 IPC_PRIVATE，保留仍有挂接的 ID；没有兼容编号入口。

- `ipc::shared_memory::SharedMemorySegment` 独占 ipc_perm、creator/last PID pins、三种时间与实际
  nattch。shmat 的临时 reservation 防止准备期间 RMID 销毁；reservation close 不更新 VMA
  dtime/PID。RMID 立即撤销 key，DEST segment 在 nattch 非零时仍可按原 shmid 挂接。
  最后实际 VMA/reservation close 才摘除 ID 和 backing；segment observer 不延长页 lifetime。
  registry→segment 是唯一锁序，PID/AVL/backing 最终析构在两个 guard 外。shmmax/shmall/shmmni
  在 owning IPC namespace 限制 admission；使用标准 unsigned long/int sysctl codec。
  SHM_INFO 聚合实际 backing 的 O(1) resident/swapped 投影；实际 PageState admission、physical replacement 和最终 Drop 更新同一账本，不再逐页扫描或保存第二份页状态。
  benchmark 决策：fault/swap/futex 复用原共享 backing，无新层；冷 VMA open/close 增加一次
  registry/segment transaction，split 只为非空 fragment 计数，纳入最终 VMA/IPC 性能门禁。
  SHM_LOCK/UNLOCK 使用实际 backing 的唯一 unevictable
  policy；所有共享页 swap admission 持同一 gate，LOCK 等待在途迁移完成但不 fault-in。
  real UID 的 locked-page quota 属于实际 UserNS/祖先 ucounts，当前限额在 policy 等待后
  重新读取；initial CAP_IPC_LOCK 才豁免配额，child capability 只决定 target IPCNS 控制权限。
  重复 LOCK 不重复收费，UNLOCK/destroy 归还原付费者；统计 mode 从实际 policy 投影。
  policy 等待/charge preparation 不持 registry/segment，最终退休在短 guard 外。
  benchmark 决策：private/file swap 不增加 policy gate；shared swap 增加 Weak owner pin 和
  try gate，纳入最终 swap/VMA 成本验证；在途 IO、quota rollback 与锁后换入保持锁定均有新增回归。
  SharedMemoryCreatorList 的真实 clist 属于 creating Thread，fork/CLONE_THREAD 不继承，
  exec 保留，实际 exit 显式退为 None；TCB/list observer Arc 不延长 creator phase。
  segment 仅保存 Weak clist identity，clist 中的 namespace/segment 也为 Weak；双方不会
  循环保活。publication 在 registry→clist guard 内使用预留节点，destroy 摘下同一节点。
  creator retirement 先退出全部 membership，再在 list guard 外执行实际 namespace cleanup。
  shm_rmid_forced 的启用以有序 slot cursor 扫描实际已 orphan、未挂接的 segment，
  既不分配全表 snapshot，也不以 PID reaping/observer 生命周期推断 creator exit。
- `ipc::semaphore::SemaphoreSet` 独占 array values/历史 PID/otime/ctime、有序操作 admission
  和 SEM_UNDO 交叉注册；sembuf 仅复制一次，多项/重复 index 原子执行，任何阻塞/范围失败
  倒序回滚全部临时 value/semadj，PID/time 只在成功后发布。array→operation→adjustments
  为锁序；等待计数由实际首个不能完成的 operation 推导，零等待与负操作分别计数。
  value 更新推进能完成的既有操作，复杂操作成功后重扫依赖。完成值与取消在 array guard
  下串行，signal/RMID 后已完成操作仍返回结果；semop 的捕获信号始终 EINTR。
- SemaphoreRegistry 的 used ledger 只计实际 ID admission 的总 sem 数，用于标准 SEMMNS
  限额与 SEM_INFO；同一 guard 在 publish/RMID 时增减，观察者 Arc 不延长收费。semaphore
  array 的 payload/PID/等待列表仍在唯一 SemaphoreSet，不另维护全局 mirror。
- `ipc::sem_undo::SemaphoreUndoHandle` 是实际 Thread participant，CLONE_SYSVSEM 增加
  shared list 的 participant 数，普通 fork 获得空 list；operation 的 Arc observer 不计入。
  最后 participant 退出才原子取走全部 actual entries，再按 actual exiting TGID 夹限到
  0..SEMVMX 并推进等待。SEM_UNDO 的 semadj 只存于一个 SemaphoreUndoEntry，list 与 array
  以同一 Arc 注册；反向 edges 为 Weak，不延长 namespace 或 Thread。唯一交叉锁序 array→
  list；list detach 先移出 entries/释放 guard，再进入 array。RMID 把双方 node 摘下并放入
  预留 retirement 字段，所有 Arc/node/PID/vector Drop 与事件通知均在 registry/array guard 外。
  SETVAL/SETALL 清相应所有 semadj；无副作用 constructor/clone rollback 可归还空 membership，
  实际退出遗漏显式带 PID 的 retire 则 fail-stop。Thread 新增 membership 只在冷 constructor
  准备，非共享 fork 分配不带 Thread guard；unshare/setns 全部 preparation 后先 retire 旧
  SEM_UNDO，再 publication/退休旧 IPC namespace，exec 保留实际 membership。
  benchmark 决策：新增 Arc/token/vector 只用于一次实际 operation/preparation；value/undo/
  waiter 变化不分配，PONR 退出使用 undo entry 已预留的 PID retirement vector。实际 owner
  回归覆盖原子提交、争用与取消，architecture-bench 测量 ID lookup、权限、GETVAL、
  NOWAIT 拒绝与含冷分配的多操作完成；热路径沿用既有阈值。

- 标准 `/proc/sys/kernel/sem` 读取四个实际 signed int，以 tab 分隔。IpcLimit selector 与
  weak inode target/strong open-fd pin 复用同一 IPC sysctl lifetime，不增加 namespace 内容副本。
  proc_dointvec 接受 base-0、负整数、部分向量与 accepted prefix；后续解析错误返回 EINVAL
  但保留已接受字段，semmni 越界返回 ERANGE 且只保留该项旧值。前三项按 Linux 接受任意
  int；负 semmsl 拒绝创建，负 semmns 无可用 admission；semopm 按原 unsigned argument 比较。
  scalar/vector 共享实际 write buffer 的完整 user fault 检查、first-page 截取、strict offset
  与 consumed-prefix；不通过全事务写掩盖标准部分效果。namespace guard 内发布整个 accepted
  prefix，权限仍要求 owning UserNS 映射 root EUID。host/guest 用例已补写。
- SemaphoreSet 的 sem_nsems 是不可变 metadata，不依赖 RMID 已退休的 values Vec；已有
  syscall preparation/undo storage 因此不受 payload 退休改变大小，count query 无 array lock。

- SHM_LOCK 与 realtime signal 的收费统一使用 namespace::RlimitCharge/typed RlimitResource，
  actual UserNS 的 (resource, real kuid) AVL 是唯一 ledger；MEMLOCK 与 SIGPENDING 不混用。
  child UserNS 的 immutable maxima 取创建时同一次 Process limits snapshot。计数 entry 在
  guard 外预备，零 entry 摘除后锁外析构；失败按实际成功 prefix 回滚，final token Drop 只
  退还原 UID/ancestor chain。共享内存锁定仍使用 page 单位，ancestor maxima 保留 Linux 原值。
