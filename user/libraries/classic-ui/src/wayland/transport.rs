//! 一条客户端连接的 wire、fd 与对象编号；不包含窗口或控件策略。

use linux_uapi::{
    process::wayland_socket_from_env,
    unix::{PollEvents, PollFd, poll, recv_fds, send_fd},
};
use std::{
    collections::{HashMap, VecDeque},
    env,
    io::{self, Write},
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::net::UnixStream,
    },
    path::PathBuf,
};
use wayland_wire::{
    protocol::{self, AnyEvent, Interface, wl_display},
    wire::{Framer, ObjectId, Reader, Writer},
};

pub(super) struct Transport {
    pub stream: UnixStream,
    pub writer: Writer,
    framer: Framer,
    fds: VecDeque<OwnedFd>,
    interfaces: HashMap<u32, Interface>,
    next_id: u32,
    reusable: VecDeque<u32>,
    // 只有带 FD 或遭遇背压的片段进入队列；普通请求直接从 Writer 非阻塞发送。
    // 每个 FD 由其 Packet 独占保活，只在首次成功 sendmsg 后释放，部分写绝不重复发送 SCM_RIGHTS。
    outgoing: VecDeque<Packet>,
}

struct Packet {
    bytes: Vec<u8>,
    position: usize,
    fd: Option<OwnedFd>,
}
const OUTPUT_LIMIT: usize = 1 << 20;
const OUTPUT_FD_LIMIT: usize = 64;
const WRITE_BUDGET: usize = 65536;

