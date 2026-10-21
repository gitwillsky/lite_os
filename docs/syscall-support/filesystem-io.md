# Filesystem 与 I/O syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 5 | `setxattr` | Partial | 当前源码tmpfs/ext4/memfd user/trusted/security属性及file-cap rootid转换；整批集中验证待执行 |
| 6 | `lsetxattr` | Partial | 同一mutation路径，最终symlink不跟随；user属性类型约束 |
| 7 | `fsetxattr` | Partial | 实际OFD path writer，按当前caller授权，不使用opener凭据 |
| 8 | `getxattr` | Partial | 属性快照与security.capability当前namespace rootid投影 |
| 9 | `lgetxattr` | Partial | get路径最终symlink不跟随 |
| 10 | `fgetxattr` | Partial | 当前caller权限，零size查询，非零size的ERANGE/E2BIG边界 |
| 11 | `listxattr` | Partial | NUL分隔名称，trusted按initial UserNS SYS_ADMIN过滤 |
| 12 | `llistxattr` | Partial | list路径最终symlink不跟随 |
| 13 | `flistxattr` | Partial | 实际inode名称快照，不要求read内容权限 |
| 14 | `removexattr` | Partial | 存在检查、同一inode变更gate及ctime/IN_ATTRIB |
| 15 | `lremovexattr` | Partial | remove路径最终symlink不跟随 |
| 16 | `fremovexattr` | Partial | 当前caller与实际SB namespace权限 |
| 17 | `getcwd` | Complete | 同锁 fs root/cwd opened identity；root-relative 路径、raw unreachable 前缀、删除 cwd 为 ENOENT |
| 23 | `dup` | Complete | lowest-free fd publication |
| 24 | `dup3` | Complete | replacement 与 CLOEXEC |
| 25 | `fcntl` | Partial | fd/status flags、dup、record lock、MemoryFile seals 子集（RO memfd F_ADD_SEALS 先返回 EPERM）、管道与 FIFO 的 `F_GETPIPE_SZ`/`F_SETPIPE_SZ` |
| 26 | `inotify_init1` | Complete | `IN_NONBLOCK`/`IN_CLOEXEC`；默认每 real UID/UserNS 128 个实例、受祖先配额限制（`EMFILE`） |
| 27 | `inotify_add_watch` | Partial | 见下“inotify”；按实例原创建者 real UID/UserNS/祖先共享 watch 配额（`ENOSPC`） |
| 28 | `inotify_rm_watch` | Complete | 删除后投递 `IN_IGNORED` |
| 29 | `ioctl` | Partial | TTY（含 `TCFLSH`）、socket、DRM、evdev、ALSA playback、块设备 `BLK*` 与管道/FIFO 的 `FIONREAD` |
| 30 | `ioprio_set` | Partial | WHO_PROCESS policy storage；无 block enforcement |
| 31 | `ioprio_get` | Partial | WHO_PROCESS policy query |
| 32 | `flock` | Complete | BSD whole-file lock lifecycle |
| 33 | `mknodat` | Partial | regular、FIFO、socket、字符/块设备节点（ext4 与 tmpfs）；设备节点需 CAP_MKNOD，目录类型 `EPERM` |
| 34 | `mkdirat` | Complete | ext4 directory transaction |
| 35 | `unlinkat` | Complete | file/directory unlink 与 lifecycle |
| 36 | `symlinkat` | Complete | ext4 fast/slow symlink |
| 37 | `linkat` | Partial | hardlink 与 link-count limit；部分 flags 未开放 |
| 38 | `renameat` | Complete | 普通原子移动与替换 |
| 39 | `umount2` | Partial | 普通卸载判忙与实际资源退休；caller root 转 superblock RO；lazy detach、两阶段 expire、nofollow；`MNT_FORCE` 不增加强制中止 I/O 能力 |
| 40 | `mount` | Partial | 已登记文件系统、挂载安全/atime 属性、remount、bind/move/propagation 与堆叠；类型解析 `data`，ext4 接受实际 `data=ordered`；未知选项/flags `EINVAL` |
| 43 | `statfs` | Complete | mounted/pseudo filesystem 同 inode projection |
| 44 | `fstatfs` | Complete | 所有 OFD 的真实 inode filesystem projection（pipefs/sockfs/anon_inodefs/shmem 区分） |
| 46 | `ftruncate` | Complete | 实际 regular inode 与各 filesystem 截断策略；page cache/mapping invalidation、memfd seal；proc 更新时间且保留固定 size/control value |
| 47 | `fallocate` | Partial | mode 0 space reservation |
| 48 | `faccessat` | Partial | current credential 与已声明 flags |
| 49 | `chdir` | Complete | opened directory publication |
| 50 | `fchdir` | Complete | directory OFD |
| 51 | `chroot` | Complete | pathname/directory/search 后检查 CAP_SYS_CHROOT；共享 fs root 替换，cwd/旧 dirfd 保留，exec 与绝对 symlink 使用 caller root |
| 52 | `fchmod` | Complete | inode mode mutation |
| 53 | `fchmodat` | Partial | pathname mode 与已声明 flags |
| 54 | `fchownat` | Partial | owner mutation 与已声明 flags |
| 55 | `fchown` | Complete | OFD inode owner mutation |
| 56 | `openat` | Partial | ext4/devfs/devpts/procfs/sysfs objects；fd admission 先于 lookup/create/device/truncate；`O_CREAT` 在 VFS transaction 原子选择 winner |
| 57 | `close` | Complete | detach 后锁外 consequence |
| 61 | `getdents64` | Complete | opaque directory `d_off` cursor、64 KiB bounded batch 与 copyout 后 publication |
| 62 | `lseek` | Partial | seekable OFD types |
| 63 | `read` | Partial | 已声明 OFD backend 与 partial/fault ordering |
| 64 | `write` | Partial | 已声明 OFD backend 与 partial/fault ordering |
| 65 | `readv` | Partial | page-batched iovec 与 backend scope |
| 66 | `writev` | Partial | page-batched iovec 与 backend scope |
| 67 | `pread64` | Complete | positioned regular-file read |
| 68 | `pwrite64` | Complete | positioned regular-file write |
| 69 | `preadv` | Complete | positioned vector regular-file read |
| 70 | `pwritev` | Complete | positioned vector regular-file write |
| 71 | `sendfile` | Partial | regular-file to regular-file |
| 285 | `copy_file_range` | Partial | regular-file to regular-file，两侧各自显式或共享 offset；flags 必须为零；目录 EISDIR，其他类型 EINVAL |
| 78 | `readlinkat` | Complete | symlink与 procfs fd projection |
| 79 | `newfstatat` | Partial | supported objects 与 flags |
| 291 | `statx` | Partial | 基本字段、ext4 `BTIME` 与纳秒、`MNT_ID`、设备号；`stx_attributes_mask` 为 0，不报告 `DIOALIGN`/`MOUNT_ROOT` |
| 80 | `fstat` | Complete | supported OFD objects |
| 81 | `sync` | Complete | 提交 ext4 running transaction；返回时全部已返回写入 durable |
| 82 | `fsync` | Complete | 提交包含该文件的 running transaction；提交失败返回 `EIO` 且 journal fail-stop |
| 83 | `fdatasync` | Complete | data durability boundary |
| 88 | `utimensat` | Partial | pathname/futimens(NULL fd)/AT_EMPTY_PATH；双 UTIME_OMIT 不访问 fd/path/flags；volatile 时间单 owner，显式过去/未来 mtime 不抬高 ctime；当前秒精度/非负范围 |
| 166 | `umask` | Complete | Thread fs-context-owned mask |
| 276 | `renameat2` | Partial | rename、NOREPLACE、EXCHANGE；其余 flags 拒绝 |
| 279 | `memfd_create` | Partial | 真实 creator FSIDs 的 regular inode，proc FD 重开独立 OFD/offset/status、共享 content/seals；anonymous shared file、`MFD_CLOEXEC`/`MFD_ALLOW_SEALING`、`ftruncate`、`MAP_SHARED` 与 grow/shrink/seal seals；hugetlb、write seals 未开放 |
| 286 | `preadv2` | Partial | positioned vector I/O 与已声明 flags |
| 287 | `pwritev2` | Partial | positioned vector I/O 与已声明 flags |
| 425 | `io_uring_setup` | Partial | 返回恒带 `O_CLOEXEC` 的 ring fd；`IORING_SETUP_CQSIZE`/`CLAMP` 与 Linux 一致，SQ 上限 32768；其余 flags、非零保留字段返回 `EINVAL`；features：`SINGLE_MMAP`/`NODROP`/`SUBMIT_STABLE`/`RW_CUR_POS`/`CUR_PERSONALITY`/`FAST_POLL`/`EXT_ARG` |
| 426 | `io_uring_enter` | Partial | 提交与 `IORING_ENTER_GETEVENTS`，`EXT_ARG` 超时（`ETIME`）与临时 sigmask；`min_wait_usec` 非零、`SQ_WAKEUP`/`SQ_WAIT`/`REGISTERED_RING` 等其余 flags 返回 `EINVAL`；非 ring fd 返回 `EOPNOTSUPP` |
| 427 | `io_uring_register` | Partial | 仅 `IORING_REGISTER_PROBE`；其余 opcode 返回 `EINVAL` |
| 463 | `setxattrat` | Partial | AT_EMPTY_PATH/AT_SYMLINK_NOFOLLOW，标准16-byte xattr_args与零扩展校验 |
| 464 | `getxattrat` | Partial | 同一get路径，xattr_args.flags必须零 |
| 465 | `listxattrat` | Partial | 同一list路径及标准at flags |
| 466 | `removexattrat` | Partial | 同一remove路径及标准at flags |

