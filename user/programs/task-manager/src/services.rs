//! 全系统原生服务观测；KOID 与 Linux PID 分离，观察句柄不会越过服务边界。
use crate::system::{TICKS_PER_SECOND, memory_text};
#[cfg(target_os = "linux")]
use std::time::Duration;
use std::{collections::HashMap, io};
#[cfg(any(target_os = "linux", test))]
use task_observer_protocol::{HEADER_BYTES, QUERY, RECORD_BYTES};
use task_observer_protocol::{Record, UNAVAILABLE};

#[derive(Clone, Default)]
pub struct Snapshot {
    pub rows: Vec<Service>,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct Service {
    pub record: Record,
    /// 百分之一百分点；全机归一化，与进程列相同。
    pub cpu_percent: Option<u16>,
}

impl Service {
    pub fn name(&self) -> &str {
        let end = self
            .record
            .name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(32);
        std::str::from_utf8(&self.record.name[..end]).unwrap_or("无法读取名称")
    }
    pub fn text(&self) -> String {
        let record = &self.record;
        let number = |value: u64| {
            if value == UNAVAILABLE {
                "—".into()
            } else {
                value.to_string()
            }
        };
        let cpu = self.cpu_percent.map_or_else(
            || "—".into(),
            |value| format!("{}.{:02}", value / 100, value % 100),
        );
        let memory = if record.resident_bytes == UNAVAILABLE {
            "—".into()
        } else {
            memory_text(record.resident_bytes / 1024)
        };
        let state = match record.state {
            0 => "运行中",
            1 => "已退出",
            _ => "无法查询",
        };
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.name(),
            record.koid,
            state,
            cpu,
            if record.cpu_ns == UNAVAILABLE {
                "—".into()
            } else {
                crate::processes::cpu_time_text(record.cpu_ns / (1_000_000_000 / TICKS_PER_SECOND))
            },
            memory,
            number(record.threads)
        )
    }
}

impl Snapshot {
    pub fn read(previous: &HashMap<u64, u64>, interval_ticks: u64) -> Self {
        match query() {
            Ok(records) => Self {
                rows: records
                    .into_iter()
                    .map(|record| {
                        let cpu_percent = previous
                            .get(&record.koid)
                            .filter(|old| **old != UNAVAILABLE)
                            .and_then(|old| {
                                (record.cpu_ns != UNAVAILABLE)
                                    .then_some(record.cpu_ns.saturating_sub(*old))
                            })
                            .and_then(|delta| {
                                u128::from(delta).checked_mul(10_000 * u128::from(TICKS_PER_SECOND))
                            })
                            .and_then(|delta| {
                                delta.checked_div(u128::from(interval_ticks) * 1_000_000_000)
                            })
                            .map(|percent| percent.min(10_000) as u16);
                        Service {
                            record,
                            cpu_percent,
                        }
                    })
                    .collect(),
                error: None,
            },
            Err(error) => Self {
                rows: Vec::new(),
                error: Some(error.to_string()),
            },
        }
    }
}

fn query() -> io::Result<Vec<Record>> {
    #[cfg(target_os = "linux")]
    {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::SocketAddr;
        let socket = linux_uapi::unix::anonymous_datagram()?;
        socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        socket.set_write_timeout(Some(Duration::from_millis(500)))?;
        socket.connect_addr(&SocketAddr::from_abstract_name(
            task_observer_protocol::ADDRESS,
        )?)?;
        socket.send(QUERY)?;
        // 单个 Unix datagram 的应用接收上限；超长消息截断后由长度校验拒绝，不解释部分记录。
        let mut bytes = vec![0; 64 * 1024];
        let count = socket.recv(&mut bytes)?;
        decode(&bytes[..count])
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(any(target_os = "linux", test))]
fn decode(bytes: &[u8]) -> io::Result<Vec<Record>> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "无效的原生任务快照");
    if bytes.get(..8) != Some(QUERY) {
        return Err(invalid());
    }
    let count = u32::from_le_bytes(
        bytes
            .get(8..12)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?,
    ) as usize;
    if count
        .checked_mul(RECORD_BYTES)
        .and_then(|size| size.checked_add(HEADER_BYTES))
        != Some(bytes.len())
    {
        return Err(invalid());
    }
    let mut records = Vec::with_capacity(count);
    for bytes in bytes[HEADER_BYTES..].as_chunks::<RECORD_BYTES>().0 {
        let record = Record::decode(bytes).ok_or_else(invalid)?;
        if records.iter().any(|old: &Record| old.koid == record.koid) {
            return Err(invalid());
        }
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reply_rejects_truncation_unknown_state_and_repeated_identity() {
        let record = Record {
            koid: 7,
            state: 0,
            ..Record::default()
        };
        let mut bytes = vec![0; HEADER_BYTES + RECORD_BYTES * 2];
        bytes[..8].copy_from_slice(QUERY);
        bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
        record.encode(&mut bytes[HEADER_BYTES..HEADER_BYTES + RECORD_BYTES]);
        let second = Record {
            koid: 8,
            state: 2,
            resident_bytes: UNAVAILABLE,
            ..Record::default()
        };
        second.encode(&mut bytes[HEADER_BYTES + RECORD_BYTES..]);
        assert_eq!(decode(&bytes).unwrap()[1].resident_bytes, UNAVAILABLE);
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
        record.encode(&mut bytes[HEADER_BYTES + RECORD_BYTES..]);
        assert!(decode(&bytes).is_err());
        let unknown = Record {
            koid: 8,
            state: 3,
            ..record
        };
        unknown.encode(&mut bytes[HEADER_BYTES + RECORD_BYTES..]);
        assert!(decode(&bytes).is_err());
    }
}
