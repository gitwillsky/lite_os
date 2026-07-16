use crate::ffi::{self, SockaddrNl};

/// Opens the compositor-owned DRM hotplug event source.
pub(super) fn open() -> Result<i32, ()> {
    let fd = unsafe {
        ffi::socket(
            i32::from(ffi::AF_NETLINK),
            ffi::SOCK_DGRAM | ffi::O_NONBLOCK | ffi::O_CLOEXEC,
            ffi::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return Err(());
    }
    let address = SockaddrNl {
        family: ffi::AF_NETLINK,
        padding: 0,
        port_id: 0,
        groups: 1,
    };
    if unsafe {
        ffi::bind(
            fd,
            (&address as *const SockaddrNl).cast(),
            core::mem::size_of::<SockaddrNl>() as u32,
        )
    } < 0
    {
        unsafe { ffi::close(fd) };
        return Err(());
    }
    Ok(fd)
}

/// Drains all queued hotplug notifications without blocking the display loop.
pub(super) fn drain(fd: i32) -> Result<bool, ()> {
    let mut received = false;
    let mut bytes = [0u8; 512];
    loop {
        let count = unsafe { ffi::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        if count > 0 {
            received = true;
        } else if count < 0 && ffi::errno() == ffi::EINTR {
            continue;
        } else if count < 0 && ffi::errno() == ffi::EAGAIN {
            return Ok(received);
        } else {
            return Err(());
        }
    }
}
