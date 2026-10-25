//! Typed filesystem capacity queries absent from std.

use std::{io, path::Path};

/// Atomically moves `from` to `to` without replacing an existing destination.
///
/// Uses Linux renameat2(RENAME_NOREPLACE), including for directories and symlinks.
/// Returns AlreadyExists when the destination exists, the native errno on other
/// failures, InvalidInput for an interior NUL, or Unsupported off Linux. A failed
/// call leaves both directory entries unchanged.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let from = CString::new(from.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let to = CString::new(to.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: both paths are live NUL-terminated native byte strings; no pointer is retained.
        if unsafe { crate::raw::renameat2(-100, from.as_ptr(), -100, to.as_ptr(), 1) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (from, to);
        Err(io::ErrorKind::Unsupported.into())
    }
}

/// Filesystem capacity in bytes, using statvfs's fragment size.
pub struct Capacity {
    /// Bytes occupied by allocated blocks.
    pub used: u64,
    /// All free blocks, including blocks reserved from ordinary users.
    pub free: u64,
    /// Total capacity reported by the filesystem.
    pub total: u64,
}

/// Queries the filesystem containing `path`, without opening or changing it.
///
/// # Returns
/// Used, free and total byte counts; multiplication saturates on unrepresentable capacities.
///
/// # Errors
/// Returns InvalidInput for NUL in the path, the libc error on failure, or Unsupported off Linux.
pub fn capacity(path: &Path) -> io::Result<Capacity> {
    #[cfg(target_os = "linux")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut result = std::mem::MaybeUninit::<crate::raw::StatVfs>::zeroed();
        // SAFETY: path remains NUL terminated; result has the target musl LP64 statvfs layout.
        if unsafe { crate::raw::statvfs(path.as_ptr(), result.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful statvfs initialized the complete result.
        let stat = unsafe { result.assume_init() };
        let unit = if stat.fragment_size == 0 {
            stat.block_size
        } else {
            stat.fragment_size
        };
        let total = stat.blocks.saturating_mul(unit);
        let free = stat.blocks_free.saturating_mul(unit);
        Ok(Capacity {
            used: total.saturating_sub(free),
            free,
            total,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Err(io::ErrorKind::Unsupported.into())
    }
}
