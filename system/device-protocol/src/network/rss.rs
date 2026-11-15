//! RSS 冷配置的版本内格式；经单个只读 VMO 交付快照，不受 channel 单条消息大小约束。
use crate::Reader;
use alloc::vec::Vec;
use device_model::network::{NetworkRssCapabilities, NetworkRssConfiguration};
use native_runtime::native_abi::{self, Status};

pub(super) fn maximum_bytes(cap: NetworkRssCapabilities) -> usize {
    12 + usize::from(cap.maximum_table_entries) * 2 + usize::from(cap.maximum_key_bytes)
}
pub(super) fn encode(config: &NetworkRssConfiguration) -> Result<Vec<u8>, Status> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(12 + config.indirection.len() * 2 + config.key.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    bytes.extend_from_slice(&config.hash_types.to_le_bytes());
    bytes.extend_from_slice(&config.unclassified_queue.to_le_bytes());
    bytes.extend_from_slice(&(config.key.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&(config.indirection.len() as u32).to_le_bytes());
    for queue in &config.indirection {
        bytes.extend_from_slice(&queue.to_le_bytes());
    }
    bytes.extend_from_slice(&config.key);
    Ok(bytes)
}
pub(super) fn decode(
    bytes: &[u8],
    queues: u16,
    cap: NetworkRssCapabilities,
) -> Result<NetworkRssConfiguration, Status> {
    if bytes.len() > maximum_bytes(cap) {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let mut reader = Reader::new(bytes);
    let hash_types = reader.u32()?;
    let unclassified_queue = reader.u16()?;
    let key_length = reader.u16()? as usize;
    let count = reader.u32()? as usize;
    if count > usize::from(cap.maximum_table_entries)
        || key_length > usize::from(cap.maximum_key_bytes)
        || 12 + count * 2 + key_length != bytes.len()
    {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let mut indirection = Vec::new();
    indirection
        .try_reserve_exact(count)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for _ in 0..count {
        indirection.push(reader.u16()?);
    }
    let mut key = Vec::new();
    key.try_reserve_exact(key_length)
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    key.extend_from_slice(&bytes[12 + count * 2..]);
    let config = NetworkRssConfiguration {
        hash_types,
        unclassified_queue,
        indirection,
        key,
    };
    config.validate(queues, cap).map_err(super::encode_error)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn large_table_snapshot_and_invalid_ownership_are_checked() {
        // 真实格式的最大可用幂次表越过 64 KiB，捕获错误地缩回 channel 消息或窄计数的回归。
        let cap = NetworkRssCapabilities {
            maximum_key_bytes: 40,
            maximum_table_entries: u16::MAX,
            hash_types: 0x3f,
        };
        let config = NetworkRssConfiguration {
            hash_types: 0x3f,
            unclassified_queue: 0,
            indirection: (0..32768).map(|index| (index % 3) as u16).collect(),
            key: vec![0x6d; 40],
        };
        let mut bytes = encode(&config).unwrap();
        assert!(bytes.len() > native_abi::CHANNEL_MAX_BYTES);
        let decoded = decode(&bytes, 3, cap).unwrap();
        assert_eq!(decoded.indirection, config.indirection);
        assert_eq!(decoded.key, config.key);
        bytes[12..14].copy_from_slice(&3u16.to_le_bytes());
        assert!(matches!(
            decode(&bytes, 3, cap),
            Err(native_abi::ERR_INVALID_ARGS)
        ));
        bytes[12..14].copy_from_slice(&0u16.to_le_bytes());
        bytes[0..4].copy_from_slice(&0x200u32.to_le_bytes());
        assert!(matches!(
            decode(&bytes, 3, cap),
            Err(native_abi::ERR_INVALID_ARGS)
        ));
        // 设备的最大 key 至少 40，不代表协议要求每次都提供 40；合法短 key 不能被额外门禁拒绝。
        bytes[0..4].copy_from_slice(&0x3fu32.to_le_bytes());
        bytes[6..8].copy_from_slice(&1u16.to_le_bytes());
        bytes.truncate(bytes.len() - 39);
        assert_eq!(decode(&bytes, 3, cap).unwrap().key, vec![0x6d]);
    }
}
