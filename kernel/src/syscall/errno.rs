//! 当前 syscall handler 实际返回的 Linux errno。

/// 文件或目录不存在。
pub const ENOENT: isize = 2;
/// 进程不存在。
pub const ESRCH: isize = 3;
/// 系统调用被中断。
pub const EINTR: isize = 4;
/// 输入输出错误。
pub const EIO: isize = 5;
/// 参数列表过长。
pub const E2BIG: isize = 7;
/// 可执行文件格式无效。
pub const ENOEXEC: isize = 8;
/// 无效文件描述符。
pub const EBADF: isize = 9;
/// 无法分配内存。
pub const ENOMEM: isize = 12;
/// 权限不足。
pub const EACCES: isize = 13;
/// 无效用户空间地址。
pub const EFAULT: isize = 14;
pub const EEXIST: isize = 17;
/// 路径分量不是目录。
pub const ENOTDIR: isize = 20;
pub const EISDIR: isize = 21;
/// 无效参数。
pub const EINVAL: isize = 22;
pub const EMFILE: isize = 24;
pub const ENOSPC: isize = 28;
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
