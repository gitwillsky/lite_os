//! OPEN 接纳与方向能力；不持 TCP/RIB、配置文件或额外协商状态副本。
use core::net::Ipv4Addr;
use packet_wire::{
    PacketError,
    bgp::{
        self,
        open::{Capabilities, Capability, CapabilityValue, Open, Parameter},
    },
};

/// 从唯一配置 owner 投影的会话参数；服务尚未装配前不构成对外配置入口。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    pub local_as: u32,
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
    /// 本地 OPEN 的两字节 My AS；四字节实际 AS 必须同时由 capability 65 携带。
    pub fn wire_as(self) -> u16 {
        u16::try_from(self.local_as).unwrap_or(23456)
    }
    /// 从本连接唯一参数生成 OPEN，始终携带真实四字节 AS；只宣告本地启用的能力。
    /// # Errors
    /// 本地参数非法或最终输出容量不足；失败不修改输出，不增加另一份 capability 配置或堆缓冲。
    pub fn emit_open(self, bytes: &mut [u8]) -> Result<usize, PacketError> {
        self.validate().map_err(|_| PacketError::Malformed)?;
        let asn = self.local_as.to_be_bytes();
        let mut capabilities = [Capability {
            code: 65,
            data: &asn,
        }; 5];
        let mut count = 1;
        for (enabled, capability) in [
            (
                self.ipv4_unicast,
                Capability {
                    code: 1,
                    data: &[0, 1, 0, 1],
                },
            ),
            (
                self.ipv6_unicast,
                Capability {
                    code: 1,
                    data: &[0, 2, 0, 1],
                },
            ),
            (self.route_refresh, Capability { code: 2, data: &[] }),
            (self.extended_messages, Capability { code: 6, data: &[] }),
        ] {
            if enabled {
                capabilities[count] = capability;
                count += 1;
            }
        }
        // AS4 六字节、两个 MP 各六字节、RR/Extended 各两字节；固定上界，不按 MTU/最大消息分配。
        let mut content = [0; 22];
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
    pub four_octet_asn: bool,
    pub send_extended: bool,
    pub receive_extended: bool,
    pub send_refresh: bool,
    pub receive_refresh: bool,
}
/// 接纳对端 OPEN；未知 capability 忽略，未知可选参数按原 wire 返回错误，重复相同 AS4 值可接纳。
/// # Errors
/// 版本、AS/Identifier/hold time、参数/capability 内容非法，返回应发送的 OPEN Error；无网络副作用。
pub(super) fn negotiate<'a>(config: Config, open: Open<'a>) -> Result<Negotiated, Notice<'a>> {
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
    let mut refresh = false;
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
                CapabilityValue::RouteRefresh => refresh = true,
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
    Ok(Negotiated {
        peer_as,
        peer_identifier: id,
        hold_seconds: config.hold_seconds.min(open.hold_time()),
        ipv4_unicast: config.ipv4_unicast && (v4 || !mp_seen),
        ipv6_unicast: config.ipv6_unicast && v6,
        four_octet_asn: asn.is_some(),
        send_extended: config.extended_messages && extended,
        receive_extended: config.extended_messages,
        send_refresh: refresh,
        receive_refresh: config.route_refresh,
    })
}
/// 两条尚未建立的反向连接发生碰撞时，保留较大 Identifier 发起的连接；ID 相等的 eBGP 按较大实际 AS 决定。
/// 已有 Established 的连接应由 peer owner 优先保留，不调用本函数替换；同 AS/ID 在 OPEN 接纳时已拒绝。
pub fn prefer_outgoing(config: Config, peer: Negotiated) -> bool {
    (u32::from(config.identifier), config.local_as)
        > (u32::from(peer.peer_identifier), peer.peer_as)
}
