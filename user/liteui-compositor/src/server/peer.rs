use crate::ffi;

use super::{CLIENT_UIDS, ClientSlot};

pub(super) enum Peer {
    Client(ClientSlot),
    Diagnostics,
}

pub(super) fn classify(fd: i32) -> Option<Peer> {
    let mut credential = ffi::Ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = core::mem::size_of::<ffi::Ucred>() as u32;
    let obtained = unsafe {
        ffi::getsockopt(
            fd,
            ffi::SOL_SOCKET,
            ffi::SO_PEERCRED,
            (&mut credential as *mut ffi::Ucred).cast(),
            &mut length,
        ) == 0
    };
    if !obtained || length as usize != core::mem::size_of::<ffi::Ucred>() {
        return None;
    }
    if credential.uid == 0 {
        return Some(Peer::Diagnostics);
    }
    CLIENT_UIDS
        .iter()
        .position(|uid| *uid == credential.uid)
        .map(ClientSlot)
        .map(Peer::Client)
}

pub(super) fn send_snapshot(fd: i32, snapshot: &[u8]) {
    let mut sent = 0;
    while sent < snapshot.len() {
        let count = unsafe {
            ffi::send(
                fd,
                snapshot[sent..].as_ptr().cast(),
                snapshot.len() - sent,
                ffi::MSG_NOSIGNAL,
            )
        };
        if count > 0 {
            sent += count as usize;
        } else if count < 0 && ffi::errno() == ffi::EINTR {
            continue;
        } else {
            break;
        }
    }
    unsafe { ffi::close(fd) };
}

const _: () = assert!(core::mem::size_of::<ffi::Ucred>() == 12);
