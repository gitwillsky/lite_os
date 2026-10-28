# 文件系统与存储契约

FUSE（`fs/fuse`）及其引入的 VFS 接口见 [FUSE 契约](fuse.md)。

- ProcNode 的 NamespaceSelector 区分 active time 与 time_for_children，两条链接都打开实际
  TimeNamespace 类型的同一 nsfs owner；readlink 显示真实类型 time，不能据显示名重建 membership。
  timens_offsets 的 per-open 只 pin 实际 proc inode/source 与 opener f_cred，读写都重新捕获
  target Thread 的 for_children，target 退休返回 ESRCH；不长期 pin TCB 或旧 namespace。
  codec 先完整 user-copy/解析，再按 f_cred 授权批量 publication，失败不消费、不部分发布。
  Linux kernel vsscanf 的 numeric prefix/wrapping 与两行 consumed-prefix 保持固定基线语义。
  proc filesystem 固定 mount 时实际 PID namespace；目录名、self 与 stat/status 按该 view
  投影，NS* 列从 mount view 到实际 leaf 编码不可变 ladder。sysctl 查找另捕获 caller 的
  actual user/PID namespace；既有 fd 不随后续 setns/unshare 重定向。

## Owner

- PIDFS 是独立 internal superblock/mount（PIDFS_MAGIC），同一实际 PID 的 LiveInodes Weak
  admission 共享唯一 inode/metadata/source；不是 anon_inodefs singleton 或专用 OFD variant。
  PidFdTarget 只提供 typed association poll、namespace selection/auth 与 native identifier snapshot；
  所有 PIDFS ioctl raw codec 在 pidfs/ioctl，Task 不读取 IoctlCall/user buffer。
  GET_INFO 只读 input mask，按实际 caller PID view 检查可见性，老结构裁掉无法容纳的字段 flag，
  新结构尾部清零。readonly inode observer 不保活运行资源；历史 exit 信息只由实际 retirement 写入。
  metadata 使用第一位 creator 的 FSUID/GID，mode700 不含 S_IFMT；mutations 保持 inode identity。
  creation/ioctl 属冷路径；poll 使用实际 graph lookup 和既有 pipe readiness protocol，相关源码、
  owner host/native guest/negative fence 属于集中门禁。

- O_PATH 只消费同一 OFD allocation 和 OpenedFile，不执行 backend open、writer admission、
  truncate 或 OPEN/CLOSE。status flags 是唯一 FMODE_PATH 投影；普通 fd_get 排除该能力，
  fget_raw 只供 native path/属性/dup/F_GETFL 操作，不绕过 I/O、ioctl、pidfs fops 或 SCM owner。
  O_PATH 忽略其他 access/create/status bits，保留 DIRECTORY/NOFOLLOW/CLOEXEC 的原生含义。
  fdinfo 在单一 table guard 内捕获实际 OFD/CLOEXEC，position/status/format 在表锁外完成；
  `chardev::FileInformationView` 仅命名该 proc mount 的 PID 视图类型，使设备 fops 不必依赖 namespace 实现路径；
  FS codec 输出实际 mount/inode，设备仅写其真实 fops 字段。PIDFS Pid/NSpid 采用 proc mount
  的 PID namespace 和不可变 ladder，reap 后为 -1，无关 view 为 0。全部 anonymous path 也借真实 pseudo internal mount 输出稳定 mnt_id；

- Proc/nsfs live inode identity 由 `LiveInodes` 的 ordered Weak admission 唯一拥有；candidate、
  AVL node 锁外准备，winner 锁内复查，loser 与摘除节点锁外析构。实际 inode Drop 精确比较
  allocation identity 后撤销 membership，迟到的旧 inode 不得删除新 winner；无 lazy sweep。
  LiveInodes 使用完整 ordered key：nsfs 为 namespace inode number，proc 为 ProcNode 与
  sysctl namespace ID，不位移拼接 PID/FD、不截断高位。ProcIdentities 独占每挂载 inode 号
  AtomicU64，Relaxed 仅保证唯一签发，不发表内容；溢出返回 ENOSPC，loser 空号不复用。
  proc 每挂载独立 registry，nsfs registry 不强保活 namespace。Snapshot backend pin 实际
  ProcInode，nsfs backend 只持 immutable NamespaceReference，不复制 inode attributes。
  readdir/lookup 使用同一 live admission，cursor 之前的 entry 不准入、不分配；新 entry 的
  分配成本需要在最后实际 owner benchmark 评估，不新增或降低性能阈值。
  kernel/pid_max 为 actual active PID namespace 的 0644 control，inode ownership 映射 owning
  user root；open 与每次 write 按 EUID/root 或 SYS_ADMIN 检查，限制由 allocation owner 同锁更新。
  ProcSnapshot 只 pin 不可变 actual PID identity，不保活 namespace 编号 registration；显示历史
  identity 不反查已复用的编号。
- nsfs inode独占canonical magic dentry的Weak slot；FD与bind view都保活同一实际dentry，
  关闭所有FD但bind仍活着不能重建path身份。winner复查只交换Weak，loser/retired Weak锁外释放。
  warm admission不重建诊断标签，仍按每次open分配唯一path shell；不缓存第二份mount或inode属性。
  anonymous/filesystem 是 pipefs/sockfs/anon_inodefs/hidden shmem/nsfs/pidfs 的唯一内部创建 owner；
  按显式规范类型选择各域的 PseudoMount，filesystem Once 持已登记 MountedFilesystem/空 root，
  mount Once 持同一 FS 的唯一 internal mount root path。内部root为固定init_pseudo的DIR0600/link1，创建时记时间，
  没有directory file operations或NamespaceReference；全局owner不pin用户namespace/quota。
  FS先准入后View构造失败可异步退休；retry必须借同一已登记FS，不能重复登记FSID11。
  VFS的internal_mount是 pseudo filesystem 唯一scoped factory，scope ID0只用于未加入任何namespace的内部view，
  无forest/index/quota/count admission，flags0；writer与退休仍归真实MountView/FS owner。
  `Dentry::anonymous`和`OpenedFile::with_dentry`仅额外供 pseudo filesystem 重用实际magic dentry与internal mount；
  普通dentry关系/mutation仍private于VFS。nsfs noexec是immutable inode permission，chmod/bind
  remount不能解除。cold首次构造成本以OOM/lifetime/alias deterministic cases覆盖，不添加wall-time门槛。