impl Transport {
    pub fn connect() -> io::Result<Self> {
        let stream = match wayland_socket_from_env()? {
            Some(stream) => stream,
            None => {
                let name = env::var_os("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into());
                let path = PathBuf::from(name);
                let path = if path.is_absolute() {
                    path
                } else {
                    PathBuf::from(
                        env::var_os("XDG_RUNTIME_DIR")
                            .ok_or_else(|| io::Error::other("XDG_RUNTIME_DIR is not set"))?,
                    )
                    .join(path)
                };
                UnixStream::connect(path)?
            }
        };
        Self::new(stream)
    }

    pub(super) fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            writer: Writer::new(),
            framer: Framer::new(),
            fds: VecDeque::new(),
            interfaces: HashMap::from([(1, Interface::WlDisplay)]),
            next_id: 2,
            reusable: VecDeque::new(),
            outgoing: VecDeque::new(),
        })
    }

    pub fn retire_server_object(&mut self, object: ObjectId) {
        self.interfaces.remove(&object.get());
    }
    pub fn server_object(&mut self, object: ObjectId, interface: Interface) -> io::Result<()> {
        if object.get() < 0xff00_0000 || self.interfaces.insert(object.get(), interface).is_some() {
            return Err(io::Error::other("invalid server object id"));
        }
        Ok(())
    }

    pub fn allocate(&mut self, interface: Interface) -> io::Result<ObjectId> {
        let raw = if let Some(raw) = self.reusable.pop_front() {
            raw
        } else {
            if self.next_id >= 0xff00_0000 {
                return Err(io::Error::other("Wayland object ids exhausted"));
            }
            let raw = self.next_id;
            self.next_id += 1;
            raw
        };
        self.interfaces.insert(raw, interface);
        Ok(ObjectId::new(raw).expect("allocated client id is nonzero"))
    }

    pub fn wants_write(&self) -> bool {
        !self.outgoing.is_empty() || !self.writer.bytes().is_empty()
    }
    fn capacity(&self, additional: usize, fd: bool) -> io::Result<()> {
        let bytes = self
            .outgoing
            .iter()
            .map(|packet| packet.bytes.len() - packet.position)
            .sum::<usize>()
            + self.writer.bytes().len();
        let fds = self
            .outgoing
            .iter()
            .filter(|packet| packet.fd.is_some())
            .count();
        if bytes.saturating_add(additional) > OUTPUT_LIMIT
            || fds + usize::from(fd) > OUTPUT_FD_LIMIT
        {
            // 队列满是连接错误，不返回可重试 WouldBlock；上层不能重放已经编码的 commit/对象创建。
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "Wayland output queue limit reached",
            ));
        }
        Ok(())
    }
    /// 尽量发送，背压保留尾部，由统一 poll 关注可写；不在 UI 线程等待 socket 空间。
    pub fn flush(&mut self) -> io::Result<()> {
        self.capacity(0, false)?;
        let mut budget = WRITE_BUDGET;
        while let Some(packet) = self.outgoing.front_mut() {
            let end = packet.bytes.len().min(packet.position + budget);
            let bytes = &packet.bytes[packet.position..end];
            let result = match &packet.fd {
                Some(fd) => send_fd(self.stream.as_fd(), bytes, fd.as_fd()),
                None => self.stream.write(bytes),
            };
            match result {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => {
                    packet.fd.take();
                    packet.position += count;
                    budget -= count;
                    if packet.position == packet.bytes.len() {
                        self.outgoing.pop_front();
                    }
                    if budget == 0 {
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        while !self.writer.bytes().is_empty() {
            let end = self.writer.bytes().len().min(budget);
            match self.stream.write(&self.writer.bytes()[..end]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => {
                    self.writer.consume(count, 0);
                    budget -= count;
                    if budget == 0 {
                        break;
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    pub fn send_fd(&mut self, bytes: &[u8], fd: BorrowedFd<'_>) -> io::Result<()> {
        if bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SCM_RIGHTS requires stream data",
            ));
        }
        self.flush()?;
        self.capacity(bytes.len(), true)?;
        let fd = fd.try_clone_to_owned()?;
        if !self.writer.bytes().is_empty() {
            let previous = self.writer.bytes().to_vec();
            self.writer.consume(previous.len(), 0);
            self.outgoing.push_back(Packet {
                bytes: previous,
                position: 0,
                fd: None,
            });
        }
        self.outgoing.push_back(Packet {
            bytes: bytes.to_vec(),
            position: 0,
            fd: Some(fd),
        });
        self.flush()
    }

    pub fn buffered_event(&mut self) -> io::Result<Option<(ObjectId, AnyEvent)>> {
        let Some(frame) = self.framer.next_frame().map_err(io::Error::other)? else {
            return Ok(None);
        };
        let interface = *self
            .interfaces
            .get(&frame.object.get())
            .ok_or_else(|| io::Error::other("event references unknown Wayland object"))?;
        let event = protocol::decode_event(
            interface,
            frame.opcode,
            &mut Reader::new(&frame.body, &mut self.fds),
        )
        .map_err(io::Error::other)?;
        match &event {
            AnyEvent::WlDisplay(wl_display::Event::Error { code, message, .. }) => {
                return Err(io::Error::other(format!("Wayland error {code}: {message}")));
            }
            AnyEvent::WlDisplay(wl_display::Event::DeleteId { id }) => {
                if *id == 1 || self.interfaces.remove(id).is_none() {
                    return Err(io::Error::other(
                        "delete_id references unknown Wayland object",
                    ));
                }
                self.reusable.push_back(*id);
            }
            _ => {}
        }
        Ok(Some((frame.object, event)))
    }

    pub fn read(&mut self) -> io::Result<()> {
        let mut bytes = [0; 4096];
        let (count, fds) = match recv_fds(self.stream.as_fd(), &mut bytes) {
            Ok(result) => result,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        if self.fds.len() + fds.len() > 64 {
            return Err(io::Error::other("too many unconsumed Wayland descriptors"));
        }
        self.fds.extend(fds);
        self.framer.push(&bytes[..count]);
        Ok(())
    }

    pub fn blocking_event(&mut self) -> io::Result<(ObjectId, AnyEvent)> {
        loop {
            self.flush()?;
            if let Some(event) = self.buffered_event()? {
                return Ok(event);
            }
            let interest = if self.wants_write() {
                PollEvents::READ | PollEvents::WRITE
            } else {
                PollEvents::READ
            };
            let mut source = [PollFd::new(self.stream.as_fd(), interest)];
            match poll(&mut source, None) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
                Ok(_) => {}
            }
            if source[0]
                .returned()
                .contains(PollEvents::READ | PollEvents::ERROR | PollEvents::HANGUP)
            {
                self.read()?;
            }
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        // Transport 独占协议连接。继承的 stdin 可能仍是同一 OFD 的别名，单独 close 副本不能断开对端。
        // shutdown 撤销整个连接的 I/O，不关闭其他 owner 借用的 fd，也不在析构中阻塞 flush。
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protocol_drop_disconnects_even_when_the_inherited_descriptor_alias_survives() {
        use std::io::Read;
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let inherited = stream.try_clone().unwrap();
        peer.set_read_timeout(Some(std::time::Duration::from_millis(20)))
            .unwrap();
        drop(Transport::new(stream).unwrap());
        assert_eq!(peer.read(&mut [0u8]).unwrap(), 0);
        drop(inherited);
    }
    #[test]
    fn ids_are_reused_only_after_delete_id_and_split_events_are_preserved() {
        let (stream, mut server) = UnixStream::pair().unwrap();
        let mut transport = Transport::new(stream).unwrap();
        let first = transport.allocate(Interface::WlCallback).unwrap();
        let second = transport.allocate(Interface::WlCallback).unwrap();
        let mut bytes = Writer::new();
        wl_display::Event::DeleteId { id: first.get() }
            .encode(ObjectId::DISPLAY, &mut bytes)
            .unwrap();
        server.write_all(&bytes.bytes()[..5]).unwrap();
        transport.read().unwrap();
        assert!(transport.buffered_event().unwrap().is_none());
        server.write_all(&bytes.bytes()[5..]).unwrap();
        transport.read().unwrap();
        assert!(transport.buffered_event().unwrap().is_some());
        assert_eq!(transport.allocate(Interface::WlCallback).unwrap(), first);
        assert_ne!(first, second);
        drop(server);
        assert_eq!(
            transport.read().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
