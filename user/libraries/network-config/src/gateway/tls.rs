//! TLS 远程管理。账户验证在升级为管理会话之前完成，原生操作走同一分派。

use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use linux_uapi::credentials::account::{Purpose, authenticate};
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject},
};
use std::{
    collections::BTreeMap,
    fs::File,
    net::{IpAddr, SocketAddr, TcpListener, ToSocketAddrs},
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
};
use tungstenite::{
    handshake::server::{ErrorResponse, Request, Response},
    http::{HeaderValue, StatusCode, header},
};
use zeroize::Zeroizing;

const CONFIG_PATH: &str = "/etc/router-api.conf";
const MAX_CREDENTIAL_BYTES: usize = 1024;
const HANDSHAKES: usize = 4;
const REMOTE_SESSIONS: usize = 16;
const MAX_TRACKED_ADDRESSES: usize = 256;
const FAILED_ATTEMPTS: usize = 5;
const ATTEMPT_WINDOW: Duration = Duration::from_secs(60);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ListenerConfiguration {
    listen: SocketAddr,
    certificate: PathBuf,
    private_key: PathBuf,
}

fn trusted_file(path: &Path, private: bool) -> io::Result<File> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    let forbidden = if private { 0o077 } else { 0o022 };
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & forbidden != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("文件属主或权限无效，{}", path.display()),
        ));
    }
    Ok(file)
}

fn server_config(certificate: File, private_key: File) -> io::Result<ServerConfig> {
    let certificates = CertificateDer::pem_reader_iter(certificate)
        .collect::<Result<Vec<_>, _>>()
        .map_err(io::Error::other)?;
    let key = PrivateKeyDer::from_pem_reader(private_key).map_err(io::Error::other)?;
    ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(io::Error::other)
}

pub(super) fn start_listener() -> io::Result<()> {
    let file = match trusted_file(Path::new(CONFIG_PATH), false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let settings: ListenerConfiguration =
        serde_json::from_reader(io::BufReader::new(file)).map_err(io::Error::other)?;
    if !settings.certificate.is_absolute() || !settings.private_key.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "证书与私钥须使用绝对路径",
        ));
    }
    let config = server_config(
        trusted_file(&settings.certificate, false)?,
        trusted_file(&settings.private_key, true)?,
    )?;
    thread::Builder::new()
        .name("router-api-tls".into())
        .spawn(move || {
            if let Err(error) =
                bind(settings.listen).and_then(|listener| listen(listener, Arc::new(config)))
            {
                eprintln!("router-api: 远程监听退出，{error}");
            }
        })?;
    Ok(())
}

fn bind(address: SocketAddr) -> io::Result<TcpListener> {
    let mut retry = Duration::from_millis(250);
    loop {
        match TcpListener::bind(address) {
            // init 可早于 DHCP 或 IPv6 DAD 完成。只在指定地址缺席时等待，绑定后没有此定时工作。
            Err(error) if error.kind() == io::ErrorKind::AddrNotAvailable => {
                thread::sleep(retry);
                retry = (retry * 2).min(Duration::from_secs(5));
            }
            result => return result,
        }
    }
}

struct Attempts {
    until: Instant,
    count: usize,
}

// 仅记录有界时间窗内远程口令尝试，锁外做账户 I/O 与散列。丢失此状态会允许快速反复猜测口令。
type AttemptLog = Mutex<BTreeMap<IpAddr, Attempts>>;

fn take_attempt(log: &AttemptLog, address: IpAddr) -> Option<Instant> {
    let mut attempts = log.lock().ok()?;
    let now = Instant::now();
    attempts.retain(|_, entry| entry.until > now);
    if attempts.len() >= MAX_TRACKED_ADDRESSES && !attempts.contains_key(&address) {
        return None;
    }
    let entry = attempts.entry(address).or_insert(Attempts {
        until: now + ATTEMPT_WINDOW,
        count: 0,
    });
    if entry.count == FAILED_ATTEMPTS {
        return None;
    }
    entry.count += 1;
    Some(entry.until)
}

fn return_attempt(log: &AttemptLog, address: IpAddr, window: Instant) {
    if let Ok(mut attempts) = log.lock()
        && let Some(entry) = attempts.get_mut(&address)
        && entry.until == window
    {
        entry.count = entry.count.saturating_sub(1);
        if entry.count == 0 {
            attempts.remove(&address);
        }
    }
}

fn login(request: &Request) -> Option<server::Role> {
    let values = request.headers().get_all(header::AUTHORIZATION);
    let mut values = values.iter();
    let value = values.next()?.to_str().ok()?;
    let (scheme, secret) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Basic")
        || secret.len() > 2 * MAX_CREDENTIAL_BYTES
        || values.next().is_some()
    {
        return None;
    }
    let bytes = Zeroizing::new(STANDARD.decode(secret).ok()?);
    if bytes.len() > MAX_CREDENTIAL_BYTES {
        return None;
    }
    let (name, password) = std::str::from_utf8(&bytes).ok()?.split_once(':')?;
    if password.is_empty() {
        return None;
    }
    let account = authenticate(name, password, Purpose::Remote).ok()??;
    let role =
        server::role_for_account(account.identity.uid, &account.name, account.identity.gid).ok()?;
    (!matches!(role, server::Role::Status)).then_some(role)
}