- tmpfs的xattr内容属于实际inode State的Xattrs.entries，不属于MemoryFile内容页或VFS旁表；
  名称/值只有owned bytes，借已有TaskMutex和regular write sequence与metadata/killpriv串行。
  change接受已经copy的value Vec并直接移交，Some(empty)保存空属性、None删除；CREATE|REPLACE
  同时设置时依实际存在状态返回EEXIST/ENODATA，不能改成统一EINVAL。OOM发生在发布新节点前，
  旧值及ctime不改变；成功后的旧buffer锁外释放，capability去权限路径不再分配。
  trusted list过滤使用当前caller在initial UserNS的SYS_ADMIN；不以child-local capability冒充。
  非零security.capability的删除不受FSETID豁免；chown按固定非directory ATTR_KILL_PRIV处理，
  空值按cap_inode_need_killpriv保留。xattr是cold syscall域，ordered borrow lookup无name拷贝；
  写路径增加的killpriv查询必须纳入最终existing regular-write性能验证，门槛保持原值。
  通用syscall权限/namespace转换、ext4存储与exec consumer已接入源码；
- memfd的实际inode由MemFileState独占OwnerModeState/Xattrs及实际MemoryFile时间owner引用；
  原pseudo Metadata不再持memfd身份，也不保留内容killpriv入口。state使用task-only TaskMutex，
  不在pseudo spin内进行属性分配；锁序write sequence→state→times，内容页与seal仍归MemoryFile。
  resize在页发布前取得state guard，allocation预留metadata waiter后才发表页；提交killpriv不再失败。
  每次非空写/resize/allocate与chown移除实际非空capability，FSETID只影响mode，后台writeback独立。
  cold属性查询/写入按同一XattrChange授权；memfd hidden shmem path没有current mount membership，
  exec不从anonymous hidden mount获得set-ID/file-cap权限。热memory write的锁变化纳入最终既有
  regular-file成本/guest验证，未降低任何性能门槛。
- `XattrChange`是一次mutation的当前caller、actual SB creator namespace与owned value请求，
  不保存inode身份或已授权flag；tmpfs在write-sequence→State gate、ext4在mutation gate内，
  读取唯一实时owner/mode，依序转换非空file-cap、检查immutable/append、DAC、mapped IDs及security
  namespace权限，再改变属性/ctime。memfd同样使用sequence→MemFileState；默认不支持xattr的inode仍授权后返回EOPNOTSUPP。
  AccessIdentity的同一DAC算法同时接受metadata和锁内OwnerModeState，不能递归metadata取锁。
  trusted按initial UserNS SYS_ADMIN；其他security按SB UserNS SYS_ADMIN；非空cap设置和cap删除
  按inode UID/GID映射的SETFCAP。空cap设置遵循fixed commoncap特殊路径，不伪造capability权限。
  OpenedFile只取得actual mount writer并在成功后通知IN_ATTRIB；syscall不读取inode状态或自己授权。
  syscall统一set/get/list/remove与nofollow/fd/at入口，标准16-byte xattr_args扩展区必须为零；
  名称空/超长ERANGE，path missing ENOENT，attribute missing ENODATA；copyout失败不改变属性。
- ext4 xattr唯一持久owner是实际inode body、i_file_acl 指向的block及原生 e_value_inum 值 inode；已有mutation TaskMutex同时
  串行属性读取和journal writer，不新增cache/map/lock。仅一次mutation工作集保留原body优先位置，
  打包后丢弃；大值分配与打包容量失败均由 working disk、分配计数与block overlay
  按现有MutationGuard恢复。external唯一引用就地写，多引用先COW再减少旧引用；每次更新重算
  entry/block hash与metadata checksum，i_blocks和inode pointer同事务提交。body offsets按first entry，
  external offsets按block start；先校验整个entry终止区，再校验value范围，禁止损坏磁盘越界读取。
  EA_INODE 的 hash 属 atime、64-bit 引用属 ctime/version，真实 extent tree 是唯一 block mapper。
  父 inode 的 i_blocks 按每个 EA value 的 block-rounded 长度计费，修改/释放与物理引用 delta 同一 journal transaction 提交。
  value blocks 以 Metadata 入 journal，不能走 ordered data；正引用先于负引用变更。
  body 每 entry 持有一引用，external block 每物理 entry 持有一引用，不能乘 h_refcount；
  共享 block COW 增加新物理持有，只有最后一个旧 block holder 才释放值。最后 EA 引用在同一
  journal transaction 回收 extents/inode bitmap；不发布普通 VFS inode、不新增 cache 或引用旁表。
  新镜像原生 ea_inode feature；旧 feature-off 卷沿同一实现保留容量限制，不自动升级。
  现代 unsigned hash 格式纳入实现，legacy Lustre backpointer/signed-name hash 尚未纳入范围；
  ACL 仍无实际权限 owner，不把 system 属性当无权限含义的普通bytes。普通相同值不写 ctime；
  EA_INODE 相同值沿原生更新路径写父 ctime。remove syscall 使用 REPLACE，缺失返回 ENODATA。
  无xattr的write/chown killpriv不分配，不读external block；后台writeback不冒充用户去权限。
  disk codec、临时工作集与冷syscall不增wall-time gate，最终以实际e2fsck/remount/concurrency及既有
  ext4成本/regular-write benchmark验证；测试源码已补充。
- generic proc registration 是 mode/UID/GID 的唯一 owner，跨挂载/最后 inode close 保留修改；
  live inode 只另持独立时间，不复制注册 mode/owner。PID/sysctl inode 持自己的 owner tuple，
  全部属性 snapshot/mutation 锁序固定 owner→times。PID lookup 经 task_dump_owner refresh，
  getattr 投影 live owner；已退出 Task 的已有 inode 仍可 stat，UID/GID 为 global root。
  除精确 0555 directory 外，nondumpable inode 归实际 MM namespace 的 root，无映射归 global
  root；FS 只接受 task scalar projection，不反查 MM。proc_nochmod_setattr 仅拒绝 PID chmod，
  sysctl 另拒绝 chown；普通注册 entry 允许标准 mutation。truncate 只更新 mtime/ctime，
  不修改固定 i_size 或控制值。Proc fd inode 编号完整包含 20-bit FD 与 30-bit Task ID。
  nsfs 的 owner/mode/time/size 由实际 inode tuple 依 generic simple_setattr 语义更新，
  namespace identity/ioctl target 不变。Snapshot backing 的 truncate 交给实际 inode policy，
  不能按 backing kind 一律 EROFS 或仅对 size==0 开例外。ftruncate 按实际 regular inode 类型
  准入，OFD 的 DeviceFile backend 只选择 I/O 操作，不能用于拒绝 proc regular file 的 metadata 操作。
  这一 admission 属于 cold lookup/open；使用既有 AVL O(log n) 操作，warm live lookup 不分配，
  以 alias/race/lifetime 的 deterministic regression 为 gate，不新增 host wall-time benchmark。