## 已知缺口

tmpfs 选项：`size=`（字节，`k/m/g`/`%`）、`nr_blocks=`、`nr_inodes=`、`mode=`、`uid=`、`gid=`；`huge=`、`mpol=` 等返回 `EINVAL`。

io_uring 已实现的 opcode：`NOP`、`READV`/`WRITEV`/`READ`/`WRITE`、`FSYNC`、`OPENAT`、`STATX`、`CLOSE`、`POLL_ADD`/`POLL_REMOVE`、`TIMEOUT`/`TIMEOUT_REMOVE`、`ASYNC_CANCEL`、`ACCEPT`、`CONNECT`、`SEND`、`RECV`；
`STATX`：路径在 prep 时复制，在阻塞车道上以提交线程的 cwd、凭据与地址空间执行并写回 `addr2` 指向的缓冲区；
参数校验与 `statx(2)` 共用同一核心（未知 flags、互斥同步类型位、保留 mask 位为 `-EINVAL`，坏输出地址为 `-EFAULT`）；
`IOSQE_FIXED_FILE` 返回 `-EBADF`，`buf_index`/`file_index` 非零返回 `-EINVAL`。
`OPENAT`：路径在 prep 时复制，在阻塞车道上以提交线程的 fd 表、cwd 与凭据执行（与 `openat(2)` 共用同一核心）；直接描述符（`file_index` 非零）返回 `-EOPNOTSUPP`，`IOSQE_FIXED_FILE` 返回 `-EBADF`，`buf_index` 非零返回 `-EINVAL`；
被信号打断的阻塞 open 以 `-EINTR` 完成。`OPENAT2` 未实现。
`ACCEPT`/`CONNECT`/`SEND`/`RECV`：`ACCEPT` 的 multishot 与直接描述符、`SEND`/`RECV` 的目的地址、`ioprio` recvsend flags、除 `MSG_DONTWAIT`/`MSG_NOSIGNAL` 外的 `msg_flags` 返回 `-EOPNOTSUPP`；`SEND` 恒带 `MSG_NOSIGNAL`（`SIGPIPE` 不投递给提交线程）；
`CONNECT` 对 Unix 目标 backlog 已满：阻塞 socket 挂起等容量后重试，`O_NONBLOCK` socket 以 `-EAGAIN` 完成；AF_INET 路径与 AF_UNIX 共用同一核心，烟测只覆盖 AF_UNIX。
其余 opcode 的 SQE 完成为 `-EOPNOTSUPP` 并在 probe 中标为不支持。
`io_uring_setup` 的 ring 页按真实 UID（沿 UserNS 祖先链）计入每 UID 预算（64 MiB，本内核策略值，Linux ≥5.12 用 memcg）；
超出返回 `ENOMEM`；有 mmap 映射时关闭 fd 不归还，munmap 之后才归还。
读写：`off == -1` 用并推进当前 position，`off >= 0` 对 regular/可定位文件是 pread/pwrite 语义，对 pipe/socket 等流式文件忽略；`ioprio`、`rw_flags`、
`personality` 非零分别返回 `EINVAL`/`EOPNOTSUPP`/`EINVAL`，请求使用提交者提交时刻的凭据。会阻塞的 pipe/socket/eventfd/timerfd/tty 请求不占用提交线程：
先做一次 NOWAIT 尝试，仍会阻塞则由 `io_uring` 内核线程等待就绪后在提交线程的地址空间中完成，用户态自己设置了 `O_NONBLOCK` 的 fd 如实返回 `-EAGAIN`；
signalfd 读无信号时返回 `-EAGAIN`（worker 无法代表提交线程登记信号等待）。regular 文件的读写不在 `io_uring_enter` 内睡眠：读（显式偏移与 `off == -1` 的当前位置）在 page cache 未命中时异步填页（ext4）；
当前位置读完成时发布新 position，发起时 position 被占用则回退车道。
写、fsync 与回退的读交给阻塞车道的 worker 池（最多 8 个线程，共享 FIFO，写与 fsync 可以重叠；Linux 同样由 io-wq 执行 ext4 缓冲写与 fsync）。
`POLL_ADD`：一次性语义，提交时先 poll 一次，已就绪同步完成，否则挂起；res 是 revents，`ERR`/`HUP` 总是报告；`IORING_POLL_ADD_MULTI` 返回 `-EOPNOTSUPP`。
`TIMEOUT`：相对或绝对（`CLOCK_MONOTONIC`，`BOOTTIME` 视同单调），到期 `-ETIME`（`ETIME_SUCCESS` 的 CQE 仍是 `-ETIME`，只是不使链断开）；`off` 非零时等“除 timeout 自身外又发布 N 个 CQE”，满足则以 0 完成；
`len` 必须为 1；`MULTISHOT`/`REALTIME` 返回 `-EOPNOTSUPP`（`MULTISHOT|ABS` 为 `-EINVAL`）。`TIMEOUT_REMOVE`、`POLL_REMOVE` 只取消各自种类，更新类 flags 返回 `-EOPNOTSUPP`。
`ASYNC_CANCEL`：按 user_data 匹配（`ALL` 取消全部并返回个数，`ANY` 匹配该 ring 的任意请求），只匹配“正在等待”的成员：取消请求同步得到 0、`-ENOENT` 或 `-EALREADY`
（目标是已在车道线程上执行的写/fsync/openat，不能取消，会照常完成并返回真实结果），目标稍后异步得到 `-ECANCELED`（两个 CQE 的先后不保证，与 Linux 相同）；
`FD`/`OP`（可叠加 `USERDATA`，条件全部满足才匹配，同 Linux `io_cancel_req_match`）按正在等待成员的 OFD 与 opcode 匹配；fd 在 issue 时解析，无效 fd 与 `FD_FIXED`（无注册文件表）返回 `-EBADF`，`FD`/`OP` 与 `ANY` 互斥返回 `-EINVAL`。
ring 关闭（含进程退出关闭 fd）取消该 ring 的全部挂起请求，释放它们持有的目标 fd；提交线程已退出而 ring 仍被共享时，挂起请求以 `-ECANCELED` 结束。
`IOSQE_IO_LINK`/`IO_HARDLINK`：链在一次提交批内组装，头先执行，成员失败（`res < 0`、读写短传输、非 `ETIME_SUCCESS` 的 `-ETIME`）取消其后非 hardlink 链的成员（`-ECANCELED`），prep 失败按 Linux 取消整条链，批末未结束的链按现状结束；
后继的 fd 在其 issue 时才解析。
`IORING_OP_LINK_TIMEOUT`：约束紧挨着它的前一个链成员，成员开始等待时计时；定时器先到则成员以 `-ECANCELED` 结束（其后成员按链规则取消）、timeout 得 `-ETIME`，
成员先完成（含同步完成）则 timeout 得 `-ECANCELED` 且不使链断开。不在链中或跟在另一个 `LINK_TIMEOUT` 之后返回 `-EINVAL`；
`off`、`REALTIME` 之外的非法 flags 同 `IORING_OP_TIMEOUT`（`REALTIME` 返回 `-EOPNOTSUPP`，`MULTISHOT`/`UPDATE` 返回 `-EINVAL`）；`ETIME_SUCCESS` 被接受但无可观察效果。
对尚未 issue 的链成员发 `ASYNC_CANCEL` 返回 `-ENOENT`，与 Linux 相同（`io_try_cancel` 只搜索 io-wq、poll、waitid、futex 与 timeout，不遍历链）。
`IOSQE_IO_DRAIN`：等所有更早的请求链完成才开始，其后的链等它完成；链中间的 DRAIN 使链头 DRAIN 且其后的下一条链也 DRAIN；ring 上用过 `CQE_SKIP_SUCCESS` 之后 DRAIN 返回 `-EOPNOTSUPP`（同 Linux）。
`BUFFER_SELECT` 返回 `-EOPNOTSUPP`；`IOSQE_ASYNC` 被接受但只是调度提示；`FIXED_FILE` 因没有注册文件表返回 `-EBADF`。
固定文件/缓冲区注册、其余 opcode、SQPOLL/IOPOLL、SQE128/CQE32 未开放。CQ 溢出链表上限为 CQ 容量的 8 倍，超出后丢弃并经 `EBADR` 通知。
未支持ACL、splice family 或完整 block I/O priority enforcement；
xattr目前接入tmpfs/ext4/memfd源码，其余filesystem返回EOPNOTSUPP；新 ext4 镜像启用原生 EA_INODE，
值 inode/extents/hash/refcount 与父属性同 journal owner。旧卷未启用该 feature 时按其实际容量返回ENOSPC；
名称区域、磁盘空间及 journal 暂存仍有限，ABI单值上限64KiB不代表filesystem保证容量。
file-cap磁盘revision1/2/3可供exec解析，非空set只接受标准revision2/3。
inotify 的已实现范围与限制见下文。

