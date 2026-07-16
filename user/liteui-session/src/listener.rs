use crate::ffi;

pub const SEAT_PATH: &[u8] = b"/run/seatd.sock\0";
pub const COMPOSITOR_PATH: &[u8] = b"/run/liteui/compositor.sock\0";

pub fn create(path: &'static [u8], uid: u32, gid: u32, mode: u32) -> Result<i32, ()> {
    if path.last() != Some(&0) || path.len() > 108 {
        return Err(());
    }
    unsafe { ffi::unlink(path.as_ptr().cast()) };
    let fd = unsafe {
        ffi::socket(
            ffi::AF_UNIX,
            ffi::SOCK_STREAM | ffi::SOCK_NONBLOCK | ffi::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(());
    }
    let mut address = ffi::SockaddrUn {
        family: ffi::AF_UNIX as u16,
        path: [0; 108],
    };
    address.path[..path.len()].copy_from_slice(path);
    let length = (core::mem::size_of::<u16>() + path.len() - 1) as u32;
    let ready = unsafe {
        ffi::bind(fd, &address, length) == 0
            && ffi::chown(path.as_ptr().cast(), uid, gid) == 0
            && ffi::chmod(path.as_ptr().cast(), mode) == 0
            && ffi::listen(fd, 4) == 0
    };
    if ready {
        Ok(fd)
    } else {
        unsafe { ffi::close(fd) };
        unsafe { ffi::unlink(path.as_ptr().cast()) };
        Err(())
    }
}

pub fn remove(path: &'static [u8]) {
    unsafe { ffi::unlink(path.as_ptr().cast()) };
}
