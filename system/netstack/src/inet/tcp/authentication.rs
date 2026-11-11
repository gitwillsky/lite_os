//! NET endpoint 的签名密钥策略；不可变版本由实际已完成握手的 child 冻结，报文只短借选中的密钥。
use super::*;
use alloc::sync::Arc;
use network_protocol::{TCP_MD5_MAX_KEY_BYTES, TcpMd5Selector};

#[derive(Clone)]
struct Entry {
    selector: TcpMd5Selector,
    key: Arc<transport::Md5Key>,
}
/// 只在配置过密钥时存在；同一不可变版本可被 listener、已握手 child 及在途回复共享。
pub(super) struct Keys(Vec<Entry>);
pub(super) type SharedKeys = Option<Arc<Keys>>;
/// 本连接已选择的密钥关联；路由变化不能在仍有同一配置项时把受保护连接悄悄降为无签名。
pub(super) struct Binding {
    selector: TcpMd5Selector,
    pub(super) key: Arc<transport::Md5Key>,
}
impl Binding {
    pub(super) fn permits(&self, interface: u32) -> bool {
        self.selector
            .interface_index
            .is_none_or(|index| index == interface)
    }
    /// 同一策略项替换/删除是显式授权，关联随之更新；仅出口改变而无新匹配项时保留保护要求。
    pub(super) fn refresh(
        current: &mut Option<Self>,
        keys: &SharedKeys,
        peer: IpAddr,
        interface: u32,
    ) {
        if let Some(entry) = Keys::select(keys, peer, interface) {
            if current.as_ref().is_none_or(|old| {
                old.selector != entry.selector || !Arc::ptr_eq(&old.key, &entry.key)
            }) {
                *current = Some(Self {
                    selector: entry.selector,
                    key: entry.key.clone(),
                });
            }
        } else if current.as_ref().is_some_and(|old| {
            !keys
                .as_ref()
                .is_some_and(|keys| keys.0.iter().any(|entry| entry.selector == old.selector))
        }) {
            *current = None;
        }
    }
}
impl Keys {
    /// 接口限定优先于通配，再取最长前缀；同等条件选最近新增项，与固定 Linux MD5 选择一致。
    fn select(keys: &SharedKeys, peer: IpAddr, interface: u32) -> Option<&Entry> {
        keys.as_ref()?
            .0
            .iter()
            .filter(|entry| entry.selector.matches(peer, interface))
            .max_by_key(|entry| {
                (
                    entry.selector.interface_index.is_some(),
                    entry.selector.prefix_length,
                )
            })
    }
    fn changed(
        old: &SharedKeys,
        selector: TcpMd5Selector,
        key: &[u8],
    ) -> Result<SharedKeys, SocketError> {
        let entries = old.as_ref().map_or(&[][..], |keys| keys.0.as_slice());
        let previous = entries.iter().position(|entry| entry.selector == selector);
        if key.is_empty() && previous.is_none() {
            return Err(SocketError::NoEntry);
        }
        let mut next = Vec::new();
        next.try_reserve_exact(entries.len() + usize::from(previous.is_none()))
            .map_err(|_| SocketError::NoMemory)?;
        next.extend(entries.iter().cloned());
        if key.is_empty() {
            next.remove(previous.expect("validated deletion target"));
        } else {
            let key = transport::Md5Key::try_new(key).map_err(|_| SocketError::NoMemory)?;
            let entry = Entry {
                selector,
                key: Arc::try_new(key).map_err(|_| SocketError::NoMemory)?,
            };
            if let Some(index) = previous {
                next[index] = entry;
            } else {
                next.push(entry);
            }
        }
        if next.is_empty() {
            Ok(None)
        } else {
            Arc::try_new(Self(next))
                .map(Some)
                .map_err(|_| SocketError::NoMemory)
        }
    }
}
impl InetSocket {
    /// 原子修改本 endpoint 密钥；已握手的 child 保留其快照，失败不改变旧配置或发送状态。
    /// # Errors
    /// 非 TCP、非法 selector/密钥、未知接口、无删除目标、已关闭 endpoint 或分配失败。
    pub(crate) fn set_md5_key(
        &self,
        selector: TcpMd5Selector,
        key: &[u8],
    ) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        if !matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return Err(SocketError::OperationNotSupported);
        }
        if !selector.valid() || key.len() > TCP_MD5_MAX_KEY_BYTES {
            return Err(SocketError::Invalid);
        }
        let mut network = self.stack()?.lock()?;
        if let Some(interface) = selector.interface_index {
            network.interface(interface)?;
        }
        let state = network
            .tcp_endpoints
            .get_mut(&endpoint_id(self))
            .ok_or(SocketError::NotConnected)?;
        if state.family == InetFamily::Ipv4 && selector.address.is_ipv6() {
            return Err(SocketError::Invalid);
        }
        let next = Keys::changed(&state.md5_keys, selector, key)?;
        state.md5_keys = next;
        for entry in &mut state.handles {
            entry.transmit_at = Instant::from_millis(0);
        }
        drop(network);
        crate::worker::request_poll();
        Ok(())
    }
}
