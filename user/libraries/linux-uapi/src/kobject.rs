//! Standard `NETLINK_KOBJECT_UEVENT` subscription used for DRM hotplug.

use std::{
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
};

#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;

#[cfg(target_os = "linux")]
use crate::raw;

#[cfg(target_os = "linux")]
const EVENT_CAPACITY: usize = 256;

/// Owned multicast subscription to Linux kobject change events.
pub struct KobjectUevent {
    socket: OwnedFd,
}

impl KobjectUevent {
    /// Opens a non-blocking group-1 `NETLINK_KOBJECT_UEVENT` endpoint.
    ///
    /// # Returns
    ///
    /// A subscribed descriptor whose readability joins the caller's main poll.
    ///
    /// # Errors
    ///
    /// Returns the exact `socket(2)` or `bind(2)` failure. Non-Linux hosts
    /// return `Unsupported` because they have no Linux netlink ABI.
    #[cfg(target_os = "linux")]
    pub fn open() -> io::Result<Self> {
        const AF_NETLINK: i32 = 16;
        const SOCK_DGRAM: i32 = 2;
        const SOCK_NONBLOCK: i32 = 0x800;
        const SOCK_CLOEXEC: i32 = 0x80000;
        const NETLINK_KOBJECT_UEVENT: i32 = 15;
        const KOBJECT_UEVENT_GROUP: u32 = 1;

        let raw_fd = unsafe {
            raw::socket(
                AF_NETLINK,
                SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC,
                NETLINK_KOBJECT_UEVENT,
            )
        };
        if raw_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `socket` returned one fresh descriptor and this function
        // transfers its unique ownership immediately into `OwnedFd`.
        let socket = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let address = raw::SockAddrNl {
            family: AF_NETLINK as u16,
            padding: 0,
            port_id: 0,
            groups: KOBJECT_UEVENT_GROUP,
        };
        let result = unsafe {
            raw::bind(
                raw_fd,
                (&raw const address).cast(),
                std::mem::size_of::<raw::SockAddrNl>() as u32,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { socket })
    }

    /// Reports that netlink is unavailable on a non-Linux build host.
    #[cfg(not(target_os = "linux"))]
    pub fn open() -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "NETLINK_KOBJECT_UEVENT requires Linux",
        ))
    }

    /// Borrows the subscription descriptor for `poll(2)`.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }

    /// Drains all queued datagrams and reports whether a standard DRM hotplug
    /// was observed. Draining to `EAGAIN` makes a resize storm latest-wins
    /// without inventing a second debounce timer.
    ///
    /// # Returns
    ///
    /// `true` when at least one exact `SUBSYSTEM=drm`, `HOTPLUG=1` event was
    /// present.
    ///
    /// # Errors
    ///
    /// Returns a non-`EAGAIN` receive failure.
    #[cfg(target_os = "linux")]
    pub fn drain_drm_hotplug(&self) -> io::Result<bool> {
        let mut found = false;
        let mut bytes = [0u8; EVENT_CAPACITY];
        loop {
            let length = unsafe {
                raw::recv(
                    self.socket.as_fd().as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                    0,
                )
            };
            if length < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(found);
                }
                return Err(error);
            }
            if length == 0 {
                return Ok(found);
            }
            found |= is_drm_hotplug(&bytes[..length as usize]);
        }
    }

    /// Non-Linux hosts cannot receive a Linux kobject datagram.
    #[cfg(not(target_os = "linux"))]
    pub fn drain_drm_hotplug(&self) -> io::Result<bool> {
        Ok(false)
    }
}

#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

#[cfg(any(target_os = "linux", test))]
fn is_drm_hotplug(bytes: &[u8]) -> bool {
    let mut drm = false;
    let mut hotplug = false;
    for field in bytes.split(|byte| *byte == 0) {
        drm |= field == b"SUBSYSTEM=drm";
        hotplug |= field == b"HOTPLUG=1";
    }
    drm && hotplug
}

#[cfg(test)]
mod tests {
    use super::is_drm_hotplug;

    #[test]
    fn requires_both_standard_drm_hotplug_fields() {
        assert!(is_drm_hotplug(
            b"change@/devices/card0\0ACTION=change\0SUBSYSTEM=drm\0HOTPLUG=1\0"
        ));
        assert!(!is_drm_hotplug(b"SUBSYSTEM=drm\0ACTION=change\0"));
        assert!(!is_drm_hotplug(b"SUBSYSTEM=input\0HOTPLUG=1\0"));
    }
}
