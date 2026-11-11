//! OPEN 接纳与方向能力；不持 TCP/RIB、配置文件或额外协商状态副本。
use core::net::Ipv4Addr;
use packet_wire::{
    PacketError,
    bgp::{
        self,
        open::{Capabilities, Capability, CapabilityValue, IPV4_VIA_IPV6, Open, Parameter},
    },
};

/// 从唯一配置 owner 投影的不可变会话参数；持久配置与授权入口由 router-config 拥有。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// 本连接实际使用的 AS；联盟外部使用公开 Identifier。
    pub local_as: u32,
    /// 配置联盟时本连接的边界；来自成员集合，不能用 OPEN AS 数值是否相同代替。
    pub confederation_peer: Option<crate::PeerKind>,
    pub peer_as: u32,
    pub identifier: Ipv4Addr,
    pub hold_seconds: u16,
    pub retry_seconds: u16,
    /// 只接收 TCP 连接，不主动发起；不能用重复失败主动连接模拟被动模式。
    pub passive: bool,
    pub ipv4_unicast: bool,
    pub ipv6_unicast: bool,
    pub route_refresh: bool,
    /// 只有完整 RFC 7606 处理已接入后，运行时才可启用并宣告扩展消息。
    pub extended_messages: bool,
}
/// 配置不能形成合法会话参数；没有执行副作用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidConfig;
/// 本次 TCP 实际发送的 OPEN 方式；邻居 owner 持有下次尝试方式，会话只冻结本次值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenOffer {
    /// 正常声明配置允许的能力。
    Capabilities,
    /// RFC 5492：对端拒绝可选参数后，不带参数重试基础 IPv4。
    BasicIpv4,
}
impl Config {
    /// 检查非零 AS/Identifier、合法 hold time 和非零重连周期；Identifier 按 RFC 6286 是整数，不是接口地址。
    /// # Errors
    /// AS/Identifier 为零、hold time 为 1/2 或重连周期为零。
    pub fn validate(self) -> Result<(), InvalidConfig> {
        if self.local_as == 0
            || self.peer_as == 0
            || self.identifier.is_unspecified()
            || matches!(self.hold_seconds, 1 | 2)
            || self.retry_seconds == 0
        {
            return Err(InvalidConfig);
        }
        Ok(())
    }
    /// 本次连接的可信关系；成员集合已经由配置 owner 判定并决定 local_as。
    pub fn peer_kind(self) -> crate::PeerKind {
        self.confederation_peer
            .unwrap_or(if self.local_as == self.peer_as {
                crate::PeerKind::Internal
            } else {
                crate::PeerKind::External
            })
    }
    /// 本地 OPEN 的两字节 My AS；不可映射的本地 AS 使用 AS_TRANS。
    pub fn wire_as(self) -> u16 {
        u16::try_from(self.local_as).unwrap_or(23456)
    }
    pub(super) fn supports_offer(self, offer: OpenOffer) -> bool {
        offer == OpenOffer::Capabilities || self.ipv4_unicast
    }
    /// 从参数和本次实际 offer 生成 OPEN；正常声明真实四字节 AS，基础 IPv4 重试不发送可选参数。
    /// # Errors
    /// 本地参数非法或最终输出容量不足；失败不修改输出，不增加另一份 capability 配置或堆缓冲。
    pub fn emit_open(self, offer: OpenOffer, bytes: &mut [u8]) -> Result<usize, PacketError> {
        self.validate().map_err(|_| PacketError::Malformed)?;
        if !self.supports_offer(offer) {
            return Err(PacketError::Malformed);
        }
        if offer == OpenOffer::BasicIpv4 {
            return Open::emit(
                bytes,
                self.wire_as(),
                self.hold_seconds,
                self.identifier,
                &[],
            );
        }
        let asn = self.local_as.to_be_bytes();
        let mut capabilities = [Capability {
            code: 65,
            data: &asn,
        }; 7];
        let mut count = 1;
        for capability in Capabilities::unicast(self.ipv4_unicast, self.ipv6_unicast) {
            capabilities[count] = capability;
            count += 1;
        }
        for (enabled, capability) in [
            (self.ipv4_unicast, IPV4_VIA_IPV6),
            (self.route_refresh, Capability { code: 2, data: &[] }),
            (
                self.route_refresh,
                Capability {
                    code: 70,
                    data: &[],
                },
            ),
            (self.extended_messages, Capability { code: 6, data: &[] }),
        ] {
            if enabled {
                capabilities[count] = capability;
                count += 1;
            }
        }
        // AS4 六字节、两个 MP 各六字节、Extended Next Hop 八字节，其他三个能力各两字节。
        let mut content = [0; 32];
        let length = Capabilities::emit(&mut content, &capabilities[..count])?;
        Open::emit(
            bytes,
            self.wire_as(),
            self.hold_seconds,
            self.identifier,
            &[Parameter {
                kind: 2,
                data: &content[..length],
            }],
        )
    }
}
/// 待发送的标准 NOTIFICATION；原始错误数据只借用本次输入，发送者必须在输入退休前消费或捕获。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Notice<'a> {
    pub code: u8,
    pub subcode: u8,
    pub data: &'a [u8],
}
impl<'a> Notice<'a> {
    pub(super) const fn simple(code: u8, subcode: u8) -> Self {
        Self {
            code,
            subcode,
            data: &[],
        }
    }
    /// 复用唯一帧 codec，直接写最终输出；不在协议状态 owner 重建消息布局。
    /// # Errors
    /// 容量或方向已授权上限不足，不截断错误数据。
    pub fn emit(self, bytes: &mut [u8], extended: bool) -> Result<usize, PacketError> {
        bgp::emit_notification(bytes, self.code, self.subcode, self.data, extended)
    }
}
/// 同一次 OPEN 接纳的冻结值；由 Session 唯一保存，不由 TCP actor 另存协商副本。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Negotiated {
    pub peer_as: u32,
    pub peer_identifier: Ipv4Addr,
    pub hold_seconds: u16,
    pub ipv4_unicast: bool,
    pub ipv6_unicast: bool,
    /// 对端允许 IPv4 单播携 IPv6 下一跳，且双方协商了 IPv4 MP。
    pub send_ipv4_via_ipv6: bool,
    /// 本端已声明接收 IPv6 下一跳，且双方协商了 IPv4 MP。
    pub receive_ipv4_via_ipv6: bool,
    pub four_octet_asn: bool,
    pub send_extended: bool,
    pub receive_extended: bool,
    pub send_refresh: bool,
    pub receive_refresh: bool,
    /// 双方交换能力 70 后才使用刷新边界并在接收端清除陈旧路由。
    pub enhanced_refresh: bool,
}
/// 接纳对端 OPEN；未知 capability 忽略，未知可选参数按原 wire 返回错误，重复相同 AS4 值可接纳。
/// # Errors
/// 版本、AS/Identifier/hold time、参数/capability 内容非法，返回应发送的 OPEN Error；无网络副作用。
pub(super) fn negotiate<'a>(
    config: Config,
    offer: OpenOffer,
    open: Open<'a>,
) -> Result<Negotiated, Notice<'a>> {
    if open.version() != 4 {
        return Err(Notice {
            code: 2,
            subcode: 1,
            data: &[0, 4],
        });
    }
    if matches!(open.hold_time(), 1 | 2) {
        return Err(Notice::simple(2, 6));
    }
    let mut asn = None;
    let mut mp_seen = false;
    let mut v4 = false;
    let mut v6 = false;
    let mut ipv4_via_ipv6 = false;
    let mut refresh = false;
    let mut enhanced_refresh = false;
    let mut extended = false;
    for (parameter, wire) in open.parameters().wire_parameters() {
        if parameter.kind != 2 {
            return Err(Notice {
                code: 2,
                subcode: 4,
                data: wire,
            });
        }
        let capabilities = parameter.capabilities().map_err(|_| Notice::simple(2, 0))?;
        for capability in capabilities {
            match capability.decode().map_err(|_| Notice::simple(2, 0))? {
                CapabilityValue::Multiprotocol { afi, safi } => {
                    mp_seen = true;
                    v4 |= afi == 1 && safi == 1;
                    v6 |= afi == 2 && safi == 1;
                }
                CapabilityValue::FourOctetAsn(value) => {
                    if value == 0 || asn.is_some_and(|previous| previous != value) {
                        return Err(Notice::simple(2, 2));
                    }
                    asn = Some(value);
                }
                CapabilityValue::ExtendedNextHop {
                    ipv4_via_ipv6: supported,
                } => {
                    ipv4_via_ipv6 |= supported;
                }
                CapabilityValue::RouteRefresh => refresh = true,
                CapabilityValue::EnhancedRouteRefresh => enhanced_refresh = true,
                CapabilityValue::ExtendedMessage => extended = true,
                CapabilityValue::Unknown(_) => {}
            }
        }
    }
    // RFC 6793：双方支持时，四字节 capability 中的 AS 取代 My AS，不能按后者覆盖或猜测截断值。
    let peer_as = asn.unwrap_or_else(|| u32::from(open.asn()));
    if peer_as != config.peer_as {
        return Err(Notice::simple(2, 2));
    }
    let id = open.identifier();
    if id.is_unspecified() || config.local_as == peer_as && id == config.identifier {
        return Err(Notice::simple(2, 3));
    }
    let capabilities = offer == OpenOffer::Capabilities;
    let ipv4_unicast = config.ipv4_unicast && (v4 || !mp_seen);
    let ipv6_unicast = capabilities && config.ipv6_unicast && v6;
    // 不能把没有共同地址族的连接报告为成功；通知只列本地方向确需的 MP 能力。
    if (config.ipv4_unicast || config.ipv6_unicast) && !ipv4_unicast && !ipv6_unicast {
        return Err(Notice {
            code: 2,
            subcode: 7,
            data: Capabilities::unicast(config.ipv4_unicast, capabilities && config.ipv6_unicast)
                .bytes(),
        });
    }
    Ok(Negotiated {
        peer_as,
        peer_identifier: id,
        hold_seconds: config.hold_seconds.min(open.hold_time()),
        ipv4_unicast,
        ipv6_unicast,
        send_ipv4_via_ipv6: capabilities && config.ipv4_unicast && v4 && ipv4_via_ipv6,
        receive_ipv4_via_ipv6: capabilities && config.ipv4_unicast && v4,
        four_octet_asn: capabilities && asn.is_some(),
        send_extended: capabilities && config.extended_messages && extended,
        receive_extended: capabilities && config.extended_messages,
        send_refresh: capabilities && refresh,
        receive_refresh: capabilities && config.route_refresh,
        enhanced_refresh: capabilities && config.route_refresh && enhanced_refresh,
    })
}
/// 两条尚未建立的反向连接发生碰撞时，保留较大 Identifier 发起的连接；ID 相等的 eBGP 按较大实际 AS 决定。
/// 已有 Established 的连接应由 peer owner 优先保留，不调用本函数替换；同 AS/ID 在 OPEN 接纳时已拒绝。
pub fn prefer_outgoing(config: Config, peer: Negotiated) -> bool {
    (u32::from(config.identifier), config.local_as)
        > (u32::from(peer.peer_identifier), peer.peer_as)
}
