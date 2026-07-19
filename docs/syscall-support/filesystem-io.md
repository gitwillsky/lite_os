# Filesystem 与 I/O syscall

| Number | Syscall | Status | 当前范围 |
|---:|---|---|---|
| 17 | `getcwd` | Complete | VFS opened-directory identity |
| 23 | `dup` | Complete | lowest-free fd publication |
| 24 | `dup3` | Complete | replacement 与 CLOEXEC |
| 25 | `fcntl` | Partial | fd/status flags、dup 与 record lock 子集 |
| 29 | `ioctl` | Partial | TTY、socket、DRM 与 evdev 已声明 request |
| 30 | `ioprio_set` | Partial | WHO_PROCESS policy storage；无 block enforcement |
| 31 | `ioprio_get` | Partial | WHO_PROCESS policy query |
| 32 | `flock` | Complete | BSD whole-file lock lifecycle |
| 33 | `mknodat` | Partial | 已支持 inode/device types |
| 34 | `mkdirat` | Complete | ext2 directory transaction |
| 35 | `unlinkat` | Complete | file/directory unlink 与 lifecycle |
| 36 | `symlinkat` | Complete | ext2 symlink |
| 37 | `linkat` | Partial | hardlink 与 link-count limit；部分 flags 未开放 |
| 43 | `statfs` | Complete | 已挂载 filesystem projection |
| 44 | `fstatfs` | Complete | OFD-backed filesystem projection |
| 46 | `ftruncate` | Complete | regular file、page cache 与 mapping invalidation |
| 47 | `fallocate` | Partial | mode 0 space reservation |
| 48 | `faccessat` | Partial | current credential 与已声明 flags |
| 49 | `chdir` | Complete | opened directory publication |
| 50 | `fchdir` | Complete | directory OFD |
| 52 | `fchmod` | Complete | inode mode mutation |
| 53 | `fchmodat` | Partial | pathname mode 与已声明 flags |
| 54 | `fchownat` | Partial | owner mutation 与已声明 flags |
| 55 | `fchown` | Complete | OFD inode owner mutation |
| 56 | `openat` | Partial | ext2/devfs/devpts/procfs/sysfs objects |
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
| 81 | `sync` | Complete | mounted writable filesystem flush |
| 82 | `fsync` | Complete | file data/metadata durability boundary |
| 83 | `fdatasync` | Complete | data durability boundary |
| 88 | `utimensat` | Partial | inode timestamps 与已声明 flags |
| 166 | `umask` | Complete | Process-owned mask |
| 276 | `renameat2` | Partial | rename、NOREPLACE、EXCHANGE；其余 flags 拒绝 |
| 286 | `preadv2` | Partial | positioned vector I/O 与已声明 flags |
| 287 | `pwritev2` | Partial | positioned vector I/O 与已声明 flags |

## 已知缺口

没有通用 mount namespace、xattr/ACL、inotify、splice family、io_uring、background writeback daemon 或完整 block I/O priority enforcement。
