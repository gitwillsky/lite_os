//! 下一跳观察的共享值与完整字节边界；只报告已确认选路，不授予安装/来源写入能力。
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr};
mod wire;
pub use wire::{OwnedSnapshot, RecordView, Snapshot};

/// 一个已解析立即出口；没有递归目标。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Adjacency {
    pub interface_index: u32,
    /// None 表示直接相连的查询目的，而非缺少解析结果。
    pub gateway: Option<IpAddr>,
    pub on_link: bool,
}
/// 立即出口及规范正整数权重，不包含新的声明/递归目标。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Member {
    pub target: Adjacency,
    pub weight: u32,
}
/// 默认实例 IPv4 查询；经默认路由证明可达必须明确请求，缺省语义不允许。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Query {
    pub address: Ipv4Addr,
    pub resolve_via_default: bool,
}
impl Query {
    /// 检查可观察的单播地址；包含回环，不接受未指定/组播/有限广播。
    pub fn valid(self) -> bool {
        !self.address.is_unspecified()
            && !self.address.is_multicast()
            && !self.address.is_broadcast()
    }
}
/// 已确认选路对一个查询的结果；终止/未解析不能伪装成空转发组。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    NoRoute,
    DefaultExcluded,
    Unresolved,
    Forward,
    Discard,
    Unreachable,
    Prohibit,
}
/// 被选中的真实来源/前缀和属性；来源身份只在当前 RIB 实例内有效。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteInfo {
    pub network: Ipv4Addr,
    pub length: u8,
    pub source: u64,
    pub preference: u32,
    pub metric: u32,
}
/// 对不可变选路结果的无分配借用；整个记录的可见值用于观察变化比较。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record<'a> {
    pub query: Query,
    pub state: State,
    pub route: Option<RouteInfo>,
    pub members: &'a [Member],
}
/// 格式捕获失败；不携内核执行状态或 Linux errno。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    NoMemory,
}

/// 编码有序查询集版本 1；允许空集合，调用者可以稍后替换订阅集合。
/// # Errors
/// 地址/数量无效或内存不足，不返回部分正文。
pub fn encode_queries(queries: &[Query]) -> Result<Vec<u8>, Error> {
    wire::encode_queries(queries)
}
/// 完整捕获有序查询集；保留重复查询和顺序，不改变调用者的行身份。
/// # Errors
/// 版本/保留位/地址/长度/包尾无效或内存不足。
pub fn decode_queries(bytes: &[u8]) -> Result<Vec<Query>, Error> {
    wire::decode_queries(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_and_snapshot_frames_preserve_proof_and_reject_hidden_or_truncated_data() {
        let direct = Query {
            address: "192.0.2.9".parse().unwrap(),
            resolve_via_default: false,
        };
        let excluded = Query {
            address: "203.0.113.9".parse().unwrap(),
            resolve_via_default: false,
        };
        let absent = Query {
            address: "198.51.100.9".parse().unwrap(),
            resolve_via_default: true,
        };
        let queries = [direct, excluded, absent, direct];
        let request = encode_queries(&queries).unwrap();
        assert_eq!(decode_queries(&request).unwrap(), queries);
        for end in 0..request.len() {
            assert!(decode_queries(&request[..end]).is_err());
        }
        for offset in [12, 13] {
            let mut invalid = request.clone();
            invalid[offset] = 2;
            assert!(decode_queries(&invalid).is_err());
        }
        let members = [Member {
            target: Adjacency {
                interface_index: 3,
                gateway: None,
                on_link: false,
            },
            weight: 1,
        }];
        let rows = [
            Record {
                query: direct,
                state: State::Forward,
                route: Some(RouteInfo {
                    network: "192.0.2.0".parse().unwrap(),
                    length: 24,
                    source: 1,
                    preference: 0,
                    metric: 0,
                }),
                members: &members,
            },
            Record {
                query: excluded,
                state: State::DefaultExcluded,
                route: Some(RouteInfo {
                    network: Ipv4Addr::UNSPECIFIED,
                    length: 0,
                    source: 2,
                    preference: 1,
                    metric: 20,
                }),
                members: &[],
            },
            Record {
                query: absent,
                state: State::NoRoute,
                route: None,
                members: &[],
            },
        ];
        let bytes = Snapshot::encode(9, rows.into_iter()).unwrap();
        assert_eq!(bytes.len(), 128);
        let owned = OwnedSnapshot::capture(bytes.clone()).unwrap();
        assert_eq!(owned.view().generation(), 9);
        for (actual, expected) in owned.view().records().zip(rows) {
            assert_eq!(
                (actual.query, actual.state, actual.route),
                (expected.query, expected.state, expected.route)
            );
            assert!(actual.members().eq(expected.members.iter().copied()));
        }
        for end in 0..bytes.len() {
            assert!(Snapshot::parse(&bytes[..end]).is_err());
        }
        for (offset, value) in [
            (0, 2),
            (8, 0),
            (20, 2),
            (21, 7),
            (22, 33),
            (23, 1),
            (24, 1),
            (40, 0),
            (48, 0),
            (52, 0),
            (56, 4),
            (57, 1),
            (60, 1),
            (68, 1),
            (104, 1),
        ] {
            let mut invalid = bytes.clone();
            invalid[offset] = value;
            assert!(Snapshot::parse(&invalid).is_err(), "offset {offset}");
        }
        let mut invalid = bytes;
        invalid.push(0);
        assert!(Snapshot::parse(&invalid).is_err());
        assert_eq!(Snapshot::encode(0, rows.into_iter()), Err(Error::Invalid));
    }
}
