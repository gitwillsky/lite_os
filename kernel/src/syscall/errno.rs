//! 当前 syscall handler 实际返回的 Linux errno。

/// 文件或目录不存在。
pub(crate) const ENOENT: isize = 2;
/// 进程不存在。
pub(crate) const ESRCH: isize = 3;
/// 系统调用被中断。
pub(crate) const EINTR: isize = 4;
/// 输入输出错误。
pub(crate) const EIO: isize = 5;
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
/// 路径分量不是目录。
pub(crate) const ENOTDIR: isize = 20;
pub(crate) const EISDIR: isize = 21;
/// 无效参数。
pub(crate) const EINVAL: isize = 22;
pub(crate) const EMFILE: isize = 24;
pub(crate) const ENOSPC: isize = 28;
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
