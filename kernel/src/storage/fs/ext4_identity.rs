//! ext4 超级块里的 `UUID` 与 `LABEL`，供 `root=UUID=`/`root=LABEL=` 解析使用。
//!
//! 纯函数，不依赖设备注册表。

use crate::block::identity::format_uuid;

/// ext4 超级块（从偏移 1024 起的 1024 字节）里的 UUID 与卷标；不是 ext4 返回 `None`。
pub(super) fn ext4_identity(superblock: &[u8]) -> Option<([u8; 16], &[u8])> {
    const MAGIC_OFFSET: usize = 0x38;
    const UUID_OFFSET: usize = 0x68;
    const LABEL_OFFSET: usize = 0x78;
    if superblock.len() < LABEL_OFFSET + 16
        || u16::from_le_bytes([superblock[MAGIC_OFFSET], superblock[MAGIC_OFFSET + 1]]) != 0xef53
    {
        return None;
    }
    let uuid = superblock[UUID_OFFSET..UUID_OFFSET + 16].try_into().ok()?;
    let label = &superblock[LABEL_OFFSET..LABEL_OFFSET + 16];
    let length = label.iter().position(|byte| *byte == 0).unwrap_or(16);
    Some((uuid, &label[..length]))
}

/// 选项值指这个 ext4 吗：`UUID=` 比 UUID 文本，`LABEL=` 比卷标（卷标区分大小写，UUID 不区分）。
pub(super) fn ext4_matches(superblock: &[u8], by_label: bool, wanted: &[u8]) -> bool {
    let Some((uuid, label)) = ext4_identity(superblock) else {
        return false;
    };
    if by_label {
        label == wanted
    } else {
        format_uuid(&uuid).as_bytes().eq_ignore_ascii_case(wanted)
    }
}
