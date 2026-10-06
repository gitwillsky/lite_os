//! 内核返回、用户态消费的 Linux errno 编号（asm-generic）。

/// 操作不允许。
pub const EPERM: isize = 1;
/// 文件或目录不存在。
pub const ENOENT: isize = 2;
/// 进程不存在。
pub const ESRCH: isize = 3;
/// 系统调用被中断。
pub const EINTR: isize = 4;
/// 输入输出错误。
pub const EIO: isize = 5;
/// 当前 Process 没有 controlling TTY 等目标设备。
pub const ENXIO: isize = 6;
/// 参数列表过长。
pub const E2BIG: isize = 7;
/// 可执行文件格式无效。
pub const ENOEXEC: isize = 8;
/// 无效文件描述符。
pub const EBADF: isize = 9;
/// 没有匹配的 child process。
pub const ECHILD: isize = 10;
/// 暂时无法创建资源。
pub const EAGAIN: isize = 11;
/// 无法分配内存。
pub const ENOMEM: isize = 12;
/// 权限不足。
pub const EACCES: isize = 13;
/// 无效用户空间地址。
pub const EFAULT: isize = 14;
/// 目标是 live mount root/mountpoint 等正在使用的 namespace object。
pub const EBUSY: isize = 16;
pub const EEXIST: isize = 17;
/// old/new pathname 不属于同一 mounted filesystem。
pub const EXDEV: isize = 18;
/// fd backend 不支持所请求的设备映射操作。
pub const ENODEV: isize = 19;
/// 路径分量不是目录。
pub const ENOTDIR: isize = 20;
pub const EISDIR: isize = 21;
/// 无效参数。
pub const EINVAL: isize = 22;
pub const EMFILE: isize = 24;
/// fd 不是 TTY 或 TTY 不属于 caller session。
pub const ENOTTY: isize = 25;
/// 写入或 truncate 超出 RLIMIT_FSIZE。
pub const EFBIG: isize = 27;
pub const ENOSPC: isize = 28;
/// 目标 filesystem 不允许 mutation。
pub const EROFS: isize = 30;
/// inode hard-link count 已达到 on-disk 表达上限。
pub const EMLINK: isize = 31;
/// kernel 无法分配 advisory lock record。
pub const ENOLCK: isize = 37;
/// pipe 没有 reader。
pub const EPIPE: isize = 32;
pub const ESPIPE: isize = 29;
/// 结果超出支持范围。
pub const ERANGE: isize = 34;
/// 路径或参数字符串过长。
pub const ENAMETOOLONG: isize = 36;
pub const ENOTEMPTY: isize = 39;
/// 系统调用未实现。
pub const ENOSYS: isize = 38;
/// 符号链接解析超出支持范围。
pub const ELOOP: isize = 40;
pub const ENOTSOCK: isize = 88;
/// datagram socket 未连接且调用者没有提供目标地址。
pub const EDESTADDRREQ: isize = 89;
/// datagram 超过协议可表达的最大 payload。
pub const EMSGSIZE: isize = 90;
pub const EOPNOTSUPP: isize = 95;
pub const EAFNOSUPPORT: isize = 97;
pub const EADDRINUSE: isize = 98;
/// 请求的本地或远端地址在当前 interface 上不可用。
pub const EADDRNOTAVAIL: isize = 99;
/// 当前 interface/route 无法到达目标网络。
pub const ENETUNREACH: isize = 101;
/// nonblocking connect 已经在进行。
pub const EALREADY: isize = 114;
/// nonblocking connect 已启动但尚未完成。
pub const EINPROGRESS: isize = 115;
pub const ENOTCONN: isize = 107;
pub const EISCONN: isize = 106;
/// real-UID SCM_RIGHTS inflight 已达到 RLIMIT_NOFILE resource bound。
pub const ETOOMANYREFS: isize = 109;
/// 已建立连接被 peer reset。
pub const ECONNRESET: isize = 104;
pub const ECONNREFUSED: isize = 111;
pub const EPROTONOSUPPORT: isize = 93;
pub const ESOCKTNOSUPPORT: isize = 94;
pub const ENOPROTOOPT: isize = 92;
/// 结果无法由目标文件系统或 ABI 字段表示。
pub const EOVERFLOW: isize = 75;
/// ALSA command 与当前 PCM file state 不匹配。
pub const EBADFD: isize = 77;
/// 等待在 deadline 前未完成。
pub const ETIMEDOUT: isize = 110;