FIFO：`open` 按 Linux `fifo_open` 汇合（阻塞只读等 writer、阻塞只写等 reader，非阻塞只写无 reader 为
`ENXIO`），`O_RDWR` 不阻塞（`exec 3<>fifo` 可用）；对端打开后立刻关闭也放行等待者；最后一个 endpoint 关闭时
未读数据丢弃。poll 的 `POLLIN` 只表示有数据，`POLLHUP` 只在曾有 writer 来过又全部离开之后出现；写入没有
reader 的 FIFO 得到 `EPIPE` 并投递 `SIGPIPE`。

块设备节点 `/dev/vdX`（`S_IFBLK`，major 254，每盘 16 个 minor）支持原始块 I/O：`read`/`write`/`pread`/
`pwrite`/`lseek`（`SEEK_END` 为容量）/`fsync`/`mmap`，经 page cache 缓冲，任意字节偏移；写入在设备末尾截断，
起点越界返回 `ENOSPC`。`BLKGETSIZE64`/`BLKGETSIZE`/`BLKSSZGET`/`BLKBSZGET`/`BLKPBSZGET`/`BLKIOMIN`/
`BLKIOOPT`/`BLKALIGNOFF`/`BLKROGET`/`BLKRRPART`/`BLKFLSBUF` 可用。设备被文件系统挂载时不能以写方式打开
（`EBUSY`），节点只读且不经缓冲；有写者时不能挂载。

