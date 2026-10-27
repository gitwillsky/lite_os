//! Owned Linux memfd and shared-mapping interfaces.

use std::{
    ffi::CString,
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    ptr::NonNull,
};

use crate::raw;

#[cfg(target_os = "linux")]
const MFD_CLOEXEC: u32 = 0x0001;
#[cfg(target_os = "linux")]
const MFD_ALLOW_SEALING: u32 = 0x0002;
#[cfg(target_os = "linux")]
const F_ADD_SEALS: i32 = 1033;
#[cfg(target_os = "linux")]
const F_SEAL_SEAL: i32 = 0x0001;
#[cfg(target_os = "linux")]
const F_SEAL_SHRINK: i32 = 0x0002;
#[cfg(target_os = "linux")]
const F_SEAL_GROW: i32 = 0x0004;
#[cfg(target_os = "linux")]
const F_SEAL_WRITE: i32 = 0x0008;

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
        let memory = Self::allocate(name, len)?;
        #[cfg(target_os = "linux")]
        memory.seal(F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL)?;
        Ok(memory)
    }

    /// 在发布前一次填充协议表；Linux 同时封印内容与尺寸，不再允许 write 或共享写映射。
    ///
    /// `name` 和 `bytes` 只在本次创建中借用；空输入、fd、写入或 sealing 失败返回 io::Error。
    pub fn from_bytes(name: &str, bytes: &[u8]) -> io::Result<Self> {
        use std::{fs::File, os::unix::fs::FileExt};
        let memory = Self::allocate(name, bytes.len())?;
        File::from(memory.fd.try_clone()?).write_all_at(bytes, 0)?;
        #[cfg(target_os = "linux")]
        memory.seal(F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_WRITE | F_SEAL_SEAL)?;
        Ok(memory)
    }

    fn allocate(name: &str, len: usize) -> io::Result<Self> {
        if len == 0 || i64::try_from(len).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shared mapping length must fit off_t",
            ));
        }
        let name = CString::new(name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "memfd name contains NUL"))?;
        let fd = create_owned_fd(&name)?;
        // SAFETY: 新 owned file 尚未发布，长度已校验为有效 off_t，不保留任何用户指针。
        if unsafe { raw::ftruncate(fd.as_raw_fd(), len as i64) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, len })
    }

    #[cfg(target_os = "linux")]
    fn seal(&self, seals: i32) -> io::Result<()> {
        // SAFETY: 标准 fcntl F_ADD_SEALS，参数是整数 mask，不传指针。
        if unsafe { raw::fcntl(self.fd.as_raw_fd(), F_ADD_SEALS, seals) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Returns the immutable mapping extent.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns whether the mapping extent is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Writes native-endian 32-bit words into the complete sealed file extent.
    ///
    /// # Parameters
    /// - `words`: Initialized words whose byte length equals this memfd's length.
    ///
    /// # Returns
    /// `()` after all bytes have been written at offset zero, without changing the fd position.
    ///
    /// # Errors
    /// Returns `InvalidInput` on an extent mismatch, or the first fd duplication/write error.
    pub fn write_words(&self, words: &[u32]) -> io::Result<()> {
        use std::{fs::File, os::unix::fs::FileExt};
        let len = std::mem::size_of_val(words);
        if len != self.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "word extent does not match memfd",
            ));
        }
        // SAFETY: u32 has no padding; all bytes are initialized, and the byte view
        // borrows the immutable input only for write_all_at. u8 requires no alignment.
        // OWNER: the caller owns frame publication; this method never retains the view.
        // Without a typed byte view the client needs thousands of tiny encoded writes.
        let bytes = unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), len) };
        File::from(self.fd.try_clone()?).write_all_at(bytes, 0)
    }
}

impl AsFd for MemFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(target_os = "linux")]
fn create_owned_fd(name: &CString) -> io::Result<OwnedFd> {
    // SAFETY: CString 保活完整 NUL-terminated 名称；只传标准 memfd flags，返回 fd 由本函数接管。
    // syscall number 属于目标 libc；固定 asm-generic 编号会在 x86 host 调用 move_pages。
    let result = unsafe { raw::memfd_create(name.as_ptr(), MFD_CLOEXEC | MFD_ALLOW_SEALING) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: 非负返回值是新创建、尚未转交其他 owner 的唯一 fd。
        Ok(unsafe { OwnedFd::from_raw_fd(result) })
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
    fn word_write_matches_native_bytes_without_changing_fd_position() {
        use std::{
            fs::File,
            io::{Seek, SeekFrom},
            os::unix::fs::FileExt,
        };
        let memory = MemFd::create("word-write", 16).unwrap();
        let mut file = File::from(memory.as_fd().try_clone_to_owned().unwrap());
        file.seek(SeekFrom::Start(7)).unwrap();
        let words = [0x12345678u32, 0xffaabbcc, 0, u32::MAX];
        memory.write_words(&words).unwrap();
        let mut actual = [0u8; 16];
        file.read_exact_at(&mut actual, 0).unwrap();
        assert_eq!(
            actual.as_slice(),
            words
                .into_iter()
                .flat_map(u32::to_ne_bytes)
                .collect::<Vec<_>>()
        );
        assert_eq!(file.stream_position().unwrap(), 7);
        assert_eq!(
            memory.write_words(&words[..3]).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        let mut unchanged = [0u8; 16];
        file.read_exact_at(&mut unchanged, 0).unwrap();
        assert_eq!(actual, unchanged);
    }

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