#[expect(
    clippy::result_large_err,
    reason = "tungstenite 的握手回调固定返回完整 HTTP 拒绝响应"
)]
fn serve_tls(
    socket: TcpStream,
    config: Arc<ServerConfig>,
    log: &AttemptLog,
    address: IpAddr,
    handshake: Arc<()>,
) -> io::Result<()> {
    let connection = ServerConnection::new(config).map_err(io::Error::other)?;
    let stream = SessionIo::Server(Box::new(StreamOwned::new(
        connection,
        DeadlineStream::new(socket),
    )));
    let mut role = None;
    let session = tungstenite::accept_hdr_with_config(
        stream,
        |request: &Request, response: Response| {
            let response = server::upgrade(request, response)?;
            let Some(window) = take_attempt(log, address) else {
                let mut error = ErrorResponse::new(None);
                *error.status_mut() = StatusCode::TOO_MANY_REQUESTS;
                return Err(error);
            };
            role = login(request);
            if role.is_none() {
                let mut error = ErrorResponse::new(None);
                *error.status_mut() = StatusCode::UNAUTHORIZED;
                error.headers_mut().insert(
                    header::WWW_AUTHENTICATE,
                    HeaderValue::from_static("Basic realm=\"LiteOS Router\", charset=\"UTF-8\""),
                );
                return Err(error);
            }
            return_attempt(log, address, window);
            Ok(response)
        },
        Some(wire_config()),
    )
    .map_err(handshake_error)?;
    drop(handshake);
    server::serve_session(session, role.ok_or(io::ErrorKind::PermissionDenied)?)
}

fn listen(listener: TcpListener, config: Arc<ServerConfig>) -> io::Result<()> {
    // 只有此 accept loop 增加计数。远程未认证连接与本地管理使用独立名额。
    let sessions = Arc::new(());
    let handshakes = Arc::new(());
    let attempts = Arc::new(AttemptLog::default());
    loop {
        let (socket, address) = match listener.accept() {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if Arc::strong_count(&sessions) > REMOTE_SESSIONS
            || Arc::strong_count(&handshakes) > HANDSHAKES
        {
            continue;
        }
        let (session, handshake) = (sessions.clone(), handshakes.clone());
        let (config, attempts) = (config.clone(), attempts.clone());
        let _ = thread::Builder::new()
            .name("router-api-remote".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                let _session = session;
                let _ = serve_tls(
                    socket,
                    config,
                    &attempts,
                    address.ip().to_canonical(),
                    handshake,
                );
            });
    }
}

pub(super) fn connect(url: &str, user: &str, password: &str, ca: &Path) -> io::Result<Session> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "远程管理须使用 wss://主机[:端口]/router 与有效账号",
        )
    };
    let mut request = websocket_request(url)?;
    let uri = request.uri();
    if uri.scheme_str() != Some("wss")
        || uri.path_and_query().map(|path| path.as_str()) != Some(management::RESOURCE)
        || uri
            .authority()
            .is_none_or(|authority| authority.as_str().contains('@'))
        || user.is_empty()
        || user.contains(':')
        || password.is_empty()
        || user.len().saturating_add(password.len()) >= MAX_CREDENTIAL_BYTES
    {
        return Err(invalid());
    }
    let host = uri.host().ok_or_else(invalid)?.trim_matches(['[', ']']);
    let name = ServerName::try_from(host.to_owned()).map_err(|_| invalid())?;
    let port = uri.port_u16().unwrap_or(443);
    let mut roots = RootCertStore::empty();
    for certificate in CertificateDer::pem_file_iter(ca).map_err(io::Error::other)? {
        roots
            .add(certificate.map_err(io::Error::other)?)
            .map_err(io::Error::other)?;
    }
    if roots.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CA 文件未包含证书",
        ));
    }
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(io::Error::other)?
            .with_root_certificates(roots)
            .with_no_client_auth();
    let mut socket = Err(io::ErrorKind::AddrNotAvailable.into());
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    for address in (host, port).to_socket_addrs()? {
        let timeout = deadline
            .checked_duration_since(Instant::now())
            .filter(|value| !value.is_zero())
            .ok_or(io::ErrorKind::TimedOut)?;
        socket = TcpStream::connect_timeout(&address, timeout);
        if socket.is_ok() {
            break;
        }
    }
    let mut stream = DeadlineStream::new(socket?);
    let mut connection = ClientConnection::new(Arc::new(config), name).map_err(io::Error::other)?;
    // 明文认证材料只在已校验证书和主机名的 TLS 连接内写入。
    connection.complete_io(&mut stream)?;
    stream.reset(HANDSHAKE_TIMEOUT);
    let credentials = Zeroizing::new(format!("{user}:{password}"));
    let encoded = Zeroizing::new(STANDARD.encode(credentials.as_bytes()));
    let authorization = Zeroizing::new(format!("Basic {}", encoded.as_str()));
    let mut value = HeaderValue::from_str(&authorization).map_err(|_| invalid())?;
    value.set_sensitive(true);
    request.headers_mut().insert(header::AUTHORIZATION, value);
    client_upgrade(
        request,
        SessionIo::Client(Box::new(StreamOwned::new(connection, stream))),
    )
}
