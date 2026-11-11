//! 已确认路由的筛选增量；来源由提供者标注，不由消费者伪造身份。
use super::{
    Error, IpPrefix,
    wire::{self, buffer},
};
use alloc::vec::Vec;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
const VERSION: u32 = 2;
const HEADER: usize = 32;
const ROW_HEADER: usize = 24;

/// 分发观察的不可变范围；协议列表为集合，连接前缀不借用 Linux 协议编号表示。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Selector {
    #[serde(default)]
    pub connected: bool,
    #[serde(default, deserialize_with = "protocols")]
    pub protocols: Vec<u8>,
}
fn protocols<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    let mut values = Vec::<u8>::deserialize(deserializer)?;
    values.sort_unstable();
    values.dedup();
    Ok(values)
}
impl Selector {
    /// 检查观察是否非空且具有唯一规范表示，手工模型也必须满足。
    pub fn valid(&self) -> bool {
        (self.connected || !self.protocols.is_empty())
            && self.protocols.windows(2).all(|pair| pair[0] < pair[1])
    }
    /// 按可信来源选择；None 为实际连接前缀，Some 为提供者的协议标记。
    pub fn permits(&self, protocol: Option<u8>) -> bool {
        protocol.map_or(self.connected, |protocol| {
            self.protocols.binary_search(&protocol).is_ok()
        })
    }
    /// 编码唯一筛选模型；原生请求仍由传输 owner 拥有。
    /// # Errors
    /// 空/无序选择或编码失败。
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        if !self.valid() {
            return Err(Error::Invalid);
        }
        serde_json::to_vec(self).map_err(|_| Error::NoMemory)
    }
    /// 捕获并规范化集合，拒绝未知字段/尾部或空选择。
    /// # Errors
    /// JSON/选择非法。
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let value: Self = serde_json::from_slice(bytes).map_err(|_| Error::Invalid)?;
        value.valid().then_some(value).ok_or(Error::Invalid)
    }
}
/// 同一冻结图中可用的已选路由；没有复制下一跳组，也不授予写入权。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub prefix: IpPrefix,
    /// 正且不复用的提供者身份。
    pub source: u64,
    /// None 为连接前缀，Some 为可信来源元数据中的协议。
    pub protocol: Option<u8>,
    pub metric: u32,
    /// 显式汇总丢弃路由；不可达/禁止/解析失败不进入此观察。
    pub discard: bool,
}
/// 每个精确前缀至多一个变化，按前缀严格递增。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Announce(Route),
    Withdraw(IpPrefix),
}
impl Change {
    /// 此变化的精确结构身份。
    pub fn prefix(self) -> IpPrefix {
        match self {
            Self::Announce(route) => route.prefix,
            Self::Withdraw(prefix) => prefix,
        }
    }
}
/// 不可变增量正文；首次 reset 只含完整宣告，其余响应基于消费者已应用的代际。
/// 捕获时完整验证，迭代不分配 DTO 或重复遍历验证。
pub struct Changes(Vec<u8>);
fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("checked word"))
}
fn long(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("checked long"))
}
fn value(row: &[u8]) -> Change {
    let prefix = IpPrefix {
        network: wire::address(&row[ROW_HEADER..], row[0]).expect("validated address"),
        length: row[4],
    };
    if row[5] == 0 {
        Change::Withdraw(prefix)
    } else {
        Change::Announce(Route {
            prefix,
            source: long(row, 8),
            protocol: (row[5] == 2).then_some(row[6]),
            metric: word(row, 16),
            discard: row[7] != 0,
        })
    }
}
impl Changes {
    /// 编码同一冻结代际的完整/增量视图，迭代器必须重放同一内容。
    /// # Errors
    /// 代际/次序/来源非法、大小超界、迭代重放失配或资源不足。
    pub fn encode(
        base: u64,
        generation: u64,
        changes: impl Iterator<Item = Change> + Clone,
    ) -> Result<Vec<u8>, Error> {
        if generation == 0 || generation < base {
            return Err(Error::Invalid);
        }
        let count = u32::try_from(changes.clone().count()).map_err(|_| Error::Invalid)?;
        if base == generation && count != 0 {
            return Err(Error::Invalid);
        }
        let length = changes.clone().try_fold(HEADER, |size, change| {
            size.checked_add(ROW_HEADER + wire::width(wire::family(change.prefix().network))?)
                .ok_or(Error::Invalid)
        })?;
        let mut bytes = buffer(length)?;
        bytes[..4].copy_from_slice(&VERSION.to_le_bytes());
        bytes[4..8].copy_from_slice(&u32::from(base == 0).to_le_bytes());
        bytes[8..12].copy_from_slice(&count.to_le_bytes());
        bytes[16..24].copy_from_slice(&base.to_le_bytes());
        bytes[24..32].copy_from_slice(&generation.to_le_bytes());
        let mut written = 0;
        let mut at = HEADER;
        for change in changes {
            let size = ROW_HEADER + wire::width(wire::family(change.prefix().network))?;
            let row = bytes.get_mut(at..at + size).ok_or(Error::Invalid)?;
            let prefix = change.prefix();
            row[0] = wire::family(prefix.network);
            wire::put_address(&mut row[ROW_HEADER..], prefix.network);
            row[4] = prefix.length;
            if let Change::Announce(route) = change {
                row[5] = if route.protocol.is_some() { 2 } else { 1 };
                row[6] = route.protocol.unwrap_or(0);
                row[7] = u8::from(route.discard);
                row[8..16].copy_from_slice(&route.source.to_le_bytes());
                row[16..20].copy_from_slice(&route.metric.to_le_bytes());
            }
            written += 1;
            at += size;
        }
        if written != count as usize || at != bytes.len() {
            return Err(Error::Invalid);
        }
        Self::validate(&bytes)?;
        Ok(bytes)
    }
    fn validate(bytes: &[u8]) -> Result<(), Error> {
        let head = bytes.get(..HEADER).ok_or(Error::Invalid)?;
        let base = long(head, 16);
        let generation = long(head, 24);
        let count = word(head, 8) as usize;
        if word(head, 0) != VERSION
            || word(head, 4) != u32::from(base == 0)
            || word(head, 12) != 0
            || generation == 0
            || generation < base
            || base == generation && count != 0
            || count > (bytes.len() - HEADER) / (ROW_HEADER + 4)
        {
            return Err(Error::Invalid);
        }
        let mut previous = None;
        let mut rest = &bytes[HEADER..];
        for _ in 0..count {
            let size = ROW_HEADER + wire::width(*rest.first().ok_or(Error::Invalid)?)?;
            let row = rest.get(..size).ok_or(Error::Invalid)?;
            rest = &rest[size..];
            let change = value(row);
            let prefix = change.prefix();
            if !prefix.valid()
                || previous.is_some_and(|old| old >= prefix)
                || row[5] > 2
                || row[7] > 1
                || row[1..4] != [0; 3]
                || row[20..24] != [0; 4]
                || row[5] == 0 && (base == 0 || row[6..24] != [0; 18])
                || row[5] != 0 && (long(row, 8) == 0 || row[5] == 1 && row[6] != 0)
            {
                return Err(Error::Invalid);
            }
            previous = Some(prefix);
        }
        if !rest.is_empty() {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    /// 对同序冻结路由流作归并差分；只为最终变化编码，不复制完整路由表。
    /// # Parameters
    /// 两个输入须按精确前缀严格递增，reset（base=0）时 previous 必须为空。
    /// # Errors
    /// 与 encode 相同；相同前缀的来源/属性改变只产生替换宣告。
    pub fn between(
        base: u64,
        generation: u64,
        previous: impl Iterator<Item = Route> + Clone,
        current: impl Iterator<Item = Route> + Clone,
    ) -> Result<Vec<u8>, Error> {
        let mut previous = previous.peekable();
        let mut current = current.peekable();
        let changes = core::iter::from_fn(move || {
            loop {
                match (previous.peek().copied(), current.peek().copied()) {
                    (Some(old), Some(new)) if old.prefix == new.prefix => {
                        previous.next();
                        current.next();
                        if old != new {
                            return Some(Change::Announce(new));
                        }
                    }
                    (Some(old), next) if next.is_none_or(|next| old.prefix < next.prefix) => {
                        previous.next();
                        return Some(Change::Withdraw(old.prefix));
                    }
                    (_, Some(new)) => {
                        current.next();
                        return Some(Change::Announce(new));
                    }
                    (None, None) => return None,
                    _ => unreachable!("ordered merge covers both inputs"),
                }
            }
        });
        Self::encode(base, generation, changes)
    }
    /// 接管并完整验证独占正文。
    /// # Errors
    /// 版本、字段、次序、代际或长度非法。
    pub fn capture(bytes: Vec<u8>) -> Result<Self, Error> {
        Self::validate(&bytes)?;
        Ok(Self(bytes))
    }
    /// 本响应基于消费者已应用的代际；0 表示完整 reset。
    pub fn base(&self) -> u64 {
        long(&self.0, 16)
    }
    /// 应用本响应成功后下次请求应确认的代际。
    pub fn generation(&self) -> u64 {
        long(&self.0, 24)
    }
    /// 原序借用变化，不重新分配/校验。
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Change> + Clone + '_ {
        let mut rest = &self.0[HEADER..];
        (0..word(&self.0, 8)).map(move |_| {
            let size = ROW_HEADER + wire::width(rest[0]).expect("validated family");
            let change = value(&rest[..size]);
            rest = &rest[size..];
            change
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    #[test]
    fn changes_retain_source_identity_and_reject_partial_hidden_or_unordered_input() {
        let route = Route {
            prefix: IpPrefix {
                network: "198.51.100.0".parse().unwrap(),
                length: 24,
            },
            source: 2,
            protocol: Some(4),
            metric: 7,
            discard: false,
        };
        let bytes = Changes::encode(0, 3, [Change::Announce(route)].into_iter()).unwrap();
        assert_eq!(
            Changes::capture(bytes.clone())
                .unwrap()
                .iter()
                .collect::<Vec<_>>(),
            [Change::Announce(route)]
        );
        for end in 0..bytes.len() {
            assert!(Changes::capture(bytes[..end].to_vec()).is_err());
        }
        for (at, value) in [
            (0, 1),
            (4, 0),
            (12, 1),
            (24, 0),
            (36, 33),
            (37, 3),
            (39, 2),
            (40, 0),
            (52, 1),
            (33, 1),
        ] {
            let mut bad = bytes.clone();
            bad[at] = value;
            assert!(Changes::capture(bad).is_err(), "offset {at}");
        }
        let mut bad = bytes;
        bad.push(0);
        assert!(Changes::capture(bad).is_err());
        assert!(Changes::encode(0, 3, [Change::Withdraw(route.prefix)].into_iter()).is_err());
        assert!(Changes::encode(3, 3, [Change::Announce(route)].into_iter()).is_err());
        assert!(
            Changes::encode(
                3,
                4,
                [Change::Announce(route), Change::Announce(route)].into_iter()
            )
            .is_err()
        );
        let changed = Route { metric: 8, ..route };
        let bytes = Changes::between(3, 4, [route].into_iter(), [changed].into_iter()).unwrap();
        assert_eq!(
            Changes::capture(bytes).unwrap().iter().collect::<Vec<_>>(),
            [Change::Announce(changed)]
        );
        assert_eq!(
            Changes::capture(
                Changes::between(3, 4, [route].into_iter(), core::iter::empty()).unwrap()
            )
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
            [Change::Withdraw(route.prefix)]
        );
        let selector = Selector::parse(br#"{"connected":true,"protocols":[89,4,4]}"#).unwrap();
        assert_eq!(selector.protocols, vec![4, 89]);
        assert!(selector.permits(None));
        assert!(!selector.permits(Some(186)));
    }
}
