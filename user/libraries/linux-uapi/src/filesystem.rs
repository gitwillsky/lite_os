//! Typed filesystem capacity queries and read-only file mappings absent from std.

use std::{fs::File, io, os::fd::AsRawFd, path::Path, ptr::NonNull};

/// Atomically moves `from` to `to` without replacing an existing destination.
///
/// Uses Linux renameat2(RENAME_NOREPLACE), or Darwin renamex_np(RENAME_EXCL) for native host validation.
/// Both operations atomically cover directories and symlinks.
/// Returns AlreadyExists when the destination exists, the native errno on other
/// failures, InvalidInput for an interior NUL, or Unsupported on other platforms. A failed
/// call leaves both directory entries unchanged.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let from = CString::new(from.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let to = CString::new(to.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        #[cfg(target_os = "linux")]
        // SAFETY: both paths are live NUL-terminated native byte strings; no pointer is retained.
        let result = unsafe { crate::raw::renameat2(-100, from.as_ptr(), -100, to.as_ptr(), 1) };
        #[cfg(target_os = "macos")]
        // SAFETY: Darwin sys/stdio.h defines RENAME_EXCL=4; native strings remain live for the call.
        let result = unsafe { crate::raw::renamex_np(from.as_ptr(), to.as_ptr(), 4) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

/// An owned read-only `MAP_PRIVATE` view of a whole file.
///
/// Pages are faulted in from the page cache on first access, so processes mapping the same
/// file share physical pages and only touched pages count toward their RSS.
pub struct ReadOnlyMapping {
    address: NonNull<u8>,
    len: usize,
}

impl ReadOnlyMapping {
    /// Maps the complete current contents of `path` read-only.
    ///
    /// # Returns
    /// A mapping of the file length at open time; an empty file yields an empty view without mmap.
    ///
    /// # Errors
    /// Returns the open/stat/mmap error, or OutOfMemory when the file does not fit the address space.
    ///
    /// # Safety
    /// The file must not be modified or truncated while the mapping lives: writes would change
    /// bytes behind the returned `&[u8]`, and truncation turns later reads into SIGBUS.
    /// Replace such files with rename, which leaves existing mappings on the old inode.
    pub unsafe fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = usize::try_from(file.metadata()?.len())
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        if len == 0 {
            return Ok(Self {
                address: NonNull::dangling(),
                len,
            });
        }
        // SAFETY: fd is open for reading for the call; the kernel picks the address, and the
        // mapping keeps its own file reference after `file` closes.
        let address = unsafe {
            crate::raw::mmap(
                std::ptr::null_mut(),
                len,
                crate::raw::PROT_READ,
                crate::raw::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if address as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            address: NonNull::new(address.cast())
                .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "mmap returned null"))?,
            len,
        })
    }
}

impl AsRef<[u8]> for ReadOnlyMapping {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: `len` readable bytes stay mapped until Drop (or the view is empty and dangling);
        // the `open` contract forbids concurrent modification of the backing file.
        unsafe { std::slice::from_raw_parts(self.address.as_ptr(), self.len) }
    }
}

// SAFETY: the mapping is never written through, so sharing or moving the owner across threads
// only creates shared reads of immutable bytes.
unsafe impl Send for ReadOnlyMapping {}
// SAFETY: see Send; `&ReadOnlyMapping` exposes nothing but `&[u8]`.
unsafe impl Sync for ReadOnlyMapping {}

impl Drop for ReadOnlyMapping {
    fn drop(&mut self) {
        if self.len == 0 {
            return;
        }
        // SAFETY: address/len are exactly the live mapping created by `open`; no borrow outlives self.
        let result = unsafe { crate::raw::munmap(self.address.as_ptr().cast(), self.len) };
        debug_assert_eq!(result, 0, "munmap failed for owned read-only mapping");
    }
}
