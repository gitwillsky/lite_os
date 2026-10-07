# Filesystem 与 I/O syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 17 | `getcwd` | Complete | VFS opened-directory identity |
| 23 | `dup` | Complete | lowest-free fd publication |
| 24 | `dup3` | Complete | replacement 与 CLOEXEC |
| 25 | `fcntl` | Partial | fd/status flags、dup、record lock 以及 memfd `F_ADD_SEALS`/`F_GET_SEALS` 子集 |
| 29 | `ioctl` | Partial | TTY（含 `TCFLSH`）、socket、DRM、evdev 与 `/dev/snd/pcmC0D0p` ALSA playback 已声明 request |
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
`ENXIO`），对端打开后立刻关闭也放行等待者；最后一个 endpoint 关闭时未读数据丢弃。`O_RDWR` 打开 FIFO 返回
`EOPNOTSUPP`（pipe OFD 只有单向 endpoint），所以 shell 的 `exec 3<>fifo` 惯用法不可用。

块设备节点 `/dev/vdX`（`S_IFBLK`，major 254，每盘 16 个 minor）支持原始块 I/O：`read`/`write`/`pread`/
`pwrite`/`lseek`（`SEEK_END` 为容量）/`fsync`/`mmap`，经 page cache 缓冲，任意字节偏移；写入在设备末尾截断，
起点越界返回 `ENOSPC`。`BLKGETSIZE64`/`BLKGETSIZE`/`BLKSSZGET`/`BLKBSZGET`/`BLKPBSZGET`/`BLKIOMIN`/
`BLKIOOPT`/`BLKALIGNOFF`/`BLKROGET`/`BLKRRPART`/`BLKFLSBUF` 可用。设备被文件系统挂载时不能以写方式打开
（`EBUSY`），节点只读且不经缓冲；有写者时不能挂载。没有分区表（整盘即唯一设备）、`O_EXCL` 独占语义与
`BLKDISCARD` 等。

`/dev/kmsg`：read 无新 record 时阻塞（`O_NONBLOCK` 为 `EAGAIN`），缓冲放不下整条 record 为 `EINVAL`，
环覆盖为一次 `EPIPE`；write 以 `<N>` 前缀发布用户 record（最长 1024 字节）；`lseek` 只接受偏移 0；
poll/epoll 可用。record 没有 Linux 的 dictionary 续行，也不做 printk 限速。

字符设备读写的范围缩减：`/dev/random`/`/dev/urandom` 写入（混入 entropy pool）返回 `EOPNOTSUPP`；
`/dev/snd/pcmC0D0p` 的 `read`/`write` 返回 `EOPNOTSUPP`，PCM 数据只经 `SNDRV_PCM_IOCTL_WRITEI_FRAMES`
或 mmap ring 传输；`/dev/dri/card0` 的 `write` 返回 `EOPNOTSUPP`。
