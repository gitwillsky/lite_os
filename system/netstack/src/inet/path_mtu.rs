//! RFC 1191 / RFC 8201 的每路径 MTU 例外；属于 NET 的 IP 层，同路径各端口共享，不在 socket 复制学习值。
use core::net::IpAddr;
use fallible_tree::FallibleMap;
use network_protocol::SocketError;

/// 出接口区分链路 zone；源地址保留源相关路径身份，端口不属于 IP 路径。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::inet) struct Path {
    pub(in crate::inet) interface: u32,
    pub(in crate::inet) source: IpAddr,
    pub(in crate::inet) destination: IpAddr,
}
struct Estimate {
    mtu: u32,
    expires: u64,
}
// RFC 8201 §4 建议十分钟后再探测增长；采用 Linux IPv6 dst 的 1024 项压力量级。
// 无界远端目的会消耗 NET 堆，因此只在合法 PTB 时懒分配，满表先清理过期项，不逐出活跃估计来放大发包。
const LIFETIME_MS: u64 = 600_000;
const CAPACITY: usize = 1024;

pub(in crate::inet) struct PathMtu(FallibleMap<Path, Estimate>);
impl PathMtu {
    pub(in crate::inet) fn new() -> Self {
        Self(FallibleMap::new())
    }
    /// 返回当前有效路径与实际出口上限的较小值；查询不延长寿命或分配。
    pub(in crate::inet) fn get(&self, path: Path, link_mtu: usize, now: u64) -> usize {
        self.0
            .get(&path)
            .filter(|value| now < value.expires)
            .map_or(link_mtu, |value| link_mtu.min(value.mtu as usize))
    }
    /// 已验证的反馈只降低估计，受各族最小链路 MTU 约束。失败不发布半个估计。
    pub(in crate::inet) fn learn(
        &mut self,
        path: Path,
        mtu: u32,
        now: u64,
    ) -> Result<(), SocketError> {
        if path.source.is_ipv4() != path.destination.is_ipv4()
            || mtu < if path.source.is_ipv4() { 68 } else { 1280 }
        {
            return Err(SocketError::Invalid);
        }
        if let Some(value) = self.0.get_mut(&path) {
            if now >= value.expires || mtu <= value.mtu {
                value.mtu = mtu;
                value.expires = now.saturating_add(LIFETIME_MS);
            }
            return Ok(());
        }
        if self.0.len() >= CAPACITY {
            self.0.retain(|_, value| now < value.expires);
            if self.0.len() >= CAPACITY {
                return Err(SocketError::NoBuffer);
            }
        }
        self.0
            .try_insert(
                path,
                Estimate {
                    mtu,
                    expires: now.saturating_add(LIFETIME_MS),
                },
            )
            .map_err(|_| SocketError::NoMemory)?;
        Ok(())
    }
    /// 地址失去使用资格或接口退休时同时退休该路径；由接口事实 owner 在变化通知前调用。
    pub(in crate::inet) fn retain(&mut self, mut active: impl FnMut(Path) -> bool) {
        self.0.retain(|path, _| active(*path));
    }
}
