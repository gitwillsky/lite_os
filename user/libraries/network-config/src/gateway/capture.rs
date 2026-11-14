//! 本地管理网关按需将标准 AF_PACKET 捕获保存为 PCAP。

use super::*;
use router_protocol::management::capture::{self, End, Frame};
use std::net::Shutdown;

/// 本地已授权的单接口抓包会话。
pub struct Capture(Session);

/// 对端写半关闭可中止抓包，接收方向保持开启以便封口。丢弃句柄也请求停止。
pub struct CaptureStop(UnixStream);

impl CaptureStop {
    /// 通知网关停止读取接口。
    /// # Errors
    /// 网关连接已断开或 shutdown 失败。
    pub fn stop(&self) -> io::Result<()> {
        self.0.shutdown(Shutdown::Write)
    }
}

impl Drop for CaptureStop {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// 保存的报文总数、套接字丢弃数与本次结束原因。
pub struct CaptureResult {
    pub packets: u64,
    pub drops: u32,
    pub reason: End,
}

impl Capture {
    /// 启动指定接口抓包。握手可能阻塞，应从后台调用。
    /// # Errors
    /// 网关不可用或验证、发送失败。权限不足在读取回复时报告。
    pub fn start(interface: u32) -> io::Result<(Self, CaptureStop)> {
        let mut session = local_session()?;
        let SessionIo::Unix(stream) = session.get_ref() else {
            unreachable!("local capture")
        };
        let stop = CaptureStop(stream.socket.try_clone()?);
        session.get_mut().reset(RPC_TIMEOUT);
        send_request(
            &mut session,
            management::CAPTURE,
            &capture::request(interface),
            None,
        )?;
        Ok((Self(session), stop))
    }

    /// 顺序写入完整 PCAP 文件，直到网关发送结束原因。应从后台执行。
    /// # Errors
    /// 写入失败、连接中断、权限不足或数据不符合 PCAP 布局。
    pub fn write(mut self, output: &mut impl Write) -> io::Result<CaptureResult> {
        self.0
            .get_mut()
            .reset(Duration::from_secs(capture::DURATION) + RPC_TIMEOUT);
        let first = receive(&mut self.0)?;
        let Frame::Header(header) = capture_frame(&first)? else {
            return Err(io::ErrorKind::InvalidData.into());
        };
        output.write_all(header)?;
        let mut length = header.len() as u64;
        let mut packets = 0u64;
        loop {
            let message = receive(&mut self.0)?;
            match capture_frame(&message)? {
                Frame::Records {
                    bytes,
                    packets: count,
                } => {
                    length += bytes.len() as u64;
                    if length > capture::FILE_LIMIT {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                    output.write_all(bytes)?;
                    packets += u64::from(count);
                }
                Frame::Finished { reason, drops } => {
                    return Ok(CaptureResult {
                        packets,
                        drops,
                        reason,
                    });
                }
                Frame::Header(_) => return Err(io::ErrorKind::InvalidData.into()),
            }
        }
    }
}

fn capture_frame(bytes: &[u8]) -> io::Result<Frame<'_>> {
    let (body, _) = success_control(management::CAPTURE, bytes)?;
    Frame::parse(body).map_err(|_| io::ErrorKind::InvalidData.into())
}

#[cfg(target_os = "linux")]
pub(super) fn serve(session: &mut Session, control: &[u8]) -> io::Result<()> {
    use linux_uapi::{
        packet::Receiver,
        unix::{PollEvents, PollFd, poll},
    };
    use tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};