- Inode.require_access 是唯一实际 permission operation；默认 inode DAC，proc user sysctl
  使用 lookup 时绑定的 user namespace 与 CAP_SYS_RESOURCE，stat 仍为固定 REG0644。
  user sysctl directory/file inode numbers 由实际 namespace identity 限定，fsnotify不能跨namespace混发；
  directory fd 保持原 scoped table，后续 child lookup 继承该 owner。
  `/proc/sys/user/max_{user,uts,mnt}_namespaces` 打开 fd 固定 target namespace；后续 writer 重新
  授权该 target，不能随 unshare/setns 转向另一个 quota owner。base-0 unsigned codec 与
  固定 sysctl_writes_strict=1 属于 proc，quota limits/counts 不存入 inode/OFD。
  AccessIdentity 分别捕获 DAC UID 与 capability EUID；setfsuid 不能借 ancestor owner 特例。
- VFS namespace/inode 拥有目录关系；每个 OpenFileDescription 唯一持有 Arc<OpenedFile>，并拥有 backend、file position、status flag 与 descriptor reference consequence。所有 fd metadata、proc target、attribute、statfs 共用该真实 inode，Kind 不复制
  opened，不另设 memfd 分派。
- `LookupOrigin` 是 caller fs root/start 的不可变 opened-entry capability；全部 user pathname
  open/create/mutation、mount/source、swap、AF_UNIX、exec/script/PT_INTERP 传同一 typed origin，
  禁止 Option start 隐式 global root。仅 boot 显式 initial_origin，同一 traversal 无旧 resolver。
  绝对 path/symlink 从 origin root 开始；magic link/dirfd 只替换 start。dotdot 先判断 caller root
  再沿实际 mount attachment 向上；每个中间 root 都重新 clamp。chroot 外已有 cwd/FD 在其 tree 顶端保持，普通 file 的 dot/dotdot 返回 ENOTDIR。
  `OpenedFile` 在真实 parent/name 链上投影 root-relative path，不缓存字符串；getcwd 不可达前缀
  为 `(unreachable)`，proc 路径不可达仍是全局链，deleted suffix 规则独立。path identity 必须同时比较
  actual mount view 与 canonical dentry，不按 inode ID 把 bind alias 或不同 namespace root 当成相同路径。
  Proc magic link source 供给 viewer root；mount table source 原子供给 TARGET mount namespace 与 TARGET FS root。lower VFS 无 Task 依赖。
- `NamespaceAccess` 是既有 namespace TaskMutex 的唯一 RAII capability；普通 lookup、
  find_child→DentryIndex.admit、mutation、整条 parent/name path projection 以及 mount/umount
  同锁串行。mutation 借同一 capability traversal，不重新取得 gate。Canonical dentry location facts
  每 dentry 单锁 snapshot，所有 Vec reserve 在该短锁外。禁止 entry admission 越过 rename。
- Proc fd snapshot 只持真实 `Arc<OpenedFile>`，不保留 label/Text lookup 路由，不在 enumeration 中取得
  namespace/projection。组件上限 255 byte，d_path/getcwd 包含 NUL 上限 4096 byte，
  超限统一 NameTooLong→ENAMETOOLONG。Inode::link_target 一次返回 LinkTarget::Text/Opened，错误直接传播；
  不先试 magic 再重读文本，避免 fd 重用/OOM 后的目标类别变化。fd producer 保留 allocation
  error 与已退休 None 的区别。mount source 是 immutable Arc；mount-table snapshot 与 mount
  topology 增长容量在短锁外 prepare，已有 mount records/attributes 在原锁内无分配移动。
  Proc fd link i_size 固定 64，root/cwd/exe 与 snapshot 为 0，
  不调用 read_link；readlink 在 namespace projection transaction 中使用 viewer root。因此 lookup
  已持 gate 时不会经 proc metadata/fd enumeration 递归取得同一 TaskMutex。
- OFD status lock 保持 private；syscall 只经 `status_flags` 查询，`set_status_flags` 更新当前
  F_SETFL 支持的 O_APPEND/O_NONBLOCK，`set_nonblocking` 处理 FIONBIO。后者保留所有其他 flags，
  与 F_SETFL 在同一临界区串行化；dup/fork 不复制状态 owner。
- `DentryIndex` 是 filesystem dentry 的 exact ordered Weak lifecycle index；key 为 parent inode/name/inode，
  不包含 mount namespace 或 opened allocation。candidate/node 锁外准备，winner 共享同一目录关系。
  `Dentry::drop` 精确撤销 membership，rename retoken 原节点，unlink 移除节点并更新实际 dentry；
  禁止重复 parent/name/deleted facts、alias 扫描与 lazy sweep。父链最终释放使用常数栈。
- `OpenedFile` 只拥有 optional actual MountView + canonical Dentry。匿名对象明确无 mount，标签
  不作为 pathname；normal pathname 不允许用 filesystem ID 猜对应 mount。旧 root/cwd/FD 的实际
  view 生命周期不随 Task namespace 重定向。writer/noexec/nodev/nosuid/statfs 均消费该 actual view。
