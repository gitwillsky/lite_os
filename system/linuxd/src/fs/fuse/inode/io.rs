//! 缓存填页与 per-open 直接 I/O 共用标准 READ/WRITE 编解码，直接短读不修改缓存 EOF。
use super::{FuseInode, HandleRef, ReplyShape, io_in, protocol};
use crate::fs::FileSystemError;
use alloc::vec::Vec;
use syscall_abi::fuse;

impl FuseInode {
    /// 用 `handle` 在 `offset` 读取至多 `size` 字节；返回唯一回复，传播连接/协议错误。
    pub(super) fn read_handle(
        &self,
        handle: HandleRef,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>, FileSystemError> {
        self.call(
            fuse::READ,
            &[&io_in(handle, offset, size as u32, 0)],
            ReplyShape::up_to(size),
        )
    }

    /// 用 `handle` 在 `offset` 写入 `buffer`，`write_flags` 区分页缓存写回与普通直接写。
    /// 返回已提交的完整前缀；首错传播，后续错误保留进度，成功写入才更新大小并作废属性。
    pub(super) fn write_handle(
        &self,
        handle: HandleRef,
        offset: u64,
        buffer: &[u8],
        write_flags: u32,
    ) -> Result<usize, FileSystemError> {
        let chunk = self.negotiated()?.write_chunk();
        let mut done = 0;
        while done < buffer.len() {
            let size = chunk.min(buffer.len() - done);
            let position = offset + done as u64;
            let reply = match self.call(
                fuse::WRITE,
                &[
                    &io_in(handle, position, size as u32, write_flags),
                    &buffer[done..done + size],
                ],
                ReplyShape::fixed(fuse::WRITE_OUT_SIZE),
            ) {
                Ok(reply) => reply,
                Err(_) if done != 0 => break,
                Err(error) => return Err(error),
            };
            let written = protocol::Reader::new(&reply).u32()? as usize;
            if written > size {
                if done == 0 {
                    return Err(FileSystemError::IoError);
                }
                break;
            }
            done += written;
            if written < size {
                break;
            }
        }
        if done != 0 {
            let mut cached = self.attributes.lock();
            cached.attributes.size = cached.attributes.size.max(offset + done as u64);
            cached.valid_until = 0;
        }
        Ok(done)
    }
}
