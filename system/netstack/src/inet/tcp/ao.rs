//! TCP-AO 主密钥策略与连接状态。实际端口、序号和接受裁决仍由 TCP 状态机拥有。
use super::*;
use alloc::{boxed::Box, sync::Arc};
use core::net::SocketAddr;
use network_protocol::{
    TCP_AO_MAX_KEY_BYTES, TcpAoAlgorithm, TcpAoCounters, TcpAoKey, TcpAoSelection, TcpAoStatus,
    TcpKeySelector,
};
use transport::{
    AoAlgorithm, AoKeyError, AoMasterKey, AoTrafficKey, Control, IncomingSegment, IpPacket,
    Segment, Sequence, SequenceExtension,
};

pub(super) type AoPolicy = Option<Arc<AoMasterKeys>>;

pub(super) struct AoMasterKeys {
    entries: Vec<Arc<MasterKey>>,
    selection: TcpAoSelection,
}

pub(super) struct MasterKey {
    config: TcpAoKey,
    key: AoMasterKey,
}

impl MasterKey {
    fn new(config: TcpAoKey, secret: &[u8]) -> Result<Arc<Self>, SocketError> {
        let algorithm = match config.algorithm {
            TcpAoAlgorithm::HmacSha1 => AoAlgorithm::HmacSha1,
            TcpAoAlgorithm::Aes128Cmac => AoAlgorithm::Aes128Cmac,
        };
        let key = AoMasterKey::new(algorithm, secret, config.cover_options).map_err(key_error)?;
        Arc::try_new(Self { config, key }).map_err(|_| SocketError::NoMemory)
    }

    pub(super) fn matches(policy: &AoPolicy, peer: IpAddr, interface: u32) -> bool {
        policy
            .iter()
            .flat_map(|keys| keys.entries.iter())
            .any(|entry| entry.config.selector.matches(peer, interface))
    }

    pub(super) fn overlaps(policy: &AoPolicy, selector: TcpKeySelector) -> bool {
        policy
            .iter()
            .flat_map(|keys| keys.entries.iter())
            .any(|entry| entry.config.selector.overlaps(selector))
    }
}

struct TrafficKeys {
    master: Arc<MasterKey>,
    send_syn: Option<Arc<AoTrafficKey>>,
    send: Option<Arc<AoTrafficKey>>,
    receive: Option<AoTrafficKey>,
}

impl TrafficKeys {
    fn new(master: &Arc<MasterKey>) -> Self {
        Self {
            master: master.clone(),
            send_syn: None,
            send: None,
            receive: None,
        }
    }
}

/// 已匹配实际端点的密钥快照。只有启用 AO 的连接分配，父监听器修改不影响半连接或子连接。
pub(super) struct AoConnection {
    /// Immutable peer-only inheritance until the TCP listener publishes its accepted child.
    pub(super) inherited_keys: AoPolicy,
    keys: Vec<TrafficKeys>,
    local: SocketAddr,
    peer: SocketAddr,
    interface: u32,
    local_isn: Sequence,
    send: SequenceExtension,
    remote: Option<(Sequence, SequenceExtension)>,
    current: usize,
    preferred: usize,
}

fn key_error(error: AoKeyError) -> SocketError {
    match error {
        AoKeyError::Invalid => SocketError::Invalid,
        AoKeyError::NoMemory => SocketError::NoMemory,
    }
}

impl AoConnection {
    /// 只准备本连接匹配项，必须在改变 TCP 状态前完成所有容器分配。
    pub(super) fn prepare(
        policy: &AoPolicy,
        local: SocketAddr,
        peer: SocketAddr,
        interface: u32,
    ) -> Result<Option<Box<Self>>, SocketError> {
        let Some(configured) = policy else {
            return Ok(None);
        };
        let matching = || {
            configured
                .entries
                .iter()
                .filter(|key| key.config.selector.matches(peer.ip(), interface))
        };
        let count = matching().count();
        if count == 0 {
            return Ok(None);
        }
        let mut keys = Vec::new();
        keys.try_reserve_exact(count)
            .map_err(|_| SocketError::NoMemory)?;
        keys.extend(matching().map(TrafficKeys::new));
        let current = configured
            .selection
            .current
            .and_then(|id| keys.iter().position(|key| key.master.config.send_id == id))
            .unwrap_or(0);
        let preferred = configured
            .selection
            .receive
            .and_then(|id| {
                keys.iter()
                    .position(|key| key.master.config.receive_id == id)
            })
            .unwrap_or(0);
        let local_isn = Sequence(0);
        Box::try_new(Self {
            inherited_keys: None,
            keys,
            local,
            peer,
            interface,
            local_isn,
            send: SequenceExtension::new(local_isn),
            remote: None,
            current,
            preferred,
        })
        .map(Some)
        .map_err(|_| SocketError::NoMemory)
    }

