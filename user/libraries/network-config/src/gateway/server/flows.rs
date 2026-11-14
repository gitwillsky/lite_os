//! 单接口按需读取报文头，逐时间窗汇总；从管理会话只发送聚合结果。

#[cfg(target_os = "linux")]
use super::*;
#[cfg(target_os = "linux")]
use linux_uapi::{
    packet::Receiver,
    unix::{PollEvents, PollFd, poll},
};
use packet_wire::{ethernet::EthernetFrame, ipv4::Ipv4Quote, ipv6::quoted_upper_prefix};
#[cfg(target_os = "linux")]
use router_protocol::management::flows::Sample;
use router_protocol::management::flows::{self, Endpoints, Key, Row};
use std::{collections::HashMap, net::Ipv6Addr};

#[cfg(target_os = "linux")]
const INTERVAL: Duration = Duration::from_secs(1);
#[cfg(target_os = "linux")]
const HEADER_BYTES: usize = 256;
const CANDIDATES: usize = 64;

#[cfg(target_os = "linux")]
pub(super) fn serve(session: &mut Session, control: &[u8]) -> io::Result<()> {
    let result = sample(session, control);
    observation::report_failure(session, result, "无法继续读取流量分布")
}

#[cfg(target_os = "linux")]
fn sample(session: &mut Session, control: &[u8]) -> io::Result<()> {
    let interface = flows::parse_request(control).map_err(|_| io::ErrorKind::InvalidData)?;
    let SessionIo::Unix(stream) = session.get_ref() else {
        return Err(io::ErrorKind::PermissionDenied.into());
    };
    let peer = stream.socket.try_clone()?;
    let receiver = Receiver::open(interface)?;
    receiver.check_error()?;
    let mut counters = [Leaders::new(), Leaders::new()];
    let mut bytes = [0; 2];
    let mut frame = [0; HEADER_BYTES];
    let mut descriptors = [
        PollFd::new(peer.as_fd(), PollEvents::READ),
        PollFd::new(receiver.as_fd(), PollEvents::READ),
    ];
    let mut start = Instant::now();
    loop {
        let interval = start.elapsed();
        if interval >= INTERVAL {
            let [_, drops] = receiver.take_statistics()?;
            let report = Sample {
                interval_ns: interval.as_nanos().min(u128::from(u64::MAX)) as u64,
                bytes: core::mem::take(&mut bytes),
                drops,
                rows: strongest(&counters),
            };
            for direction in &mut counters {
                direction.clear();
            }
            start = Instant::now();
            let body = report.encode().map_err(|_| io::ErrorKind::InvalidData)?;
            observation::send_update(session, &body)?;
            continue;
        }
        match poll(&mut descriptors, Some(INTERVAL.saturating_sub(interval))) {
            Ok(0) => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            ready => {
                ready?;
            }
        }
        if descriptors[0].returned() != PollEvents::EMPTY {
            return Ok(());
        }
        let events = descriptors[1].returned();
        if events.contains(PollEvents::ERROR) {
            receiver.check_error()?;
        }
        if events.contains(PollEvents::HANGUP) {
            return Err(io::ErrorKind::NotConnected.into());
        }
        // 繁忙接口也必须定期检查时间窗及订阅者是否已离开。
        for _ in 0..64 {
            let received = match receiver.receive(&mut frame) {
                Ok(received) => received,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            if received.loopback_outgoing {
                continue;
            }
            let direction = usize::from(received.outgoing);
            let length = received.full_length as u64;
            let prefix = &frame[..received.full_length.min(HEADER_BYTES)];
            bytes[direction] += length;
            counters[direction].record(
                Key {
                    outgoing: received.outgoing,
                    endpoints: endpoints(prefix),
                },
                length,
            );
        }
    }
}

fn endpoints(prefix: &[u8]) -> Endpoints {
    let frame = EthernetFrame::parse(prefix).expect("validated by packet receiver");
    if let Ok(network) = frame.network() {
        let bytes = network.payload();
        match network.type_or_length() {
            0x0800 => {
                if let Ok(packet) = Ipv4Quote::parse(bytes) {
                    return Endpoints::Ip {
                        source: packet.source().into(),
                        destination: packet.destination().into(),
                        protocol: packet.protocol(),
                        ports: ports(packet.protocol(), packet.upper_layer()),
                    };
                }
            }
            0x86dd if bytes.len() >= 40 && bytes[0] >> 4 == 6 => {
                let source = Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[8..24]).unwrap());
                let destination = Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[24..40]).unwrap());
                let upper = quoted_upper_prefix(bytes).ok().flatten();
                let protocol = upper.map_or(bytes[6], |(protocol, _)| protocol);
                return Endpoints::Ip {
                    source: source.into(),
                    destination: destination.into(),
                    protocol,
                    ports: ports(protocol, upper.map(|(_, bytes)| bytes)),
                };
            }
            _ => {}
        }
        return Endpoints::Ethernet {
            source: frame.source(),
            destination: frame.destination(),
            ether_type: network.type_or_length(),
        };
    }
    Endpoints::Ethernet {
        source: frame.source(),
        destination: frame.destination(),
        ether_type: frame.type_or_length(),
    }
}

fn ports(protocol: u8, bytes: Option<&[u8]>) -> Option<[u16; 2]> {
    if !matches!(protocol, 6 | 17 | 132) {
        return None;
    }
    let bytes = bytes?.first_chunk::<4>()?;
    Some([
        u16::from_be_bytes([bytes[0], bytes[1]]),
        u16::from_be_bytes([bytes[2], bytes[3]]),
    ])
}

