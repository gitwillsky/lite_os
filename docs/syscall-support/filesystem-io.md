# Filesystem 与 I/O syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 17 | `getcwd` | Complete | VFS opened-directory identity |
| 23 | `dup` | Complete | lowest-free fd publication |
| 24 | `dup3` | Complete | replacement 与 CLOEXEC |
| 25 | `fcntl` | Partial | fd/status flags、dup、record lock、memfd seals 子集、管道与 FIFO 的 `F_GETPIPE_SZ`/`F_SETPIPE_SZ` |
| 29 | `ioctl` | Partial | TTY（含 `TCFLSH`）、socket、DRM、evdev、ALSA playback、块设备 `BLK*` 与管道/FIFO 的 `FIONREAD` |
| 30 | `ioprio_set` | Partial | WHO_PROCESS policy storage；无 block enforcement |
| 31 | `ioprio_get` | Partial | WHO_PROCESS policy query |
| 32 | `flock` | Complete | BSD whole-file lock lifecycle |
| 33 | `mknodat` | Partial | regular、FIFO、socket、字符/块设备节点（ext4 与 tmpfs）；设备节点需 effective UID 0，目录类型 `EPERM` |
| 34 | `mkdirat` | Complete | ext4 directory transaction |
| 35 | `unlinkat` | Complete | file/directory unlink 与 lifecycle |
| 36 | `symlinkat` | Complete | ext4 fast/slow symlink |
| 37 | `linkat` | Partial | hardlink 与 link-count limit；部分 flags 未开放 |
| 38 | `renameat` | Complete | 普通原子移动与替换 |
| 39 | `umount2` | Partial | 挂载根卸载、子挂载/打开文件/cwd/mmap 判忙 `EBUSY`、page cache 写回与 ext4 写回线程停止；`MNT_FORCE` 等同普通卸载，`MNT_DETACH`/`MNT_EXPIRE` 返回 `EINVAL` |
| 40 | `mount` | Partial | 新挂载 `ext4`、`proc`、`sysfs`、`devpts`、`devtmpfs`、`tmpfs`；`ro`/`nosuid`/`nodev`/`noexec`/`remount` 与 atime 策略；`data` 由类型解析，未知选项 `EINVAL`；bind/move/propagation/堆叠 `EINVAL`/`EBUSY` |
| 43 | `statfs` | Complete | 已挂载 filesystem projection |
| 44 | `fstatfs` | Complete | OFD-backed filesystem projection |
| 46 | `ftruncate` | Complete | regular file/memfd、page cache 与 mapping invalidation；memfd seal 精确拒绝 grow/shrink |
| 47 | `fallocate` | Partial | mode 0 space reservation |
| 48 | `faccessat` | Partial | current credential 与已声明 flags |
| 49 | `chdir` | Complete | opened directory publication |
| 50 | `fchdir` | Complete | directory OFD |
| 52 | `fchmod` | Complete | inode mode mutation |
| 53 | `fchmodat` | Partial | pathname mode 与已声明 flags |
| 54 | `fchownat` | Partial | owner mutation 与已声明 flags |
| 55 | `fchown` | Complete | OFD inode owner mutation |
| 56 | `openat` | Partial | ext4/devfs/devpts/procfs/sysfs objects；`O_CREAT` lookup/create 在 VFS namespace transaction 内原子提交，非 `O_EXCL` 并发创建打开 winner |
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
| 78 | `readlinkat` | Complete | symlink与 procfs fd projection |
| 79 | `newfstatat` | Partial | supported objects 与 flags |
| 80 | `fstat` | Complete | supported OFD objects |
| 81 | `sync` | Complete | 提交 ext4 running transaction；返回时全部已返回写入 durable |
| 82 | `fsync` | Complete | 提交包含该文件的 running transaction；提交失败返回 `EIO` 且 journal fail-stop |
| 83 | `fdatasync` | Complete | data durability boundary |
| 88 | `utimensat` | Partial | inode timestamps 与已声明 flags |
| 166 | `umask` | Complete | Process-owned mask |
| 276 | `renameat2` | Partial | rename、NOREPLACE、EXCHANGE；其余 flags 拒绝 |
| 279 | `memfd_create` | Partial | anonymous shared file、`MFD_CLOEXEC`/`MFD_ALLOW_SEALING`、`ftruncate`、`MAP_SHARED` 与 grow/shrink/seal seals；hugetlb、write seals 未开放 |
| 286 | `preadv2` | Partial | positioned vector I/O 与已声明 flags |
| 287 | `pwritev2` | Partial | positioned vector I/O 与已声明 flags |

## 已知缺口

tmpfs 选项：`size=`（字节，`k/m/g`/`%`）、`nr_blocks=`、`nr_inodes=`、`mode=`、`uid=`、`gid=`；`huge=`、`mpol=` 等返回 `EINVAL`。

没有通用 mount namespace、xattr/ACL、inotify、splice family、io_uring 或完整 block I/O priority enforcement。

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
