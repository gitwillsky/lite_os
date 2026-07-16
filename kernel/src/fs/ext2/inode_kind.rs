use super::InodeType;

/// @description 解码 ext2 `i_mode` 的 packed inode type。
/// @param mode disk inode mode。
/// @return VFS inode kind；未知/regular encoding 按 ext2 regular file 处理。
pub(super) fn from_mode(mode: u16) -> InodeType {
    match mode & 0xF000 {
        0x1000 => InodeType::Fifo,
        0x2000 => InodeType::CharacterDevice,
        0x4000 => InodeType::Directory,
        0xA000 => InodeType::SymLink,
        0xC000 => InodeType::Socket,
        _ => InodeType::File,
    }
}

/// @description 编码 ext2 directory entry file type。
/// @param kind VFS inode kind。
/// @return ext2 dirent type byte。
pub(super) fn file_type(kind: InodeType) -> u8 {
    match kind {
        InodeType::Fifo => 5,
        InodeType::CharacterDevice => 3,
        InodeType::Directory => 2,
        InodeType::File => 1,
        InodeType::SymLink => 7,
        InodeType::Socket => 6,
    }
}

/// @description 编码 create transaction 的 inode type 与 permission bits。
/// @param kind 已由 caller 限制为 regular、directory 或 socket。
/// @param permissions VFS 已应用 umask/setgid 的 mode。
/// @return ext2 packed `i_mode`。
pub(super) fn create_mode(kind: InodeType, permissions: u32) -> u16 {
    let kind = match kind {
        InodeType::Directory => 0x4000,
        InodeType::Socket => 0xC000,
        InodeType::File => 0x8000,
        _ => unreachable!("unsupported ext2 create kind crossed validation"),
    };
    kind | permissions as u16 & 0o7777
}
