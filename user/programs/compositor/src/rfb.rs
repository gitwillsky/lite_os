//! RFB 3.8 desktop access over the root-owned Unix socket forwarded by OpenSSH.
use crate::{input::Event, scanout::Scanout};
use linux_uapi::unix::{PollEvents, PollFd};
use std::{
    collections::BTreeSet,
    fs, io,
    io::{Read, Write},
    os::{
        fd::AsFd,
        unix::net::{UnixListener, UnixStream},
    },
    path::Path,
};

const VERSION: &[u8] = b"RFB 003.008\n";
const FORMAT: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0];
const BUTTONS: [u16; 3] = [0x110, 0x112, 0x111];

pub(super) struct Desktop {
    listener: UnixListener,
    client: Option<Client>,
}

impl Desktop {
    pub(super) fn bind(runtime: &Path) -> io::Result<Option<Self>> {
        let directory = runtime.join("diagnostics");
        if !directory.is_dir() {
            return Ok(None);
        }
        let path = directory.join("rfb.sock");
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(path)?;
        listener.set_nonblocking(true)?;
        Ok(Some(Self {
            listener,
            client: None,
        }))
    }

    pub(super) fn descriptor(&self) -> PollFd<'_> {
        if let Some(client) = &self.client {
            let interest = if client.sent == client.output.len() {
                PollEvents::READ
            } else {
                PollEvents::WRITE
            };
            PollFd::new(client.socket.as_fd(), interest)
        } else {
            PollFd::new(self.listener.as_fd(), PollEvents::READ)
        }
    }

    pub(super) fn dispatch(
        &mut self,
        ready: PollEvents,
        scanout: &Scanout,
        events: &mut Vec<Event>,
    ) {
        if let Some(client) = &mut self.client {
            if let Err(error) = client.dispatch(ready, scanout, events) {
                eprintln!("compositor: RFB session closed: {error}");
                client.pointer(0, client.position, events);
                events.extend(client.keys.iter().map(|&code| Event::Key {
                    code,
                    pressed: false,
                }));
                self.client = None;
            }
        } else if ready.contains(PollEvents::READ) {
            match self
                .listener
                .accept()
                .and_then(|(socket, _)| Client::new(socket, scanout))
            {
                Ok(client) => self.client = Some(client),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => eprintln!("compositor: RFB accept: {error}"),
            }
        }
    }
}

enum Phase {
    Version,
    Security,
    ClientInit,
    Messages,
}

struct Client {
    socket: UnixStream,
    phase: Phase,
    incoming: Vec<u8>,
    output: Vec<u8>,
    // 只在远程读屏时分配，连接独占并复用；断开即释放。大小由已验证的请求矩形限制。
    capture_backing: Option<linux_uapi::drm::VirglResource>,
    sent: usize,
    dimensions: (u16, u16),
    position: (i32, i32),
    buttons: u8,
    keys: BTreeSet<u16>,
}

