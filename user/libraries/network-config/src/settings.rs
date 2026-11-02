//! `/dev/network-settings` 的客户端：一次请求一个 datagram，回复带状态码。
use netcfg_protocol::{Decoder, Encoder, InterfaceConfig, ordinal};
use std::{io, time::Duration};

pub use netcfg_protocol::Snapshot;

/// 普通请求的超时；WAIT 不设超时。
const TIMEOUT: Duration = Duration::from_secs(5);

fn request(ordinal: u32, payload: &[u8], timeout: Option<Duration>) -> io::Result<Vec<u8>> {
    #[cfg(target_os = "linux")]
    {
        use netcfg_protocol::bridge;
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
        // 回复发往自动绑定的 abstract 源地址；端点本身的访问由其 inode 权限（netdev 组）决定。
        let socket = linux_uapi::unix::anonymous_datagram()?;
        socket.set_read_timeout(timeout)?;
        socket.set_write_timeout(Some(TIMEOUT))?;
        socket
            .connect(OsStr::from_bytes(bridge::PATH))
            .map_err(|error| {
                if error.kind() == io::ErrorKind::PermissionDenied {
                    io::Error::new(error.kind(), "需要 netdev 组成员身份才能读取或修改网络设置")
                } else {
                    error
                }
            })?;
        let mut message = Vec::with_capacity(bridge::HEADER_BYTES + payload.len());
        message.extend_from_slice(&ordinal.to_le_bytes());
        message.extend_from_slice(payload);
        socket.send(&message)?;
        let mut reply = vec![0; bridge::DATAGRAM_MAX];
        let count = socket.recv(&mut reply)?;
        reply.truncate(count);
        if reply.len() < bridge::HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "网络设置回复无效",
            ));
        }
        let status = i32::from_le_bytes(reply[..4].try_into().expect("four status bytes"));
        if status != 0 {
            return Err(io::Error::other(format!(
                "网络设置服务拒绝请求（状态 {status}）"
            )));
        }
        reply.drain(..bridge::HEADER_BYTES);
        Ok(reply)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (ordinal, payload, timeout);
        Err(io::ErrorKind::Unsupported.into())
    }
}

fn invalid(_: netcfg_protocol::Invalid) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "网络设置回复无效")
}

/// 当前设置快照。
///
/// # Errors
///
/// 端点不可用、没有权限或回复无效。
pub fn snapshot() -> io::Result<Snapshot> {
    let reply = request(ordinal::GET, &[], Some(TIMEOUT))?;
    let mut decoder = Decoder::new(&reply);
    let snapshot = decoder.snapshot().map_err(invalid)?;
    decoder.finish().map_err(invalid)?;
    Ok(snapshot)
}

/// 阻塞到设置代际超过 `generation`，返回新的代际。
///
/// # Errors
///
/// 端点不可用、没有权限或在途等待过多。
pub fn wait(generation: u64) -> io::Result<u64> {
    let reply = request(ordinal::WAIT, &generation.to_le_bytes(), None)?;
    reply
        .try_into()
        .map(u64::from_le_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "网络设置回复无效"))
}

pub(crate) fn set(name: &[u8], config: &InterfaceConfig) -> io::Result<()> {
    let mut encoder = Encoder::default();
    encoder.bytes(name).config(config);
    request(ordinal::SET, &encoder.0, Some(TIMEOUT)).map(drop)
}
