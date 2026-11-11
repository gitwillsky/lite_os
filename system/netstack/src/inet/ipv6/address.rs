//! 每个 IPv6 地址唯一拥有其 DAD 资格与 nonce；邻居和组播监听属于接口，不在地址中复制。
use super::{MAX_MULTICAST_SOLICIT, Packet, SendTime, random_delay};
use core::net::Ipv6Addr;
use network_protocol::AddressState;

enum Phase {
    Off,
    Probing {
        next: SendTime,
        remaining: u8,
        nonce: Option<[u8; 6]>,
        looped: bool,
    },
    Preferred,
    Deprecated,
    Duplicate,
}
/// 地址探测对有效 NS/NA 的裁决；组播退组由接口按剩余地址使用者决定。
pub(super) enum Observation {
    NotProbing,
    Handled,
    Duplicate,
}
/// 同一 NET guard 内的地址与唯一使用资格；不持锁、邻居、报文或独立轮询任务。
pub(super) struct Address {
    pub(super) address: Ipv6Addr,
    pub(super) prefix_length: u8,
    pub(super) on_link: bool,
    phase: Phase,
}
impl Address {
    pub(super) fn new(address: Ipv6Addr, prefix_length: u8, on_link: bool) -> Self {
        Self {
            address,
            prefix_length,
            on_link,
            phase: Phase::Off,
        }
    }
    pub(super) fn definition(&self) -> router_protocol::configuration::Ipv6Address {
        router_protocol::configuration::Ipv6Address {
            address: self.address,
            prefix_length: self.prefix_length,
            on_link: self.on_link,
        }
    }
    /// 同一地址改前缀或撤销事务时移交当前资格；不倒退已经观察到的冲突或复制 nonce。
    pub(super) fn exchange_state(&mut self, other: &mut Self) {
        core::mem::swap(&mut self.phase, &mut other.phase);
    }
    /// IP-medium 回环不执行链路 DAD；资格仍随实际管理开关与最小 MTU 生灭。
    pub(super) fn enable_loopback(&mut self) {
        self.phase = Phase::Preferred;
    }
    pub(super) fn state(&self) -> Option<AddressState> {
        Some(match self.phase {
            Phase::Off => return None,
            Phase::Probing { .. } => AddressState::Tentative,
            Phase::Preferred => AddressState::Preferred,
            Phase::Deprecated => AddressState::Deprecated,
            Phase::Duplicate => AddressState::Duplicate,
        })
    }
    pub(super) fn preferred(&self) -> bool {
        matches!(self.phase, Phase::Preferred)
    }
    pub(super) fn usable(&self) -> bool {
        matches!(self.phase, Phase::Preferred | Phase::Deprecated)
    }
    pub(super) fn set_deprecated(&mut self, deprecated: bool) -> bool {
        match (&self.phase, deprecated) {
            (Phase::Preferred, true) => self.phase = Phase::Deprecated,
            (Phase::Deprecated, false) => self.phase = Phase::Preferred,
            _ => return false,
        }
        true
    }
    /// 撤销资格，保留配置身份；重新启用必须完整探测，旧 nonce 不可用于新一轮。
    pub(super) fn disable(&mut self) {
        self.phase = Phase::Off;
    }
    /// 首次启用时返回随机开始时刻，调用者在此前加入所属 solicited-node 组。
    /// Duplicate 不会因普通 poll 被自动恢复；必须经过明确 disable/enable。
    pub(super) fn start(&mut self, now: u64) -> Option<u64> {
        if self.state().is_some() {
            return None;
        }
        let start = now.saturating_add(random_delay(1000));
        self.phase = Phase::Probing {
            next: SendTime::At(start),
            remaining: 1,
            nonce: None,
            looped: false,
        };
        Some(start)
    }
    /// 未成功加入组时不暴露 DAD 截止，防止发送背压变成定时器忙循环。
    pub(super) fn deadline(&self, joined: bool) -> Option<u64> {
        if joined && let Phase::Probing { next, .. } = self.phase {
            next.deadline()
        } else {
            None
        }
    }
    /// 一次至多发一个探测；提交成功才计数，背压等待实际设备活动。
    pub(super) fn poll(
        &mut self,
        now: u64,
        joined: bool,
        retransmit_ms: u64,
        mut send: impl FnMut(Packet<'_>) -> bool,
    ) {
        if joined
            && let Phase::Probing {
                next,
                remaining,
                nonce,
                looped,
            } = &mut self.phase
            && next.ready(now)
            && *remaining != 0
        {
            let mut value = [0; 6];
            native_runtime::system::cprng_draw(&mut value);
            if send(Packet::Probe {
                target: self.address,
                nonce: value,
            }) {
                *remaining -= 1;
                *nonce = Some(value);
                *looped = false;
                *next = SendTime::At(now.saturating_add(retransmit_ms));
            } else {
                *next = SendTime::Writable;
            }
        }
    }
    /// 只有 RX 已排空才能调用，避免截止先于队列中的真实冲突生效。
    pub(super) fn confirm(&mut self, now: u64, joined: bool) -> bool {
        if joined
            && matches!(self.phase, Phase::Probing { remaining: 0, next: SendTime::At(at), .. } if at <= now)
        {
            self.phase = Phase::Preferred;
            true
        } else {
            false
        }
    }
    /// 消费已校验、明确指向本地址的 NS/NA；普通解析 NS 不能被当作 DAD 冲突。
    pub(super) fn observe(
        &mut self,
        advertisement: bool,
        unspecified: bool,
        nonce: Option<&[u8]>,
    ) -> Observation {
        let Phase::Probing {
            remaining,
            nonce: current,
            looped,
            ..
        } = &mut self.phase
        else {
            return Observation::NotProbing;
        };
        if !advertisement && !unspecified {
            return Observation::Handled;
        }
        if !advertisement
            && unspecified
            && current
                .as_ref()
                .is_some_and(|current| nonce == Some(current.as_slice()))
        {
            if !*looped {
                info!("IPv6 DAD loopback detected for {}", self.address);
                *looped = true;
            }
            // RFC 7527：观察三个额外无回送间隔，每次发包重新取得 nonce。
            *remaining = MAX_MULTICAST_SOLICIT;
            return Observation::Handled;
        }
        self.phase = Phase::Duplicate;
        error!("IPv6 duplicate address {}", self.address);
        Observation::Duplicate
    }
}
