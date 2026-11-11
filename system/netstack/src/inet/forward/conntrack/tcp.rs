//! 被动跟踪两侧已观察到的 SYN、序列边界和通告窗口。只在完成转发后采用候选状态。

use packet_wire::tcp::{ACK, FIN, RST, SYN, TcpPacket, options::TcpOption};
use router_protocol::connections::Phase;
use smoltcp::time::Duration;

const ACK_SLACK: u32 = 66_000;

#[derive(Clone, Copy)]
pub(in crate::inet::forward) struct Segment {
    sequence: u32,
    end: u32,
    acknowledgment: u32,
    window: u16,
    scale: Option<u8>,
    flags: u8,
}

impl Segment {
    pub(super) fn capture(packet: TcpPacket<'_>) -> Option<Self> {
        let header = packet.header();
        let flags = header.flags() & (SYN | ACK | FIN | RST);
        if ![SYN, RST, ACK, FIN | ACK, SYN | ACK, RST | ACK].contains(&flags) {
            return None;
        }
        let length = u32::try_from(packet.payload().len())
            .ok()?
            .checked_add(u32::from(flags & (SYN | FIN) != 0))?;
        if length >= 1 << 31 || flags & RST != 0 && length != 0 {
            return None;
        }
        let scale = if flags & SYN != 0 {
            packet.options().find_map(|option| match option.value {
                TcpOption::WindowScale(scale) => Some(scale.min(14)),
                _ => None,
            })
        } else {
            None
        };
        let sequence = header.sequence();
        Some(Self {
            sequence,
            end: sequence.wrapping_add(length),
            acknowledgment: header.acknowledgment(),
            window: header.window(),
            scale,
            flags,
        })
    }

