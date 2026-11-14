//! 系统设置的管理网关客户端。
use super::ManagementClient;
use router_protocol::{Decoder, DhcpAction, Encoder, Revision, Update, bridge, ordinal};
use std::{io, time::Duration};

pub use router_protocol::Snapshot;

/// 普通请求的超时；等待请求使用比服务端更长的单次时限。
const TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const WATCH_TIMEOUT: Duration = Duration::from_secs(bridge::WATCH_SECONDS + 5);

fn invalid(_: router_protocol::Invalid) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "网络设置回复无效")
}

/// 当前设置快照。
///
/// # Errors
///
/// 端点不可用、没有权限或回复无效。
pub fn snapshot() -> io::Result<Snapshot> {
    ManagementClient::local().snapshot()
}

/// 等待本机设置代际变化。
/// # Errors
/// 本地管理端点不可用或回复无效。
pub fn wait(generation: Revision) -> io::Result<Revision> {
    ManagementClient::local().wait(generation)
}

/// 请求本机 DHCP 续租或释放。
/// # Errors
/// 接口没有 DHCP 会话、等待超时、无权限或服务不可用。
pub fn dhcp(name: &[u8], action: DhcpAction) -> io::Result<()> {
    ManagementClient::local().dhcp(name, action)
}

impl ManagementClient {
    pub(super) fn settings_request(
        &mut self,
        ordinal: u32,
        payload: &[u8],
        timeout: Duration,
    ) -> io::Result<Vec<u8>> {
        self.request(ordinal, payload, None, timeout)
            .map(|(reply, _)| reply)
    }

    /// 读取当前设置快照。
    /// # Errors
    /// 连接不可用、无权限或回复无效。
    pub fn snapshot(&mut self) -> io::Result<Snapshot> {
        let reply = self.settings_request(ordinal::GET, &[], TIMEOUT)?;
        let mut decoder = Decoder::new(&reply);
        let snapshot = decoder.snapshot().map_err(invalid)?;
        decoder.finish().map_err(invalid)?;
        Ok(snapshot)
    }

    /// 阻塞到设置代际改变，返回当前代际。服务端等待到期而代际未变时继续等待。
    ///
    /// # Errors
    ///
    /// 端点不可用、没有权限或在途等待过多。
    pub fn wait(&mut self, generation: Revision) -> io::Result<Revision> {
        let bytes = generation.to_bytes();
        loop {
            let reply = self.settings_request(ordinal::WAIT, &bytes, WATCH_TIMEOUT)?;
            let current = Revision::from_bytes(&reply).map_err(invalid)?;
            if current != generation {
                return Ok(current);
            }
        }
    }

    /// 在指定接口续租或释放 DHCP，等待执行结果。
    /// # Errors
    /// 接口无 DHCP 会话、等待超时、无权限或服务不可用。
    pub fn dhcp(&mut self, name: &[u8], action: DhcpAction) -> io::Result<()> {
        let mut encoder = Encoder::default();
        encoder.bytes(name).u8(action as u8);
        self.settings_request(ordinal::DHCP, &encoder.0, Duration::from_secs(17))
            .map(drop)
    }
}

pub(crate) fn set(update: Update<'_>) -> io::Result<()> {
    let mut encoder = Encoder::default();
    encoder.update(&update);
    ManagementClient::local()
        .settings_request(ordinal::SET, &encoder.0, TIMEOUT)
        .map(drop)
}

pub(crate) fn rename(interface: &str, name: &str) -> io::Result<()> {
    let mut encoder = Encoder::default();
    encoder.bytes(interface.as_bytes()).bytes(name.as_bytes());
    ManagementClient::local()
        .settings_request(ordinal::RENAME, &encoder.0, TIMEOUT)
        .map(drop)
}
