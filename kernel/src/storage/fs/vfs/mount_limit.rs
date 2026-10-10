//! Linux fs.mount-max的唯一全局policy owner；各MountScope只核算其真实live membership。
use super::FileSystemError;
use core::sync::atomic::{AtomicU32, Ordering};
// OWNER: fixed Linux默认100000，sysctl可降至既有count以下；不撤销既有mount/move/unmount。
// Relaxed只发布一个整数，不承担森林关系的publication，count准入仍持VFS mutation gate。
static MAXIMUM: AtomicU32 = AtomicU32::new(100_000);
pub(crate) fn mount_limit() -> u32 {
    MAXIMUM.load(Ordering::Relaxed)
}
/// proc_dointvec_minmax的1..INT_MAX范围；caller在proc/sysctl权限边界授权，owner不借Task。
pub(crate) fn set_mount_limit(value: u32) -> Result<(), FileSystemError> {
    if !(1..=i32::MAX as u32).contains(&value) {
        return Err(FileSystemError::InvalidOperation);
    }
    MAXIMUM.store(value, Ordering::Relaxed);
    Ok(())
}
