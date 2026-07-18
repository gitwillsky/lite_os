use core::{mem, ptr};

use super::{Ext2DirEntry2Header, Ext2GroupDesc, Ext2InodeDisk, Ext2SuperBlock};

macro_rules! disk_layout {
    ($layout:ty) => {
        impl $layout {
            pub(super) const SIZE: usize = mem::size_of::<Self>();

            /// 从磁盘字节窗口解码一个可能未对齐的 packed 值。
            pub(super) fn decode(bytes: &[u8], offset: usize) -> Option<Self> {
                let end = offset.checked_add(Self::SIZE)?;
                let source = bytes.get(offset..end)?;
                // SAFETY: `source` 覆盖完整 packed 值；read_unaligned 按值复制且不形成
                // 指向磁盘缓冲区的引用。
                Some(unsafe { ptr::read_unaligned(source.as_ptr().cast::<Self>()) })
            }

            /// 把 packed 值编码到完整磁盘字节窗口。
            pub(super) fn encode(&self, bytes: &mut [u8], offset: usize) -> bool {
                let Some(end) = offset.checked_add(Self::SIZE) else {
                    return false;
                };
                let Some(target) = bytes.get_mut(offset..end) else {
                    return false;
                };
                // SAFETY: `target` 覆盖完整 packed 值；write_unaligned 按值写入且不会
                // 读取目标缓冲区中原有的未初始化内容。
                unsafe { ptr::write_unaligned(target.as_mut_ptr().cast::<Self>(), *self) };
                true
            }
        }
    };
}

disk_layout!(Ext2SuperBlock);
disk_layout!(Ext2GroupDesc);
disk_layout!(Ext2InodeDisk);
disk_layout!(Ext2DirEntry2Header);

const _: () = assert!(Ext2SuperBlock::SIZE == 1024);
const _: () = assert!(Ext2GroupDesc::SIZE == 32);
const _: () = assert!(Ext2InodeDisk::SIZE == 128);
const _: () = assert!(Ext2DirEntry2Header::SIZE == 8);

impl Ext2InodeDisk {
    /// 复制 fast-symlink 的 inode-inline payload，不暴露 packed field 地址。
    pub(super) fn copy_inline_symlink(&self, target: &mut [u8]) -> bool {
        if target.len() > mem::size_of::<[u32; 15]>() {
            return false;
        }
        // SAFETY: i_block 的 packed raw address 覆盖 60-byte inline payload；target 长度
        // 已受其上限约束，源与目标属于不同 owner 且不重叠。
        unsafe {
            ptr::copy_nonoverlapping(
                ptr::addr_of!(self.i_block).cast::<u8>(),
                target.as_mut_ptr(),
                target.len(),
            )
        };
        true
    }

    /// 写入 fast-symlink 的 inode-inline payload，不暴露 packed field 地址。
    pub(super) fn set_inline_symlink(&mut self, target: &[u8]) -> bool {
        if target.len() > mem::size_of::<[u32; 15]>() {
            return false;
        }
        // SAFETY: i_block 的 packed raw address 覆盖 60-byte inline payload；target 长度
        // 已受其上限约束，源与目标属于不同 owner 且不重叠。
        unsafe {
            ptr::copy_nonoverlapping(
                target.as_ptr(),
                ptr::addr_of_mut!(self.i_block).cast::<u8>(),
                target.len(),
            )
        };
        true
    }
}