    fn has(self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

fn before(left: u32, right: u32) -> bool {
    (left.wrapping_sub(right) as i32) < 0
}
fn later(left: u32, right: u32) -> u32 {
    if before(left, right) { right } else { left }
}

#[derive(Clone, Copy)]
struct Peer {
    syn: u32,
    syn_end: u32,
    end: u32,
    acked: u32,
    limit: u32,
    max_window: u32,
    scale: Option<u8>,
    syn_acked: bool,
    fin: bool,
}

impl Peer {
    fn opening(segment: Segment) -> Self {
        Self {
            syn: segment.sequence,
            syn_end: segment.end,
            end: segment.end,
            acked: segment.sequence,
            limit: segment.end,
            max_window: u32::from(segment.window).max(1),
            scale: segment.scale,
            syn_acked: false,
            fin: false,
        }
    }

    fn acknowledges_syn(self, ack: u32) -> bool {
        !before(ack, self.syn.wrapping_add(1)) && !before(self.syn_end, ack)
    }

    fn finished(self) -> bool {
        self.fin && self.acked == self.end
    }
}

#[derive(Clone, Copy)]
pub(super) struct State {
    peers: [Option<Peer>; 2],
    reset: bool,
}

impl State {
    pub(super) fn open(segment: Segment, direction: usize) -> Option<Self> {
        if segment.flags != SYN {
            return None;
        }
        let mut peers = [None; 2];
        peers[direction] = Some(Peer::opening(segment));
        Some(Self {
            peers,
            reset: false,
        })
    }

    pub(super) fn may_restart(self, segment: Segment, direction: usize) -> bool {
        segment.flags == SYN
            && (self.reset || self.time_wait())
            && self.peers[direction].is_none_or(|peer| peer.syn != segment.sequence)
    }

    fn time_wait(self) -> bool {
        self.peers
            .iter()
            .all(|peer| peer.is_some_and(Peer::finished))
    }

    fn handshake_complete(self) -> bool {
        self.peers
            .iter()
            .all(|peer| peer.is_some_and(|peer| peer.syn_acked))
    }

    pub(super) fn advance(mut self, segment: Segment, direction: usize) -> Option<Self> {
        if self.reset {
            return None;
        }
        let opposite = direction ^ 1;
        let mut sender = self.peers[direction];
        let mut receiver = self.peers[opposite];
        if segment.has(SYN) {
            if segment.has(ACK)
                && !receiver.is_some_and(|peer| peer.acknowledges_syn(segment.acknowledgment))
            {
                return None;
            }
            if let Some(previous) = sender {
                if previous.syn != segment.sequence
                    || previous.scale != segment.scale
                    || before(previous.syn_end, segment.end)
                {
                    return None;
                }
                if self.handshake_complete() {
                    return Some(self);
                }
            } else {
                sender = Some(Peer::opening(segment));
            }
        } else if segment.has(RST) && sender.is_none() {
            if !segment.has(ACK)
                || !receiver.is_some_and(|peer| peer.acknowledges_syn(segment.acknowledgment))
            {
                return None;
            }
            self.reset = true;
            return Some(self);
        } else {
            let from = sender.as_mut()?;
            let to = receiver?;
            if !segment.has(RST) && !from.syn_acked && !to.syn_acked {
                return None;
            }
            if !self.handshake_complete()
                && !segment.has(RST)
                && before(segment.sequence, from.syn.wrapping_add(1))
            {
                return None;
            }
            if segment.has(ACK)
                && (before(to.end, segment.acknowledgment)
                    || before(
                        segment.acknowledgment,
                        to.acked.wrapping_sub(ACK_SLACK.max(from.max_window)),
                    )
                    || !to.syn_acked && before(segment.acknowledgment, to.syn.wrapping_add(1)))
            {
                return None;
            }
            let floor = from.acked.wrapping_sub(to.max_window);
            if before(segment.end, floor)
                || before(from.limit, segment.sequence)
                || before(from.limit, segment.end)
                || from.fin && before(from.end, segment.end)
                || segment.has(FIN) && before(segment.end, from.end)
            {
                return None;
            }
            if segment.has(RST) {
                if segment.sequence != from.acked && segment.sequence != from.end {
                    return None;
                }
                self.reset = true;
                return Some(self);
            }
            from.end = later(from.end, segment.end);
            from.fin |= segment.has(FIN);
        }

        if segment.has(ACK) {
            let from = sender.as_mut()?;
            let to = receiver.as_mut()?;
            let shift = if !segment.has(SYN) && to.scale.is_some() {
                from.scale.unwrap_or(0)
            } else {
                0
            };
            let window = (u32::from(segment.window) << shift).max(1);
            from.max_window = from.max_window.max(window);
            to.acked = later(to.acked, segment.acknowledgment);
            // 零窗口探测最多可携带一个字节。窗口右沿只前进，旧通告仍可有在途数据。
            to.limit = later(to.limit, segment.acknowledgment.wrapping_add(window));
            to.syn_acked = true;
        }
        self.peers[direction] = sender;
        self.peers[opposite] = receiver;
        Some(self)
    }

    pub(super) fn contains(self, direction: usize, sequence: u32) -> bool {
        !self.reset
            && self.peers[direction].is_some_and(|sender| {
                let window = self.peers[direction ^ 1].map_or(1, |peer| peer.max_window);
                let mut start = sender.acked.wrapping_sub(window);
                if !self.handshake_complete() {
                    start = later(start, sender.syn);
                }
                !before(sequence, start) && !before(sender.end, sequence)
            })
    }

    pub(super) fn awaits_reply(self, segment: Segment) -> bool {
        segment.flags == SYN && self.peers.iter().any(Option::is_none)
    }

    pub(super) fn phase(&self) -> Phase {
        if self.reset {
            Phase::Reset
        } else if self.peers.iter().any(|peer| peer.is_some_and(|peer| peer.fin)) {
            Phase::Closing
        } else if self.handshake_complete() {
            Phase::Established
        } else {
            Phase::Opening
        }
    }

    pub(super) fn idle(self) -> Duration {
        let seconds = if self.reset {
            10
        } else if self
            .peers
            .iter()
            .any(|peer| peer.is_some_and(|peer| peer.fin))
        {
            120
        } else if self.handshake_complete() {
            5 * 24 * 60 * 60
        } else if self.peers.iter().all(Option::is_some) {
            60
        } else {
            120
        };
        Duration::from_secs(seconds)
    }
}
