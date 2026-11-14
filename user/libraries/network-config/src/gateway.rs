//! 管理会话。控制帧沿用原生配置协议，文档按声明长度分块传输。

use router_protocol::{administration, bridge, management, ordinal};
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    os::{fd::AsFd, unix::net::UnixStream},
    path::Path,
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket, client::IntoClientRequest, protocol::WebSocketConfig};

mod capture;
mod observation;
mod server;
mod tls;
pub use capture::{Capture, CaptureResult, CaptureStop};
pub use observation::{
    FirewallObservation, FlowObservation, NeighborObservation, PendingConnectionWatch,
    RouteObservation, TrafficObservation,
};
pub use server::serve_router_api;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const FRAME_BYTES: usize = bridge::DATAGRAM_MAX;
const CONTROL_HEADER: &str = "Sec-WebSocket-Protocol";

fn wire_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .read_buffer_size(4096)
        .write_buffer_size(0)
        .max_write_buffer_size(FRAME_BYTES + 32)
        .max_message_size(Some(FRAME_BYTES))
        .max_frame_size(Some(FRAME_BYTES))
}

/// 单次握手或操作使用绝对 I/O 截止，少量字节的持续写入不能无限占住会话。
#[derive(Debug)]
struct DeadlineStream<S> {
    socket: S,
    deadline: Instant,
}

impl<S> DeadlineStream<S> {
    fn new(socket: S) -> Self {
        Self {
            socket,
            deadline: Instant::now() + HANDSHAKE_TIMEOUT,
        }
    }

    fn reset(&mut self, timeout: Duration) {
        self.deadline = Instant::now() + timeout;
    }

    fn remaining(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| io::ErrorKind::TimedOut.into())
    }
}

trait SocketTimeouts: Read + Write {
    fn read_timeout(&self, timeout: Duration) -> io::Result<()>;
    fn write_timeout(&self, timeout: Duration) -> io::Result<()>;
}

macro_rules! socket_timeouts {
    ($socket:ty) => {
        impl SocketTimeouts for $socket {
            fn read_timeout(&self, timeout: Duration) -> io::Result<()> {
                self.set_read_timeout(Some(timeout))
            }

            fn write_timeout(&self, timeout: Duration) -> io::Result<()> {
                self.set_write_timeout(Some(timeout))
            }
        }
    };
}

socket_timeouts!(UnixStream);
socket_timeouts!(TcpStream);

impl<S: SocketTimeouts> Read for DeadlineStream<S> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.socket.read_timeout(self.remaining()?)?;
        self.socket.read(bytes)
    }
}

impl<S: SocketTimeouts> Write for DeadlineStream<S> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.socket.write_timeout(self.remaining()?)?;
        self.socket.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.socket.flush()
    }
}

#[derive(Debug)]
enum SessionIo {
    Unix(DeadlineStream<UnixStream>),
    Client(Box<rustls::StreamOwned<rustls::ClientConnection, DeadlineStream<TcpStream>>>),
    Server(Box<rustls::StreamOwned<rustls::ServerConnection, DeadlineStream<TcpStream>>>),
}

impl SessionIo {
    fn reset(&mut self, timeout: Duration) {
        match self {
            Self::Unix(stream) => stream.reset(timeout),
            Self::Client(stream) => stream.sock.reset(timeout),
            Self::Server(stream) => stream.sock.reset(timeout),
        }
    }
}

impl Read for SessionIo {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.read(bytes),
            Self::Client(stream) => stream.read(bytes),
            Self::Server(stream) => stream.read(bytes),
        }
    }
}

impl Write for SessionIo {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.write(bytes),
            Self::Client(stream) => stream.write(bytes),
            Self::Server(stream) => stream.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.flush(),
            Self::Client(stream) => stream.flush(),
            Self::Server(stream) => stream.flush(),
        }
    }
}

type Session = WebSocket<SessionIo>;

/// 与本地或远程路由管理端点通信。远程实例持有一次已认证会话。
pub struct ManagementClient {
    remote: Option<Session>,
}

