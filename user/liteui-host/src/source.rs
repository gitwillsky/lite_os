use alloc::vec::Vec;

use crate::ffi;

const APPLICATION_PREFIX: &[u8] = b"/usr/lib/liteui/apps/";
const MAX_APPLICATION_NAME: usize = 64;
const MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 16 * 1024;
const MAX_STYLES_BYTES: usize = 2 * 1024 * 1024;
const READ_CHUNK_BYTES: usize = 16 * 1024;

pub enum ReadError {
    NotFound,
    Failed,
}

pub struct Application {
    pub filename: Vec<u8>,
    pub manifest: Vec<u8>,
    pub program: Vec<u8>,
    pub styles: Vec<u8>,
}

pub fn read_application(directory: *const u8) -> Result<Application, ()> {
    let root = application_root(directory)?;
    let filename = joined(&root, b"/app.mjs\0")?;
    let manifest_path = joined(&root, b"/manifest.cbor\0")?;
    let styles_path = joined(&root, b"/styles.bin\0")?;
    let mut program = read_path(&filename, MAX_SOURCE_BYTES).map_err(|_| ())?;
    program.try_reserve(1).map_err(|_| ())?;
    program.push(0);
    let manifest = read_path(&manifest_path, MAX_MANIFEST_BYTES).map_err(|_| ())?;
    let styles = read_path(&styles_path, MAX_STYLES_BYTES).map_err(|_| ())?;
    if !styles.starts_with(b"LSTY\0\x01\0\0") {
        return Err(());
    }
    Ok(Application {
        filename,
        manifest,
        program,
        styles,
    })
}

pub fn read_path(path: &[u8], limit: usize) -> Result<Vec<u8>, ReadError> {
    if path.last() != Some(&0) {
        return Err(ReadError::Failed);
    }
    let descriptor = unsafe { ffi::open(path.as_ptr().cast(), ffi::O_RDONLY | ffi::O_CLOEXEC) };
    if descriptor < 0 {
        return Err(if ffi::errno() == ffi::ENOENT {
            ReadError::NotFound
        } else {
            ReadError::Failed
        });
    }
    let result = read_all(descriptor, limit);
    let close_result = unsafe { ffi::close(descriptor) };
    if close_result != 0 {
        return Err(ReadError::Failed);
    }
    result
}

fn read_all(descriptor: i32, limit: usize) -> Result<Vec<u8>, ReadError> {
    let mut output: Vec<u8> = Vec::new();
    loop {
        if output.len() == limit {
            return Err(ReadError::Failed);
        }
        let available = (limit - output.len()).min(READ_CHUNK_BYTES);
        output
            .try_reserve(available)
            .map_err(|_| ReadError::Failed)?;
        let count = unsafe {
            ffi::read(
                descriptor,
                output.as_mut_ptr().add(output.len()).cast(),
                available,
            )
        };
        if count > 0 {
            let length = output
                .len()
                .checked_add(count as usize)
                .ok_or(ReadError::Failed)?;
            unsafe { output.set_len(length) };
        } else if count == 0 {
            return Ok(output);
        } else if ffi::errno() != ffi::EINTR {
            return Err(ReadError::Failed);
        }
    }
}

fn application_root(directory: *const u8) -> Result<Vec<u8>, ()> {
    if directory.is_null() {
        return Err(());
    }
    let maximum = APPLICATION_PREFIX.len() + MAX_APPLICATION_NAME + 1;
    let mut root = Vec::new();
    root.try_reserve(maximum).map_err(|_| ())?;
    let mut terminated = false;
    for index in 0..maximum {
        let byte = unsafe { *directory.add(index) };
        if byte == 0 {
            terminated = true;
            break;
        }
        root.push(byte);
    }
    if !terminated {
        return Err(());
    }
    let Some(name) = root.strip_prefix(APPLICATION_PREFIX) else {
        return Err(());
    };
    if name.is_empty()
        || name.len() > MAX_APPLICATION_NAME
        || !name
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
    {
        return Err(());
    }
    Ok(root)
}

fn joined(root: &[u8], suffix: &[u8]) -> Result<Vec<u8>, ()> {
    let length = root.len().checked_add(suffix.len()).ok_or(())?;
    let mut path = Vec::new();
    path.try_reserve_exact(length).map_err(|_| ())?;
    path.extend_from_slice(root);
    path.extend_from_slice(suffix);
    Ok(path)
}
