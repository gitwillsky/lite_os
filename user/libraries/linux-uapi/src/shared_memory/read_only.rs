use std::{io, os::fd::BorrowedFd, sync::OnceLock};

use super::SharedMapping;
use crate::raw;

unsafe extern "C" {
    fn liteos_install_shm_bus_handler() -> i32;
    fn liteos_copy_shm_pixels(
        source: *const u8,
        source_stride: usize,
        output: *mut u32,
        output_stride: usize,
        width: usize,
        height: usize,
        mask: u32,
    ) -> i32;
}

// One installation per process preserves the previous disposition. A failed installation
// prevents faultable reads. OnceLock keeps concurrent admissions out of the C setup.
static BUS_HANDLER: OnceLock<io::Result<()>> = OnceLock::new();

/// Owned read-only shared mapping with recovery from backing-file SIGBUS.
pub struct ReadOnlySharedMapping(SharedMapping);

impl ReadOnlySharedMapping {
    /// Maps `len` bytes from offset zero of `fd`; the mapping outlives the borrowed fd.
    ///
    /// Returns an I/O error if the mapping or SIGBUS handler cannot be established.
    pub fn map(fd: BorrowedFd<'_>, len: usize) -> io::Result<Self> {
        let mapping = SharedMapping::map_with_protection(fd, len, raw::PROT_READ)?;
        let handler = BUS_HANDLER.get_or_init(|| {
            // SAFETY: OnceLock serializes installation before any guarded access.
            if unsafe { liteos_install_shm_bus_handler() } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
        if let Err(error) = handler {
            return Err(io::Error::new(
                error.kind(),
                "failed to install mapped access handler",
            ));
        }
        Ok(Self(mapping))
    }

    /// Exact mapped byte length.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the mapped extent is empty. Successful mappings have positive lengths.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Copies native-endian words from a two-dimensional region of this mapping.
    ///
    /// `offset` and `source_stride` count bytes. `output_stride` and `extent`
    /// ([width, height]) count words. Each word is ORed with `mask` before storing.
    /// Returns `InvalidInput` for invalid extents or `UnexpectedEof` for a faulting
    /// source page, which may leave a partial output. A short backing file can still
    /// supply zeroes from the end of its last page. Callers requiring exact file
    /// contents must verify the current file length separately.
    pub fn copy_words(
        &self,
        offset: usize,
        source_stride: usize,
        output: &mut [u32],
        output_stride: usize,
        [width, height]: [usize; 2],
        mask: u32,
    ) -> io::Result<()> {
        let invalid = io::ErrorKind::InvalidInput;
        let last_row = height.checked_sub(1).ok_or(invalid)?;
        let row_bytes = width.checked_mul(size_of::<u32>()).ok_or(invalid)?;
        let source_end = last_row
            .checked_mul(source_stride)
            .and_then(|bytes| bytes.checked_add(row_bytes))
            .and_then(|bytes| offset.checked_add(bytes))
            .ok_or(invalid)?;
        let output_end = last_row
            .checked_mul(output_stride)
            .and_then(|words| words.checked_add(width))
            .ok_or(invalid)?;
        if width == 0
            || row_bytes > source_stride
            || width > output_stride
            || source_end > self.len()
            || output_end > output.len()
        {
            return Err(invalid.into());
        }
        // SAFETY: both extents have been checked and stay live for the call. This
        // owner exposes no pointer; concurrent mapped writers only affect read values.
        // The signal handler resumes the C call before control returns to Rust.
        if unsafe {
            liteos_copy_shm_pixels(
                self.0.as_non_null().as_ptr().add(offset),
                source_stride,
                output.as_mut_ptr(),
                output_stride,
                width,
                height,
                mask,
            )
        } == 0
        {
            Ok(())
        } else {
            Err(io::ErrorKind::UnexpectedEof.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_memory::MemFd;
    use std::os::fd::AsFd;

    #[test]
    fn mapped_access_recovers_from_repeated_source_faults() {
        let memory = MemFd::allocate("faulting-mapping", 4096).unwrap();
        let mapping = ReadOnlySharedMapping::map(memory.as_fd(), 4096).unwrap();
        memory.file.set_len(0).unwrap();

        let mut output = [0u32; 1];
        for _ in 0..2 {
            let error = mapping
                .copy_words(0, 4, &mut output, 1, [1, 1], 0)
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        }
        memory.file.set_len(4096).unwrap();
        mapping
            .copy_words(0, 4, &mut output, 1, [1, 1], 0xff00_0000)
            .unwrap();
        assert_eq!(output, [0xff00_0000]);
    }
}
