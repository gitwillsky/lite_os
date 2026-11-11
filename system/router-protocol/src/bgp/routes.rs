//! 按规范前缀查询原始接收与已确认协议判定；不把协议选择冒充最终转发表。
use crate::Invalid;
use alloc::{string::String, vec::Vec};
use core::net::IpAddr;
use serde::{Deserialize, Serialize};

/// 单条接收路由最近一次实际选路判定；Pending 不得当作已经安装或拒绝。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum RouteDecision {
    #[default]
    Pending,
    Selected,
    NotBest,
    InvalidPeer,
    ImportPolicy,
    Unresolved,
    AsLoop,
    OriginatorLoop,
    ClusterLoop,
}
/// 规范路径段；kind 使用标准 AS_PATH 段类型编号，序列重复和集合边界保留。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathSegment {
    pub kind: u8,
    pub asns: Vec<u32>,
}
/// 原始或已选属性的只读观察投影；只为本次查询捕获，不是另一个可写 RIB。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathAttributes {
    pub path: Vec<PathSegment>,
    pub origin: u8,
    pub med: Option<u32>,
    pub local_preference: Option<u32>,
    pub communities: Vec<u32>,
    pub extended_communities: Vec<[u8; 8]>,
    pub large_communities: Vec<[u8; 12]>,
}
/// 当前连接提供的一条原始接收；连接身份不暴露为配置名称或 PID。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteObservation {
    pub neighbor: String,
    pub address: IpAddr,
    pub decision: RouteDecision,
    pub next_hop: IpAddr,
    pub attributes: PathAttributes,
}
/// 当前配置邻居对此目标的发送观察；退出连接不沿用旧 TCP 的记录。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportObservation {
    pub neighbor: String,
    pub address: IpAddr,
    /// 已完整送入当前 TCP 的宣告，不代表远端确认。
    pub advertised: bool,
}
/// 同一 actor 的单前缀结果；selected 是已经来源确认的后政策属性，不是最终 FIB 安装证明。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Routes {
    pub query: crate::tracking::IpPrefix,
    /// 域有未确认工作时所有 received 判定显示 Pending；selected 仍是上次确认值。
    pub pending: bool,
    pub received: Vec<RouteObservation>,
    pub exported: Vec<ExportObservation>,
    /// None 未配置本机宣告，false 暂无精确可用路由，true 有路由依据；实际发送仍由出口政策控制。
    pub originated: Option<bool>,
    /// 其他来源候选的实际提供者及度量；本机显式 network 可以覆盖其出口，仍保留真实元数据。
    pub redistributed: Option<crate::tracking::routes::Route>,
    pub selected: Option<Selected>,
}
/// 已确认协议选择的后政策值；可能被全局路由策略或其他来源压过。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selected {
    /// 本机网段宣告的当前出口选择；true 时零下一跳表示本方向使用实际 TCP 本机地址。
    pub local: bool,
    pub next_hop: IpAddr,
    pub preference: u32,
    pub attributes: PathAttributes,
}
impl Routes {
    /// 编码当前只读结果。
    /// # Errors
    /// 编码失败。
    pub fn encode(&self) -> Result<Vec<u8>, Invalid> {
        serde_json::to_vec(self).map_err(|_| Invalid)
    }
    /// 完整捕获结果。
    /// # Errors
    /// 字段或 JSON 非法。
    pub fn parse(bytes: &[u8]) -> Result<Self, Invalid> {
        serde_json::from_slice(bytes).map_err(|_| Invalid)
    }
}