    /// 核验初始 SYN 后准备被动连接，未通过的报文没有持久对象。
    pub(super) fn open_passive<'a>(
        policy: &AoPolicy,
        ip: IpPacket,
        incoming: IncomingSegment<'a>,
        interface: u32,
        counters: &mut TcpAoCounters,
    ) -> Option<(Box<Self>, Segment<'a>)> {
        let (sequence, ack) = incoming.sequence();
        if incoming.control() != Control::Syn || ack.is_some() {
            return None;
        }
        let Some((id, requested)) = incoming.ao_ids() else {
            counters.unsigned = counters.unsigned.wrapping_add(1);
            return None;
        };
        let (source, destination) = incoming.ports();
        let peer = SocketAddr::new(ip.src_addr(), source);
        let local = SocketAddr::new(ip.dst_addr(), destination);
        let Some(master) = policy.as_ref()?.entries.iter().find(|entry| {
            entry.config.receive_id == id && entry.config.selector.matches(peer.ip(), interface)
        }) else {
            counters.key_not_found = counters.key_not_found.wrapping_add(1);
            return None;
        };
        let key = master
            .key
            .derive((peer, sequence.0 as u32), (local, 0))
            .ok()?;
        let Some(verified) = incoming.authenticate_ao((peer.ip(), local.ip()), &key, 0) else {
            counters.bad_mac = counters.bad_mac.wrapping_add(1);
            return None;
        };
        counters.verified = counters.verified.wrapping_add(1);
        let mut connection = Self::prepare(policy, local, peer, interface).ok()??;
        // A multi-peer listener must not retain unrelated keys in each child. Reuse
        // the immutable policy when all of its entries already belong to this peer.
        connection.inherited_keys = if connection.keys.len() == policy.as_ref()?.entries.len() {
            policy.clone()
        } else {
            let mut entries = Vec::new();
            entries.try_reserve_exact(connection.keys.len()).ok()?;
            entries.extend(connection.keys.iter().map(|key| key.master.clone()));
            Some(
                Arc::try_new(AoMasterKeys {
                    entries,
                    selection: TcpAoSelection::default(),
                })
                .ok()?,
            )
        };
        connection.remote = Some((sequence, SequenceExtension::new(sequence)));
        connection.preferred = connection
            .keys
            .iter()
            .position(|key| key.master.config.receive_id == id)?;
        connection.current = connection.preferred;
        connection.apply_request(requested, interface);
        Some((connection, verified))
    }

    pub(super) fn start(&mut self, socket: &transport::Socket<'_>) {
        self.local_isn = socket.authentication_checkpoints().0;
        self.send = SequenceExtension::new(self.local_isn);
    }

    fn eligible(&self, index: usize, interface: u32) -> bool {
        // prepare 已匹配固定的对端地址，只有本次实际接口会变化。
        self.keys[index]
            .master
            .config
            .selector
            .interface_index
            .is_none_or(|scope| scope == interface)
    }

    fn selection(&self) -> TcpAoSelection {
        TcpAoSelection {
            current: Some(self.keys[self.current].master.config.send_id),
            receive: Some(self.keys[self.preferred].master.config.receive_id),
        }
    }

    fn select(&mut self, selection: TcpAoSelection) -> Result<(), SocketError> {
        let (current, preferred) = key_indices(&self.keys, self.selection(), selection)?;
        self.current = current;
        self.preferred = preferred;
        Ok(())
    }

    // All allocation and selection checks precede moving an existing cache. Invalid
    // replacement IDs and OOM leave the old connection usable with its original keys.
    fn reconfigure(
        &mut self,
        policy: &AoPolicy,
        selection: TcpAoSelection,
    ) -> Result<(), SocketError> {
        let matching = || {
            policy
                .iter()
                .flat_map(|keys| keys.entries.iter())
                .filter(|key| key.config.selector.matches(self.peer.ip(), self.interface))
        };
        let mut keys = Vec::new();
        keys.try_reserve_exact(matching().count())
            .map_err(|_| SocketError::NoMemory)?;
        keys.extend(matching().map(TrafficKeys::new));
        let (current, preferred) = key_indices(&keys, self.selection(), selection)?;
        for entry in &mut keys {
            if let Some(old) = self
                .keys
                .iter_mut()
                .find(|old| Arc::ptr_eq(&old.master, &entry.master))
            {
                core::mem::swap(old, entry);
            }
        }
        self.keys = keys;
        self.current = current;
        self.preferred = preferred;
        Ok(())
    }

    pub(super) fn output_scope(&self) -> Option<u32> {
        self.keys[self.current]
            .master
            .config
            .selector
            .interface_index
            .or(self.keys[self.preferred]
                .master
                .config
                .selector
                .interface_index)
    }

    fn apply_request(&mut self, requested: u8, interface: u32) {
        if self.keys[self.current].master.config.send_id == requested
            && self.eligible(self.current, interface)
        {
            return;
        }
        if let Some(index) = self.keys.iter().enumerate().position(|(index, entry)| {
            entry.master.config.send_id == requested && self.eligible(index, interface)
        }) {
            self.current = index;
        }
    }

    /// 使用匹配的 RecvID 验证实际方向和 SNE。未知 RNext 保留当前发送密钥。
    pub(super) fn authenticate<'a>(
        &mut self,
        incoming: IncomingSegment<'a>,
        interface: u32,
        counters: &mut TcpAoCounters,
    ) -> Option<Segment<'a>> {
        let Some((id, requested)) = incoming.ao_ids() else {
            counters.unsigned = counters.unsigned.wrapping_add(1);
            return None;
        };
        let Some(index) = self.keys.iter().enumerate().position(|(index, entry)| {
            entry.master.config.receive_id == id && self.eligible(index, interface)
        }) else {
            counters.key_not_found = counters.key_not_found.wrapping_add(1);
            return None;
        };
        let (sequence, ack) = incoming.sequence();
        let syn = incoming.control() == Control::Syn;
        if syn && self.remote.is_some_and(|(isn, _)| isn != sequence) {
            return None;
        }
        let addresses = (self.peer.ip(), self.local.ip());
        let slot = &mut self.keys[index];
        let verified = if syn && ack.is_none() {
            let sne = self.remote.map_or(Some(0), |(_, sequence_space)| {
                sequence_space.for_segment(sequence)
            })?;
            let key = slot
                .master
                .key
                .derive((self.peer, sequence.0 as u32), (self.local, 0))
                .ok()?;
            incoming.authenticate_ao(addresses, &key, sne)
        } else if let Some((isn, sequence_space)) = self.remote {
            let sne = sequence_space.for_segment(sequence)?;
            if slot.receive.is_none() {
                slot.receive = Some(
                    slot.master
                        .key
                        .derive(
                            (self.peer, isn.0 as u32),
                            (self.local, self.local_isn.0 as u32),
                        )
                        .ok()?,
                );
            }
            incoming.authenticate_ao(addresses, slot.receive.as_ref()?, sne)
        } else if syn {
            let key = slot
                .master
                .key
                .derive(
                    (self.peer, sequence.0 as u32),
                    (self.local, self.local_isn.0 as u32),
                )
                .ok()?;
            incoming.authenticate_ao(addresses, &key, 0)
        } else {
            return None;
        };
        let Some(verified) = verified else {
            counters.bad_mac = counters.bad_mac.wrapping_add(1);
            return None;
        };
        counters.verified = counters.verified.wrapping_add(1);
        self.apply_request(requested, interface);
        Some(verified)
    }

    /// 只有真实 TCP 接受对端 SYN 才建立其 ISN。只用已提交的累积检查点推进两方向。
    pub(super) fn advance(&mut self, socket: &transport::Socket<'_>, segment: &Segment<'_>) {
        let (send, receive) = socket.authentication_checkpoints();
        if self.remote.is_none()
            && segment.control == Control::Syn
            && matches!(socket.state(), State::SynReceived | State::Established)
        {
            let isn = segment.seq_number;
            self.remote = Some((isn, SequenceExtension::new(isn)));
        }
        self.send.advance(send);
        if let Some((_, sequence)) = &mut self.remote {
            sequence.advance(receive);
        }
    }

    /// 取实际发送方向的缓存。返回借用，设备发送无 Arc 原子计数或每段密钥分配。
    pub(super) fn signing_key(
        &mut self,
        segment: &Segment<'_>,
        interface: u32,
    ) -> Option<(&Arc<AoTrafficKey>, (u8, u8), u32)> {
        if !self.eligible(self.current, interface) || !self.eligible(self.preferred, interface) {
            return None;
        }
        let receive_id = self.keys[self.preferred].master.config.receive_id;
        let slot = &mut self.keys[self.current];
        let syn = segment.control == Control::Syn && segment.ack_number.is_none();
        let (cache, destination_isn) = if syn {
            (&mut slot.send_syn, 0)
        } else {
            (&mut slot.send, self.remote?.0.0 as u32)
        };
        if cache.is_none() {
            let key = slot
                .master
                .key
                .derive(
                    (self.local, self.local_isn.0 as u32),
                    (self.peer, destination_isn),
                )
                .ok()?;
            *cache = Some(Arc::try_new(key).ok()?);
        }
        Some((
            cache.as_ref()?,
            (slot.master.config.send_id, receive_id),
            self.send.for_segment(segment.seq_number)?,
        ))
    }
}

