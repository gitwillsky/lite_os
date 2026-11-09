//! FUSE dirent 的共享字节机制；READDIRPLUS 的 entry 前缀仍由各调用者的元数据 owner 解码。

use native_runtime::native_abi::{self, Status};

/// 一条完整、8-byte 对齐的 FUSE 目录记录，仅借用原回复。
pub struct Record<'a> {
    /// 文件系统 inode 号。
    pub inode: u64,
    /// 守护进程提供的下一个不透明目录 cookie。
    pub offset: u64,
    /// Linux DT_* 目录项类型；未知值保留给调用者处理。
    pub kind: u32,
    /// 不含 NUL 的名字。
    pub name: &'a [u8],
    /// 包括尾部对齐的完整记录长度。
    pub bytes: usize,
}

impl<'a> Record<'a> {
    /// 从回复前缀检查完整目录项，不接受被截断的名称或 padding。
    /// # Errors
    /// 帧过短、名字为空/超长/含分隔符或 NUL、长度溢出。
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Status> {
        if bytes.len() < 24 {
            return Err(native_abi::ERR_IO);
        }
        let length = u32::from_ne_bytes(bytes[16..20].try_into().expect("header")) as usize;
        if !(1..=1024).contains(&length) {
            return Err(native_abi::ERR_IO);
        }
        let end = 24usize.checked_add(length).ok_or(native_abi::ERR_IO)?;
        let consumed = end.checked_next_multiple_of(8).ok_or(native_abi::ERR_IO)?;
        if consumed > bytes.len() {
            return Err(native_abi::ERR_IO);
        }
        let name = &bytes[24..end];
        if name.contains(&0) || name.contains(&b'/') {
            return Err(native_abi::ERR_IO);
        }
        Ok(Self {
            inode: u64::from_ne_bytes(bytes[..8].try_into().expect("header")),
            offset: u64::from_ne_bytes(bytes[8..16].try_into().expect("header")),
            kind: u32::from_ne_bytes(bytes[20..24].try_into().expect("header")),
            name,
            bytes: consumed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fuse_directory_record_checks_full_alignment_and_borrows_name() {
        let mut bytes = [0; 32];
        bytes[..8].copy_from_slice(&7u64.to_ne_bytes());
        bytes[8..16].copy_from_slice(&91u64.to_ne_bytes());
        bytes[16..20].copy_from_slice(&2u32.to_ne_bytes());
        bytes[20..24].copy_from_slice(&8u32.to_ne_bytes());
        bytes[24..26].copy_from_slice(b"ok");
        let record = Record::decode(&bytes).unwrap();
        assert_eq!(
            (record.inode, record.offset, record.kind, record.bytes),
            (7, 91, 8, 32)
        );
        assert_eq!(record.name.as_ptr(), bytes[24..].as_ptr());
        assert!(Record::decode(&bytes[..31]).is_err());
        bytes[24] = b'/';
        assert!(Record::decode(&bytes).is_err());
    }
}
