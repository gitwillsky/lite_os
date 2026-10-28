//! FUSE：VFS 经 `/dev/fuse` 把 inode 操作交给用户态守护进程（Linux `fs/fuse`，协议 7.45）。
//!
//! - `device`：`/dev/fuse`（10:229）；每次 open 一个未绑定端点，挂载或 CLONE 后绑定连接。
//! - `connection`：请求队列、回复匹配、中断与中止；一个挂载一个连接。
//! - `protocol`：请求与回复的字节编解码。
//! - `filesystem`：`fuse` 类型、superblock 与守护进程通知；`options` 解析挂载选项。
//! - `inode`：VFS inode 操作到 FUSE 请求的翻译，以及 entry/attr 超时缓存与服务器文件句柄。
use super::FileSystemError;

mod connection;
mod control;
mod device;
mod filesystem;
mod inode;
mod options;
mod protocol;

/// 登记 `/dev/fuse` 与 `fuse`/`fusectl` 文件系统类型。
///
/// # Errors
///
/// 设备号或类型名已登记返回 `AlreadyExists`；分配失败返回 `OutOfMemory`。
pub(super) fn register() -> Result<(), FileSystemError> {
    device::register()?;
    super::register_filesystem(&filesystem::FuseFileSystemType)?;
    super::register_filesystem(&control::ControlFileSystemType)
}