fn strongest(counters: &[Leaders; 2]) -> Vec<Row> {
    let mut rows: Vec<_> = counters
        .iter()
        .flat_map(|direction| &direction.entries)
        .map(|entry| Row {
            key: entry.key,
            minimum_bytes: entry.estimate - entry.error,
            maximum_bytes: entry.estimate,
        })
        .collect();
    rows.sort_unstable_by_key(|row| core::cmp::Reverse((row.minimum_bytes, row.maximum_bytes)));
    rows.truncate(flows::TOP_ROWS);
    rows
}

struct Counter {
    key: Key,
    estimate: u64,
    error: u64,
    rank: usize,
}

// Weighted Space-Saving。哈希表指向固定槽，最小堆只移动槽索引；每包不申请内存。
struct Leaders {
    positions: HashMap<Key, usize>,
    entries: Vec<Counter>,
    heap: Vec<usize>,
}

impl Leaders {
    fn new() -> Self {
        Self {
            positions: HashMap::with_capacity(2 * CANDIDATES),
            entries: Vec::with_capacity(CANDIDATES),
            heap: Vec::with_capacity(CANDIDATES),
        }
    }

    fn clear(&mut self) {
        self.positions.clear();
        self.entries.clear();
        self.heap.clear();
    }

    fn record(&mut self, key: Key, weight: u64) {
        if let Some(&slot) = self.positions.get(&key) {
            self.entries[slot].estimate += weight;
            self.sift_down(self.entries[slot].rank);
        } else if self.entries.len() == CANDIDATES {
            let slot = self.heap[0];
            let previous = &mut self.entries[slot];
            self.positions.remove(&previous.key);
            let error = previous.estimate;
            *previous = Counter {
                key,
                estimate: error + weight,
                error,
                rank: 0,
            };
            self.positions.insert(key, slot);
            self.sift_down(0);
        } else {
            let slot = self.entries.len();
            let mut rank = self.heap.len();
            self.positions.insert(key, slot);
            self.entries.push(Counter {
                key,
                estimate: weight,
                error: 0,
                rank,
            });
            self.heap.push(slot);
            while rank != 0 {
                let parent = (rank - 1) / 2;
                if self.estimate(parent) <= self.estimate(rank) {
                    break;
                }
                self.swap(parent, rank);
                rank = parent;
            }
        }
    }

    fn estimate(&self, rank: usize) -> u64 {
        self.entries[self.heap[rank]].estimate
    }

    fn swap(&mut self, left: usize, right: usize) {
        self.heap.swap(left, right);
        self.entries[self.heap[left]].rank = left;
        self.entries[self.heap[right]].rank = right;
    }

    fn sift_down(&mut self, mut rank: usize) {
        loop {
            let left = 2 * rank + 1;
            if left >= self.heap.len() {
                break;
            }
            let right = left + 1;
            let child = if right < self.heap.len() && self.estimate(right) < self.estimate(left) {
                right
            } else {
                left
            };
            if self.estimate(rank) <= self.estimate(child) {
                break;
            }
            self.swap(rank, child);
            rank = child;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use router_protocol::management::flows::Sample;

    #[test]
    fn weighted_flow_bounds_survive_replacement_and_vlan_ipv6_keeps_endpoints() {
        let mut packet = [0u8; 14 + 4 + 40 + 8];
        packet[12..14].copy_from_slice(&0x8100u16.to_be_bytes());
        packet[16..18].copy_from_slice(&0x86ddu16.to_be_bytes());
        let ip = &mut packet[18..];
        ip[0] = 0x60;
        ip[5] = 8;
        ip[6] = 17;
        ip[7] = 64;
        ip[23] = 1;
        ip[39] = 2;
        ip[40..44].copy_from_slice(&[0x12, 0x34, 0, 53]);
        let key = Key {
            outgoing: false,
            endpoints: endpoints(&packet),
        };
        assert_eq!(
            key.endpoints,
            Endpoints::Ip {
                source: Ipv6Addr::LOCALHOST.into(),
                destination: Ipv6Addr::from(2u128).into(),
                protocol: 17,
                ports: Some([0x1234, 53]),
            }
        );

        let mut counts = [Leaders::new(), Leaders::new()];
        let mut exact = HashMap::<Key, u64>::new();
        for index in 0..4096u16 {
            let [high, low] = index.to_be_bytes();
            let noise = Key {
                outgoing: false,
                endpoints: Endpoints::Ethernet {
                    source: [2, 0, 0, 0, high, low],
                    destination: [0; 6],
                    ether_type: 0x0806,
                },
            };
            for (key, weight) in [(noise, u64::from(index % 29 + 64)), (key, 1500)] {
                counts[0].record(key, weight);
                *exact.entry(key).or_default() += weight;
            }
        }
        assert_eq!(counts[0].entries.len(), CANDIDATES);
        for entry in &counts[0].entries {
            assert!((entry.estimate - entry.error..=entry.estimate).contains(&exact[&entry.key]));
        }
        let sample = Sample {
            interval_ns: 1_000_000_000,
            bytes: [exact.values().sum(), 0],
            drops: 12,
            rows: strongest(&counts),
        };
        assert_eq!(sample.rows[0].key, key);
        assert_eq!(Sample::parse(&sample.encode().unwrap()).unwrap(), sample);
        counts[0].clear();
        assert!(strongest(&counts).is_empty());
    }
}
