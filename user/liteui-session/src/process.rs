use core::ptr;

use crate::ffi;

const ACTIVATED_FD: i32 = 3;
const LITEUI_GID: u32 = 100;

#[derive(Clone, Copy)]
pub enum Identity {
    Root,
    SystemShell,
    Terminal,
    Application,
}

pub fn spawn(
    binary: &'static [u8],
    argument: Option<&'static [u8]>,
    activation: Option<(i32, &'static [u8])>,
    identity: Identity,
) -> Result<i32, ()> {
    if binary.last() != Some(&0) || argument.is_some_and(|value| value.last() != Some(&0)) {
        return Err(());
    }
    let pid = unsafe { ffi::fork() };
    if pid < 0 {
        return Err(());
    }
    if pid != 0 {
        return Ok(pid);
    }
    if let Some((listener, name)) = activation
        && activate(listener, name).is_err()
    {
        unsafe { ffi::_exit(126) };
    }
    if let Some(uid) = identity.uid()
        && (unsafe { ffi::setgroups(0, ptr::null()) } != 0
            || unsafe { ffi::setgid(LITEUI_GID) } != 0
            || unsafe { ffi::setuid(uid) } != 0)
    {
        unsafe { ffi::_exit(126) };
    }
    let arguments = [
        binary.as_ptr().cast(),
        argument.map_or(ptr::null(), |value| value.as_ptr().cast()),
        ptr::null(),
    ];
    unsafe { ffi::execv(binary.as_ptr().cast(), arguments.as_ptr()) };
    unsafe { ffi::_exit(127) };
}

impl Identity {
    fn uid(self) -> Option<u32> {
        match self {
            Self::Root => None,
            Self::SystemShell => Some(100),
            Self::Terminal => Some(101),
            Self::Application => Some(102),
        }
    }
}

pub fn wait_any() -> Result<i32, ()> {
    loop {
        let mut status = 0;
        let pid = unsafe { ffi::waitpid(-1, &mut status, 0) };
        if pid > 0 {
            return Ok(pid);
        }
        if pid < 0 && ffi::errno() == ffi::EINTR {
            continue;
        }
        return Err(());
    }
}

pub fn terminate(pid: i32) {
    if pid <= 0 {
        return;
    }
    if unsafe { ffi::kill(pid, ffi::SIGKILL) } != 0 && ffi::errno() != ffi::ESRCH {
        return;
    }
    loop {
        let mut status = 0;
        let result = unsafe { ffi::waitpid(pid, &mut status, 0) };
        if result == pid || result < 0 && ffi::errno() == ffi::ECHILD {
            return;
        }
        if result < 0 && ffi::errno() == ffi::EINTR {
            continue;
        }
        return;
    }
}

fn activate(listener: i32, name: &'static [u8]) -> Result<(), ()> {
    if name.is_empty() || name.last() != Some(&0) {
        return Err(());
    }
    if listener != ACTIVATED_FD {
        if unsafe { ffi::dup2(listener, ACTIVATED_FD) } != ACTIVATED_FD {
            return Err(());
        }
        unsafe { ffi::close(listener) };
    } else if unsafe { ffi::fcntl(ACTIVATED_FD, ffi::F_SETFD, 0) } != 0 {
        return Err(());
    }
    let mut pid = [0u8; 12];
    let length = decimal(unsafe { ffi::getpid() } as u32, &mut pid);
    pid[length] = 0;
    if unsafe { ffi::setenv(b"LISTEN_PID\0".as_ptr().cast(), pid.as_ptr().cast(), 1) } != 0
        || unsafe { ffi::setenv(b"LISTEN_FDS\0".as_ptr().cast(), b"1\0".as_ptr().cast(), 1) } != 0
        || unsafe { ffi::setenv(b"LISTEN_FDNAMES\0".as_ptr().cast(), name.as_ptr().cast(), 1) } != 0
    {
        return Err(());
    }
    Ok(())
}

fn decimal(mut value: u32, output: &mut [u8]) -> usize {
    let mut reversed = [0u8; 10];
    let mut length = 0;
    loop {
        reversed[length] = b'0' + (value % 10) as u8;
        length += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for index in 0..length {
        output[index] = reversed[length - index - 1];
    }
    length
}
