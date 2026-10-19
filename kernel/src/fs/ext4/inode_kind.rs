use super::InodeType;

/// 解码 ext4 `i_mode` 的 packed inode type。
///
/// # Parameters
///
/// - `mode`: disk inode mode。
///
/// # Returns
///
/// VFS inode kind；未知/regular encoding 按 ext4 regular file 处理。
pub(super) fn from_mode(mode: u16) -> InodeType {
    match mode & 0xF000 {
        0x1000 => InodeType::Fifo,
        0x2000 => InodeType::CharacterDevice,
        0x6000 => InodeType::BlockDevice,
        0x4000 => InodeType::Directory,
        0xA000 => InodeType::SymLink,
        0xC000 => InodeType::Socket,
        _ => InodeType::File,
    }
}

/// 解码 directory entry file type；未知值按 regular file 处理。
pub(super) fn from_file_type(file_type: u8) -> InodeType {
    match file_type {
        2 => InodeType::Directory,
        7 => InodeType::SymLink,
        3 => InodeType::CharacterDevice,
        4 => InodeType::BlockDevice,
        5 => InodeType::Fifo,
        6 => InodeType::Socket,
        _ => InodeType::File,
    }
}

/// 编码 ext4 directory entry file type。
///
/// # Parameters
///
/// - `kind`: VFS inode kind。
///
/// # Returns
///
/// ext4 dirent type byte。
pub(super) fn file_type(kind: InodeType) -> u8 {
    match kind {
        InodeType::Fifo => 5,
        InodeType::CharacterDevice => 3,
        InodeType::BlockDevice => 4,
        InodeType::Directory => 2,
        InodeType::File => 1,
        InodeType::SymLink => 7,
        InodeType::Socket => 6,
    }
}

/// 编码 create transaction 的 inode type 与 permission bits。
///
/// # Parameters
///
/// - `kind`: 已由 caller 限制为 regular、directory、socket、FIFO 或设备节点。
/// - `permissions`: VFS 已应用 umask/setgid 的 mode。
///
/// # Returns
///
/// ext4 packed `i_mode`。
pub(super) fn create_mode(kind: InodeType, permissions: u32) -> u16 {
    let kind = match kind {
        InodeType::Directory => 0x4000,
        InodeType::Socket => 0xC000,
        InodeType::File => 0x8000,
        InodeType::Fifo => 0x1000,
        InodeType::CharacterDevice => 0x2000,
        InodeType::BlockDevice => 0x6000,
        _ => unreachable!("unsupported ext4 create kind crossed validation"),
    };
    kind | permissions as u16 & 0o7777
}

/// 设备节点把设备号存进 `i_block`（Linux `ext4_set_inode_rdev`）：
/// major、minor 都小于 256 用 `i_block[0]` 的旧编码，否则用 `i_block[1]` 的 `new_encode_dev`。
///
/// # Returns
///
/// 60 字节的 little-endian `i_block` 内容。
pub(super) fn encode_device(major: u32, minor: u32) -> [u8; 60] {
    let mut words = [0u32; 15];
    if major < 256 && minor < 256 {
        words[0] = major << 8 | minor;
    } else {
        words[1] = minor & 0xff | major << 8 | (minor & !0xff) << 12;
    }
    let mut bytes = [0u8; 60];
    for (chunk, word) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(words) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    bytes
}

/// 解码 [`encode_device`] 写入的设备号（Linux `ext4_iget` 的 rdev 解码）。
///
/// # Returns
///
/// `(major, minor)`。
pub(super) fn decode_device(block: &[u8; 60]) -> (u32, u32) {
    let word = |index: usize| {
        u32::from_le_bytes(
            block[index * 4..index * 4 + 4]
                .try_into()
                .expect("i_block word is four bytes"),
        )
    };
    let old = word(0);
    if old != 0 {
        return ((old >> 8) & 0xff, old & 0xff);
    }
    let new = word(1);
    ((new & 0xfff00) >> 8, new & 0xff | (new >> 12) & 0xfff00)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_device_numbers_use_the_old_encoding_and_large_ones_the_new() {
        let small = encode_device(254, 16);
        assert_eq!(&small[..4], &(254u32 << 8 | 16).to_le_bytes());
        assert_eq!(decode_device(&small), (254, 16));
        let large = encode_device(8, 300);
        assert_eq!(&large[..4], &[0; 4]);
        assert_eq!(decode_device(&large), (8, 300));
        assert_eq!(
            decode_device(&encode_device(0x123, 0x4567)),
            (0x123, 0x4567)
        );
    }

    #[test]
    fn device_zero_zero_decodes_to_zero_zero() {
        assert_eq!(decode_device(&encode_device(0, 0)), (0, 0));
    }

    #[test]
    fn create_mode_encodes_special_file_types() {
        assert_eq!(create_mode(InodeType::Fifo, 0o600), 0x1000 | 0o600);
        assert_eq!(
            create_mode(InodeType::CharacterDevice, 0o666),
            0x2000 | 0o666
        );
        assert_eq!(create_mode(InodeType::BlockDevice, 0o660), 0x6000 | 0o660);
    }
}
