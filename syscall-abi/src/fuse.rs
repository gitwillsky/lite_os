//! Linux FUSE UAPI（`include/uapi/linux/fuse.h`，协议 7.45）的 opcode、flag、ioctl 与定长结构尺寸。
//!
//! 内核 `/dev/fuse` 与用户态守护进程共用这些常量；结构体按原生字节序逐字段编解码，不依赖 Rust 布局。

/// 内核协议主版本。
pub const KERNEL_VERSION: u32 = 7;
/// 内核协议次版本。
pub const KERNEL_MINOR_VERSION: u32 = 45;
/// 根目录的 nodeid。
pub const ROOT_ID: u64 = 1;
/// 守护进程 read 缓冲区下限；更小的缓冲区返回 `EINVAL`。
pub const MIN_READ_BUFFER: usize = 8192;
/// `fuse_in_header.uid/gid` 在调用者 ID 无法映射进连接 user namespace 时的值。
pub const INVALID_UIDGID: u32 = u32::MAX;

pub const LOOKUP: u32 = 1;
pub const FORGET: u32 = 2;
pub const GETATTR: u32 = 3;
pub const SETATTR: u32 = 4;
pub const READLINK: u32 = 5;
pub const SYMLINK: u32 = 6;
pub const MKNOD: u32 = 8;
pub const MKDIR: u32 = 9;
pub const UNLINK: u32 = 10;
pub const RMDIR: u32 = 11;
pub const RENAME: u32 = 12;
pub const LINK: u32 = 13;
pub const OPEN: u32 = 14;
pub const READ: u32 = 15;
pub const WRITE: u32 = 16;
pub const STATFS: u32 = 17;
pub const RELEASE: u32 = 18;
pub const FSYNC: u32 = 20;
pub const SETXATTR: u32 = 21;
pub const GETXATTR: u32 = 22;
pub const LISTXATTR: u32 = 23;
pub const REMOVEXATTR: u32 = 24;
pub const FLUSH: u32 = 25;
pub const INIT: u32 = 26;
pub const OPENDIR: u32 = 27;
pub const READDIR: u32 = 28;
pub const RELEASEDIR: u32 = 29;
pub const FSYNCDIR: u32 = 30;
pub const ACCESS: u32 = 34;
pub const CREATE: u32 = 35;
pub const INTERRUPT: u32 = 36;
pub const DESTROY: u32 = 38;
pub const BATCH_FORGET: u32 = 42;
pub const FALLOCATE: u32 = 43;
pub const READDIRPLUS: u32 = 44;
pub const RENAME2: u32 = 45;

pub const NOTIFY_INVAL_INODE: i32 = 2;
pub const NOTIFY_INVAL_ENTRY: i32 = 3;
pub const NOTIFY_DELETE: i32 = 6;

pub const ASYNC_READ: u64 = 1 << 0;
pub const ATOMIC_O_TRUNC: u64 = 1 << 3;
pub const BIG_WRITES: u64 = 1 << 5;
pub const DONT_MASK: u64 = 1 << 6;
pub const AUTO_INVAL_DATA: u64 = 1 << 12;
pub const DO_READDIRPLUS: u64 = 1 << 13;
pub const READDIRPLUS_AUTO: u64 = 1 << 14;
pub const WRITEBACK_CACHE: u64 = 1 << 16;
pub const PARALLEL_DIROPS: u64 = 1 << 18;
pub const ABORT_ERROR: u64 = 1 << 21;
pub const MAX_PAGES: u64 = 1 << 22;
pub const INIT_EXT: u64 = 1 << 30;
pub const PASSTHROUGH: u64 = 1 << 37;
pub const OVER_IO_URING: u64 = 1 << 41;

pub const FATTR_MODE: u32 = 1 << 0;
pub const FATTR_UID: u32 = 1 << 1;
pub const FATTR_GID: u32 = 1 << 2;
pub const FATTR_SIZE: u32 = 1 << 3;
pub const FATTR_ATIME: u32 = 1 << 4;
pub const FATTR_MTIME: u32 = 1 << 5;
pub const FATTR_FH: u32 = 1 << 6;

pub const FOPEN_DIRECT_IO: u32 = 1 << 0;
pub const FOPEN_KEEP_CACHE: u32 = 1 << 1;
pub const FOPEN_NONSEEKABLE: u32 = 1 << 2;
pub const FOPEN_CACHE_DIR: u32 = 1 << 3;
pub const FOPEN_NOFLUSH: u32 = 1 << 5;

pub const RELEASE_FLUSH: u32 = 1 << 0;
pub const GETATTR_FH: u32 = 1 << 0;
pub const WRITE_CACHE: u32 = 1 << 0;
pub const WRITE_KILL_SUIDGID: u32 = 1 << 2;
pub const FSYNC_FDATASYNC: u32 = 1 << 0;

/// `RENAME_NOREPLACE`，经 `fuse_rename2_in.flags` 传给守护进程。
pub const RENAME_NOREPLACE: u32 = 1 << 0;

/// `FUSE_DEV_IOC_CLONE`：`_IOR(229, 0, uint32_t)`，把新 `/dev/fuse` fd 绑定到参数 fd 的连接。
pub const DEV_IOC_CLONE: usize = 0x8004_e500;

/// `fuse_in_header` 字节数。
pub const IN_HEADER_SIZE: usize = 40;
/// `fuse_out_header` 字节数。
pub const OUT_HEADER_SIZE: usize = 16;
/// `fuse_attr` 字节数。
pub const ATTR_SIZE: usize = 88;
/// `fuse_entry_out` 字节数。
pub const ENTRY_OUT_SIZE: usize = 40 + ATTR_SIZE;
/// `fuse_attr_out` 字节数。
pub const ATTR_OUT_SIZE: usize = 16 + ATTR_SIZE;
/// `fuse_init_in` 字节数。
pub const INIT_IN_SIZE: usize = 64;
/// `fuse_init_out` 字节数；早于 7.23 的守护进程回复更短的前缀。
pub const INIT_OUT_SIZE: usize = 64;
/// `fuse_open_out` 字节数。
pub const OPEN_OUT_SIZE: usize = 16;
/// `fuse_write_out` 字节数。
pub const WRITE_OUT_SIZE: usize = 8;
/// `fuse_statfs_out` 字节数。
pub const STATFS_OUT_SIZE: usize = 80;
/// `fuse_dirent` 名字之前的定长部分。
pub const DIRENT_NAME_OFFSET: usize = 24;
/// `fuse_direntplus` 名字之前的定长部分。
pub const DIRENTPLUS_NAME_OFFSET: usize = ENTRY_OUT_SIZE + DIRENT_NAME_OFFSET;