分区：启动时解析 GPT（校验头与分区项数组的 CRC32，主表损坏时用备份表）或 MBR（含扩展分区的逻辑分区，
编号从 5 起），为每个分区发布 `/dev/<disk>N`（盘名以数字结尾时为 `<disk>pN`，minor 为整盘 minor 加 N，
每盘最多 15 个）。分区的起点与长度必须 4 KiB 对齐（现代工具默认 1 MiB 对齐），未对齐的不发布并记录警告；
长度向下取整到 4 KiB。分区可以挂载 ext4 并做原始 I/O，越过分区末尾得到 `ENOSPC`。`BLKRRPART` 重读分区表：
与已发布的相同为空操作，不同返回 `EBUSY`（设备注册表只追加，不支持热移除或改号节点）。没有 `O_EXCL` 独占语义与
`BLKDISCARD` 等。

inotify：事件 `IN_ACCESS`/`MODIFY`/`ATTRIB`/`CLOSE_WRITE`/`CLOSE_NOWRITE`/`OPEN`/`MOVED_FROM`/`MOVED_TO`/
`CREATE`/`DELETE`/`DELETE_SELF`/`MOVE_SELF`，加 `IN_UNMOUNT`、`IN_IGNORED`、`IN_ISDIR`、`IN_Q_OVERFLOW`；
`IN_ONLYDIR`、`IN_DONT_FOLLOW`、`IN_MASK_ADD`、`IN_MASK_CREATE`、`IN_EXCL_UNLINK`、`IN_ONESHOT`。文件事件同时投递给文件自身的 watch（无名字）和父目录
的 watch（带名字）；`rename` 的 `MOVED_FROM`/`MOVED_TO` 共用 cookie；相邻相同事件合并；队列默认上限 16384（创建时快照），满后丢弃并
保留一个 `IN_Q_OVERFLOW`；读缓冲放不下首个事件为 `EINVAL`；支持阻塞与 `O_NONBLOCK` read、`FIONREAD`、
poll/epoll。read 逐条在 copy 前出队，copy fault 为 EFAULT 且当前记录不重放。
`/proc/sys/fs/inotify/{max_user_instances,max_user_watches,max_queued_events}` 与 `/proc/sys/user/max_inotify_*` 支持标准配额修改。
`IN_DELETE_SELF`/`IN_IGNORED` 在 inode 已无 hard link 且 actual canonical dentry 最后一次 dput 时生成，
打开的 OFD/cwd 延迟这一步；self move/delete 不携带 ISDIR。范围缩减：mmap 写入、`sendfile` 与 `splice` 不产生事件；fanotify 语义不支持。

