//! seatd 客户端：compositor 以 `Role::Desktop` 注册，按 `Acquire`/`Release` 交接显示器与键盘。
//!
//! 持有关系：compositor 在 `Release` 时只 `drop_master` 并关闭 evdev，保留 DRM fd、VirGL context 与
//! 全部客户端缓冲，因此桌面状态跨切换保留；`Acquire` 时重新 `set_master` 并把仍在前台的 target
//! 设回 scanout。主循环在两者之间阻塞于 [`SeatClient::wait_acquire`]，不处理任何 client 帧。

use std::{
    io::{self, Read, Write},
    os::{
        fd::{AsFd, BorrowedFd},
        unix::net::UnixStream,
    },
    thread,
    time::Duration,
};

use linux_uapi::unix::{self, PollEvents, PollFd};
use seat_proto::{ClientFrame, FRAME_BYTES, Role, SOCKET_PATH, ServerFrame};

const RETRY: Duration = Duration::from_millis(200);

pub struct SeatClient {
    stream: UnixStream,
}

impl SeatClient {
    /// 连接 seatd 并注册；seatd 尚未启动时持续重试（init 同时拉起各服务，顺序不确定）。
    pub fn connect() -> io::Result<Self> {
        loop {
            match Self::try_connect() {
                Ok(client) => return Ok(client),
                Err(_) => thread::sleep(RETRY),
            }
        }
    }

    fn try_connect() -> io::Result<Self> {
        let mut stream = UnixStream::connect(SOCKET_PATH)?;
        stream.write_all(&ClientFrame::Register(Role::Desktop).encode())?;
        Ok(Self { stream })
    }

    /// 供主循环 poll：seat 帧到达时唤醒。
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }

    /// 读取一帧。`Ok(None)` 表示没有数据；seatd 断开时返回 `Err`，调用方负责重连。
    pub fn try_frame(&mut self) -> io::Result<Option<ServerFrame>> {
        let mut descriptors = [PollFd::new(self.stream.as_fd(), PollEvents::READ)];
        unix::poll(&mut descriptors, Some(Duration::ZERO))?;
        if descriptors[0].returned() == PollEvents::EMPTY {
            return Ok(None);
        }
        self.read_frame().map(Some)
    }

    fn read_frame(&mut self) -> io::Result<ServerFrame> {
        let mut bytes = [0u8; FRAME_BYTES];
        self.stream.read_exact(&mut bytes)?;
        ServerFrame::decode(bytes).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "seatd sent an unknown frame")
        })
    }

    pub fn send(&mut self, frame: ClientFrame) -> io::Result<()> {
        self.stream.write_all(&frame.encode())
    }

    /// 阻塞直到 seatd 授予座位；等待期间收到的 `Release` 立即确认（本进程此时并不持有）。
    pub fn wait_acquire(&mut self) -> io::Result<()> {
        loop {
            match self.read_frame()? {
                ServerFrame::Acquire => return Ok(()),
                ServerFrame::Release => self.send(ClientFrame::Released)?,
                ServerFrame::Switched(_) => {}
            }
        }
    }
}
