//! 接收侧按流散列的设备能力与配置；队列分配策略属于协议引擎，不属于硬件适配器。
use super::{NetworkError, RECEIVE_HASH_TYPES};
use alloc::vec::Vec;

/// RSS 的真实硬件上限；只有可配置 Toeplitz 分流时才公布。
#[derive(Debug, Clone, Copy)]
pub struct NetworkRssCapabilities {
    /// 最大密钥字节数，至少 40。
    pub maximum_key_bytes: u8,
    /// 最大间接表条目数，至少 128；具体表长必须为 2 的幂。
    pub maximum_table_entries: u16,
    /// 支持的 RECEIVE_HASH_TYPES 位集合。
    pub hash_types: u32,
}

/// 同一硬件会话的 RSS 参数。配置改变可能把既有流交给另一队列，执行者负责 worker 切换。
pub struct NetworkRssConfiguration {
    /// 启用的散列类型；零表示所有包进入 unclassified_queue。
    pub hash_types: u32,
    /// 无可用散列的报文进入的 pair 下标。
    pub unclassified_queue: u16,
    /// 以散列低位索引的 pair 下标表。
    pub indirection: Vec<u16>,
    /// Toeplitz 密钥；启用散列时非空且不超过设备上限，长度策略由使用者选择。
    pub key: Vec<u8>,
}
impl NetworkRssConfiguration {
    /// 校验能力、散列、密钥和每个队列引用；不改变设备，不分配内存。
    /// # Errors
    /// 参数越过能力或指向未创建的队列时返回 InvalidConfiguration。
    pub fn validate(
        &self,
        queues: u16,
        capability: NetworkRssCapabilities,
    ) -> Result<(), NetworkError> {
        if queues == 0
            || self.unclassified_queue >= queues
            || self.hash_types & !capability.hash_types != 0
            || self.hash_types & !RECEIVE_HASH_TYPES != 0
            || !self.indirection.len().is_power_of_two()
            || self.indirection.len() > usize::from(capability.maximum_table_entries)
            || self.indirection.iter().any(|queue| *queue >= queues)
            || self.key.len() > usize::from(capability.maximum_key_bytes)
            || (self.hash_types != 0 && self.key.is_empty())
        {
            return Err(NetworkError::InvalidConfiguration);
        }
        Ok(())
    }
}