impl ManagementClient {
    /// 创建连接本机管理入口的客户端。
    pub const fn local() -> Self {
        Self { remote: None }
    }

    /// 通过 wss URL 和信任的 PEM CA 登录远程系统。密码只用于握手。
    /// # Errors
    /// URL、证书、主机名、网络连接或账户认证失败。
    pub fn remote(url: &str, user: &str, password: &str, ca: &Path) -> io::Result<Self> {
        Ok(Self {
            remote: Some(tls::connect(url, user, password, ca)?),
        })
    }

    /// 当前客户端是否连接远程系统。界面可据此避免读取本机接口元数据。
    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }
}

fn websocket_error(error: tungstenite::Error) -> io::Error {
    match error {
        tungstenite::Error::Io(error) => error,
        tungstenite::Error::ConnectionClosed
        | tungstenite::Error::AlreadyClosed
        | tungstenite::Error::Protocol(
            tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
        ) => io::ErrorKind::UnexpectedEof.into(),
        error => io::Error::other(error),
    }
}

fn handshake_error<R: tungstenite::handshake::HandshakeRole>(
    error: tungstenite::HandshakeError<R>,
) -> io::Error {
    match error {
        tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response))
            if response.status() == tungstenite::http::StatusCode::UNAUTHORIZED =>
        {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "账户认证失败或无远程管理权限",
            )
        }
        tungstenite::HandshakeError::Failure(error) => websocket_error(error),
        tungstenite::HandshakeError::Interrupted(_) => io::ErrorKind::TimedOut.into(),
    }
}

fn receive(session: &mut Session) -> io::Result<tungstenite::Bytes> {
    loop {
        match session.read().map_err(websocket_error)? {
            Message::Binary(body) => return Ok(body),
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(frame) => {
                let _ = session.flush();
                if let Some(frame) = frame {
                    use tungstenite::protocol::frame::coding::CloseCode;
                    match frame.code {
                        CloseCode::Policy => {
                            return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                frame.reason.to_string(),
                            ));
                        }
                        CloseCode::Error => return Err(io::Error::other(frame.reason.to_string())),
                        _ => {}
                    }
                }
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            _ => return Err(io::ErrorKind::InvalidData.into()),
        }
    }
}

fn receive_document(
    session: &mut Session,
    mut remaining: usize,
    mut write: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    while remaining != 0 {
        let chunk = receive(session)?;
        if chunk.is_empty() || chunk.len() > remaining {
            return Err(io::ErrorKind::InvalidData.into());
        }
        write(&chunk)?;
        remaining -= chunk.len();
    }
    Ok(())
}

fn response_control(operation: u32, frame: &[u8]) -> io::Result<(i32, &[u8], usize)> {
    let (status, control) = frame
        .split_at_checked(bridge::HEADER_BYTES)
        .ok_or(io::ErrorKind::InvalidData)?;
    let status = i32::from_le_bytes(status.try_into().expect("fixed status field"));
    let document = if status == 0 && ordinal::is_configuration(operation) {
        let metadata = administration::Reply::decode(operation, control)
            .map_err(|_| io::ErrorKind::InvalidData)?;
        usize::try_from(metadata.length).map_err(|_| io::ErrorKind::InvalidData)?
    } else {
        if status != 0 && !control.is_empty() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        0
    };
    Ok((status, control, document))
}

