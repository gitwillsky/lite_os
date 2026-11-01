//! 默认全局0644 sysctl的Linux test_perm；按GLOBAL_ROOT_EUID选owner，无inode DAC capability豁免。
use super::super::FileSystemError;
pub(super) fn require(effective_uid: u32, requested: u8) -> Result<(), FileSystemError> {
    let granted = if effective_uid == 0 { 6 } else { 4 };
    if requested & granted != requested {
        Err(FileSystemError::AccessDenied)
    } else {
        Ok(())
    }
}
