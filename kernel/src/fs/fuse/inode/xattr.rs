//! FUSE 扩展属性：属性值仅属于服务端，不在 inode 中复制或缓存。
use alloc::vec::Vec;
use syscall_abi::{errno, fuse};

use super::{FuseInode, Inode, OwnerModeState, ReplyShape, Unsupported};
use crate::fs::fuse::protocol::{Fields, Reader};
use crate::fs::{FileSystemError, XattrChange, xattr::MAX_VALUE_SIZE};

impl FuseInode {
    /// get/list 共用的请求：零缓冲查询长度，非零缓冲请求实际值；不作长度查询后再读的竞态重试。
    fn xattr_read(&self, name: Option<&[u8]>, size: usize) -> Result<Vec<u8>, FileSystemError> {
        let options = &self.mount.options;
        if !options.allow_other
            && !self
                .mount
                .connection
                .current_allowed(options.user_id, options.group_id)
        {
            return Err(FileSystemError::AccessDenied);
        }
        let (opcode, unsupported) = if name.is_some() {
            (fuse::GETXATTR, Unsupported::GetXattr)
        } else {
            (fuse::LISTXATTR, Unsupported::ListXattr)
        };
        let body = Fields::<8>::new().u32(size as u32).finish();
        let shape = if size == 0 {
            ReplyShape::fixed(8)
        } else {
            ReplyShape::up_to(size)
        };
        match name {
            Some(name) => self.xattr_call(opcode, unsupported, &[&body, name, &[0]], shape),
            None => self.xattr_call(opcode, unsupported, &[&body], shape),
        }
    }

    /// 查询原始属性或名称列表长度；`name` 为 None 时查询 LISTXATTR。
    /// 返回 Linux 上限内的长度，传播服务端 errno、协议与分配错误。
    pub(super) fn remote_xattr_size(&self, name: Option<&[u8]>) -> Result<usize, FileSystemError> {
        let bytes = self.xattr_read(name, 0)?;
        Ok((Reader::new(&bytes).u32()? as usize).min(MAX_VALUE_SIZE))
    }

    /// 读取原始属性或名称列表；`name` 为 None 时读取 LISTXATTR。
    /// 返回唯一 owned 回复；列表必须完整 NUL 结尾且没有空名称，否则返回 EIO。
    pub(super) fn remote_xattr(&self, name: Option<&[u8]>) -> Result<Vec<u8>, FileSystemError> {
        let bytes = self.xattr_read(name, MAX_VALUE_SIZE)?;
        if name.is_none() {
            // Linux fuse_verify_xattr_list：空列表合法，空名称和未终止名称均是服务端协议错误。
            let mut remaining = bytes.as_slice();
            while !remaining.is_empty() {
                let end = remaining
                    .iter()
                    .position(|byte| *byte == 0)
                    .filter(|end| *end != 0)
                    .ok_or(FileSystemError::IoError)?;
                remaining = &remaining[end + 1..];
            }
        }
        Ok(bytes)
    }

    /// 修改服务端唯一属性事实；`change` 已持有 user-copy 后的名称/值和实际 mount authority。
    /// 成功仅使 inode 属性缓存过期；权限/能力转换、服务端 errno 或分配失败不发表本地属性。
    pub(super) fn change_remote_xattr(
        &self,
        mut change: XattrChange<'_>,
    ) -> Result<(), FileSystemError> {
        // 1. 复用共同 namespace/security/capability policy，普通 DAC 通过实际 FUSE permission 判定。
        let identity = change.identity;
        self.require_access(identity, 0)?;
        let metadata = self.metadata()?;
        change.prepare_access(
            &OwnerModeState::new(
                metadata.kind,
                metadata.mode as u16,
                metadata.uid,
                metadata.gid,
            ),
            self.check_xattr_write(),
            || self.require_access(identity, 2),
        )?;
        // 2. 未协商 SETXATTR_EXT 时使用标准 8-byte 头；名称含终止 NUL，空值仍为 SET 而非 REMOVE。
        if let Some(value) = change.value {
            if value.len() > MAX_VALUE_SIZE {
                return Err(FileSystemError::Errno(errno::E2BIG as u16));
            }
            let body = Fields::<8>::new()
                .u32(value.len() as u32)
                .u32(change.flags)
                .finish();
            self.xattr_call(
                fuse::SETXATTR,
                Unsupported::SetXattr,
                &[&body, change.name, &[0], &value],
                ReplyShape::EMPTY,
            )?;
        } else {
            self.xattr_call(
                fuse::REMOVEXATTR,
                Unsupported::RemoveXattr,
                &[change.name, &[0]],
                ReplyShape::EMPTY,
            )?;
        }
        // 3. ctime 由服务端的下一次 GETATTR 提供，不制造另一份属性时钟或本地 xattr 缓存。
        self.invalidate_attributes();
        Ok(())
    }

    fn xattr_call(
        &self,
        opcode: u32,
        unsupported: Unsupported,
        arguments: &[&[u8]],
        shape: ReplyShape,
    ) -> Result<Vec<u8>, FileSystemError> {
        if self.mount.unsupported(unsupported) {
            return Err(FileSystemError::NotSupported);
        }
        match self.call(opcode, arguments, shape) {
            Err(FileSystemError::Errno(code)) if code as isize == errno::ENOSYS => {
                self.mount.mark_unsupported(unsupported);
                Err(FileSystemError::NotSupported)
            }
            // raw inode xattr 的缺失统一为 NotFound；exec 的 capability 读取必须将其当作“无属性”。
            Err(FileSystemError::Errno(code)) if code as isize == errno::ENODATA => {
                Err(FileSystemError::NotFound)
            }
            result => result,
        }
    }
}