impl Client {
    fn new(socket: UnixStream, scanout: &Scanout) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            phase: Phase::Version,
            incoming: Vec::new(),
            output: VERSION.to_vec(),
            capture_backing: None,
            sent: 0,
            dimensions: (scanout.width() as u16, scanout.height() as u16),
            position: (0, 0),
            buttons: 0,
            keys: BTreeSet::new(),
        })
    }

    fn dispatch(
        &mut self,
        ready: PollEvents,
        scanout: &Scanout,
        events: &mut Vec<Event>,
    ) -> io::Result<()> {
        if ready.contains(PollEvents::ERROR | PollEvents::HANGUP) {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        if ready.contains(PollEvents::WRITE) {
            match self.socket.write(&self.output[self.sent..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => self.sent += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        if self.sent != self.output.len() {
            return Ok(());
        }
        if ready.contains(PollEvents::READ) {
            let mut input = [0; 4096];
            match self.socket.read(&mut input) {
                Ok(0) => return Err(io::ErrorKind::ConnectionAborted.into()),
                Ok(count) => self.incoming.extend_from_slice(&input[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        self.output.clear();
        self.sent = 0;
        while let Some(length) = self.message_len()? {
            // Frames are queued one at a time. While one is being written the client
            // cannot make the compositor allocate another or wait for socket space.
            let input: Vec<_> = self.incoming.drain(..length).collect();
            self.message(&input, scanout, events)?;
            if !self.output.is_empty() {
                break;
            }
        }
        Ok(())
    }

    fn message_len(&self) -> io::Result<Option<usize>> {
        let bytes = &self.incoming;
        let length = match self.phase {
            Phase::Version => VERSION.len(),
            Phase::Security | Phase::ClientInit => 1,
            Phase::Messages => match bytes.first() {
                None => return Ok(None),
                Some(0) => 20,
                Some(2) if bytes.len() >= 4 => 4 + usize::from(be16(&bytes[2..])) * 4,
                Some(2 | 6) if bytes.len() < 8 => return Ok(None),
                Some(3) => 10,
                Some(4) => 8,
                Some(5) => 6,
                Some(6) => 8usize.saturating_add(be32(&bytes[4..]) as usize),
                _ => return Err(invalid("unsupported RFB message")),
            },
        };
        if length > 16 * 1024 {
            return Err(invalid("RFB message exceeds limit"));
        }
        Ok((bytes.len() >= length).then_some(length))
    }

    fn message(
        &mut self,
        bytes: &[u8],
        scanout: &Scanout,
        events: &mut Vec<Event>,
    ) -> io::Result<()> {
        match self.phase {
            Phase::Version if bytes == VERSION => {
                self.output.extend_from_slice(&[1, 1]); // None, inside authenticated SSH.
                self.phase = Phase::Security;
            }
            Phase::Security if bytes == [1] => {
                self.output.extend_from_slice(&[0; 4]);
                self.phase = Phase::ClientInit;
            }
            Phase::ClientInit => {
                self.output
                    .extend_from_slice(&self.dimensions.0.to_be_bytes());
                self.output
                    .extend_from_slice(&self.dimensions.1.to_be_bytes());
                self.output.extend_from_slice(&FORMAT);
                self.output.extend_from_slice(&6u32.to_be_bytes());
                self.output.extend_from_slice(b"LiteOS");
                self.phase = Phase::Messages;
            }
            Phase::Messages => match bytes[0] {
                0 if bytes[4..17] == FORMAT[..13] => {}
                0 => return Err(invalid("unsupported RFB pixel format")),
                2 | 6 => {}
                3 => self.frame(bytes, scanout)?,
                4 => {
                    if let Some(code) = keycode(be32(&bytes[4..])) {
                        let pressed = bytes[1] != 0;
                        let changed = if pressed {
                            self.keys.insert(code)
                        } else {
                            self.keys.remove(&code)
                        };
                        if changed {
                            events.push(Event::Key { code, pressed });
                        }
                    }
                }
                5 => {
                    let x = be16(&bytes[2..]).min(self.dimensions.0 - 1);
                    let y = be16(&bytes[4..]).min(self.dimensions.1 - 1);
                    self.pointer(bytes[1], (i32::from(x), i32::from(y)), events);
                }
                _ => return Err(invalid("unsupported RFB message")),
            },
            _ => return Err(invalid("invalid RFB 3.8 handshake")),
        }
        Ok(())
    }

    fn pointer(&mut self, buttons: u8, position: (i32, i32), events: &mut Vec<Event>) {
        if position != self.position {
            let (x, y) = position;
            events.push(Event::PointerMoved { x, y });
            self.position = position;
        }
        let changed = buttons ^ self.buttons;
        for (index, code) in BUTTONS.into_iter().enumerate() {
            let bit = 1 << index;
            if changed & bit != 0 {
                events.push(Event::Button {
                    code,
                    pressed: buttons & bit != 0,
                    position,
                });
            }
        }
        let wheel = changed & buttons;
        let vertical = i32::from(wheel & 16 != 0) - i32::from(wheel & 8 != 0);
        if vertical != 0 {
            events.push(Event::Wheel {
                vertical,
                horizontal: 0,
            });
        }
        self.buttons = buttons;
    }

    fn frame(&mut self, bytes: &[u8], scanout: &Scanout) -> io::Result<()> {
        if (scanout.width(), scanout.height())
            != (
                usize::from(self.dimensions.0),
                usize::from(self.dimensions.1),
            )
        {
            return Err(invalid("output size changed; reconnect the RFB client"));
        }
        let [x, y, width, height] = [2, 4, 6, 8].map(|offset| usize::from(be16(&bytes[offset..])));
        if width == 0
            || height == 0
            || x + width > scanout.width()
            || y + height > scanout.height()
            || width * height > 4096 * 4096
        {
            return Err(invalid("RFB update rectangle out of range"));
        }
        self.output
            .try_reserve_exact(16 + width * height * 4)
            .map_err(io::Error::other)?;
        self.output.extend_from_slice(&[0, 0, 0, 1]); // FramebufferUpdate, one Raw rectangle.
        self.output.extend_from_slice(&bytes[2..10]);
        self.output.extend_from_slice(&[0; 4]);
        scanout.capture(
            [x as u32, y as u32, width as u32, height as u32],
            &mut self.capture_backing,
            &mut self.output,
        )
    }
}

fn be16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn keycode(symbol: u32) -> Option<u16> {
    match symbol {
        0xff08 => Some(14),
        0xff09 => Some(15),
        0xff0d => Some(28),
        0xff13 => Some(119),
        0xff1b => Some(1),
        0xff50 => Some(102),
        0xff51 => Some(105),
        0xff52 => Some(103),
        0xff53 => Some(106),
        0xff54 => Some(108),
        0xff55 => Some(104),
        0xff56 => Some(109),
        0xff57 => Some(107),
        0xff63 => Some(110),
        0xffff => Some(111),
        0xffbe..=0xffc7 => Some((symbol - 0xffbe) as u16 + 59),
        0xffc8 => Some(87),
        0xffc9 => Some(88),
        0xffe1 => Some(42),
        0xffe2 => Some(54),
        0xffe3 => Some(29),
        0xffe4 => Some(97),
        0xffe5 => Some(58),
        0xffe9 => Some(56),
        0xffea => Some(100),
        0xffeb => Some(125),
        0xffec => Some(126),
        0x20..=0x7e => (2..=58).find(|&code| {
            let byte = symbol as u8;
            keyboard::plain_key(code) == Some(byte) || keyboard::shifted_key(code) == Some(byte)
        }),
        _ => None,
    }
}