impl ManagementClient {
    pub(super) fn request(
        &mut self,
        operation: u32,
        control: &[u8],
        document: Option<&[u8]>,
        timeout: Duration,
    ) -> io::Result<(Vec<u8>, Vec<u8>)> {
        if control.len() > FRAME_BYTES - bridge::HEADER_BYTES {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if let Some(session) = &mut self.remote {
            return exchange(session, operation, control, document, timeout);
        }
        let mut session = local_session()?;
        let response = exchange(&mut session, operation, control, document, timeout);
        let _ = session.close(None);
        response
    }
}

fn exchange(
    session: &mut Session,
    operation: u32,
    control: &[u8],
    document: Option<&[u8]>,
    timeout: Duration,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    session.get_mut().reset(timeout);
    send_request(session, operation, control, document)?;
    let response = receive(session)?;
    let (control, length) = success_control(operation, &response)?;
    let mut body = Vec::new();
    receive_document(session, length, |chunk| {
        body.try_reserve(chunk.len())
            .map_err(|_| io::ErrorKind::OutOfMemory)?;
        body.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok((control.to_vec(), body))
}

fn send_request(
    session: &mut Session,
    operation: u32,
    control: &[u8],
    document: Option<&[u8]>,
) -> io::Result<()> {
    let mut frame = Vec::with_capacity(bridge::HEADER_BYTES + control.len());
    frame.extend_from_slice(&operation.to_le_bytes());
    frame.extend_from_slice(control);
    session
        .send(Message::Binary(frame.into()))
        .map_err(websocket_error)?;
    for chunk in document.unwrap_or_default().chunks(FRAME_BYTES) {
        session
            .send(Message::Binary(chunk.to_vec().into()))
            .map_err(websocket_error)?;
    }
    Ok(())
}

fn success_control(operation: u32, response: &[u8]) -> io::Result<(&[u8], usize)> {
    let (status, control, length) = response_control(operation, response)?;
    if operation == ordinal::WAIT_CONNECTION_CHANGES && status == bridge::CONNECTION_WATCH_BUSY {
        return Err(io::Error::new(
            io::ErrorKind::ResourceBusy,
            "转发连接观察等待名额暂满",
        ));
    }
    if status != 0 {
        return Err(io::Error::other(format!(
            "网络设置服务拒绝请求（状态 {status}）"
        )));
    }
    Ok((control, length))
}

/// 打开本机已配置具名路由域的网络命名空间描述符。
/// # Errors
/// 权限不足、域未就绪、服务不可用或回复无效。
pub fn open_network_namespace(name: &str) -> io::Result<std::os::fd::OwnedFd> {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

    let operation = ordinal::OPEN_NETWORK_NAMESPACE;
    let request = administration::Request {
        domain: Some(name),
        ..Default::default()
    }
    .encode(operation)
    .map_err(|_| io::ErrorKind::InvalidInput)?;
    let socket = linux_uapi::unix::anonymous_datagram()?;
    socket.connect(OsStr::from_bytes(bridge::PATH))?;
    socket.set_read_timeout(Some(RPC_TIMEOUT))?;
    socket.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let mut frame = Vec::with_capacity(bridge::HEADER_BYTES + request.len());
    frame.extend_from_slice(&operation.to_le_bytes());
    frame.extend_from_slice(&request);
    if socket.send(&frame)? != frame.len() {
        return Err(io::ErrorKind::WriteZero.into());
    }
    let mut response = [0; bridge::HEADER_BYTES + administration::Reply::BYTES];
    let (length, files) = linux_uapi::unix::recv_fds(socket.as_fd(), &mut response)?;
    success_control(operation, &response[..length])?;
    let [namespace] = <[_; 1]>::try_from(files).map_err(|_| io::ErrorKind::InvalidData)?;
    Ok(namespace)
}

fn local_session() -> io::Result<Session> {
    let socket = UnixStream::connect(management::SOCKET_PATH)?;
    if linux_uapi::unix::peer_credentials(socket.as_fd())?.uid != 0 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    connect_session(socket)
}

fn connect_session(socket: UnixStream) -> io::Result<Session> {
    let request = websocket_request(&format!("ws://localhost{}", management::RESOURCE))?;
    client_upgrade(request, SessionIo::Unix(DeadlineStream::new(socket)))
}

fn websocket_request(url: &str) -> io::Result<tungstenite::handshake::client::Request> {
    let mut request = url.into_client_request().map_err(websocket_error)?;
    request.headers_mut().insert(
        CONTROL_HEADER,
        tungstenite::http::HeaderValue::from_static(management::SUBPROTOCOL),
    );
    Ok(request)
}

fn client_upgrade(
    request: tungstenite::handshake::client::Request,
    stream: SessionIo,
) -> io::Result<Session> {
    let (session, _) =
        tungstenite::client::client_with_config(request, stream, Some(wire_config()))
            .map_err(handshake_error)?;
    Ok(session)
}
