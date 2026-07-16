use core::{ffi::c_void, ptr};

use crate::{ffi, protocol};

pub const MAX_CLIENTS: usize = 16;
const MAX_DEVICES: usize = 128;
const OUTPUT_SLOTS: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    Unknown,
    Controller,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    New,
    Active,
    Closed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeviceKind {
    Drm,
    Input,
}

#[derive(Clone, Copy)]
struct Device {
    id: i32,
    fd: i32,
    key: u16,
    references: u16,
    kind: DeviceKind,
}

impl Device {
    const EMPTY: Self = Self {
        id: 0,
        fd: -1,
        key: 0,
        references: 0,
        kind: DeviceKind::Input,
    };
}

#[derive(Clone, Copy)]
struct Output {
    bytes: [u8; 80],
    length: usize,
    written: usize,
    rights_fd: i32,
}

impl Output {
    const EMPTY: Self = Self {
        bytes: [0; 80],
        length: 0,
        written: 0,
        rights_fd: -1,
    };
}

#[derive(Clone, Copy)]
pub struct Client {
    pub fd: i32,
    pub pid: i32,
    pub uid: u32,
    pub identity: Identity,
    pub state: SessionState,
    input: [u8; 512],
    input_length: usize,
    output: [Output; OUTPUT_SLOTS],
    output_head: usize,
    output_length: usize,
    devices: [Device; MAX_DEVICES],
    next_device_id: i32,
}

impl Client {
    pub const EMPTY: Self = Self {
        fd: -1,
        pid: 0,
        uid: 0,
        identity: Identity::Unknown,
        state: SessionState::New,
        input: [0; 512],
        input_length: 0,
        output: [Output::EMPTY; OUTPUT_SLOTS],
        output_head: 0,
        output_length: 0,
        devices: [Device::EMPTY; MAX_DEVICES],
        next_device_id: 1,
    };

    pub fn initialize(&mut self, fd: i32, credential: ffi::Ucred, identity: Identity) {
        *self = Self::EMPTY;
        self.fd = fd;
        self.pid = credential.pid;
        self.uid = credential.uid;
        self.identity = identity;
    }

    pub fn poll_events(&self) -> i16 {
        ffi::POLLIN
            | if self.output_length != 0 {
                ffi::POLLOUT
            } else {
                0
            }
    }

    pub fn read(&mut self) -> Result<bool, ()> {
        if self.input_length == self.input.len() {
            return Err(());
        }
        let count = unsafe {
            ffi::read(
                self.fd,
                self.input[self.input_length..].as_mut_ptr().cast(),
                self.input.len() - self.input_length,
            )
        };
        if count > 0 {
            self.input_length += count as usize;
            Ok(true)
        } else if count < 0 && matches!(ffi::errno(), ffi::EAGAIN | ffi::EINTR) {
            Ok(false)
        } else {
            Err(())
        }
    }

    pub fn request(&self) -> protocol::Decode {
        protocol::decode(&self.input[..self.input_length])
    }

    pub fn consume(&mut self, count: usize) {
        self.input.copy_within(count..self.input_length, 0);
        self.input_length -= count;
    }

    pub fn queue(&mut self, opcode: u16, payload: &[u8], rights_fd: i32) -> Result<(), ()> {
        if self.output_length == self.output.len() {
            return Err(());
        }
        let index = (self.output_head + self.output_length) % self.output.len();
        let output = &mut self.output[index];
        output.length = protocol::frame(opcode, payload, &mut output.bytes).ok_or(())?;
        output.written = 0;
        output.rights_fd = rights_fd;
        self.output_length += 1;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), ()> {
        while self.output_length != 0 {
            let output = &mut self.output[self.output_head];
            let count = if output.rights_fd >= 0 && output.written == 0 {
                send_with_right(self.fd, &output.bytes[..output.length], output.rights_fd)
            } else {
                unsafe {
                    ffi::write(
                        self.fd,
                        output.bytes[output.written..output.length].as_ptr().cast(),
                        output.length - output.written,
                    )
                }
            };
            if count > 0 {
                output.written += count as usize;
                output.rights_fd = -1;
                if output.written == output.length {
                    *output = Output::EMPTY;
                    self.output_head = (self.output_head + 1) % self.output.len();
                    self.output_length -= 1;
                }
            } else if count < 0 && matches!(ffi::errno(), ffi::EAGAIN | ffi::EINTR) {
                return Ok(());
            } else {
                return Err(());
            }
        }
        Ok(())
    }