fn key_indices(
    keys: &[TrafficKeys],
    old: TcpAoSelection,
    selection: TcpAoSelection,
) -> Result<(usize, usize), SocketError> {
    let current = keys
        .iter()
        .position(|key| Some(key.master.config.send_id) == selection.current.or(old.current))
        .ok_or_else(|| {
            if selection.current.is_some() {
                SocketError::NoEntry
            } else {
                SocketError::Busy
            }
        })?;
    let preferred = keys
        .iter()
        .position(|key| Some(key.master.config.receive_id) == selection.receive.or(old.receive))
        .ok_or_else(|| {
            if selection.receive.is_some() {
                SocketError::NoEntry
            } else {
                SocketError::Busy
            }
        })?;
    Ok((current, preferred))
}

fn policy_selection(
    keys: &[Arc<MasterKey>],
    old: TcpAoSelection,
    update: TcpAoSelection,
    active: bool,
) -> Result<TcpAoSelection, SocketError> {
    let send = |id| keys.iter().any(|key| key.config.send_id == id);
    let receive = |id| keys.iter().any(|key| key.config.receive_id == id);
    if update.current.is_some_and(|id| !send(id)) || update.receive.is_some_and(|id| !receive(id)) {
        return Err(SocketError::NoEntry);
    }
    let current = update.current.or(old.current);
    let preferred = update.receive.or(old.receive);
    if !active && (current.is_some_and(|id| !send(id)) || preferred.is_some_and(|id| !receive(id)))
    {
        return Err(SocketError::Busy);
    }
    Ok(TcpAoSelection {
        current: current.filter(|&id| send(id)),
        receive: preferred.filter(|&id| receive(id)),
    })
}

