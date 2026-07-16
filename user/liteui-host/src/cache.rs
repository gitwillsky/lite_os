use alloc::vec::Vec;

use crate::{ffi, source};

const MAGIC: &[u8; 8] = b"LUIQBC1\0";
const FORMAT_VERSION: u32 = 1;
const LITEUI_ABI: u32 = 1;
const COMPILER_OPTIONS: u32 = 0;
const QUICKJS_BUILD_ID: &[u8; 10] = b"2026-06-04";
const HEADER_BYTES: usize = 72;
const MAX_BYTECODE_BYTES: usize = 2 * 1024 * 1024;
const SHELL_CACHE_PREFIX: &[u8] = b"/var/cache/liteui/100/qjs-2026-06-04-abi1-opt0-";
const APPLICATION_CACHE_PREFIX: &[u8] = b"/var/cache/liteui/102/qjs-2026-06-04-abi1-opt0-";

pub struct Key {
    path: Vec<u8>,
    temporary: Vec<u8>,
    bundle_sha256: [u8; 32],
}

pub struct Hit {
    bytes: Vec<u8>,
}

impl Hit {
    pub fn bytecode(&self) -> &[u8] {
        &self.bytes[HEADER_BYTES..]
    }
}

impl Key {
    pub fn try_new(bundle_sha256: [u8; 32]) -> Result<Self, ()> {
        let prefix = match unsafe { ffi::getuid() } {
            100 => SHELL_CACHE_PREFIX,
            102 => APPLICATION_CACHE_PREFIX,
            _ => return Err(()),
        };
        let mut path = Vec::new();
        path.try_reserve_exact(prefix.len() + 64 + 5)
            .map_err(|_| ())?;
        path.extend_from_slice(prefix);
        for byte in bundle_sha256 {
            path.push(hex(byte >> 4));
            path.push(hex(byte & 0x0f));
        }
        path.extend_from_slice(b".qbc\0");
        let mut temporary = Vec::new();
        temporary
            .try_reserve_exact(path.len() - 1 + 5)
            .map_err(|_| ())?;
        temporary.extend_from_slice(&path[..path.len() - 1]);
        temporary.extend_from_slice(b".tmp\0");
        Ok(Self {
            path,
            temporary,
            bundle_sha256,
        })
    }

    pub fn load(&self) -> Result<Option<Hit>, ()> {
        let bytes = match source::read_path(&self.path, HEADER_BYTES + MAX_BYTECODE_BYTES) {
            Ok(bytes) => bytes,
            Err(source::ReadError::NotFound) => return Ok(None),
            Err(source::ReadError::Failed) => return Err(()),
        };
        if !valid_header(&bytes, self.bundle_sha256) {
            self.remove();
            return Ok(None);
        }
        Ok(Some(Hit { bytes }))
    }

    pub fn store(&self, bytecode: &[u8]) -> Result<(), ()> {
        if bytecode.is_empty() || bytecode.len() > MAX_BYTECODE_BYTES {
            return Err(());
        }
        self.remove_temporary();
        let descriptor = unsafe {
            ffi::open(
                self.temporary.as_ptr().cast(),
                ffi::O_WRONLY | ffi::O_CREAT | ffi::O_TRUNC | ffi::O_CLOEXEC,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(());
        }
        let header = header(self.bundle_sha256, bytecode.len())?;
        let result = write_all(descriptor, &header)
            .and_then(|()| write_all(descriptor, bytecode))
            .and_then(|()| {
                (unsafe { ffi::fsync(descriptor) } == 0)
                    .then_some(())
                    .ok_or(())
            });
        let close = unsafe { ffi::close(descriptor) };
        if result.is_err()
            || close != 0
            || unsafe { ffi::rename(self.temporary.as_ptr().cast(), self.path.as_ptr().cast()) }
                != 0
        {
            self.remove_temporary();
            return Err(());
        }
        Ok(())
    }

    pub fn remove(&self) {
        unsafe { ffi::unlink(self.path.as_ptr().cast()) };
    }

    fn remove_temporary(&self) {
        unsafe { ffi::unlink(self.temporary.as_ptr().cast()) };
    }
}

fn header(bundle_sha256: [u8; 32], payload_length: usize) -> Result<[u8; HEADER_BYTES], ()> {
    let payload_length = u32::try_from(payload_length).map_err(|_| ())?;
    let mut output = [0u8; HEADER_BYTES];
    output[..8].copy_from_slice(MAGIC);
    output[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    output[12..16].copy_from_slice(&LITEUI_ABI.to_le_bytes());
    output[16..20].copy_from_slice(&COMPILER_OPTIONS.to_le_bytes());
    output[20..24].copy_from_slice(&payload_length.to_le_bytes());
    output[24..56].copy_from_slice(&bundle_sha256);
    output[56..66].copy_from_slice(QUICKJS_BUILD_ID);
    Ok(output)
}

fn valid_header(bytes: &[u8], bundle_sha256: [u8; 32]) -> bool {
    if bytes.len() < HEADER_BYTES || bytes[..8] != *MAGIC {
        return false;
    }
    read_u32(bytes, 8) == Some(FORMAT_VERSION)
        && read_u32(bytes, 12) == Some(LITEUI_ABI)
        && read_u32(bytes, 16) == Some(COMPILER_OPTIONS)
        && read_u32(bytes, 20).is_some_and(|length| length as usize == bytes.len() - HEADER_BYTES)
        && bytes[24..56] == bundle_sha256
        && bytes[56..66] == *QUICKJS_BUILD_ID
        && bytes[66..HEADER_BYTES].iter().all(|byte| *byte == 0)
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
        .get(offset..offset + 4)
        .and_then(|value| <[u8; 4]>::try_from(value).ok())
        .map(u32::from_le_bytes)
}

fn write_all(descriptor: i32, bytes: &[u8]) -> Result<(), ()> {
    let mut written = 0;
    while written < bytes.len() {
        let count = unsafe {
            ffi::write(
                descriptor,
                bytes[written..].as_ptr().cast(),
                bytes.len() - written,
            )
        };
        if count > 0 {
            written += count as usize;
        } else if count < 0 && ffi::errno() == ffi::EINTR {
            continue;
        } else {
            return Err(());
        }
    }
    Ok(())
}

fn hex(value: u8) -> u8 {
    if value < 10 {
        b'0' + value
    } else {
        b'a' + value - 10
    }
}
