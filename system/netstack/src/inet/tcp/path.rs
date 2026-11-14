//! 本次 TCP 操作的冻结路径/时钟；不保存接口、邻居或配置副本。
use super::*;
use core::net::IpAddr;
use hmac::Mac;
pub(in crate::inet) type SequenceKey = hmac::Hmac<sha2::Sha256>;
pub(in crate::inet) fn sequence_key() -> SequenceKey {
    let mut key = [0; 32];
    native_runtime::system::cprng_draw(&mut key);
    SequenceKey::new_from_slice(&key).expect("HMAC-SHA256 accepts any key length")
}
pub(super) struct Path<'a> {
    now: transport::time::Instant,
    logical: u32,
    route: Option<(usize, network_protocol::RouteMetrics)>,
    source: Option<IpAddr>,
    secret: &'a SequenceKey,
}
impl<'a> Path<'a> {
    pub(super) fn new(
        timestamp: Instant,
        logical: u32,
        route: Option<(usize, network_protocol::RouteMetrics)>,
        source: Option<IpAddr>,
        secret: &'a SequenceKey,
    ) -> Self {
        Self {
            now: timestamp,
            logical,
            route,
            source,
            secret,
        }
    }

    pub(super) fn with_route(
        &self,
        route: Option<(usize, network_protocol::RouteMetrics)>,
    ) -> Self {
        Self { route, ..*self }
    }
}
impl transport::Context for Path<'_> {
    fn now(&self) -> transport::time::Instant {
        self.now
    }
    fn ip_mtu(&self) -> usize {
        self.route.map_or(0, |route| route.0)
    }
    fn interface_index(&self) -> u32 {
        self.logical
    }
    fn transport_route(&self) -> Option<transport::TransportRoute> {
        self.route.map(|route| transport::TransportRoute {
            advertised_mss: if route.1.advertised_mss == 0 {
                route
                    .0
                    .saturating_sub(if self.source.is_some_and(|ip| ip.is_ipv6()) {
                        60
                    } else {
                        40
                    })
            } else {
                route.1.advertised_mss as usize
            },
            receive_window: route.1.receive_window as usize,
            initial_rtt: route.1.round_trip_time / 8,
        })
    }
    fn source_address(&self, destination: IpAddr) -> Option<IpAddr> {
        self.source
            .filter(|source| source.is_ipv4() == destination.is_ipv4())
    }
    fn initial_sequence(&mut self, local: IpEndpoint, remote: IpEndpoint) -> transport::Sequence {
        // RFC 9293/RFC 6528: 同一 NET 的秘密 PRF 保持 tuple 偏移，4-us 单调时钟提供进度。
        // 秘密只属于 NET，按操作克隆已展开的 HMAC 状态；没有全局 key、逐包随机调用或数据面分配。
        let mut mac = self.secret.clone();
        mac.update(b"liteos.tcp.isn\0");
        for endpoint in [local, remote] {
            match endpoint.addr {
                IpAddr::V4(address) => {
                    mac.update(&[4]);
                    mac.update(&address.octets());
                }
                IpAddr::V6(address) => {
                    mac.update(&[6]);
                    mac.update(&address.octets());
                }
            }
            mac.update(&endpoint.port.to_be_bytes());
        }
        let value = mac.finalize().into_bytes();
        let offset = u32::from_be_bytes(value[..4].try_into().expect("SHA256 has 32 bytes"));
        transport::Sequence(
            offset.wrapping_add(((self.now.total_micros() as u64) / 4) as u32) as i32,
        )
    }
}
