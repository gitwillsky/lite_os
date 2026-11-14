//! 本地网关负责认证、授权和传输。配置决定仍归原生 router-config。

use super::*;
use linux_uapi::{
    shared_memory::MemFd,
    unix::{anonymous_datagram, recv_fds, send_fd},
};
use std::{
    ffi::OsStr,
    fs::{self, File, Permissions},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileExt, FileTypeExt, MetadataExt, PermissionsExt},
        net::UnixListener,
    },
    sync::Arc,
    thread,
};
use tungstenite::{
    handshake::server::{ErrorResponse, Request, Response},
    http::{HeaderValue, StatusCode},
    protocol::{CloseFrame, frame::coding::CloseCode},
};

mod firewall;
#[cfg(any(target_os = "linux", test))]
mod flows;
mod neighbors;
mod observation;
mod routes;
#[cfg(target_os = "linux")]
mod traffic;

#[derive(Clone, Copy)]
pub(super) enum Role {
    Status,
    Viewer,
    Administrator,
}

impl Role {
    fn connection_limit(self) -> usize {
        // 普通状态连接不能用尽管理员及观察者的等待名额。
        match self {
            Self::Status | Self::Viewer => 8,
            Self::Administrator => 16,
        }
    }

    fn permits(self, operation: u32) -> bool {
        match operation {
            ordinal::GET => true,
            ordinal::SCHEMA
            | ordinal::WAIT
            | ordinal::BGP_STATUS
            | ordinal::OSPF_STATUS
            | ordinal::BGP_ROUTES
            | ordinal::DHCP_SERVER_BINDINGS
            | ordinal::CONNECTIONS
            | ordinal::CONNECTION_CHANGES
            | ordinal::WAIT_CONNECTION_CHANGES
            | ordinal::FIREWALL_COUNTERS
            | ordinal::NEIGHBOR_PAGE
            | ordinal::ROUTE_PAGE
            | ordinal::ROUTING_DOMAINS
            | management::WATCH_FIREWALL_COUNTERS
            | management::WATCH_NEIGHBORS
            | management::WATCH_ROUTES
            | management::WATCH_LINK_TRAFFIC => !matches!(self, Self::Status),
            ordinal::SET
            | ordinal::DHCP
            | ordinal::RENAME
            | ordinal::EXPORT
            | ordinal::VALIDATE
            | ordinal::IMPORT
            | ordinal::DIFF
            | ordinal::BACKUPS
            | ordinal::BACKUP_CREATE
            | ordinal::BACKUP_EXPORT
            | ordinal::BACKUP_DELETE
            | ordinal::BGP_REFRESH
            | ordinal::CLEAR_CONNECTION
            | management::CAPTURE
            | management::WATCH_FLOWS
            | ordinal::COMMIT_RESULT => matches!(self, Self::Administrator),
            _ => false,
        }
    }
}

fn role(socket: &UnixStream) -> io::Result<Role> {
    let uid = linux_uapi::unix::peer_credentials(socket.as_fd())?.uid;
    if uid == 0 {
        return Ok(Role::Administrator);
    }
    let passwd = fs::read_to_string("/etc/passwd")?;
    let mut accounts = passwd.lines().filter_map(|row| {
        let fields: [&str; 7] = row.split(':').collect::<Vec<_>>().try_into().ok()?;
        (fields[2].parse().ok() == Some(uid)).then_some((fields[0], fields[3]))
    });
    let Some((name, primary)) = accounts.next() else {
        return Ok(Role::Status);
    };
    if name.is_empty()
        || accounts.next().is_some()
        || passwd
            .lines()
            .filter(|row| row.split(':').next() == Some(name))
            .count()
            != 1
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let primary = primary.parse().map_err(|_| io::ErrorKind::InvalidData)?;
    role_for_account(uid, name, primary)
}

pub(super) fn role_for_account(uid: u32, name: &str, primary: u32) -> io::Result<Role> {
    if uid == 0 {
        return Ok(Role::Administrator);
    }
    let database = fs::read_to_string("/etc/group")?;
    let memberships = linux_uapi::credentials::group_memberships(&database, name, primary)?;
    if memberships.iter().any(|(group, _)| *group == "netdev") {
        Ok(Role::Administrator)
    } else if memberships.iter().any(|(group, _)| *group == "netview") {
        Ok(Role::Viewer)
    } else {
        Ok(Role::Status)
    }
}