块设备枚举：`/proc/partitions`（`major minor #blocks name`，容量以 1 KiB 计）；sysfs 的 `/sys/class/block`
（全部设备）、`/sys/block`（整盘，其下是它的分区目录）与 `/sys/dev/block/MAJ:MIN`，每个设备目录有 `dev`、
`size`（512 字节扇区）、`ro`、`removable`、`uevent`，分区另有 `partition`、`start`，供 lsblk、blkid、mdev 枚举。
分区的 `uevent` 含 `PARTUUID=`。`/sys/dev/block/MAJ:MIN` 是与 `/sys/class/block/<name>` 内容相同的目录而不是符号链接（sysfs 没有符号链接节点）。

管道与 FIFO：`FIONREAD` 返回未读字节数；`F_GETPIPE_SZ` 返回环容量（缺省 64 KiB），`F_SETPIPE_SZ` 按 2 的幂次
页数取整（最小 4 KiB、最大 1 MiB，超过为 `EPERM`），未读数据装不进新容量为 `EBUSY`，扩容立即唤醒阻塞的写者。

`/dev/kmsg`：read 无新 record 时阻塞（`O_NONBLOCK` 为 `EAGAIN`），缓冲放不下整条 record 为 `EINVAL`，
环覆盖为一次 `EPIPE`；write 以 `<N>` 前缀发布用户 record（最长 1024 字节）；`lseek` 只接受偏移 0；
poll/epoll 可用。record 没有 Linux 的 dictionary 续行，也不做 printk 限速。

