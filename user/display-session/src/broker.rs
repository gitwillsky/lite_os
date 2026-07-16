use alloc::vec::Vec;
use core::ptr;

use crate::{
    client::{Client, Identity, MAX_CLIENTS, SessionState},
    ffi, peer, protocol,
};

const SOCKET_PATH: &[u8] = b"/run/seatd.sock\0";
const REVOKE_DEADLINE_MS: u64 = 250;

#[derive(Clone, Copy)]
struct Transition {
    source: usize,
    target: usize,
    generation: u64,
    deadline: u64,
}

struct Broker {
    listener: i32,
    clients: Vec<Client>,
    terminal: Option<usize>,
    active: Option<usize>,
    transition: Option<Transition>,
    next_generation: u64,
}

pub fn run() -> Result<(), ()> {
    let listener = create_listener()?;
    let mut clients = Vec::new();
    // 所有 client slot 在 reactor 启动前一次性预留；缺失这条容量证明会让
    // disconnect/timeout recovery 在内存压力下重新分配并失去确定性。
    clients.try_reserve_exact(MAX_CLIENTS).map_err(|_| ())?;
    clients.resize(MAX_CLIENTS, Client::EMPTY);
    let mut broker = Broker {
        listener,
        clients,
        terminal: None,
        active: None,
        transition: None,
        next_generation: 1,
    };
    broker.event_loop()
}

