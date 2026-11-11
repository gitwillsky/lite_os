//! 地址替换的全部资源先准备，激活/逆向只交换资格与容器；组播协议义务始终只留一份。
use super::*;
use alloc::vec::Vec;
use network_protocol::SocketError;
use router_protocol::interface::Ipv6Addresses;

pub(in crate::inet) struct PreparedAddresses {
    pub(super) addresses: Vec<Address>,
    groups: Vec<Ipv6Addr>,
}
impl InterfaceAddresses {
    /// 捕获完整手动地址替换，不修改地址资格、邻居或组播；相同意图无须重新 DAD。
    pub(in crate::inet) fn prepare_addresses(
        &mut self,
        intent: Ipv6Addresses<'_>,
    ) -> Result<Option<PreparedAddresses>, SocketError> {
        if self
            .addresses
            .iter()
            .map(Address::definition)
            .eq(intent.iter())
        {
            return Ok(None);
        }
        let mut addresses = Vec::new();
        addresses
            .try_reserve_exact(intent.len())
            .map_err(|_| SocketError::NoMemory)?;
        for definition in intent.iter() {
            if definition.address == self.local.address {
                return Err(SocketError::Invalid);
            }
            addresses.push(Address::new(
                definition.address,
                definition.prefix_length,
                definition.on_link,
            ));
        }
        let mut groups = Vec::new();
        let capacity = self
            .groups
            .len()
            .checked_add(intent.len())
            .ok_or(SocketError::NoMemory)?;
        groups
            .try_reserve_exact(capacity)
            .map_err(|_| SocketError::NoMemory)?;
        if let Some(automatic) = &self.listener {
            groups.extend(self.groups.iter().map(Listener::group));
            for definition in intent.iter() {
                let group = solicited(definition.address);
                if group != automatic.group() && !groups.contains(&group) {
                    groups.push(group);
                }
            }
            groups.sort_unstable();
            // 只预留容量，不改变可见地址/监听状态；激活及回滚都可能重新装入已完成退组的组。
            self.groups
                .try_reserve_exact(groups.len().saturating_sub(self.groups.len()))
                .map_err(|_| SocketError::NoMemory)?;
        }
        Ok(Some(PreparedAddresses { addresses, groups }))
    }
    /// 激活与撤销使用同一交换；公共地址保留最新 DAD，公共组保留唯一监听状态。
    /// 所有向量容量已预留，转移不分配；删除组的退组义务留在实际接口，不在逆向对象中重放旧协议时钟。
    pub(in crate::inet) fn exchange_addresses(
        &mut self,
        prepared: &mut PreparedAddresses,
        now: u64,
    ) {
        self.discovery.expire(now);
        for next in &mut prepared.addresses {
            if let Some(previous) = self
                .addresses
                .iter_mut()
                .find(|previous| previous.address == next.address)
            {
                next.exchange_state(previous);
            } else if let Some(learned) = self
                .discovery
                .addresses
                .iter_mut()
                .find(|row| row.address.address == next.address)
            {
                // 移交唯一的 DAD 资格。学习寿命仍留在发现 owner，撤销时可以继续使用。
                next.exchange_state(&mut learned.address);
                next.set_deprecated(false);
            }
        }
        // 未过期的 SLAAC 地址接回最新资格。没有学习来源的已撤销地址回滚时重新 DAD。
        // 共同地址的最新状态已经转入 next，不能用预备时的旧时钟覆盖它。
        for previous in &mut self.addresses {
            if !prepared
                .addresses
                .iter()
                .any(|next| next.address == previous.address)
            {
                self.discovery.resume_address(previous, now);
            }
            previous.disable();
        }
        core::mem::swap(&mut self.addresses, &mut prepared.addresses);
        for group in &prepared.groups {
            if !self
                .groups
                .iter()
                .any(|listener| listener.group() == *group)
            {
                self.groups.push(Listener::new(*group));
            }
        }
        self.groups.sort_unstable_by_key(Listener::group);
        self.group_cursor = 0;
        self.address_cursor = 0;
        self.synchronize_groups(now);
        self.reschedule_advertisements();
    }
}