- `MountNamespace` 只强持唯一 forest root；每个 view 强持实际 children，placement 弱指 parent，
  因而没有父子 Arc 环或另一份 topology Vec。`MountIndex` 只拥有 exact Weak membership。
  publication/move/clone 在 VFS gate 下预分配全部节点与 child capacity，再无失败提交。
  copy 共享 filesystem/dentry、创建新 view identity；clone FS 值先冻结，再于同一 gate 重定位 root/cwd。
  `mount_copy::copy_tree`唯一构造复制view；namespace/bind/传播选择同一parent-first实际输入树、
  root/attachment和immutable复制policy，临时fallible correspondence只保活本次准备，不继续拥有
  topology。`MountView::snapshot_subtree`统一枚举真实child owners，禁止全局index冒充source树。
  固定Linux may_copy_tree对普通bind source要求当前MNT namespace；保留的foreign dirfd可以
  读旧路径，不因此授予跨namespace的普通bind准入。namespace-file特殊复制与loop准入单独核对。
- `mount_publication::Publication`唯一提交new/clone/move/堆叠位置：prepared key排序检查冲突，
  只允许本批moving owner让出旧key；prepared父图复查live/同批incoming父节点及完整最终图无环。
  Gray/Black的fallible memo与edge lookup仅属于cold事务，保持常数栈且不加永久visited状态；
  所有OOM/无效图在效果前返回。容量按旧数减removals加additions准备，必须先批量移出全部旧
  owning edges，再批量插入新位置和incoming；逐项搬移的峰值不能由net capacity保证。
  PreparedShared只拥有本次事务未来group identity，secondary准备可借它，实际关系提交前不改变
  live source；全部group容量包含live计划成员与incoming。旧attachment移交原预置退休payload，
  event按实际scope在完整提交后发表一次；不引入反向namespace引用。该图检查只在mount事务执行，
  不加入pathname/read/write热路径；最终验证评估cold复制/挂载成本，既有热路径阈值保持。
- 传播关系由每个实际 View 的 Propagation 独占：可选 PeerGroup、弱 master 与双向弱 slave links，
  unbindable policy。PeerGroup 只拥有不复用group ID和exact Weak成员Vec，不强持view/master；
  NEXT_GROUP_ID 只分配identity，不发布内容，耗尽/OOM在关系提交前失败。所有关系变更持VFS gate。
  clone 的未发表view保存一次性copy_origin意图，全部parent/index/group容量成功后才消费登记；
  peers和继承slave都插在真实source之后，lower-user Shared→Slave插source的slave链头。
  不把独立nonshared slaves合并成peer group。MS_SHARED整组预分配；其他属性无分配转接owned
  slaves/注销旧peer与master，MS_SLAVE保留Linux已有unbindable位。namespace最后Drop仍保活
  实际root至退休；detach先转接/注销传播关系再释放forest edge，弱master始终有实际forest保活，
  不存在强master递归Drop。短锁只修改已预备成员/弱边，最终Arc/Weak Drop与wake在锁外。
  mountinfo 从实际关系渲染shared/master/unbindable；propagate_from逐master、逐peer检查目标
  namespace与实际FSroot可达性。per-view RO与SB RO分别输出，不能把superblock-only变更写回view。
  传播关系修改/展示是cold VFS transaction；不新增pathname lookup或普通读写的锁/分配。
  `mount_attachment`统一new/bind/move接入：先pin已有接收者，逐实际master/peer选择可见源copy，
  所有secondary tree在primary之后准备/发表，foreign UserNS锁定继承属性与子边；old overmount
  的placement及隐藏锁职责与new trees在同一Publication提交。`mount_unmount`在cold事务用
  fallible exact索引选择最大non-shifting/non-revealing集合；独立child阻止上游卸载，surviving
  root-overmount重接到首个存活祖先，不能按group整棵误删。普通卸载等待selection最终状态后、
  新增placement pins前证明zero-external的实际view既有completion；合法FD租约不被等待或提前释放。
  所有selection/index/容量/completion准备在效果前完成；
  MountScope的Atomic live count仅由实际index admission与统一namespace retirement各计一次；
  move换registration但不重复计数，new/propagation逐scope在effects前检查标准fs.mount-max。
  其全局AtomicU32 policy独占于mount_limit，默认100000、sysctl范围1..INT_MAX；Relaxed不承担
  forest publication，VFS gate仍串行实际membership。降低limit不撤销既有view、不阻止纯move/umount。
  proc全局sysctl的test_perm按当前GLOBAL_ROOT_EUID选0644 owner bits，不以FSUID或DAC_OVERRIDE
  绕过；open拒绝EACCES，已打开FD的当前writer复查拒绝EPERM。无额外Task/凭据snapshot owner。
- bind普通source要求调用者实际namespace membership；typed nsfs inode可借内部/foreign实际view，
  仍走同一copy_tree/Publication，不从path字符串识别、不补造FS或mount标志。mount-namespace
  文件只可pin较晚创建的namespace，否则bind返回EINVAL、move返回ELOOP；move检查全部选中实际子树。
  namespace clone与传播secondary按CL_COPY_MNT_NS_FILE=false剪掉Mount类型文件及其overmount
  子树，User/UTS/Time文件保留；bind允许复制Mount文件。root命中禁止复制时返回EINVAL且不发表任何效果。
  最短未剪overmount的actual root dentry即shorter mountpoint，covered view按该真实边重新连接；
  clone relocation与pruned source采用同一序列，不按全树zip错配保留的FSroot/cwd。
- Namespace 与 view final Drop 只移交构造时预留的 actual payload/queue node，不取得 sleeping gate、
  不修改索引、不分配。`mount_retirement` 是唯一退休 owner：既有 gate admission 与独立 boot worker
  清理精确 registrations/actual edges，资源 batch 在 gate 释放后析构。worker 使用预留 waiter，
  不依赖下次用户 lookup，不反入完整 MOUNT_TRANSACTION。遍历与 queue 消费保持常数栈。
  MNT_DETACH 断开普通子边，只保留父也退休时的 locked/inseparable 边；旧 FD 只保活其实际 view。
  gate 冻结 actual edge 修改与活 namespace topology；已 detached 的 weak parent 可以锁外失效，
  每次 path walk 的成功 upgrade Arc 只证明本次 parent 生存期，不能宣称旧 tree 永久稳定。
- Path 字段析构必须 dput 在 mntput 前；MountPayload 成功退休与 constructor OOM 都必须先释放
  placement/children/root 再释放 actual filesystem。Attachment 持 mountpoint 所属 parent filesystem
  的真实 lease，确保最后 point inode cleanup 时物理 claim 尚未归还。normal umount 等待 final
  resource completion 后返回，lazy detach 不等待旧 FD。禁止提前 shutdown bind/unlinked inode。
