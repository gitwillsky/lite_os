//! 当前 syscall handler 实际返回的 Linux errno。

/// 操作不允许。
pub(crate) const EPERM: isize = 1;
/// 文件或目录不存在。
pub(crate) const ENOENT: isize = 2;
/// 进程不存在。
pub(crate) const ESRCH: isize = 3;
/// 系统调用被中断。
pub(crate) const EINTR: isize = 4;
/// 输入输出错误。
pub(crate) const EIO: isize = 5;
/// 当前 Process 没有 controlling TTY 等目标设备。
pub(crate) const ENXIO: isize = 6;
/// 参数列表过长。
pub(crate) const E2BIG: isize = 7;
/// 可执行文件格式无效。
pub(crate) const ENOEXEC: isize = 8;
/// 无效文件描述符。
pub(crate) const EBADF: isize = 9;
/// 没有匹配的 child process。
pub(crate) const ECHILD: isize = 10;
/// 暂时无法创建资源。
pub(crate) const EAGAIN: isize = 11;
/// 无法分配内存。
pub(crate) const ENOMEM: isize = 12;
/// 权限不足。
pub(crate) const EACCES: isize = 13;
/// 无效用户空间地址。
pub(crate) const EFAULT: isize = 14;
pub(crate) const EEXIST: isize = 17;
/// old/new pathname 不属于同一 mounted filesystem。
pub(crate) const EXDEV: isize = 18;
/// fd backend 不支持所请求的设备映射操作。
pub(crate) const ENODEV: isize = 19;
/// 路径分量不是目录。
pub(crate) const ENOTDIR: isize = 20;
pub(crate) const EISDIR: isize = 21;
/// 无效参数。
pub(crate) const EINVAL: isize = 22;
pub(crate) const EMFILE: isize = 24;
/// fd 不是 TTY 或 TTY 不属于 caller session。
pub(crate) const ENOTTY: isize = 25;
pub(crate) const ENOSPC: isize = 28;
/// 目标 filesystem 不允许 mutation。
pub(crate) const EROFS: isize = 30;
/// inode hard-link count 已达到 on-disk 表达上限。
pub(crate) const EMLINK: isize = 31;
/// pipe 没有 reader。
pub(crate) const EPIPE: isize = 32;
pub(crate) const ESPIPE: isize = 29;
/// 结果超出支持范围。
pub(crate) const ERANGE: isize = 34;
/// 路径或参数字符串过长。
pub(crate) const ENAMETOOLONG: isize = 36;
pub(crate) const ENOTEMPTY: isize = 39;
/// 系统调用未实现。
pub(crate) const ENOSYS: isize = 38;
/// 符号链接解析超出支持范围。
pub(crate) const ELOOP: isize = 40;
pub(crate) const ENOTSOCK: isize = 88;
/// datagram socket 未连接且调用者没有提供目标地址。
pub(crate) const EDESTADDRREQ: isize = 89;
/// datagram 超过协议可表达的最大 payload。
pub(crate) const EMSGSIZE: isize = 90;
pub(crate) const EOPNOTSUPP: isize = 95;
pub(crate) const EAFNOSUPPORT: isize = 97;
pub(crate) const EADDRINUSE: isize = 98;
/// 请求的本地或远端地址在当前 interface 上不可用。
pub(crate) const EADDRNOTAVAIL: isize = 99;
/// 当前 interface/route 无法到达目标网络。
pub(crate) const ENETUNREACH: isize = 101;
/// nonblocking connect 已经在进行。
pub(crate) const EALREADY: isize = 114;
/// nonblocking connect 已启动但尚未完成。
pub(crate) const EINPROGRESS: isize = 115;
pub(crate) const ENOTCONN: isize = 107;
pub(crate) const EISCONN: isize = 106;
/// 已建立连接被 peer reset。
pub(crate) const ECONNRESET: isize = 104;
pub(crate) const ECONNREFUSED: isize = 111;
pub(crate) const EPROTONOSUPPORT: isize = 93;
pub(crate) const ESOCKTNOSUPPORT: isize = 94;
pub(crate) const ENOPROTOOPT: isize = 92;
/// 结果无法由目标文件系统或 ABI 字段表示。
pub(crate) const EOVERFLOW: isize = 75;
/// 等待在 deadline 前未完成。
pub(crate) const ETIMEDOUT: isize = 110;