#[expect(
    clippy::result_large_err,
    reason = "tungstenite Callback 固定返回 HTTP 拒绝响应，仅在冷握手路径使用"
)]
pub(super) fn upgrade(
    request: &Request,
    mut response: Response,
) -> Result<Response, ErrorResponse> {
    if request.uri().path_and_query().map(|path| path.as_str()) != Some(management::RESOURCE)
        || request.headers().contains_key("Origin")
        || request
            .headers()
            .get(CONTROL_HEADER)
            .and_then(|value| value.to_str().ok())
            != Some(management::SUBPROTOCOL)
    {
        let mut error = ErrorResponse::new(None);
        *error.status_mut() = StatusCode::BAD_REQUEST;
        return Err(error);
    }
    response.headers_mut().insert(
        CONTROL_HEADER,
        HeaderValue::from_static(management::SUBPROTOCOL),
    );
    Ok(response)
}

fn serve(socket: UnixStream, role: Role) -> io::Result<()> {
    let session = tungstenite::accept_hdr_with_config(
        SessionIo::Unix(DeadlineStream::new(socket)),
        upgrade,
        Some(wire_config()),
    )
    .map_err(handshake_error)?;
    serve_session(session, role)
}

pub(super) fn serve_session(mut session: Session, role: Role) -> io::Result<()> {
    loop {
        session.get_mut().reset(RPC_TIMEOUT);
        let frame = receive(&mut session)?;
        let (operation, control) = frame
            .split_at_checked(bridge::HEADER_BYTES)
            .ok_or(io::ErrorKind::InvalidData)?;
        let operation = u32::from_le_bytes(operation.try_into().expect("fixed operation field"));
        if !role.permits(operation)
            || matches!(operation, management::CAPTURE | management::WATCH_FLOWS)
                && !matches!(session.get_ref(), SessionIo::Unix(_))
        {
            let _ = session.close(Some(CloseFrame {
                code: CloseCode::Policy,
                reason: "没有此网络管理权限".into(),
            }));
            return Ok(());
        }
        if operation == management::WATCH_FIREWALL_COUNTERS {
            return firewall::serve(&mut session, control);
        }
        if operation == management::WATCH_NEIGHBORS {
            return neighbors::serve(&mut session, control);
        }
        if operation == management::WATCH_ROUTES {
            return routes::serve(&mut session, control);
        }
        #[cfg(target_os = "linux")]
        if operation == management::CAPTURE {
            return super::capture::serve(&mut session, control);
        }
        #[cfg(target_os = "linux")]
        if operation == management::WATCH_LINK_TRAFFIC {
            return traffic::serve(&mut session, control);
        }
        #[cfg(target_os = "linux")]
        if operation == management::WATCH_FLOWS {
            return flows::serve(&mut session, control);
        }
        let memory = if ordinal::is_configuration(operation) {
            let metadata = administration::Request::decode(operation, control)
                .map_err(|_| io::ErrorKind::InvalidData)?;
            let length =
                usize::try_from(metadata.length).map_err(|_| io::ErrorKind::InvalidData)?;
            if length == 0 {
                None
            } else {
                Some(MemFd::from_writer(
                    "router-configuration",
                    length,
                    |file| receive_document(&mut session, length, |chunk| file.write_all(chunk)),
                )?)
            }
        } else {
            None
        };
        let reply = forward(operation, &frame, memory.as_ref())?;
        drop(memory);
        session.get_mut().reset(RPC_TIMEOUT);
        session
            .send(Message::Binary(reply.control.into()))
            .map_err(websocket_error)?;
        if let Some((file, length)) = reply.document {
            for offset in (0..length).step_by(FRAME_BYTES) {
                let mut chunk = vec![0; (length - offset).min(FRAME_BYTES)];
                file.read_exact_at(&mut chunk, offset as u64)?;
                session
                    .send(Message::Binary(chunk.into()))
                    .map_err(websocket_error)?;
            }
        }
    }
}

struct BackendReply {
    control: Vec<u8>,
    document: Option<(File, usize)>,
}