- `MountedFilesystem` 唯一拥有 adapter shutdown、physical MountDeviceClaim、immutable 创建者 UserNS，
  shared superblock RO/writer count 与 sleeping admission gate；copy/bind view 不重复创建 adapter/claim。
  同设备新 mount 通过 actual filesystem Weak registry 复用相同类型/RO/owning UserNS 的 owner；
  不同类型/RO 拒绝 EBUSY，UserNS 不同拒绝 EPERM，类型选项仍须校验。每个 view 的 source/mnt_devname
  独立，namespace copy/bind 共享其 immutable source Arc，不能把 superblock 的首个 source 当全局真相。
  per-view flags/writers/restrictions 在 ViewState 一锁拥有。普通 remount 还要求 superblock UserNS 的
  CAP_SYS_ADMIN；bind remount 只更改 view。backend 全部准备成功后发表 flags，禁止临时 RW 与 rollback。
  新 UserNS copy 继承不可清除的安全 flags、不可拆分 attachment；旧 namespace 的 privileged remount
  不能以低权限副本的 owning UserNS 替代实际 superblock owner。
- MountScope 不反向持 namespace topology/charge；它拥有 namespace identity、UserNS、change generation
  与合并 notification Pipe。topology change/remount 一次更新 event 并唤醒；proc per-open 自己记录
  seen event，poll 比較 actual event，mountstats 使用普通 regular-file poll。通知 token 不复制 topology。
  `/proc/mounts` 是 self/mounts symlink；PID/thread mounts/info/stats per-open pin target namespace/root，
  三种格式由一个 actual-topology producer 渲染，chroot 外的 mount skipped，不输出 unreachable 伪路径。
  mount ID 每 actual view 分配且不复用；mountinfo 的 parent、filesystem root、dev_t 来自 actual owners。
  新路径/锁成本沿 existing root lookup benchmark 与新增拓扑 regression 评估。
- `FilePosition` 是 OFD position 的唯一 lock owner：sequential read/write、`lseek` 与 `getdents64`
  必须在单次 `with_position` 临界区内完成依赖 position 的完整操作；`sendfile` 必须通过
  `with_positions` 的稳定地址全序取得两个不同 OFD，禁止 syscall 直接取得 raw position lock。
  position 使用 TaskMutex，完整 I/O 可等待文件系统/MM；spin 会阻止同 CPU 的持锁任务恢复。
  等待分配失败返回 ENOMEM 且不进入 I/O；双 owner 第二次获取失败释放第一把锁，不发布半次操作。
- FileDescriptorTable 独占 slot、FD_CLOEXEC、reservation、publication 与 lowest-free index；fd slot 使用
  heap-owned 7/7/6-bit sparse radix，fullness summary 只能由同一 occupancy transition 更新。lookup、
  replace 与 take 固定三层，iteration/fork 只访问 materialized branch/chunk，禁止按最高 fd 物化或复制
  dense `Option<FileDescriptor>` table。RV64 reviewed payload 为 table inline 24 B、root/branch 各
  1040 B、64-slot FileDescriptor chunk 1024 B；仅 fd 0 与 1,048,575 时 heap payload 为 5168 B，
  全物化时 metadata/chunk payload 上限分别为 134,160 B/16,777,216 B（不含 allocator header）。
- ext4 owner 独占 inode/directory/link/allocation mutation；packed disk value 定义与字段保持
  `fs::ext4` parent-private，`fs::ext4::layout` 只封装定长 decode/encode 与 raw byte access，
  `fs::ext4::block_io` 封装 filesystem/device block 换算，`fs::ext4::inode` 独占 inode identity 与
  VFS projection，`fs::ext4::extent` 独占 logical-block mapping，`fs::ext4::htree` 独占 dx index，
  `fs::ext4::metadata_csum` 独占全部 metadata checksum 公式；`Ext4FileSystem` 的 64-entry metadata
  block cache 独占 directory/htree/extent/orphan block identity 与 LRU reclaim；JBD2 journal 独占
  transaction/commit/replay；page cache 独占 cached page lifecycle。
- ext4 写路径固定为 Linux `data=ordered` + group commit：每次 mutation 的 handle overlay 独占本次
  staged metadata/data、释放的 block range 与 allocation dirty-group bitset；bitset 必须在 handle 发布前
  按 group count fallible reserve，OOM 不得开始 mutation。`MutationGuard::commit` 一次性物化 primary
  superblock 与每个 dirty descriptor block 后，以只移动 tree node 的方式把 overlay 原子并入唯一
  running transaction；并入后 mutation 对内存可见且不可回滚，并入前的任何失败都只丢弃 overlay。
- ext4 首次挂载和 remount 共用固定 profile 的选项解析，接受自身报告的 `data=ordered`，拒绝
  未支持的 data 模式与未知 key。`FileSystem::remount` 在后端内先校验再进行 ro → rw 准备；
  `Ext4FileSystem.writable` 仅在 superblock flush、orphan 回收及一致性验证全部成功后发布，
  I/O 失败保留未完成状态，重试不能跳过实际准备；VFS admission 串行该转换。
- 每层 `StagedBlocks.entries` 对同一物理块只保存一个 image/classification；metadata credit aggregate
  与节点由同一 owner 更新。目录块的初始化本身就是 metadata，不能另存 ordered-data 零副本；
  换型、跨层合并和 OOM/credit 失败均保持单个 image。
- running transaction 独占全部未提交 metadata、ordered data 与未提交释放 range。metadata 经 JBD2
  原子提交；data 不进 journal，必须在 commit record 之前写回 home 并共用同一 durability barrier。
  未提交事务释放的 block 在提交前不得重新分配，否则崩溃后旧文件会指向新文件已写回的数据。
- running transaction 只在以下时机提交：handle 并入后 metadata 超过单个 journal transaction 容量
  （先提交 running，再并入）、staged data 超过 16 MiB、年龄超过 5 秒、`fsync`/`fdatasync`/`sync`。
  每个 ext4 filesystem 在 mount 时创建一个写回内核线程（对应 Linux jbd2）：running transaction
  由空变非空时经 `TaskEvent` 唤醒，睡到开始时刻加 5 秒后在 mutation owner 下提交；没有未提交
  mutation 时线程无限期阻塞，不产生周期唤醒。mount 发布前预备唯一 shutdown waiter；
  `shutdown` 消费它无分配取得 mutation owner，同锁先发布 `stopping` 再最终 drain 并 signal。
  daemon 醒来取得 mutation owner 后复查停止，新的 mutation/sync 也在该 owner 下拒绝；
  drain OOM/I/O 失败后不能在已释放物理租约上恢复写回。shutdown 在 mount lifecycle 恰好一次。
