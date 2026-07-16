use alloc::vec::Vec;
use core::ptr;

use crate::{
    client::{Client, Identity, MAX_CLIENTS, SessionState},
    ffi, peer, protocol,
};

struct Broker {
    listener: i32,
    clients: Vec<Client>,
    active: Option<usize>,
}

pub fn run() -> Result<(), ()> {
    let listener = service_activation::take_listener(b"display-session")?;
    let mut clients = Vec::new();
    // Slots are allocated before publication. Disconnect and forced revoke must
    // remain available under memory pressure or a replacement owner could race
    // a still-live DRM/input OFD.
    clients.try_reserve_exact(MAX_CLIENTS).map_err(|_| ())?;
    clients.resize(MAX_CLIENTS, Client::EMPTY);
    Broker {
        listener,
        clients,
        active: None,
    }
    .event_loop()
}

impl Broker {
    fn event_loop(&mut self) -> Result<(), ()> {
        loop {
            let mut descriptors = [ffi::PollFd {
                fd: -1,
                events: 0,
                returned: 0,
            }; MAX_CLIENTS + 1];
            descriptors[0] = ffi::PollFd {
                fd: self.listener,
                events: ffi::POLLIN,
                returned: 0,
            };
            for (index, client) in self.clients.iter().enumerate() {
                if client.fd >= 0 {
                    descriptors[index + 1] = ffi::PollFd {
                        fd: client.fd,
                        events: client.poll_events(),
                        returned: 0,
                    };
                }
            }
            let ready = unsafe { ffi::poll(descriptors.as_mut_ptr(), descriptors.len(), -1) };
            if ready < 0 {
                if ffi::errno() == ffi::EINTR {
                    continue;
                }
                return Err(());
            }
            if descriptors[0].returned & (ffi::POLLERR | ffi::POLLHUP) != 0 {
                return Err(());
            }
            if descriptors[0].returned & ffi::POLLIN != 0 {
                self.accept_clients()?;
            }
            for index in 0..self.clients.len() {
                let events = descriptors[index + 1].returned;
                if self.clients[index].fd < 0 || events == 0 {
                    continue;
                }
                if events & (ffi::POLLERR | ffi::POLLHUP) != 0 {
                    self.disconnect(index)?;
                    continue;
                }
                if events & ffi::POLLIN != 0 && self.read_requests(index).is_err() {
                    self.disconnect(index)?;
                    continue;
                }
                if self.clients[index].fd >= 0
                    && events & ffi::POLLOUT != 0
                    && self.clients[index].flush().is_err()
                {
                    self.disconnect(index)?;
                }
            }
        }
    }

    fn accept_clients(&mut self) -> Result<(), ()> {
        loop {
            let fd = unsafe {
                ffi::accept4(
                    self.listener,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ffi::SOCK_NONBLOCK | ffi::SOCK_CLOEXEC,
                )
            };
            if fd < 0 {
                return match ffi::errno() {
                    ffi::EAGAIN => Ok(()),
                    ffi::EINTR => continue,
                    _ => Err(()),
                };
            }
            let Some(index) = self.clients.iter().position(|client| client.fd < 0) else {
                unsafe { ffi::close(fd) };
                continue;
            };
            let mut credential = ffi::Ucred {
                pid: 0,
                uid: 0,
                gid: 0,
            };
            let mut length = core::mem::size_of::<ffi::Ucred>() as u32;
            if unsafe {
                ffi::getsockopt(
                    fd,
                    ffi::SOL_SOCKET,
                    ffi::SO_PEERCRED,
                    (&mut credential as *mut ffi::Ucred).cast(),
                    &mut length,
                )
            } != 0
                || length as usize != core::mem::size_of::<ffi::Ucred>()
            {
                unsafe { ffi::close(fd) };
                continue;
            }
            let identity = if peer::is_controller(credential.uid, credential.pid) {
                Identity::Controller
            } else {
                Identity::Unknown
            };
            self.clients[index].initialize(fd, credential, identity);
        }
    }

    fn read_requests(&mut self, index: usize) -> Result<(), ()> {
        self.clients[index].read()?;
        loop {
            match self.clients[index].request() {
                protocol::Decode::Incomplete => break,
                protocol::Decode::Invalid => return Err(()),
                protocol::Decode::Complete { request, consumed } => {
                    self.handle(index, request)?;
                    if self.clients[index].fd < 0 {
                        break;
                    }
                    self.clients[index].consume(consumed);
                }
            }
        }
        if self.clients[index].fd >= 0 {
            self.clients[index].flush()?;
        }
        Ok(())
    }

    fn handle(&mut self, index: usize, request: protocol::Request) -> Result<(), ()> {
        match request {
            protocol::Request::OpenSeat => self.open_seat(index),
            protocol::Request::CloseSeat => self.close_seat(index),
            protocol::Request::OpenDevice { path, length } => {
                self.open_device(index, &path[..length])
            }
            protocol::Request::CloseDevice(id) => self.close_device(index, id),
            protocol::Request::Ping => self.queue(index, protocol::SERVER_PONG, &[], -1),
            protocol::Request::DisableSeat | protocol::Request::SwitchSession => {
                self.error(index, ffi::ENOTSUP)
            }
        }
    }

    fn open_seat(&mut self, index: usize) -> Result<(), ()> {
        if self.clients[index].state != SessionState::New {
            return Err(());
        }
        if self.clients[index].identity != Identity::Controller {
            return self.error(index, ffi::EPERM);
        }
        if self.active.is_some() {
            return self.error(index, ffi::EBUSY);
        }
        self.active = Some(index);
        self.clients[index].state = SessionState::Active;
        let mut payload = [0u8; 8];
        payload[..2].copy_from_slice(&6u16.to_ne_bytes());
        payload[2..].copy_from_slice(b"seat0\0");
        self.queue(index, protocol::SERVER_SEAT_OPENED, &payload, -1)
    }

    fn open_device(&mut self, index: usize, path: &[u8]) -> Result<(), ()> {
        if self.active != Some(index) || self.clients[index].state != SessionState::Active {
            return self.error(index, ffi::EPERM);
        }
        match self.clients[index].open_device(path) {
            Ok((id, fd)) => {
                self.queue(index, protocol::SERVER_DEVICE_OPENED, &id.to_ne_bytes(), fd)
            }
            Err(error) => self.error(index, error),
        }
    }

    fn close_device(&mut self, index: usize, id: i32) -> Result<(), ()> {
        match self.clients[index].close_device(id) {
            Ok(()) => self.queue(index, protocol::SERVER_DEVICE_CLOSED, &[], -1),
            Err(error) => self.error(index, error),
        }
    }

    fn close_seat(&mut self, index: usize) -> Result<(), ()> {
        if self.active != Some(index) || self.clients[index].state != SessionState::Active {
            return Err(());
        }
        self.queue(index, protocol::SERVER_SEAT_CLOSED, &[], -1)?;
        self.clients[index].close_devices();
        self.clients[index].state = SessionState::Closed;
        self.active = None;
        Ok(())
    }

    fn disconnect(&mut self, index: usize) -> Result<(), ()> {
        if self.active == Some(index) {
            self.clients[index].force_revoke()?;
            self.active = None;
        }
        self.clients[index].close();
        Ok(())
    }

    fn error(&mut self, index: usize, error: i32) -> Result<(), ()> {
        self.queue(index, protocol::SERVER_ERROR, &error.to_ne_bytes(), -1)
    }

    fn queue(&mut self, index: usize, opcode: u16, payload: &[u8], fd: i32) -> Result<(), ()> {
        self.clients[index].queue(opcode, payload, fd)
    }
}
