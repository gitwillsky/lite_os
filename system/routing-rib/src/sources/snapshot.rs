//! 唯一已确认 RIB 快照；保留刚参与编译的解析图，不重建第二份 FIB/前缀索引。
use super::*;
use router_protocol::tracking::{self, Query, Record};

/// 同一编译代际的解析图；上层必须在执行确认后与原始来源/策略一并发表，不能独立发表。
pub struct Snapshot {
    pub(super) generation: u64,
    pub(super) resolved: crate::resolution::Resolved<u32, (), Origin>,
}
impl Snapshot {
    /// 当前 RIB 实例的编译代际；服务仅在执行确认后发表，来源预留/观察集合变化不增加此值。
    pub fn generation(&self) -> u64 {
        self.generation
    }
    /// 无分配借用一个查询的已选解析结果；默认路由是否可用由查询明确指定。
    pub fn lookup(&self, query: Query) -> Record<'_> {
        self.resolved.track(query)
    }
    /// 比较查询可见的完整结果；代际变化本身不唤醒无关观察者，不分配或增加引用计数。
    pub fn changes_from(&self, previous: &Self, queries: &[Query]) -> bool {
        queries
            .iter()
            .any(|query| self.lookup(*query) != previous.lookup(*query))
    }
    /// 流式编码冻结的有序查询集，不复制行或成员 DTO 数组。
    /// # Errors
    /// 非法查询/数量/字段、容量溢出或内存不足，不返回部分正文。
    pub fn encode(&self, queries: &[Query]) -> Result<Vec<u8>, Status> {
        tracking::Snapshot::encode(
            self.generation,
            queries.iter().map(|query| self.lookup(*query)),
        )
        .map_err(|error| match error {
            tracking::Error::Invalid => native_abi::ERR_INVALID_ARGS,
            tracking::Error::NoMemory => native_abi::ERR_NO_MEMORY,
        })
    }
}