- `fs::mount` 的 `MOUNT_TRANSACTION` 串行化完整新实例 mount/umount 事务；source/claim 在
  adapter create 前准备。实例发布失败与最后 owner Drop 都走实际资源 retirement，不制造 dummy adapter。
  所有 umount 分支先以同一 VFS gate 校验 caller membership、真实 mount root 与 inseparable restriction。
  普通子 mount 有子 attachment 或 actual FD/cwd/root/mmap path pin 时 Busy；caller FS root 的普通
  umount 转为实际 superblock RO，不覆盖 per-view flags；绝对 namespace root 不允许 detach。
  MNT_EXPIRE 只在两次连续闲置尝试后拆除；普通 path release 清除真实 view mark，本次 umount
  独占 path 的 release 保留 mark。MS_MOVE 在准备新 membership 后才摘旧位置，
  view ID 不变；MS_BIND 共享实际 superblock 和 source dentry subtree，创建新 attachment/view ID。
  recursive bind 整组准备再发表，非 recursive bind 不得剥离 locked child。Source 使用实际 mounted
  view，允许其他 namespace 的已有 path；target 必须属于 caller namespace。
  Ext4 构造显式接收实际 block dev_t，所有 inode st_dev/mountinfo 共用它；internal cache/filesystem
  identity 独立于 dev_t，禁止恢复固定 st_dev=1。DeviceNumber codec 属于 `fs::devnum`，host 加载实际 codec。
- `RegularFileWrite` 的 write-sequence 与 operation gates 共同独占一次 syscall 的 position、append placement、storage transaction 和 resident-cache publication 顺序。
- VFS namespace read/mutation 与 ext4 live-state transaction 使用 `TaskMutex` 逻辑 owner；其内部
  spin gate 只发布 `Available/Held/Handoff(ticket)` 与预分配 waiter 链，logical guard 可以跨
  block I/O 和 task handoff，但不得保留 spin guard。竞争 waiter 必须进入 scheduler `Blocked`，
  unlock 在短锁内把 owner 直接交给最旧 ticket、锁外 exact wake；禁止恢复 spin/yield polling。
- `AccessIdentity` 是 calling Thread credential snapshot；DAC_OVERRIDE/READ_SEARCH、FOWNER/CHOWN/
  FSETID与sticky predicate由permission唯一解释。UID0本身不豁免DAC/sticky；已开启的非owner
  writable FD可按ATTR_FORCE清除set-ID，不能复用chmod owner授权。`write_kill_mask`只clear权限位。
- `Inode` content write/append/truncate/allocate携带显式Some(caller identity)/内部None；普通write
  在非零与RLIMIT检查后、user-copy前commit killpriv，实际storage mutation重验live mode，
  因此后续EFAULT/ENOSPC不能回滚此前已提交的清位。Ext4每次data/allocation和最终size事务
  都用同一MutationGuard+inode preimage；writeback/orphan无caller policy。MAP_SHARED dirty沿
  固定Linux只更新时间/标dirty，不从fault caller或后台线程重新计算write权限。
- Memory facade保留canonical inode与MemoryWrite sequence token；TmpInode.content_mutation唯一
  更改mode，验证token属于实际MemoryFile。tmpfs chmod/chown也先sequence再metadata State；
  truncate/allocation metadata hook在相同sequence内，State/PageOp释放后再MM invalidate。
  零write保留set-ID，等长truncate仍清位；CAP_FSETID保留write/resize特殊位，chown规则独立。
- `page_cache::regular_file::RegularFile` 拥有单次 I/O 的 backend pin；CachedWrite 的
  write-sequence guard 留在 page-cache owner，read/backend selection 只经该 facade。
- `page_cache::registry::FILES` 独占 cache membership；node/Arc/reclaimer registration 在锁外
  fallible prepare，发布时重查 winner 与 backing policy。reclaimer 只持 Weak，竞争败者锁外
  析构，dead slot 可复用。sync/umount 每次只 pin 一个有序 owner，不在 FILES 内分配全局快照、
  执行 I/O 或析构最终 inode；并发新 mutation 不属于本轮 sync 的保证。
- page-cache `operation`/`write_sequence` 同样是可跨 cache fill、writeback 与 storage mutation
  保活的 task-only blocking owner；只有 resident page map 与全局 registry 的短临界区使用 spin。
- volatile inode 的 atime/mtime/ctime 只属于 `TimeOwner`：非 content inode inline，MemoryFile、tmpfs/MemFile metadata 与 resident mmap page 共用 Arc<InodeTimes>。短锁只保存三个 scalar，所有 clock reads 在 times 锁外，绝不反入
  metadata；metadata→times 是唯一锁序。content touch 一次更新 mtime/ctime；显式过去/未来 mtime 不改变 ctime 为该值，ctime 始终取变更时刻，不保留 max(mtime) 影子路径。
- 每个 `MemoryFile`（memfd 与 tmpfs regular file 共用）的 `state` 同时拥有文件长度、稀疏页与 seal mask；
  `ftruncate`/write/fallocate 和 `F_ADD_SEALS` 必须在该 owner 内线性化，禁止用独立 atomic seal 留出
  seal 与 resize 的竞态窗口。`write_sequence`（外层，整次 write/append/truncate/fallocate，持有期间会做用户
  态拷贝）先于 `state`（内层，page fault 只取它）；`published_length` 是 `state` 的无锁只读投影，只在持有
  `state` 时更新。页在最后一个 `Arc` 释放时归还配额。`allocated_pages` 包含所有已分配内容页，
  不等于 RAM residency。稀疏 page-byte seam 传播访问错误；单页错误不修改该页/目标 slice，
  已完成的前缀按短读写返回。truncate 先完成保留尾页的 fallible zero，再无失败移出整页/
  发布长度；任何前置错误保持旧长度与其他页。state/页 operation 释放后才广播 MM invalidation。
  每个内容页仅持 canonical SwappablePage，不保存物理镜像副本；read/write/zero 与 mmap
  通过 SharedPagePin 保活迁移 operation。文件发布前注册 weak 粗粒度 reclaimer，扫描
  在 state 内只选择 Arc 候选，释放 state/write-sequence 后才允许 background swap I/O；
  未映射内容也可迁移，配额/st_blocks 仍按 allocated_pages 计，不因换出归还。
