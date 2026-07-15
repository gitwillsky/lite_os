use alloc::vec::Vec;

use super::{OpenFileDescription, OpenFileKind};
use crate::fs::{FileSystemError, try_format_bytes, vfs};

impl OpenFileDescription {
    /// @description 投影 Linux `/proc/<pid>/fd/<n>` symbolic-link target。
    /// @return pathname-backed OFD 返回 VFS opened path；anonymous backend 返回标准 label。
    /// @errors VFS opened-entry 链损坏或内存不足时返回明确错误。
    pub(crate) fn proc_target(&self) -> Result<Vec<u8>, FileSystemError> {
        if let Some(opened) = self.opened_ref() {
            return vfs().opened_path(&opened);
        }
        match &self.kind {
            OpenFileKind::Pipe(endpoint) => {
                try_format_bytes(format_args!("pipe:[{}]", endpoint.pipe().object_id()))
            }
            OpenFileKind::Socket(socket) => {
                try_format_bytes(format_args!("socket:[{}]", socket.object_id()))
            }
            OpenFileKind::Epoll(_) | OpenFileKind::EventFd(_) => {
                let label = if matches!(self.kind, OpenFileKind::Epoll(_)) {
                    &b"anon_inode:[eventpoll]"[..]
                } else {
                    &b"anon_inode:[eventfd]"[..]
                };
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(label.len())
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                bytes.extend_from_slice(label);
                Ok(bytes)
            }
            OpenFileKind::Character(_) | OpenFileKind::Inode(_) => {
                unreachable!("pathname-backed OFD lost opened identity")
            }
        }
    }
}