impl InetSocket {
    /// 读取当前密钥 ID 与认证计数。监听器没有活动连接的密钥选择。
    /// # Errors
    /// socket 不是 TCP、未配置过 TCP-AO，或已经关闭。
    pub(crate) fn ao_status(&self) -> Result<TcpAoStatus, SocketError> {
        let _operation = self.open_operation()?;
        if !matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return Err(SocketError::OperationNotSupported);
        }
        let network = self.stack()?.lock()?;
        let state = network
            .tcp_endpoints
            .get(&endpoint_id(self))
            .ok_or(SocketError::NotConnected)?;
        let counters = *state.ao_counters.as_deref().ok_or(SocketError::NoEntry)?;
        let selection = if matches!(state.mode, TcpMode::Listening { .. }) {
            TcpAoSelection::default()
        } else if let Some(connection) = &state.handles[0].ao {
            connection.selection()
        } else {
            state
                .ao_keys
                .as_ref()
                .map_or_else(TcpAoSelection::default, |keys| keys.selection)
        };
        Ok(TcpAoStatus {
            selection,
            counters,
        })
    }

    /// 原子修改当前发送密钥和请求对端使用的接收密钥。
    /// # Errors
    /// 当前状态不支持选择、ID 不在实际策略内，或 socket 已关闭。
    pub(crate) fn select_ao_keys(&self, selection: TcpAoSelection) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        if !matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return Err(SocketError::OperationNotSupported);
        }
        let mut network = self.stack()?.lock()?;
        let state = network
            .tcp_endpoints
            .get_mut(&endpoint_id(self))
            .ok_or(SocketError::NotConnected)?;
        if matches!(state.mode, TcpMode::Listening { .. }) && selection != TcpAoSelection::default()
        {
            return Err(SocketError::Invalid);
        }
        let entry = &mut state.handles[0];
        if matches!(
            state.mode,
            TcpMode::Connecting { .. } | TcpMode::Connected(_)
        ) && entry.transport.state() != State::Closed
        {
            entry
                .ao
                .as_mut()
                .ok_or(SocketError::Invalid)?
                .select(selection)?;
            entry.transmit_at = Instant::from_millis(0);
        } else {
            let old = state.ao_keys.as_ref().ok_or(SocketError::NoEntry)?;
            let selection = policy_selection(&old.entries, old.selection, selection, false)?;
            let mut entries = Vec::new();
            entries
                .try_reserve_exact(old.entries.len())
                .map_err(|_| SocketError::NoMemory)?;
            entries.extend(old.entries.iter().cloned());
            state.ao_keys = Some(
                Arc::try_new(AoMasterKeys { entries, selection })
                    .map_err(|_| SocketError::NoMemory)?,
            );
        }
        drop(network);
        crate::worker::request_poll(self.namespace.id());
        Ok(())
    }

    /// 安装或删除一个 TCP-AO 主密钥。监听器已接纳的连接保留其原密钥快照。
    /// # Errors
    /// 选择器、设备或状态无效，身份冲突，删除目标缺失，或无法分配新策略。
    pub(crate) fn set_ao_key(
        &self,
        config: TcpAoKey,
        secret: &[u8],
        selection: TcpAoSelection,
    ) -> Result<(), SocketError> {
        if secret.len() > TCP_AO_MAX_KEY_BYTES {
            return Err(SocketError::Invalid);
        }
        self.update_ao_keys(config.selector, selection, |old, md5_overlap| {
            let identity = |entry: &MasterKey| {
                entry.config.selector == config.selector
                    && entry.config.send_id == config.send_id
                    && entry.config.receive_id == config.receive_id
            };
            let deletion = secret.is_empty();
            if deletion && !old.iter().any(|entry| identity(entry)) {
                return Err(SocketError::NoEntry);
            }
            if !deletion && md5_overlap {
                return Err(SocketError::KeyRejected);
            }
            if !deletion && old.iter().any(|entry| conflicts(entry, config)) {
                return Err(SocketError::AddressInUse);
            }
            let mut next = Vec::new();
            next.try_reserve_exact(if deletion {
                old.len() - 1
            } else {
                old.len() + 1
            })
            .map_err(|_| SocketError::NoMemory)?;
            next.extend(
                old.iter()
                    .filter(|entry| !deletion || !identity(entry))
                    .cloned(),
            );
            if !deletion {
                next.push(MasterKey::new(config, secret)?);
            }
            Ok(next)
        })
    }

    /// Replace the keys of one exact peer, preserving every other peer's policy.
    /// `keys` must name the same host and scope. `selection` chooses the new active IDs.
    /// # Errors
    /// Invalid keys or state, conflicting policies, unavailable replacement IDs or allocation failure.
    pub(crate) fn replace_ao_keys(
        &self,
        keys: &[(TcpAoKey, &[u8])],
        selection: TcpAoSelection,
    ) -> Result<(), SocketError> {
        let selector = keys.first().ok_or(SocketError::Invalid)?.0.selector;
        if selector.address.is_unspecified()
            || selector != TcpKeySelector::host(selector.address, selector.interface_index)
            || keys.len() > 256
        {
            return Err(SocketError::Invalid);
        }
        self.update_ao_keys(selector, selection, |old, md5_overlap| {
            if md5_overlap {
                return Err(SocketError::KeyRejected);
            }
            let keep = || old.iter().filter(|key| key.config.selector != selector);
            let mut next = Vec::new();
            next.try_reserve_exact(keep().count() + keys.len())
                .map_err(|_| SocketError::NoMemory)?;
            next.extend(keep().cloned());
            for &(config, secret) in keys {
                if config.selector != selector
                    || secret.is_empty()
                    || secret.len() > TCP_AO_MAX_KEY_BYTES
                {
                    return Err(SocketError::Invalid);
                }
                if next.iter().any(|entry| conflicts(entry, config)) {
                    return Err(SocketError::AddressInUse);
                }
                next.push(MasterKey::new(config, secret)?);
            }
            Ok(next)
        })
    }

    fn update_ao_keys(
        &self,
        selector: TcpKeySelector,
        selection: TcpAoSelection,
        update: impl FnOnce(&[Arc<MasterKey>], bool) -> Result<Vec<Arc<MasterKey>>, SocketError>,
    ) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        if !matches!(self.endpoint, InetEndpoint::Tcp(_)) {
            return Err(SocketError::OperationNotSupported);
        }
        if !selector.valid() {
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
        if matches!(state.mode, TcpMode::Listening { .. }) && selection != TcpAoSelection::default()
        {
            return Err(SocketError::Invalid);
        }
        if state.family == InetFamily::Ipv4 && selector.address.is_ipv6() {
            return Err(SocketError::Invalid);
        }
        let entry = &mut state.handles[0];
        let active = matches!(
            state.mode,
            TcpMode::Connecting { .. } | TcpMode::Connected(_)
        ) && entry.transport.state() != State::Closed;
        let live = if active {
            let live = entry.ao.as_mut().ok_or(SocketError::Invalid)?;
            if !selector.matches(live.peer.ip(), live.interface) {
                return Err(SocketError::Invalid);
            }
            Some(live)
        } else {
            None
        };
        let old = state
            .ao_keys
            .as_deref()
            .map_or(&[][..], |keys| keys.entries.as_slice());
        let previous = state
            .ao_keys
            .as_ref()
            .map_or_else(TcpAoSelection::default, |keys| keys.selection);
        let next = update(
            old,
            authentication::Keys::overlaps(&state.md5_keys, selector),
        )?;
        let next_selection = policy_selection(&next, previous, selection, active)?;
        let policy = if next.is_empty() {
            None
        } else {
            Some(
                Arc::try_new(AoMasterKeys {
                    entries: next,
                    selection: next_selection,
                })
                .map_err(|_| SocketError::NoMemory)?,
            )
        };
        // Allocate only for AO endpoints. A failed change never installs counters or keys.
        let counters = if state.ao_counters.is_none() {
            Some(Box::try_new(TcpAoCounters::default()).map_err(|_| SocketError::NoMemory)?)
        } else {
            None
        };
        if let Some(connection) = live {
            connection.reconfigure(&policy, selection)?;
            entry.transmit_at = Instant::from_millis(0);
        }
        state.ao_keys = policy;
        if let Some(counters) = counters {
            state.ao_counters = Some(counters);
        }
        drop(network);
        crate::worker::request_poll(self.namespace.id());
        Ok(())
    }
}

fn conflicts(entry: &MasterKey, config: TcpAoKey) -> bool {
    entry.config.selector.overlaps(config.selector)
        && (entry.config.send_id == config.send_id || entry.config.receive_id == config.receive_id)
}
