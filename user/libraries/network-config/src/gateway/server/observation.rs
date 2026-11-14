//! 管理网关的采样会话，共用期限、保活和断线处理。

use super::*;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

pub(super) fn page<T>(
    session: &mut Session,
    operation: u32,
    domain: Option<&str>,
    query: &[u8],
    decode: impl Fn(&[u8]) -> Result<T, router_protocol::Invalid>,
    encode: impl Fn(&T, Option<&T>) -> Result<Option<Vec<u8>>, router_protocol::Invalid>,
    failure: &'static str,
) -> io::Result<()> {
    let mut previous = None;
    let result = stream(session, || {
        let bytes = read_management_page(operation, domain, query)?;
        let current = decode(&bytes).map_err(|_| io::ErrorKind::InvalidData)?;
        let update = encode(&current, previous.as_ref()).map_err(|_| io::ErrorKind::InvalidData)?;
        previous = Some(current);
        Ok(update)
    });
    report_failure(session, result, failure)
}

pub(super) fn report_failure(
    session: &mut Session,
    result: io::Result<()>,
    reason: &'static str,
) -> io::Result<()> {
    if result.is_err() {
        session.get_mut().reset(HANDSHAKE_TIMEOUT);
        let _ = session.close(Some(CloseFrame {
            code: CloseCode::Error,
            reason: reason.into(),
        }));
    }
    result
}

pub(super) fn stream(
    session: &mut Session,
    mut sample: impl FnMut() -> io::Result<Option<Vec<u8>>>,
) -> io::Result<()> {
    let mut sent_at = Instant::now();
    loop {
        let update = sample()?;
        let heartbeat = (sent_at.elapsed() >= HEARTBEAT_INTERVAL)
            .then_some([management::OBSERVATION_HEARTBEAT].as_slice());
        if let Some(body) = update.as_deref().or(heartbeat) {
            send_update(session, body)?;
            sent_at = Instant::now();
        }
        // 同一个阻塞读既计时也侦测对端离开，无需新线程或每包通知。
        session.get_mut().reset(SAMPLE_INTERVAL);
        match receive(session) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
            Ok(_) => return Err(io::ErrorKind::InvalidData.into()),
        }
    }
}

/// 用观察会话的状态头和发送期限发布一条聚合更新。
/// # Errors
/// 传输失败或订阅者已关闭。
pub(super) fn send_update(session: &mut Session, body: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(bridge::HEADER_BYTES + body.len());
    frame.extend_from_slice(&0i32.to_le_bytes());
    frame.extend_from_slice(body);
    session.get_mut().reset(RPC_TIMEOUT);
    session
        .send(Message::Binary(frame.into()))
        .map_err(websocket_error)
}