    pub fn open_device(&mut self, path: &[u8]) -> Result<(i32, i32), i32> {
        let (key, kind, flags) = classify(path).ok_or(ffi::ENODEV)?;
        if let Some(device) = self
            .devices
            .iter_mut()
            .find(|device| device.fd >= 0 && device.key == key)
        {
            device.references = device.references.checked_add(1).ok_or(ffi::EMFILE)?;
            return Ok((device.id, device.fd));
        }
        let slot = self
            .devices
            .iter_mut()
            .find(|device| device.fd < 0)
            .ok_or(ffi::EMFILE)?;
        let fd = unsafe { ffi::open(path.as_ptr().cast(), flags) };
        if fd < 0 {
            return Err(ffi::errno());
        }
        let id = self.next_device_id;
        self.next_device_id = self.next_device_id.checked_add(1).ok_or_else(|| {
            unsafe { ffi::close(fd) };
            ffi::EMFILE
        })?;
        *slot = Device {
            id,
            fd,
            key,
            references: 1,
            kind,
        };
        Ok((id, fd))
    }

    pub fn close_device(&mut self, id: i32) -> Result<(), i32> {
        let device = self
            .devices
            .iter_mut()
            .find(|device| device.fd >= 0 && device.id == id)
            .ok_or(ffi::EBADF)?;
        device.references -= 1;
        if device.references == 0 {
            unsafe { ffi::close(device.fd) };
            *device = Device::EMPTY;
        }
        Ok(())
    }

    pub fn close_devices(&mut self) {
        for device in &mut self.devices {
            if device.fd >= 0 {
                unsafe { ffi::close(device.fd) };
                *device = Device::EMPTY;
            }
        }
    }

    pub fn force_revoke(&mut self) -> Result<(), ()> {
        let mut failed = false;
        for device in &mut self.devices {
            if device.fd < 0 {
                continue;
            }
            let request = match device.kind {
                DeviceKind::Drm => ffi::DRM_IOCTL_DROP_MASTER,
                DeviceKind::Input => ffi::EVIOCREVOKE,
            };
            if unsafe { ffi::ioctl(device.fd, request, ptr::null_mut()) } < 0 {
                let error = ffi::errno();
                // DROP_MASTER(EINVAL) 与 EVIOCREVOKE(ENODEV) 都证明 capability 已经
                // 不可用；其他错误无法证明撤销完成，必须进入 cold-reboot domain。
                failed |= !matches!(
                    (device.kind, error),
                    (DeviceKind::Drm, ffi::EINVAL) | (DeviceKind::Input, ffi::ENODEV)
                );
            }
            unsafe { ffi::close(device.fd) };
            *device = Device::EMPTY;
        }
        (!failed).then_some(()).ok_or(())
    }

    pub fn close(&mut self) {
        self.close_devices();
        if self.fd >= 0 {
            unsafe { ffi::close(self.fd) };
        }
        *self = Self::EMPTY;
    }
}

fn classify(path: &[u8]) -> Option<(u16, DeviceKind, i32)> {
    if path == b"/dev/dri/card0\0" {
        return Some((
            0,
            DeviceKind::Drm,
            ffi::O_RDWR | ffi::O_NONBLOCK | ffi::O_CLOEXEC,
        ));
    }
    let suffix = path.strip_prefix(b"/dev/input/event")?;
    let digits = suffix.strip_suffix(&[0])?;
    if digits.is_empty() || digits.len() > 2 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let index = digits
        .iter()
        .fold(0u16, |value, byte| value * 10 + u16::from(byte - b'0'));
    (index < 16).then_some((
        index + 1,
        DeviceKind::Input,
        ffi::O_RDONLY | ffi::O_NONBLOCK | ffi::O_CLOEXEC,
    ))
}

fn send_with_right(socket: i32, bytes: &[u8], fd: i32) -> isize {
    let mut iov = ffi::Iovec {
        base: bytes.as_ptr().cast_mut().cast::<c_void>(),
        length: bytes.len(),
    };
    let mut control = [0usize; 3];
    let header = control.as_mut_ptr().cast::<ffi::ControlHeader>();
    unsafe {
        (*header).length = core::mem::size_of::<ffi::ControlHeader>() + core::mem::size_of::<i32>();
        (*header).level = ffi::SOL_SOCKET;
        (*header).kind = ffi::SCM_RIGHTS;
        header.add(1).cast::<i32>().write(fd);
    }
    let message = ffi::MessageHeader {
        name: ptr::null_mut(),
        name_length: 0,
        iov: &mut iov,
        iov_length: 1,
        control: control.as_mut_ptr().cast(),
        control_length: core::mem::size_of::<ffi::ControlHeader>() + core::mem::size_of::<i32>(),
        flags: 0,
    };
    unsafe { ffi::sendmsg(socket, &message, ffi::MSG_DONTWAIT | ffi::MSG_NOSIGNAL) }
}
