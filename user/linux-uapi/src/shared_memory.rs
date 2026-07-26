//! Owned Linux memfd and shared-mapping interfaces.

use std::{
    ffi::CString,
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    ptr::NonNull,
};

use crate::raw;

#[cfg(target_os = "linux")]
const MFD_CLOEXEC: usize = 0x0001;
#[cfg(target_os = "linux")]
const MFD_ALLOW_SEALING: usize = 0x0002;
#[cfg(target_os = "linux")]
const F_ADD_SEALS: i32 = 1033;
#[cfg(target_os = "linux")]
const F_SEAL_SEAL: i32 = 0x0001;
#[cfg(target_os = "linux")]
const F_SEAL_SHRINK: i32 = 0x0002;
#[cfg(target_os = "linux")]
const F_SEAL_GROW: i32 = 0x0004;
#[cfg(target_os = "linux")]
const SYS_MEMFD_CREATE: isize = 279;

/// A sized anonymous shared-memory file.
pub struct MemFd {
    fd: OwnedFd,
    len: usize,
}

impl MemFd {
    /// Creates, sizes, and seals an anonymous shared-memory file.
    ///
    /// `F_SEAL_GROW|F_SEAL_SHRINK|F_SEAL_SEAL` fixes the ring extent while
    /// intentionally preserving writable mappings for producer and consumer.
    pub fn create(name: &str, len: usize) -> io::Result<Self> {
        if len == 0 || i64::try_from(len).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared mapping length must fit off_t",
            ));
        }
        let name = CString::new(name).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "memfd name contains an interior NUL",
            )
        })?;
        let fd = create_owned_fd(&name)?;
        if unsafe { raw::ftruncate(fd.as_raw_fd(), len as i64) } != 0 {
            return Err(io::Error::last_os_error());
        }
        #[cfg(target_os = "linux")]
        if unsafe {
            raw::fcntl(
                fd.as_raw_fd(),
                F_ADD_SEALS,
                F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, len })
    }

    /// Returns the immutable mapping extent.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns whether the mapping extent is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl AsFd for MemFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(target_os = "linux")]
fn create_owned_fd(name: &CString) -> io::Result<OwnedFd> {
    let result = unsafe {
        raw::syscall(
            SYS_MEMFD_CREATE,
            name.as_ptr(),
            MFD_CLOEXEC | MFD_ALLOW_SEALING,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(result as i32) })
    }
}

#[cfg(not(target_os = "linux"))]
fn create_owned_fd(_name: &CString) -> io::Result<OwnedFd> {
    use std::{
        fs::{self, OpenOptions},
        os::fd::IntoRawFd,
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(1);
    for _ in 0..64 {
        let identity = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("liteos-memfd-{}-{identity}", std::process::id()));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                fs::remove_file(path)?;
                return Ok(unsafe { OwnedFd::from_raw_fd(file.into_raw_fd()) });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "host shared-memory identity exhausted",
    ))
}

/// An owned `MAP_SHARED` mapping.
pub struct SharedMapping {
    address: NonNull<u8>,
    len: usize,
}

impl SharedMapping {
    /// Maps exactly `len` bytes from offset zero of `fd`.
    pub fn map(fd: BorrowedFd<'_>, len: usize) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared mapping cannot be empty",
            ));
        }
        let address = unsafe {
            raw::mmap(
                std::ptr::null_mut(),
                len,
                raw::PROT_READ | raw::PROT_WRITE,
                raw::MAP_SHARED,
                fd.as_raw_fd(),
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

    /// Returns the non-null mapping base.
    pub fn as_non_null(&self) -> NonNull<u8> {
        self.address
    }

    /// Returns the exact mapped byte length.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns whether the mapped byte length is zero.
    pub fn is_empty(&self) -> bool {
        false
    }
}

// SAFETY: moving the unique mapping owner between threads does not create an
// alias. Cross-thread access still requires the ring protocol's atomics.
unsafe impl Send for SharedMapping {}

impl Drop for SharedMapping {
    fn drop(&mut self) {
        let result = unsafe { raw::munmap(self.address.as_ptr().cast(), self.len) };
        debug_assert_eq!(result, 0, "munmap failed for owned shared mapping");
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use super::{MemFd, SharedMapping};

    #[test]
    fn owned_shared_mapping_preserves_bytes_for_the_memfd_lifetime() {
        let memfd = MemFd::create("linux-uapi-test", 4096).unwrap();
        let mapping = SharedMapping::map(memfd.as_fd(), memfd.len()).unwrap();
        // SAFETY: this test owns the only mapping access and writes within its exact extent.
        unsafe {
            mapping.as_non_null().as_ptr().write(0x5a);
            assert_eq!(mapping.as_non_null().as_ptr().read(), 0x5a);
        }
    }
}
