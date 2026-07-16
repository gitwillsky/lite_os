#![no_std]

use core::ffi::{c_char, c_int};

const ACTIVATED_FD: c_int = 3;
const F_GETFD: c_int = 1;
const MAX_ENV_BYTES: usize = 32;

/// Consumes one systemd-compatible activated listener from descriptor three.
///
/// `LISTEN_PID`, `LISTEN_FDS=1` and `LISTEN_FDNAMES` jointly bind the descriptor
/// to the direct exec child. All variables are erased before returning so a
/// later application exec cannot accidentally inherit the authority claim.
pub fn take_listener(expected_name: &[u8]) -> Result<c_int, ()> {
    if expected_name.is_empty()
        || expected_name.len() >= MAX_ENV_BYTES
        || environment(b"LISTEN_FDS\0")? != b"1"
        || decimal(environment(b"LISTEN_PID\0")?)? != unsafe { getpid() as u32 }
        || environment(b"LISTEN_FDNAMES\0")? != expected_name
        || unsafe { fcntl(ACTIVATED_FD, F_GETFD) } < 0
    {
        return Err(());
    }
    if unsafe { unsetenv(b"LISTEN_PID\0".as_ptr().cast()) } != 0
        || unsafe { unsetenv(b"LISTEN_FDS\0".as_ptr().cast()) } != 0
        || unsafe { unsetenv(b"LISTEN_FDNAMES\0".as_ptr().cast()) } != 0
    {
        return Err(());
    }
    Ok(ACTIVATED_FD)
}

fn environment(name: &'static [u8]) -> Result<&'static [u8], ()> {
    let value = unsafe { getenv(name.as_ptr().cast()) };
    if value.is_null() {
        return Err(());
    }
    for length in 0..MAX_ENV_BYTES {
        if unsafe { *value.add(length) } == 0 {
            return Ok(unsafe { core::slice::from_raw_parts(value.cast(), length) });
        }
    }
    Err(())
}

fn decimal(bytes: &[u8]) -> Result<u32, ()> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(());
    }
    bytes
        .iter()
        .try_fold(0u32, |value, byte| {
            value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))
        })
        .ok_or(())
}

unsafe extern "C" {
    fn getenv(name: *const c_char) -> *mut c_char;
    fn unsetenv(name: *const c_char) -> c_int;
    fn getpid() -> c_int;
    fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
}
