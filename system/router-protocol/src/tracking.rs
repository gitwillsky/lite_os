//! 冻结下一跳观察的共享值与字节边界；已确认观察或事务候选由提供能力区分，不授予安装/来源写入能力。
use crate::Invalid;
use alloc::vec::Vec;
use core::net::{IpAddr, Ipv4Addr};
use serde::{Deserialize, Serialize};
mod wire;
pub use wire::{OwnedSnapshot, RecordView, Snapshot};
pub mod installation;
pub mod routes;

/// 规范双栈网段，用于精确观察和发布意图；不隐式掩掉主机位。
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, PartialOrd, Ord,
)]
#[serde(deny_unknown_fields)]
pub struct IpPrefix {
    /// 规范网络地址。
    #[schemars(schema_with = "crate::configuration::address_schema")]
    pub network: IpAddr,
    /// 有效前缀位数，IPv4 为 0..=32，IPv6 为 0..=128。
    pub length: u8,
}
impl IpPrefix {
    /// 校验规范前缀；非法长度或主机位返回 false。
    pub fn valid(self) -> bool {
        crate::policy::Prefix {
            network: self.network,
            length: self.length,
            minimum: None,
            maximum: None,
        }
        .lengths()
        .is_some()
    }
    /// 编码完整只读查询。
    /// # Errors
    /// 非规范前缀或编码失败。
    pub fn encode(self) -> Result<Vec<u8>, Invalid> {
        if !self.valid() {
            return Err(Invalid);
        }
        serde_json::to_vec(&self).map_err(|_| Invalid)
    }
    /// 捕获并校验查询；不接受配置字段或尾部。
    /// # Errors
    /// 非法 JSON、字段或前缀。
    pub fn parse(bytes: &[u8]) -> Result<Self, Invalid> {
        let query: Self = serde_json::from_slice(bytes).map_err(|_| Invalid)?;
        if !query.valid() {
            return Err(Invalid);
        }
        Ok(query)
    }
}
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
/// 默认实例双栈地址 LPM 或规范网段精确查询；默认路由许可只适用于地址 LPM。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Query {
    pub address: IpAddr,
    pub resolve_via_default: bool,
    /// Some(length) 为精确网段观察（含 0/0），此时 address 是规范网络且不使用默认路由许可位。
    pub prefix_length: Option<u8>,
}
impl Query {
    /// LPM 必须为可观察单播地址；精确模式须为规范网段，并禁止混用默认路由许可。
    pub fn valid(self) -> bool {
        match self.prefix_length {
            Some(length) => {
                !self.resolve_via_default
                    && crate::policy::Prefix {
                        network: self.address,
                        length,
                        minimum: None,
                        maximum: None,
                    }
                    .lengths()
                    .is_some()
            }
            None => {
                !self.address.is_unspecified()
                    && !self.address.is_multicast()
                    && !matches!(self.address, IpAddr::V4(address) if address.is_broadcast())
                    && !matches!(self.address, IpAddr::V6(address) if address.to_ipv4_mapped().is_some() || address.is_unicast_link_local())
            }
        }
    }
}
/// 冻结选路对一个查询的结果；终止/未解析不能伪装成空转发组，是否已发表由提供能力确定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    NoRoute,
    DefaultExcluded,
    Unresolved,
    Forward,
    Discard,
    Unreachable,
    Prohibit,
    /// 本机交付，允许精确本机网段发布，不代表存在递归转发出口。
    Local,
}
/// 被选中的真实来源/前缀和属性；来源身份只在当前 RIB 实例内有效。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteInfo {
    pub network: IpAddr,
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