- `StorageBudget` 的单个 IrqMutex state 拥有 tmpfs 的 page/inode limits 与占用；memfd 只使用
  无限页配额。reserve/release/set_limits 同锁，remount 对两类上限一起验证并提交，任一不足
  都保持旧值；statfs 读取同锁 snapshot。禁止独立 limit/used Atomics 留出按旧上限预留的窗口。
  固定 Linux `mm/shmem.c::shmem_reconfigure` 的 unlimited→有限 remount 必须拒绝；初次挂载
  的有限值由 constructor 设置，不经这一追溯规则。
  锁内只执行固定整数校验，不分配、不 I/O、不调用 scheduler；root.state/MemoryFile.state
  可先于 quota lock，但释放 quota 后才进行 frame/Arc allocation、子树拆除或字段 Drop。
- 挂载属性与“以写方式打开的 OFD 数”同在 VFS 的 `MountAttributes`，由 `mounts`/`root_fs` 锁一起保护：
  actual writer admission 与 `remount,ro` 的判忙互斥，OFD 的 `ContentWrite` 在 Drop 时撤销。读路径与 `read`/`write`
  热路径不取该锁。
- 块设备节点在 VFS 里只有一种视图：`OpenedFile::child` 经 `wrap_block_node` 发布 `BlockSpecial`；
  文件系统的块设备 inode 不实现设备 I/O。命名管道的 open 汇合等待 `PeerOpened`（对端累计打开次数），
  `Pipe.rendezvous_waiters` 使对端 open 即使不改变 readable/writable 也会走到唤醒路径。
- 分区节点只经 `PartitionDevice` 把块号平移到整盘的同一 `BlockDevice`，不复制任何状态；`FileSystem::remount`
  与 `Ext4FileSystem.writable` 共同保证写前准备（`RECOVER`、orphan 回收）恰好执行一次。
- inotify 锁序：实例的 watch 列表 → 全局注册表；投递时只在注册表锁内克隆匹配的 `Arc<Watch>`，随后在锁外入队
  （队列锁）并经合并 readiness Pipe 唤醒 reader，所以注册表锁内不取队列锁或调度器锁。
  watch 的真实 membership 与事件 publish 共用实例→queue 顺序，ONESHOT 原子摘 mark 后锁外撤销全局索引；
  captured Arc 不会在 IN_IGNORED 后继续投递。
  unlink/rename victim 持有实际 canonical path，link-count 变化先投递 ATTRIB；deleted Dentry 最后一次 dput
  从实际 inode 的 `notification_link_state` 最小事实检查 links==0 后生成 DELETE_SELF/IGNORED，OFD/cwd pins 决定时机，匿名 path 不参与。
  tmpfs 的唯一链接数为 `TmpInode.links: AtomicU32`，所有写入仍由实际 namespace/state guard 串行化；metadata 与通知读取同一字段，目录存活也由链接数决定，没有独立 dead 标记。
  ext4 从实际 disk inode 读取编号和链接数，BlockSpecial 委托原 inode；通知和 page-cache 最后回收不依赖可能分配、失败的完整 stat。
  所有 name/AVL storage 在 guard 外准备，唯一 overflow node 预留并复用。
  read 按 Linux get_one_event 逐条原子移出完整记录，再用固定栈 header/padding 在锁外 copy；没有 staging allocation 或 reader mutex。
  当前记录 copy fault 返回 ReadFault/EFAULT，覆盖此前进度，且不重放已消费记录。FIONREAD 读取同一 publish/pop 维护的总字节数。
  `DeviceFile::release` 由 actual OFD final fput 消费实例/watch 的同一 UserResourceCharge，backend observer 不延长名额。
  实例默认每 real UID/UserNS 128，watch 默认按实际 RAM 的 1%/实际 mark+两 inode 成本限制到 8192..1048576；
  新 child UserNS 配额 INT_MAX 但仍受祖先 owner 限制。`/proc/sys/user/max_inotify_*` 与
  `/proc/sys/fs/inotify/max_user_*` 指向 initial 的同一账本；前者按当前 UserNS SYS_RESOURCE，后者按 global EUID 0644。
  max_queued_events 默认16384、允许0，并在 instance 创建时快照。`WATCH_COUNT` 为零时变更点立即返回。
  benchmark 决策：集中验证测量实际 enqueue/pop、codec、FIONREAD、可复用 overflow 和既有/新 kuid 收费，不调整已有阈值。
- `block_claim::DiskClaims.extents` 是同一物理盘上整盘/分区范围及 mount/writer 的唯一 owner；
  `BlockNode.claim` 只保留 owner 引用和稳定 key，分区保留父整盘 Arc，引用图无环。短 spin guard 内
  不分配、不 I/O、不调度；节点在锁外 fallible prepare，Drop 只删除已空闲节点。重叠别名之间
  mount/writer 排他，不重叠分区可分别挂载；同一 bdev 可有多个 writer OFD。claim 先领取，再锁外
  逐出重叠 alias 的缓存（writer 保留自身缓存）；失败先回滚 claim。任何重叠 alias 已挂载，或其他 key 有 writer 时 raw read
  绕过独立 page cache；自身 writer 保留自有 cache。cache admission 在 FILES lock 内
  重查该投影，claim guard 不跨 FILES；缓存身份仍由 `Inode::page_cache_id` 给出，与 devtmpfs 实例无关。
- `/dev/kmsg` 的唤醒经 logger 发布 `deferred` vector → `fs::mem_device` 的合并 readiness Pipe；logger 在任何
  上下文（含 hardirq）都只做原子发布，不取 scheduler 锁。`KmsgReader.gate` 序列化同一 OFD 的 reader，
  `cursor` 只在整条 record 交给用户后推进。
