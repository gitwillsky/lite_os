//! seatd 连接：注册为文本 session，转交 `Acquire`/`Release`，并在 seatd 重启后自动重连。

use std::{
    io::{Read, Write},
    os::{
        fd::{AsFd, BorrowedFd},
        unix::net::UnixStream,
    },
    time::{Duration, Instant},
};

use seat_proto::{ClientFrame, FRAME_BYTES, Role, SOCKET_PATH, ServerFrame};

const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);

pub struct SeatLink {
    role: Role,
    stream: Option<UnixStream>,
    last_attempt: Option<Instant>,
}

/// 读取结果：一帧、暂无数据、或连接已断开。
pub enum Received {
    Frame(ServerFrame),
    Idle,
    Closed,
}

impl SeatLink {
    pub fn new(role: Role) -> Self {
        Self {
            role,
            stream: None,
            last_attempt: None,
        }
    }

    /// 未连接时按间隔重试；连接成功立即发送 Register。
    pub fn ensure_connected(&mut self) {
        if self.stream.is_some() {
            return;
        }
        let now = Instant::now();
        if self
            .last_attempt
            .is_some_and(|last| now.duration_since(last) < RECONNECT_INTERVAL)
        {
            return;
        }
        self.last_attempt = Some(now);
        let registered = UnixStream::connect(SOCKET_PATH)
            .ok()
            .and_then(|mut stream| {
                stream
                    .write_all(&ClientFrame::Register(self.role).encode())
                    .ok()
                    .map(|()| stream)
            });
        self.stream = registered;
    }

    pub fn fd(&self) -> Option<BorrowedFd<'_>> {
        self.stream.as_ref().map(AsFd::as_fd)
    }

    /// 距离下次重连尝试还需等待多久；已连接时为 `None`。
    pub fn reconnect_in(&self) -> Option<Duration> {
        if self.stream.is_some() {
            return None;
        }
        Some(self.last_attempt.map_or(Duration::ZERO, |last| {
            RECONNECT_INTERVAL.saturating_sub(last.elapsed())
        }))
    }

    /// 读取一帧；协议帧固定 2 字节，seatd 一次写入一帧，因此 readable 后可以阻塞读完。
    pub fn receive(&mut self) -> Received {
        let Some(stream) = self.stream.as_mut() else {
            return Received::Idle;
        };
        let mut bytes = [0u8; FRAME_BYTES];
        match stream.read_exact(&mut bytes) {
            Ok(()) => match ServerFrame::decode(bytes) {
                Some(frame) => Received::Frame(frame),
                None => self.close(),
            },
            Err(_) => self.close(),
        }
    }

    fn close(&mut self) -> Received {
        self.stream = None;
        Received::Closed
    }

    pub fn send(&mut self, frame: ClientFrame) {
        let failed = self
            .stream
            .as_mut()
            .is_some_and(|stream| stream.write_all(&frame.encode()).is_err());
        if failed {
            self.stream = None;
        }
    }
}
