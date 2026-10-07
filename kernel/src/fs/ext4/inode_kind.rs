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
/// - `kind`: 已由 caller 限制为 regular、directory 或 socket。
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
        _ => unreachable!("unsupported ext4 create kind crossed validation"),
    };
    kind | permissions as u16 & 0o7777
}