fn read_management_page(operation: u32, domain: Option<&str>, query: &[u8]) -> io::Result<Vec<u8>> {
    let metadata = administration::Request {
        length: query.len() as u64,
        domain,
        ..Default::default()
    };
    let control = metadata
        .encode(operation)
        .map_err(|_| io::ErrorKind::InvalidData)?;
    let mut frame = Vec::with_capacity(bridge::HEADER_BYTES + control.len());
    frame.extend_from_slice(&operation.to_le_bytes());
    frame.extend_from_slice(&control);
    let memory = MemFd::from_bytes("router-query", query)?;
    let reply = forward(operation, &frame, Some(&memory))?;
    success_control(operation, &reply.control)?;
    let (document, length) = reply.document.ok_or(io::ErrorKind::InvalidData)?;
    let mut bytes = vec![0; length];
    document.read_exact_at(&mut bytes, 0)?;
    Ok(bytes)
}

fn forward(operation: u32, frame: &[u8], input: Option<&MemFd>) -> io::Result<BackendReply> {
    let socket = anonymous_datagram()?;
    let path = if operation == ordinal::GET {
        bridge::STATUS_PATH
    } else {
        bridge::PATH
    };
    socket.connect(OsStr::from_bytes(path))?;
    socket.set_read_timeout(Some(RPC_TIMEOUT))?;
    socket.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let count = if let Some(memory) = input {
        send_fd(socket.as_fd(), frame, memory.as_fd())?
    } else {
        socket.send(frame)?
    };
    if count != frame.len() {
        return Err(io::ErrorKind::WriteZero.into());
    }
    let response_capacity = if ordinal::is_configuration(operation) {
        bridge::HEADER_BYTES + administration::Reply::BYTES
    } else {
        FRAME_BYTES
    };
    let mut response = vec![0; response_capacity];
    let (count, files) = recv_fds(socket.as_fd(), &mut response)?;
    response.truncate(count);
    let (status, _, length) = response_control(operation, &response)?;
    let document = if status == 0 && administration::Reply::has_document(operation) {
        let [file] = <[_; 1]>::try_from(files).map_err(|_| io::ErrorKind::InvalidData)?;
        let file = File::from(file);
        let info = file.metadata()?;
        if !info.is_file() || info.len() != length as u64 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Some((file, length))
    } else if files.is_empty() {
        None
    } else {
        return Err(io::ErrorKind::InvalidData.into());
    };
    Ok(BackendReply {
        control: response,
        document,
    })
}

/// Publishes the local WebSocket management endpoint and serves concurrent Unix sessions.
/// # Errors
/// An existing live listener, an invalid socket path, or a terminal accept error.
pub fn serve_router_api() -> io::Result<()> {
    let path = management::SOCKET_PATH;
    match fs::symlink_metadata(path) {
        Ok(info) if info.file_type().is_socket() && info.uid() == 0 => {
            match UnixStream::connect(path) {
                Ok(_) => return Err(io::ErrorKind::AddrInUse.into()),
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    fs::remove_file(path)?
                }
                Err(error) => return Err(error),
            }
        }
        Ok(_) => return Err(io::ErrorKind::AddrInUse.into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, Permissions::from_mode(0o666))?;
    if let Err(error) = super::tls::start_listener() {
        eprintln!("router-api: 远程监听未启动，{error}");
    }
    // Only this accept loop increases the count. Each worker holds one Arc until it leaves the session.
    let sessions: [Arc<()>; 3] = std::array::from_fn(|_| Arc::new(()));
    loop {
        let (socket, _) = match listener.accept() {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let Ok(role) = role(&socket) else { continue };
        let active = &sessions[role as usize];
        if Arc::strong_count(active) > role.connection_limit() {
            continue;
        }
        let session = Arc::clone(active);
        let _ = thread::Builder::new()
            .name("router-api-session".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let _session = session;
                let _ = serve(socket, role);
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_import_is_denied_before_document_or_native_forwarding() {
        let (client, server) = UnixStream::pair().unwrap();
        thread::scope(|scope| {
            let server = scope.spawn(move || serve(server, Role::Viewer));
            let mut session = connect_session(client).unwrap();
            let operation = ordinal::IMPORT;
            let metadata = administration::Request {
                generation: Some(router_protocol::Revision::initial(
                    core::num::NonZeroU128::MIN,
                )),
                commit: Some(administration::CommitId::from_random_bytes([1; 16])),
                length: 8,
                ..Default::default()
            };
            let mut frame = operation.to_le_bytes().to_vec();
            frame.extend_from_slice(&metadata.encode(operation).unwrap());
            session.send(Message::Binary(frame.into())).unwrap();
            assert_eq!(
                receive(&mut session).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            server.join().unwrap().unwrap();
        });
    }
}
