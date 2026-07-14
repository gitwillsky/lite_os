use alloc::{sync::Arc, vec::Vec};

use super::{FileDescriptorTable, OpenFileDescription, OpenFileKind};
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

impl FileDescriptorTable {
    /// @description 在 fd-table lock 内复制 live descriptor/OFD identity，供 procfs 锁外解析路径。
    /// @return 按 fd 递增的 `(descriptor, OFD)` 快照；内存不足返回错误。
    pub(crate) fn snapshot(&self) -> Result<Vec<(usize, Arc<OpenFileDescription>)>, ()> {
        let count = self.entries.iter().filter(|entry| entry.is_some()).count();
        let mut snapshot = Vec::new();
        snapshot.try_reserve_exact(count).map_err(|_| ())?;
        snapshot.extend(
            self.entries
                .iter()
                .enumerate()
                .filter_map(|(fd, entry)| entry.as_ref().map(|entry| (fd, entry.ofd.clone()))),
        );
        Ok(snapshot)
    }
}