impl Broker {
    fn event_loop(&mut self) -> Result<(), ()> {
        loop {
            let now = ffi::monotonic_milliseconds()?;
            self.expire_transition(now)?;
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
            let timeout = self.transition.map_or(-1, |transition| {
                transition.deadline.saturating_sub(now).min(i32::MAX as u64) as i32
            });
            let ready = unsafe { ffi::poll(descriptors.as_mut_ptr(), descriptors.len(), timeout) };
            if ready < 0 {
                if ffi::errno() == ffi::EINTR {
                    continue;
                }
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
            let identity = peer::read_stat(credential.pid)
                .filter(|stat| peer::is_terminal(credential.uid, *stat))
                .map_or(Identity::Unknown, |_| Identity::Terminal);
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
            protocol::Request::DisableSeat => self.acknowledge_disable(index),
            protocol::Request::SwitchSession => self.error(index, ffi::ENOTSUP),
            protocol::Request::Ping => self.queue(index, protocol::SERVER_PONG, &[], -1),
        }
    }

    fn open_seat(&mut self, index: usize) -> Result<(), ()> {
        if self.clients[index].state != SessionState::New {
            return Err(());
        }
        match self.clients[index].identity {
            Identity::Terminal => {
                if self.terminal.is_some() {
                    return self.error(index, ffi::EBUSY);
                }
                self.terminal = Some(index);
                if self.active.is_none() && self.transition.is_none() {
                    self.activate(index)
                } else {
                    self.request_transition(index)
                }
            }
            Identity::Unknown => {
                let Some(terminal) = self.terminal else {
                    return self.error(index, ffi::EPERM);
                };
                if self.clients[index].uid != 0
                    || !peer::is_foreground_descendant(
                        self.clients[index].pid,
                        self.clients[terminal].pid,
                    )
                {
                    return self.error(index, ffi::EPERM);
                }
                self.clients[index].identity = Identity::Graphics;
                self.request_transition(index)
            }
            Identity::Graphics => Err(()),
        }
    }

    fn request_transition(&mut self, target: usize) -> Result<(), ()> {
        if self.transition.is_some() {
            return self.error(target, ffi::EBUSY);
        }
        let Some(source) = self.active else {
            return if self.clients[target].identity == Identity::Terminal {
                self.activate(target)
            } else {
                self.error(target, ffi::EPERM)
            };
        };
        if source == target {
            return Err(());
        }
        let now = ffi::monotonic_milliseconds()?;
        let generation = self.next_generation;
        self.next_generation = self.next_generation.checked_add(1).ok_or(())?;
        if self.clients[target].state == SessionState::New {
            self.clients[target].state = SessionState::PendingOpen;
        } else if self.clients[target].state != SessionState::Inactive {
            return self.error(target, ffi::EBUSY);
        }
        self.clients[source].state = SessionState::Disabling;
        self.transition = Some(Transition {
            source,
            target,
            generation,
            deadline: now.checked_add(REVOKE_DEADLINE_MS).ok_or(())?,
        });
        self.queue(source, protocol::SERVER_DISABLE_SEAT, &[], -1)
    }

    fn acknowledge_disable(&mut self, index: usize) -> Result<(), ()> {
        let Some(transition) = self.transition else {
            return self.error(index, ffi::EINVAL);
        };
        if transition.source != index || self.clients[index].state != SessionState::Disabling {
            return self.error(index, ffi::EINVAL);
        }
        self.clients[index].close_devices();
        self.clients[index].state = SessionState::Inactive;
        self.active = None;
        self.transition = None;
        self.queue(index, protocol::SERVER_SEAT_DISABLED, &[], -1)?;
        self.activate(transition.target)
    }

    fn activate(&mut self, index: usize) -> Result<(), ()> {
        if self.clients[index].fd < 0 {
            return Ok(());
        }
        let enable = self.clients[index].state == SessionState::Inactive;
        self.active = Some(index);
        self.clients[index].state = SessionState::Active;
        if enable {
            self.queue(index, protocol::SERVER_ENABLE_SEAT, &[], -1)
        } else {
            let mut payload = [0u8; 8];
            payload[..2].copy_from_slice(&6u16.to_ne_bytes());
            payload[2..].copy_from_slice(b"seat0\0");
            self.queue(index, protocol::SERVER_SEAT_OPENED, &payload, -1)
        }
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
        if matches!(
            self.clients[index].state,
            SessionState::New | SessionState::Closed | SessionState::Disabling
        ) || self.transition.is_some()
        {
            return Err(());
        }
        self.queue(index, protocol::SERVER_SEAT_CLOSED, &[], -1)?;
        self.clients[index].close_devices();
        let was_terminal = self.terminal == Some(index);
        let was_active = self.active == Some(index);
        if was_terminal {
            self.terminal = None;
        }
        if was_active {
            self.active = None;
        }
        self.clients[index].state = SessionState::Closed;
        if was_active && !was_terminal {
            if let Some(terminal) = self.terminal {
                self.activate(terminal)?;
            }
        }
        Ok(())
    }

    fn disconnect(&mut self, index: usize) -> Result<(), ()> {
        let was_terminal = self.terminal == Some(index);
        let was_active = self.active == Some(index);
        let transition = self.transition;
        if was_active && self.clients[index].force_revoke().is_err() {
            return Err(());
        }
        self.clients[index].close();
        if was_terminal {
            self.terminal = None;
        }
        if was_active {
            self.active = None;
        }
        if let Some(current) = transition {
            if current.source == index || current.target == index {
                self.transition = None;
                let other = if current.source == index {
                    current.target
                } else {
                    current.source
                };
                if self.clients[other].fd >= 0
                    && self.clients[other].state == SessionState::PendingOpen
                {
                    if self.clients[other].identity == Identity::Terminal {
                        self.activate(other)?;
                    } else {
                        self.error(other, ffi::EPERM)?;
                        self.clients[other].state = SessionState::New;
                    }
                } else if self.clients[other].fd >= 0 {
                    self.clients[other].state = SessionState::Inactive;
                }
            }
        }
        if was_active && !was_terminal && self.transition.is_none() {
            if let Some(terminal) = self.terminal {
                if self.clients[terminal].fd >= 0 {
                    self.activate(terminal)?;
                }
            }
        }
        Ok(())
    }

    fn expire_transition(&mut self, now: u64) -> Result<(), ()> {
        let Some(transition) = self.transition else {
            return Ok(());
        };
        if now < transition.deadline {
            return Ok(());
        }
        let _generation = transition.generation;
        self.disconnect(transition.source)
    }

    fn error(&mut self, index: usize, error: i32) -> Result<(), ()> {
        self.queue(index, protocol::SERVER_ERROR, &error.to_ne_bytes(), -1)
    }

    fn queue(&mut self, index: usize, opcode: u16, payload: &[u8], fd: i32) -> Result<(), ()> {
        self.clients[index].queue(opcode, payload, fd)
    }
}

fn create_listener() -> Result<i32, ()> {
    unsafe { ffi::unlink(SOCKET_PATH.as_ptr().cast()) };
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
    address.path[..SOCKET_PATH.len()].copy_from_slice(SOCKET_PATH);
    let length = (core::mem::size_of::<u16>() + SOCKET_PATH.len() - 1) as u32;
    if unsafe { ffi::bind(fd, &address, length) } != 0
        || unsafe { ffi::chmod(SOCKET_PATH.as_ptr().cast(), 0o600) } != 0
        || unsafe { ffi::listen(fd, MAX_CLIENTS as i32) } != 0
    {
        unsafe { ffi::close(fd) };
        return Err(());
    }
    Ok(fd)
}
