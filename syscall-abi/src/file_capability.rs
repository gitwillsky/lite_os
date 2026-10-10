//! 固定 Linux security.capability xattr wire codec；namespace/credential policy留在实际kernel owner。

pub const NAME: &[u8] = b"security.capability";
pub const REVISION_1: u32 = 0x0100_0000;
pub const REVISION_2: u32 = 0x0200_0000;
pub const REVISION_3: u32 = 0x0300_0000;
pub const REVISION_MASK: u32 = 0xff00_0000;
pub const EFFECTIVE: u32 = 1;
pub const MAX_SIZE: usize = 24;

/// 一个完整的little-endian vfs_cap_data/vfs_ns_cap_data；不拥有xattr、inode或capability状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileCapabilityData {
    pub magic: u32,
    pub permitted: u64,
    pub inheritable: u64,
    /// revision1/2隐含superblock user namespace的UID0；revision3是该namespace内的rootid。
    pub root_uid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileCapabilityError {
    Invalid,
}

fn size(magic: u32) -> Result<usize, FileCapabilityError> {
    match magic & REVISION_MASK {
        REVISION_1 => Ok(12),
        REVISION_2 => Ok(20),
        REVISION_3 => Ok(24),
        _ => Err(FileCapabilityError::Invalid),
    }
}

impl FileCapabilityData {
    /// 读取一个完整disk xattr；get_vfs_caps_from_disk接受旧revision1及未知非revision标志。
    /// 长度与revision必须精确匹配；不把未来capability bits截断成当前VALID_MASK。
    pub fn from_disk(bytes: &[u8]) -> Result<Self, FileCapabilityError> {
        let magic = u32::from_le_bytes(
            bytes
                .get(..4)
                .ok_or(FileCapabilityError::Invalid)?
                .try_into()
                .unwrap(),
        );
        let length = size(magic)?;
        if bytes.len() != length {
            return Err(FileCapabilityError::Invalid);
        }
        let word = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        let mut permitted = u64::from(word(4));
        let mut inheritable = u64::from(word(8));
        if length >= 20 {
            permitted |= u64::from(word(12)) << 32;
            inheritable |= u64::from(word(16)) << 32;
        }
        Ok(Self {
            magic,
            permitted,
            inheritable,
            root_uid: if length == 24 { word(20) } else { 0 },
        })
    }

    /// 校验setxattr输入的fixed commoncap validheader；只允许revision2/3与EFFECTIVE标志。
    /// namespace rootid转换和CAP_SETFCAP检查由调用者对实际inode/superblock授权后执行。
    pub fn for_setxattr(bytes: &[u8]) -> Result<Self, FileCapabilityError> {
        let value = Self::from_disk(bytes)?;
        if !matches!(value.magic & !EFFECTIVE, REVISION_2 | REVISION_3) {
            return Err(FileCapabilityError::Invalid);
        }
        Ok(value)
    }

    pub const fn effective(self) -> bool {
        self.magic & EFFECTIVE != 0
    }

    /// 按自身revision编码；返回有效prefix长度，末尾padding不属于xattr内容。
    /// 未知revision返回Invalid；存储读出的非revision标志原样保存。
    pub fn encode(self) -> Result<([u8; MAX_SIZE], usize), FileCapabilityError> {
        let length = size(self.magic)?;
        if (length < 24 && self.root_uid != 0)
            || (length == 12 && (self.permitted | self.inheritable) >> 32 != 0)
        {
            return Err(FileCapabilityError::Invalid);
        }
        let mut output = [0; MAX_SIZE];
        for (offset, word) in [
            (0, self.magic),
            (4, self.permitted as u32),
            (8, self.inheritable as u32),
        ] {
            output[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
        }
        if length >= 20 {
            output[12..16].copy_from_slice(&((self.permitted >> 32) as u32).to_le_bytes());
            output[16..20].copy_from_slice(&((self.inheritable >> 32) as u32).to_le_bytes());
        }
        if length == 24 {
            output[20..24].copy_from_slice(&self.root_uid.to_le_bytes());
        }
        Ok((output, length))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V3: [u8; 24] = [
        1, 0, 0, 3, 2, 0, 0, 0, 4, 0, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 232, 3, 0, 0,
    ];

    #[test]
    fn disk_layout_preserves_high_words_and_namespaced_root() {
        let value = FileCapabilityData::from_disk(&V3).unwrap();
        assert_eq!(value.permitted, (1 << 40) | 2);
        assert_eq!(value.inheritable, (1 << 41) | 4);
        assert_eq!(value.root_uid, 1000);
        assert!(value.effective());
        let (encoded, length) = value.encode().unwrap();
        assert_eq!(&encoded[..length], V3);
    }

    #[test]
    fn disk_revision1_and_reserved_flags_do_not_become_valid_setxattr_input() {
        let v1 = [1, 0, 0, 1, 8, 0, 0, 0, 16, 0, 0, 0];
        assert_eq!(FileCapabilityData::from_disk(&v1).unwrap().permitted, 8);
        assert_eq!(
            FileCapabilityData::for_setxattr(&v1),
            Err(FileCapabilityError::Invalid)
        );
        let mut flags = V3;
        flags[0] = 3;
        assert!(FileCapabilityData::from_disk(&flags).is_ok());
        assert_eq!(
            FileCapabilityData::for_setxattr(&flags),
            Err(FileCapabilityError::Invalid)
        );
    }

    #[test]
    fn truncated_extended_or_unknown_revision_data_is_invalid() {
        for length in 0..V3.len() {
            assert_eq!(
                FileCapabilityData::from_disk(&V3[..length]),
                Err(FileCapabilityError::Invalid)
            );
        }
        let mut extended = [0; 25];
        extended[..24].copy_from_slice(&V3);
        assert_eq!(
            FileCapabilityData::from_disk(&extended),
            Err(FileCapabilityError::Invalid)
        );
        let mut future = V3;
        future[3] = 4;
        assert_eq!(
            FileCapabilityData::from_disk(&future),
            Err(FileCapabilityError::Invalid)
        );
    }

    #[test]
    fn older_encoding_cannot_silently_drop_rootid_or_high_capability_bits() {
        let original = FileCapabilityData::from_disk(&V3).unwrap();
        assert_eq!(
            FileCapabilityData {
                magic: REVISION_2,
                ..original
            }
            .encode(),
            Err(FileCapabilityError::Invalid)
        );
        assert_eq!(
            FileCapabilityData {
                magic: REVISION_1,
                root_uid: 0,
                ..original
            }
            .encode(),
            Err(FileCapabilityError::Invalid)
        );
        let value = FileCapabilityData {
            magic: REVISION_2 | EFFECTIVE,
            root_uid: 0,
            ..original
        };
        let (encoded, length) = value.encode().unwrap();
        assert_eq!(length, 20);
        assert_eq!(
            FileCapabilityData::for_setxattr(&encoded[..length]),
            Ok(value)
        );
    }
}
