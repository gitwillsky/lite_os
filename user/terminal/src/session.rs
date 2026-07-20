//! PTY 监督：spawn_shell / set_window_size / terminate_child / read_pty /
//! replay_boot_log，搬自 console-session 的 `reactor/session.rs`。

use core::ptr;

use crate::{
    ffi::{self, WindowSize},
    input::{InputQueue, PTY_REPLY_EXPANSION},
    model::Model,
};

const PTY_BUDGET: usize = 64 * 1024;

pub fn read_pty(master: i32, model: &mut Model, input: &mut InputQueue) -> (bool, bool) {
    let mut total = 0;
    let mut changed = false;
    let mut bytes = [0u8; 8 * 1024];
    while total < PTY_BUDGET {
        let capacity = bytes
            .len()
            .min(PTY_BUDGET - total)
            .min(input.remaining() / PTY_REPLY_EXPANSION);
        if capacity == 0 {
            return (changed, false);
        }
        let count = unsafe { ffi::read(master, bytes.as_mut_ptr().cast(), capacity) };
        if count > 0 {
            model.feed(&bytes[..count as usize], |reply| {
                input.push(reply);
            });
            total += count as usize;
            changed = true;
        } else if count < 0 && ffi::errno() == ffi::EINTR {
            continue;
        } else if count < 0 && ffi::errno() == ffi::EAGAIN {
            return (changed, false);
        } else {
            return (changed, true);
        }
    }
    (changed, false)
}

pub fn replay_boot_log(model: &mut Model) {
    let fd = unsafe {
        ffi::open(
            ffi::c_str(b"/dev/kmsg\0"),
            ffi::O_RDONLY | ffi::O_NONBLOCK | ffi::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return;
    }
    let mut record = [0u8; 256];
    loop {
        let count = unsafe { ffi::read(fd, record.as_mut_ptr().cast(), record.len()) };
        if count < 0 && ffi::errno() == ffi::EPIPE {
            continue;
        }
        if count <= 0 {
            break;
        }
        let bytes = &record[..count as usize];
        if let Some(separator) = bytes.iter().position(|byte| *byte == b';') {
            model.feed(&bytes[separator + 1..], |_| {});
            if bytes.last() != Some(&b'\n') {
                model.feed(b"\n", |_| {});
            }
        }
    }
    unsafe { ffi::close(fd) };
}

pub fn spawn_shell(
    columns: usize,
    rows: usize,
    pixel_width: u16,
    pixel_height: u16,
) -> Option<(i32, i32)> {
    let master = unsafe {
        ffi::open(
            ffi::c_str(b"/dev/ptmx\0"),
            ffi::O_RDWR | ffi::O_NONBLOCK | ffi::O_CLOEXEC,
        )
    };
    if master < 0 {
        return None;
    }
    let mut index = 0u32;
    let mut unlocked = 0i32;
    if unsafe {
        ffi::ioctl(master, ffi::TIOCGPTN, (&mut index as *mut u32).cast()) < 0
            || ffi::ioctl(master, ffi::TIOCSPTLCK, (&mut unlocked as *mut i32).cast()) < 0
    } {
        unsafe { ffi::close(master) };
        return None;
    }
    let mut path = [0u8; 32];
    let prefix = b"/dev/pts/";
    path[..prefix.len()].copy_from_slice(prefix);
    let capacity = path.len() - 1;
    let length = prefix.len() + decimal(index, &mut path[prefix.len()..capacity]);
    path[length] = 0;
    let slave = unsafe { ffi::open(path.as_ptr().cast(), ffi::O_RDWR | ffi::O_CLOEXEC) };
    if slave < 0 || set_window_size(master, columns, rows, pixel_width, pixel_height).is_err() {
        unsafe {
            if slave >= 0 {
                ffi::close(slave);
            }
            ffi::close(master);
        }
        return None;
    }
    let parent = unsafe { ffi::getpid() };
    let child = unsafe { ffi::fork() };
    if child < 0 {
        unsafe {
            ffi::close(slave);
            ffi::close(master);
        }
        return None;
    }
    if child == 0 {
        unsafe {
            ffi::close(master);
            if ffi::prctl(ffi::PR_SET_PDEATHSIG, ffi::SIGKILL) < 0
                || ffi::getppid() != parent
                || ffi::setsid() < 0
                || ffi::ioctl(slave, ffi::TIOCSCTTY, ptr::null_mut()) < 0
                || ffi::dup2(slave, 0) < 0
                || ffi::dup2(slave, 1) < 0
                || ffi::dup2(slave, 2) < 0
            {
                ffi::_exit(126);
            }
            if slave > 2 {
                ffi::close(slave);
            }
            ffi::setenv(ffi::c_str(b"TERM\0"), ffi::c_str(b"liteos\0"), 1);
            ffi::setenv(ffi::c_str(b"HOME\0"), ffi::c_str(b"/root\0"), 1);
            ffi::setenv(
                ffi::c_str(b"PATH\0"),
                ffi::c_str(b"/sbin:/usr/sbin:/bin:/usr/bin\0"),
                1,
            );
            ffi::chdir(ffi::c_str(b"/root\0"));
            let arguments = [ffi::c_str(b"-sh\0"), ptr::null()];
            ffi::execve(
                ffi::c_str(b"/bin/sh\0"),
                arguments.as_ptr(),
                ffi::environ.cast_const(),
            );
            ffi::_exit(127);
        }
    }
    unsafe { ffi::close(slave) };
    Some((master, child))
}

/// Terminates the shell session process group after the display session exits.
///
/// `child` is the PID returned by `fork`. Reaping it here prevents an init
/// respawn from leaving the previous PTY session or one of its jobs behind the new owner.
pub fn terminate_child(child: i32) {
    if child <= 0 {
        return;
    }
    unsafe { ffi::kill(-child, ffi::SIGKILL) };
    loop {
        let mut status = 0;
        let result = unsafe { ffi::waitpid(child, &mut status, 0) };
        if result == child || result < 0 && ffi::errno() == ffi::ECHILD {
            return;
        }
        if result < 0 && ffi::errno() == ffi::EINTR {
            continue;
        }
        return;
    }
}

pub fn set_window_size(
    master: i32,
    columns: usize,
    rows: usize,
    pixel_width: u16,
    pixel_height: u16,
) -> Result<(), ()> {
    let mut size = WindowSize {
        rows: u16::try_from(rows).map_err(|_| ())?,
        columns: u16::try_from(columns).map_err(|_| ())?,
        pixel_width,
        pixel_height,
    };
    (unsafe {
        ffi::ioctl(
            master,
            ffi::TIOCSWINSZ,
            (&mut size as *mut WindowSize).cast(),
        )
    } >= 0)
        .then_some(())
        .ok_or(())
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
