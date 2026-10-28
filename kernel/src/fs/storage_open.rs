//! OFD 拥有的 storage 打开状态与可选的直接 I/O；不把 per-open 选择复制到 inode。
use alloc::sync::Arc;

use super::{AccessIdentity, FileSystemError};

/// 绕过内容页缓存的 per-open I/O；调用者仍由同一 inode 的 cache owner 排序与维护一致性。
pub(crate) trait DirectIo: Send + Sync {
    /// 从 `offset` 读取到 `output`；返回实际字节数（短读不修改 inode EOF），传播后端错误。
    fn read(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError>;
    /// 按 `identity` 在 `offset` 写入 `input`；返回实际提交长度，传播后端错误。
    fn write(
        &self,
        offset: u64,
        input: &[u8],
        identity: &AccessIdentity,
    ) -> Result<usize, FileSystemError>;
    /// 在 append 选址前刷新长度；返回实际文件大小，传播元数据错误。
    fn size(&self) -> Result<u64, FileSystemError>;
}

/// 一次 storage 打开的 filesystem 私有状态（Linux `file->private_data`）。
/// OFD 持有它直到最后一个引用释放，Drop 即 release；缺失时 FUSE 的服务器句柄不能随 OFD 关闭。
pub(crate) trait StorageOpen: Send + Sync {
    /// 消费一个 `Arc` 引用，投影本次打开的直接 I/O；缺省 None，内容仍走 inode backing。
    fn direct_io(self: Arc<Self>) -> Option<Arc<dyn DirectIo>> {
        None
    }

    /// 为 `shared` 指定的 mmap 模式准入；成功返回 ()，后端可拒绝不能保证一致性的映射。
    /// 在 MAP_FIXED 替换旧 VMA 前调用，失败不能破坏旧映射。
    fn prepare_mapping(&self, _shared: bool) -> Result<(), FileSystemError> {
        Ok(())
    }

    /// 一个 fd 关闭（Linux `filp_close`）；`owner` 是关闭它的文件表身份。
    /// close/dup2 替换/CLOEXEC/进程退出各调用一次，与 OFD 最后释放的 release 不同。
    /// 后端错误由 close(2) 返回，描述符照常关闭；缺省无事可做。
    fn flush(&self, _owner: u64) -> Result<(), FileSystemError> {
        Ok(())
    }
}