    let result = (|| {
        let index = capture::parse_request(control).map_err(|_| io::ErrorKind::InvalidData)?;
        let SessionIo::Unix(stream) = session.get_ref() else {
            return Err(io::ErrorKind::PermissionDenied.into());
        };
        let peer = stream.socket.try_clone()?;
        let receiver = Receiver::open(index)?;
        receiver.check_error()?;
        let mut header = message(1, capture::HEADER.len());
        header.extend_from_slice(&capture::HEADER);
        send(session, header)?;
        let deadline = Instant::now() + Duration::from_secs(capture::DURATION);
        let mut frame = vec![0u8; capture::SNAPLEN];
        let mut length = capture::HEADER.len() as u64;
        let mut descriptors = [
            PollFd::new(peer.as_fd(), PollEvents::READ),
            PollFd::new(receiver.as_fd(), PollEvents::READ),
        ];
        let reason = 'capture: loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break End::TimeLimit;
            }
            match poll(&mut descriptors, Some(remaining)) {
                Ok(0) => break End::TimeLimit,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                ready => {
                    ready?;
                }
            }
            // 抓包请求后对端只读。写侧半关闭可在空闲接口立即唤醒。
            if descriptors[0].returned() != PollEvents::EMPTY {
                break End::Stopped;
            }
            if descriptors[1].returned().contains(PollEvents::ERROR) {
                match receiver.check_error() {
                    Err(error) if matches!(error.raw_os_error(), Some(19 | 100)) => {
                        break End::InterfaceDown;
                    }
                    Err(error) => return Err(error),
                    Ok(()) => {}
                }
            }
            if descriptors[1].returned().contains(PollEvents::HANGUP) {
                break End::InterfaceDown;
            }
            let mut batch = message(2, FRAME_BYTES - bridge::HEADER_BYTES - 1);
            for _ in 0..64 {
                let info = match receiver.receive(&mut frame) {
                    Ok(info) => info,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if matches!(error.raw_os_error(), Some(19 | 100)) => {
                        send_records(session, batch)?;
                        break 'capture End::InterfaceDown;
                    }
                    Err(error) => return Err(error),
                };
                if info.loopback_outgoing {
                    continue;
                }
                let captured = info.full_length.min(frame.len());
                let record_length = capture::RECORD_BYTES + captured;
                if length + record_length as u64 > capture::FILE_LIMIT {
                    send_records(session, batch)?;
                    break 'capture End::SizeLimit;
                }
                if batch.len() + record_length > FRAME_BYTES {
                    send_records(session, batch)?;
                    batch = message(2, FRAME_BYTES - bridge::HEADER_BYTES - 1);
                }
                capture::record(
                    &mut batch,
                    info.seconds,
                    info.nanoseconds,
                    info.full_length,
                    &frame[..captured],
                )
                .map_err(|_| io::ErrorKind::InvalidData)?;
                length += record_length as u64;
            }
            send_records(session, batch)?;
        };
        let [_, drops] = receiver.take_statistics()?;
        drop(receiver);
        let mut finished = message(3, 5);
        finished.push(reason as u8);
        finished.extend_from_slice(&drops.to_le_bytes());
        send(session, finished)
    })();
    if let Err(error) = &result {
        session.get_mut().reset(HANDSHAKE_TIMEOUT);
        let _ = session.close(Some(CloseFrame {
            code: CloseCode::Error,
            reason: format!("抓包失败：{error}").into(),
        }));
    }
    result
}

#[cfg(target_os = "linux")]
fn message(tag: u8, body: usize) -> Vec<u8> {
    let mut frame = Vec::with_capacity(bridge::HEADER_BYTES + 1 + body);
    frame.extend_from_slice(&0i32.to_le_bytes());
    frame.push(tag);
    frame
}

#[cfg(target_os = "linux")]
fn send_records(session: &mut Session, frame: Vec<u8>) -> io::Result<()> {
    if frame.len() > bridge::HEADER_BYTES + 1 {
        send(session, frame)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn send(session: &mut Session, frame: Vec<u8>) -> io::Result<()> {
    session.get_mut().reset(HANDSHAKE_TIMEOUT);
    session
        .send(Message::Binary(frame.into()))
        .map_err(websocket_error)
}