字符设备读写的范围缩减：`/dev/random`/`/dev/urandom` 写入（混入 entropy pool）返回 `EOPNOTSUPP`；
`/dev/snd/pcmC0D0p` 的 `read`/`write` 返回 `EOPNOTSUPP`，PCM 数据只经 `SNDRV_PCM_IOCTL_WRITEI_FRAMES`
或 mmap ring 传输；`/dev/dri/card0` 的 `write` 返回 `EOPNOTSUPP`。

`statx` 的时间戳精度取决于 filesystem 实际记录的内容：ext4 保存纳秒与创建时间并如实报告；其余 filesystem 的
`Inode::set_times`/`touch_access` 以秒为单位，纳秒恒为 0、不报告 `BTIME`（Linux 对不记录创建时间的 filesystem 同样不设该位）。
`stx_attributes_mask` 为 0，因为 immutable/append/nodump/compressed/encrypted 在本内核没有对应语义，不能声称支持。

`copy_file_range` 与 `sendfile` 共用同一条 regular-file 数据路径（逐页读写、同文件重叠 EINVAL、RLIMIT_FSIZE），数据经内核缓冲复制，
不做 reflink/服务端拷贝；跨 filesystem 复制按普通读写完成，与 Linux 5.19 之后的回退行为一致。