- `fs::FileSystemType` 注册表只追加、在 `init_vfs` 发布；`MOUNT_TRANSACTION` 串行化 mount/umount 事务。
  `FileSystemType::create` 返回 `mount::CreatedFileSystem`，同时交付实际 adapter 和 immutable
  owning UserNS；VFS 只消费创建结果，不再次按 caller 推断 superblock owner。proc 创建时捕获
  active PIDNS 并要求其 owning UserNS 的 CAP_SYS_ADMIN，该同一 PIDNS 进入 proc root，UserNS
  进入 MountedFilesystem；FS_USERNS_MOUNT 不授权 child UserNS 挂载 ancestor PIDNS 的 proc。
  tmpfs 的目录结构变更先取 `Shared::namespace`，再取涉及的 inode 状态锁（rename 的两个目录按 inode 编号
  顺序）；只读路径只取单个 inode 状态锁，因此不存在两个多锁路径互相等待。inode 编号只增不复用。
  rename 在任何 link-count/entry 发布前取得 child/victim metadata guards，按最终 parent count
  校验；跨目录覆盖 directory 的 destination 增减相抵，不先加一制造溢出。commit 后只执行
  无失败 mutation，随后显式释放 child/victim/parents/namespace guards，再退休 victim Arc。
  同一 inode 的 rename 保持 namespace/link count 不变；NOREPLACE 仍优先返回 EEXIST。
  tmpfs/ext4 deleted directory 的 getdents 按固定 Linux iterate_dir 返回 ENOENT，使用实际
  zero-link directory fact，没有独立 dead flag；打开的 inode/backing 仍留到最后引用退休。
  连续 getdents cursor 可越过固定 dot-prefix，不能要求每次读取时仍小于 FIRST_ENTRY_COOKIE；
  原样保留 caller cursor 并从严格后继继续，双 backend guest 用32字节单记录读取检查不重复、不漏项。

- ext4 所有 zero-link open inode（包括被 rmdir/rename 覆盖的 directory）共用 orphan file 与
  last-iput reclaim；directory 即时清零逻辑 size，实际 extent/inode number 留到最终引用退休。
  reclaim 的 directory group 归还参数来自实际 disk mode，不能把目录排除或按 file 回收。
  同一 parent mutation owner 拒绝向 zero-link 目录插入 entry，open/cwd 不复活 namespace。


## 不阻塞的 page-cache 填页

io_uring 的 reactor 线程多路复用全部挂起请求，不能睡眠，所以持久文件读有第二条填页路径：`NowaitRead`（`fs/page_cache/nowait_read.rs`）。
它与持有 `operation` 的同步填页共存，共享同一个 cache 与同一套一致性规则，不是双轨实现：

- **版本协议**：`CachedFile::storage_epoch`（`StorageEpoch`，seqlock）。每个改变文件内容或 EOF 的 operation（write、append、truncate、allocate）
  在持有 `operation` 之后、第一次存储改动之前 `begin_mutation()`，在释放 `operation` 之前结束。异步填页取偶数快照、读存储、在 `pages` 锁内确认版本未变才发布；
  快照为奇数（有改写者在运行）、版本变了且重试用尽、inode 不支持异步读都退回阻塞车道。新增任何改变文件内容的入口都必须 `begin_mutation`，
  否则异步填页会把改写之前读到的旧块当作文件内容发布；围栏 `page-cache-epoch` 固定这一点。
- **锁序**：异步填页从不获取 `operation`；读存储时不持有任何锁；`pages` 锁内只做版本校验和发布，不 I/O、不分配（cache 节点与页在读之前预留）。
  同步填页发布时若发现同页已被异步填页发布，直接使用已发布的页（两者读到的是同一份未被改写的数据）。
- **ext4 读**：`Inode::begin_storage_read` 返回可恢复的 `StorageRead`。映射只使用已在 metadata cache 的 extent 节点，未缓存的节点先异步读并经
  `insert_if_unchanged` 进入 cache；每个 fs 块先查 journal 暂存（命中同步），否则异步读 home，完成后再校验 `metadata_cache.generation()`，变了就重读
  （与同步 `read_fs_block` 同一规则）。`Ext4FileSystem::new` 只接受 4 KiB 文件系统块与 4 KiB 设备块，所以一个 fs 块恒为一次设备块读。请求槽暂满、元数据反复失效返回 `Fallback`。
- **块层**：`BlockDevice::begin_read` 提交整块读但不等待，返回 `PendingRead`；丢弃未完成的句柄表示取消，请求槽由完成路径自己归还
  （`detached`）。没有异步能力的设备使用默认实现（同步读完再返回已完成的句柄）。

补充契约见 [文件系统接口、事务与退休契约](filesystem-transactions.md)。

VFS 初始化即创建唯一 initial MountNamespace，
先于 mount-retire/ext4-writeback 等 kernel worker 的 nsproxy 捕获。Namespace identity 保持不变，真实 root forest 在 root mount 事务内一次安装。
安装前 root lookup 返回 NotFound，不能用假 root 或替换 membership。Root 安装失败不发表森林，未安装 root 的 owner 直接释放预置 retirement token。

## memfd 写入封印

MemoryFile 的 state 独占 seals，write_sequence 串行化 write/append/truncate 与 F_ADD_SEALS。
F_SEAL_WRITE 在同一 state guard 内检查共享写映射 admission，存在任何 lease 时返回 EBUSY 且不发布 seal；
F_SEAL_FUTURE_WRITE 保留旧 lease 的写入和 mprotect 能力，阻止 write/pwrite 及新共享写映射。
新共享只读映射在写封印后清除 VM_MAYWRITE 语义，后续 mprotect 返回 EACCES；新共享 PROT_WRITE 返回 EPERM。
私有 COW 不持共享写 admission，不受内容写封印影响；增长/缩小仍由相应尺寸 seal 独立裁决。

writable_mappings 是 MemoryFile 唯一的 AtomicUsize lease count；递增与 seal 检查共用 state guard，
递减由最后一个 MemoryMapping Arc Drop 原子执行，因此 MM 退休不分配 waiter、不睡眠。映射 split/fork 共享 lease，
最后一个实际 VMA/退休引用退出才归还；不能把单个页的 writable PTE 数量当作潜在可写 VMA 数量。
MappedFile 的 may_write 同时裁决 fd 权限和实际 backing admission，不保留第二份 seal 状态。
失败 mmap、范围校验、安装或分配都通过候选 Arc Drop 回滚；新增开销仅在 mmap/最终 unmap，不进入每像素路径。