/// 编码有序双栈查询集版本 4；允许空集合，调用者可以稍后替换订阅集合。
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
            prefix_length: None,
            address: "192.0.2.9".parse().unwrap(),
            resolve_via_default: false,
        };
        let excluded = Query {
            prefix_length: None,
            address: "203.0.113.9".parse().unwrap(),
            resolve_via_default: false,
        };
        let absent = Query {
            prefix_length: None,
            address: "198.51.100.9".parse().unwrap(),
            resolve_via_default: true,
        };
        let queries = [direct, excluded, absent, direct];
        let request = encode_queries(&queries).unwrap();
        assert_eq!(decode_queries(&request).unwrap(), queries);
        for end in 0..request.len() {
            assert!(decode_queries(&request[..end]).is_err());
        }
        for offset in [8, 9, 10, 11] {
            let mut invalid = request.clone();
            invalid[offset] = if offset == 8 { 5 } else { 4 };
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
                    network: Ipv4Addr::UNSPECIFIED.into(),
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
        assert_eq!(bytes.len(), 140);
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
            (0, 1),
            (8, 0),
            (16, 5),
            (17, 4),
            (18, 1),
            (19, 7),
            (20, 33),
            (21, 1),
            (48, 1),
            (52, 0),
            (56, 0),
            (60, 4),
            (61, 1),
            (64, 1),
            (71, 0),
            (108, 1),
            (112, 1),
        ] {
            let mut invalid = bytes.clone();
            invalid[offset] = value;
            assert!(Snapshot::parse(&invalid).is_err(), "offset {offset}");
        }
        let mut invalid = bytes;
        invalid.push(0);
        assert!(Snapshot::parse(&invalid).is_err());
        assert_eq!(Snapshot::encode(0, rows.into_iter()), Err(Error::Invalid));
        // 精确模式含默认网段，且捕获后的快速投影必须保留模式；普通 LPM 用例无法覆盖。
        let exact = Query {
            address: Ipv4Addr::UNSPECIFIED.into(),
            resolve_via_default: false,
            prefix_length: Some(0),
        };
        let request = encode_queries(&[exact]).unwrap();
        assert_eq!(decode_queries(&request).unwrap(), [exact]);
        let route = RouteInfo {
            network: Ipv4Addr::UNSPECIFIED.into(),
            length: 0,
            source: 2,
            preference: 1,
            metric: 0,
        };
        let bytes = Snapshot::encode(
            10,
            [Record {
                query: exact,
                state: State::Discard,
                route: Some(route),
                members: &[],
            }]
            .into_iter(),
        )
        .unwrap();
        let exact_snapshot = OwnedSnapshot::capture(bytes.clone()).unwrap();
        assert_eq!(exact_snapshot.view().records().next().unwrap().query, exact);
        let mut invalid = bytes;
        invalid[18] = 1;
        assert!(OwnedSnapshot::capture(invalid).is_err()); // 查询 /1 不能拿 /0 作为精确证明。
        let bad = Query {
            prefix_length: Some(1),
            ..exact
        };
        assert!(
            Snapshot::encode(
                10,
                [Record {
                    query: bad,
                    state: State::Discard,
                    route: Some(route),
                    members: &[]
                }]
                .into_iter()
            )
            .is_err()
        );
        assert!(
            !Query {
                resolve_via_default: true,
                ..exact
            }
            .valid()
        );
        // 混合族必须保留地址宽度、次序与邻居 zone，不允许 IPv6 行携带 IPv4 网关。
        let six = Query {
            address: "2001:db8:2::9".parse().unwrap(),
            ..direct
        };
        let exact6 = Query {
            address: "::".parse().unwrap(),
            prefix_length: Some(0),
            resolve_via_default: false,
        };
        let queries = [direct, six, exact6, direct];
        let mixed = encode_queries(&queries).unwrap();
        assert_eq!(mixed.len(), 64);
        assert_eq!(decode_queries(&mixed).unwrap(), queries);
        for end in 0..mixed.len() {
            assert!(decode_queries(&mixed[..end]).is_err());
        }
        assert!(
            !Query {
                address: "fe80::1".parse().unwrap(),
                ..direct
            }
            .valid()
        );
        let hops6 = [Member {
            target: Adjacency {
                interface_index: 3,
                gateway: Some("fe80::1".parse().unwrap()),
                on_link: true,
            },
            weight: 7,
        }];
        let row6 = Record {
            query: six,
            state: State::Forward,
            route: Some(RouteInfo {
                network: "2001:db8:2::".parse().unwrap(),
                length: 64,
                source: 7,
                preference: 20,
                metric: 0,
            }),
            members: &hops6,
        };
        let mixed = Snapshot::encode(12, [rows[0], row6].into_iter()).unwrap();
        let parsed = OwnedSnapshot::capture(mixed.clone()).unwrap();
        let last = parsed.view().records().last().unwrap();
        assert_eq!((last.query, last.route), (row6.query, row6.route));
        assert!(last.members().eq(hops6));
        for end in 0..mixed.len() {
            assert!(Snapshot::parse(&mixed[..end]).is_err());
        }
        let hops = [members[0], hops6[0]];
        let cross = Record {
            members: &hops,
            ..rows[0]
        };
        let mixed = Snapshot::encode(12, [cross, row6].into_iter()).unwrap();
        let mut parsed = Snapshot::parse(&mixed).unwrap().records();
        assert!(parsed.next().unwrap().members().eq(hops));
        assert!(parsed.next().unwrap().members().eq(hops6));
        // 首成员的零填充，以及两个层级的族标记，都必须经过完整验证。
        for (offset, value) in [(21, 0), (68, 1), (88, 3)] {
            let mut malformed = mixed.clone();
            malformed[offset] = value;
            assert!(Snapshot::parse(&malformed).is_err());
        }
        assert!(
            Snapshot::encode(
                12,
                [Record {
                    query: direct,
                    ..row6
                }]
                .into_iter()
            )
            .is_err()
        );
        let wrong = [Member {
            target: Adjacency {
                gateway: Some("192.0.2.1".parse().unwrap()),
                ..hops6[0].target
            },
            ..hops6[0]
        }];
        assert!(
            Snapshot::encode(
                12,
                [Record {
                    members: &wrong,
                    ..row6
                }]
                .into_iter()
            )
            .is_err()
        );
        // 反事实检查多了目的前缀与事务身份，普通 NHT framing 用例无法覆盖错批或隐藏字段。
        use installation::{Installation, Outcome};
        let probes = [Installation {
            interface: None,
            network: "198.51.100.0".parse().unwrap(),
            length: 24,
            next_hop: direct,
        }];
        let bytes = installation::encode(&probes).unwrap();
        assert_eq!(installation::decode(&bytes).unwrap(), probes);
        for end in 0..bytes.len() {
            assert!(installation::decode(&bytes[..end]).is_err());
        }
        for offset in [10, 11, 15, 23] {
            let mut invalid = bytes.clone();
            invalid[offset] = 1;
            assert!(installation::decode(&invalid).is_err());
        }
        let mut invalid = probes;
        invalid[0].network = "198.51.100.1".parse().unwrap();
        assert!(installation::encode(&invalid).is_err());
        let mixed_probes = [
            probes[0],
            Installation {
                interface: None,
                network: "2001:db8:3::".parse().unwrap(),
                length: 64,
                next_hop: six,
            },
            Installation {
                next_hop: six,
                ..probes[0]
            },
        ];
        let mixed = installation::encode(&mixed_probes).unwrap();
        assert_eq!(installation::decode(&mixed).unwrap(), mixed_probes);
        for end in 0..mixed.len() {
            assert!(installation::decode(&mixed[..end]).is_err());
        }
        assert!(
            installation::encode(&[Installation {
                next_hop: direct,
                ..mixed_probes[1]
            }])
            .is_err()
        );
        let outcomes = [
            Outcome {
                reachable: false,
                interior_cost: None,
            },
            Outcome {
                reachable: true,
                interior_cost: Some(0),
            },
        ];
        let bytes = installation::encode_outcomes(7, 19, &outcomes).unwrap();
        assert_eq!(
            installation::decode_outcomes(&bytes, 7, 19, 2).unwrap(),
            outcomes
        );
        assert!(installation::decode_outcomes(&bytes, 8, 19, 2).is_err());
        assert!(installation::decode_outcomes(&bytes, 7, 20, 2).is_err());
        for end in 0..bytes.len() {
            assert!(installation::decode_outcomes(&bytes[..end], 7, 19, 2).is_err());
        }
        for (offset, value) in [(0, 1), (24, 2), (25, 1), (26, 1), (28, 1), (33, 2)] {
            let mut invalid = bytes.clone();
            invalid[offset] = value;
            assert!(installation::decode_outcomes(&invalid, 7, 19, 2).is_err());
        }
    }
}
