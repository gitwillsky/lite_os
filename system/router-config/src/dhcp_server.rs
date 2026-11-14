//! 下游 DHCPv4 会话随已确认的接口和持久配置生效。

use super::*;
use router_protocol::{
    configuration::Name,
    dhcp_server::{self as protocol, Pool},
    interface::InterfaceConfiguration,
    switching::Ports,
};

struct PoolSession {
    index: u32,
    key: u64,
    control: Channel,
    // 当前服务已确认应用的池，None 表示停用但仍保留未过期的地址占用。
    applied: Option<Pool>,
    // Unconfirmed execution holds this pause until the durable decision is replayed.
    // Releasing it early would allow an obsolete pool to answer on a changed interface.
    pending_pause: Option<Channel>,
}

fn deadline() -> i64 {
    native_runtime::system::monotonic().saturating_add(5_000_000_000)
}

impl PoolSession {
    fn closed(&self) -> bool {
        self.control.wait_one(signals::PEER_CLOSED, 0).is_ok()
    }

    fn command(&self, operation: u32, body: &[u8], token: Option<Channel>) -> Result<(), Status> {
        let (receive, reply) = Channel::create()?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(4 + body.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        bytes.extend_from_slice(&operation.to_le_bytes());
        bytes.extend_from_slice(body);
        let mut handles = Vec::new();
        handles
            .try_reserve_exact(1 + usize::from(token.is_some()))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        handles.push(reply.into_handle());
        handles.extend(token.map(Channel::into_handle));
        self.control.write(&bytes, handles)?;
        let result = receive.read_wait(deadline())?;
        if !result.handles.is_empty() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let status = i32::from_le_bytes(
            result
                .bytes
                .as_slice()
                .try_into()
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
        );
        if status == native_abi::OK {
            Ok(())
        } else {
            Err(status)
        }
    }

    /// Retain the pause through an interface transaction. Dropping it releases the pause.
    fn pause_for(&self, desired: Option<&Pool>) -> Result<Option<Channel>, Status> {
        if let Some(token) = &self.pending_pause {
            return token.duplicate(native_abi::rights::SAME).map(Some);
        }
        if self.applied.as_ref() == desired {
            return Ok(None);
        }
        let (token, peer) = Channel::create()?;
        self.command(protocol::PAUSE, &[], Some(peer))?;
        Ok(Some(token))
    }

    fn apply(&mut self, desired: Option<Pool>) -> Result<(), Status> {
        if self.applied == desired && self.pending_pause.is_none() {
            return Ok(());
        }
        let bytes = desired
            .as_ref()
            .map(Pool::encode)
            .transpose()
            .map_err(|_| native_abi::ERR_NO_MEMORY)?
            .unwrap_or_default();
        self.command(protocol::APPLY, &bytes, None)?;
        self.applied = desired;
        self.pending_pause = None;
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct PoolExecution {
    sessions: Vec<PoolSession>,
    // 单独重试 DHCP 服务应用，避免服务重启时重新计算全部 FIB。
    pub(super) retry_at: Option<i64>,
}

impl PoolExecution {
    pub(super) fn remove_interface(&mut self, index: u32) {
        self.sessions.retain(|session| session.index != index);
    }

    pub(super) fn remove_closed(&mut self, key: u64) -> Option<bool> {
        let position = self
            .sessions
            .iter()
            .position(|session| session.key == key)?;
        let session = self.sessions.swap_remove(position);
        Some(session.applied.is_some() || session.pending_pause.is_some())
    }

    pub(super) fn bindings_channel(&self, index: u32) -> Result<Channel, Status> {
        let session = self
            .sessions
            .iter()
            .find(|session| session.index == index && session.applied.is_some())
            .ok_or_else(|| {
                if self.retry_at.is_some() {
                    native_abi::ERR_SHOULD_WAIT
                } else {
                    native_abi::ERR_NOT_FOUND
                }
            })?;
        if session.pending_pause.is_some() || session.closed() {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        session.control.duplicate(native_abi::rights::SAME)
    }
}

struct Change {
    index: u32,
    desired: Option<Pool>,
    pause: Option<Channel>,
    retire: bool,
}

pub(super) struct StagedPools(Vec<Change>);

impl StagedPools {
    pub(super) fn prepare(
        execution: &mut PoolExecution,
        mut desired: Vec<(u32, Pool)>,
        present: impl Fn(u32) -> bool,
    ) -> Result<Self, Status> {
        execution.sessions.retain(|session| !session.closed());
        let mut changes = Vec::new();
        changes
            .try_reserve_exact(execution.sessions.len() + desired.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for session in &execution.sessions {
            let index = session.index;
            let desired = desired
                .iter()
                .position(|(candidate, _)| *candidate == index)
                .map(|at| desired.swap_remove(at).1);
            let retire = !present(index);
            if !retire && session.applied == desired && session.pending_pause.is_none() {
                continue;
            }
            let pause = match session.pause_for(desired.as_ref()) {
                Ok(token) => token,
                Err(native_abi::ERR_PEER_CLOSED) => None,
                Err(error) => return Err(error),
            };
            changes.push(Change {
                index,
                desired,
                pause,
                retire,
            });
        }
        changes.extend(desired.into_iter().map(|(index, pool)| Change {
            index,
            desired: Some(pool),
            pause: None,
            retire: false,
        }));
        Ok(Self(changes))
    }

    /// 未确认撤销或发布时保留旧池的暂停，直到重放持久决定。
    pub(super) fn hold(&mut self, execution: &mut PoolExecution) {
        for change in &mut self.0 {
            if let Some(token) = change.pause.take()
                && let Some(session) = execution
                    .sessions
                    .iter_mut()
                    .find(|session| session.index == change.index)
            {
                session.pending_pause = Some(token);
            }
        }
    }

    pub(super) fn confirm(self, owner: &RouterConfig, inner: &mut Inner) -> Result<(), Status> {
        inner.pools.retry_at = None;
        let link = inner.link.as_ref().ok_or(native_abi::ERR_PEER_CLOSED)?;
        let result = self.apply(owner, &mut inner.pools, &mut inner.next_key, &link.network);
        if result.is_err() {
            owner.queue_pools(inner);
        }
        result
    }

    pub(super) fn apply(
        self,
        owner: &RouterConfig,
        execution: &mut PoolExecution,
        next_key: &mut u64,
        network: &RemoteNamespace,
    ) -> Result<(), Status> {
        let mut result = Ok(());
        for change in self.0 {
            if change.retire {
                execution.remove_interface(change.index);
                continue;
            }
            if let Err(status) =
                owner.apply_pool(execution, next_key, network, change.index, change.desired)
            {
                if let Some(session) = execution
                    .sessions
                    .iter_mut()
                    .find(|session| session.index == change.index)
                    && let Some(pause) = change.pause
                {
                    session.pending_pause = Some(pause);
                }
                result = Err(status);
            }
        }
        result
    }
}

fn compile(
    configuration: Option<&Configuration>,
    interfaces: &[Interface],
    physical_up: impl Fn(&Interface) -> bool,
    binding: impl Fn(&Name) -> Option<(u32, u32)>,
) -> Result<Vec<(u32, Pool)>, Status> {
    let mut desired = Vec::new();
    let Some(configuration) = configuration else {
        return Ok(desired);
    };
    for interface in interfaces {
        if !physical_up(interface) {
            continue;
        }
        let profile = configuration
            .matching(&interface.name, interface.mac)
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        if let Some(profile) = profile
            && let Some(pool) = profile
                .dhcp_pool()
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?
        {
            desired
                .try_reserve(1)
                .map_err(|_| native_abi::ERR_NO_MEMORY)?;
            desired.push((interface.index, pool));
        }
    }
    append_vlan_pools(&mut desired, configuration.bridges.values(), binding)?;
    Ok(desired)
}

/// Append the DHCP pools for resolved VLAN gateways; absent VLANs have no active listener.
/// # Errors
/// Invalid DHCP settings, an insufficient VLAN MTU or allocation failure.
pub(super) fn append_vlan_pools<'a>(
    desired: &mut Vec<(u32, Pool)>,
    bridges: impl Iterator<Item = &'a router_protocol::configuration::Bridge>,
    binding: impl Fn(&Name) -> Option<(u32, u32)>,
) -> Result<(), Status> {
    for gateway in bridges.flat_map(|bridge| bridge.gateways.values()) {
        if gateway.dhcp_v4_server.is_none() {
            continue;
        }
        let Some((index, mtu)) = binding(&gateway.name) else {
            continue;
        };
        if mtu < 576 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let pool = gateway
            .dhcp_pool()
            .map_err(|_| native_abi::ERR_INVALID_ARGS)?
            .expect("gateway has a DHCPv4 policy");
        desired
            .try_reserve(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        desired.push((index, pool));
    }
    Ok(())
}

/// Read a VLAN pool binding from one active snapshot of its own routing domain.
pub(super) fn active_vlan_binding(
    snapshots: &[network_protocol::InterfaceSnapshot],
    name: &Name,
) -> Option<(u32, u32)> {
    let bytes = name.as_str().as_bytes();
    let snapshot = snapshots.iter().find(|snapshot| {
        snapshot.vlan
            && snapshot.name.get(..bytes.len()) == Some(bytes)
            && snapshot.name.get(bytes.len()) == Some(&0)
    })?;
    Some((snapshot.index, snapshot.mtu as u32))
}

pub(super) fn stage(
    execution: &mut PoolExecution,
    interfaces: &[Interface],
    configuration: Option<&Configuration>,
    ports: Ports<'_>,
    plans: &[InterfaceConfiguration<'_>],
) -> Result<StagedPools, Status> {
    let desired = compile(
        configuration,
        interfaces,
        |interface| {
            plans
                .iter()
                .find(|plan| plan.index == interface.index)
                .map_or(interface.config.method != Method::Disabled, |plan| plan.up)
        },
        |name| {
            let binding = ports.gateway(name.as_str().as_bytes())?.binding;
            (binding.index != 0).then_some((binding.index, u32::from(binding.mtu)))
        },
    )?;
    StagedPools::prepare(execution, desired, |index| {
        interfaces.iter().any(|interface| interface.index == index)
            || ports
                .gateways()
                .any(|gateway| gateway.binding.index == index)
    })
}

impl RouterConfig {
    fn apply_pool(
        &self,
        execution: &mut PoolExecution,
        next_key: &mut u64,
        network: &RemoteNamespace,
        index: u32,
        pool: Option<Pool>,
    ) -> Result<(), Status> {
        if let Some(position) = execution
            .sessions
            .iter()
            .position(|session| session.index == index)
        {
            let session = &mut execution.sessions[position];
            if !session.closed() {
                return session.apply(pool);
            }
            execution.sessions.swap_remove(position);
        }
        let Some(pool) = pool else { return Ok(()) };
        execution
            .sessions
            .try_reserve(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let key = *next_key;
        *next_key = key.checked_add(1).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        let service = self.services.connect(service_directory::DHCP_SERVER)?;
        let mut request = service_rpc::Writer::request(protocol::START);
        let bytes = pool.encode().map_err(|_| native_abi::ERR_NO_MEMORY)?;
        request.try_reserve(8 + bytes.len())?;
        request.u32(index).bytes(&bytes);
        let network = network.channel().duplicate(native_abi::rights::SAME)?;
        let mut reply = service_rpc::call(&service, &request, alloc::vec![network.into_handle()])?;
        reply.reader().finish()?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let control = Channel::from_handle(service_rpc::take_handle(&mut reply.handles, 0)?);
        control.wait_async(&self.port, key, signals::PEER_CLOSED)?;
        execution.sessions.push(PoolSession {
            index,
            key,
            control,
            applied: Some(pool),
            pending_pause: None,
        });
        Ok(())
    }

    pub(super) fn queue_pools(&self, inner: &mut Inner) {
        if inner
            .pools
            .retry_at
            .replace(transaction::retry_at())
            .is_none()
        {
            self.wake_configuration();
        }
    }

    pub(super) fn apply_pools(&self, inner: &mut Inner) -> Result<(), Status> {
        let result = (|| {
            let link = inner.link.as_ref().ok_or(native_abi::ERR_PEER_CLOSED)?;
            let snapshots = link
                .network
                .interface_snapshots()
                .map_err(configuration_status)?;
            let desired = compile(
                inner.saved.as_deref(),
                &inner.interfaces,
                |interface| {
                    snapshots
                        .iter()
                        .any(|snapshot| snapshot.index == interface.index && snapshot.up)
                },
                |name| active_vlan_binding(&snapshots, name),
            )?;
            StagedPools::prepare(&mut inner.pools, desired, |index| {
                snapshots.iter().any(|snapshot| snapshot.index == index)
            })?
            .confirm(self, inner)
        })();
        if result.is_err() {
            self.queue_pools(inner);
        }
        result
    }

    pub(super) fn pool_closed(&self, key: u64) -> bool {
        let mut inner = self.inner.lock();
        let Some(retry) = inner.pools.remove_closed(key) else {
            return self.domain_pool_closed(&mut inner, key);
        };
        if retry {
            self.queue_pools(&mut inner);
            Self::changed(&mut inner);
        }
        true
    }

    /// Read a page through the authorized settings endpoint without holding the configuration lock.
    pub(super) fn pool_bindings(
        &self,
        domain: Option<&str>,
        name: &[u8],
        after: Ipv4Addr,
    ) -> Result<(Revision, Vec<u8>), Status> {
        let (generation, control) = if let Some(domain) = domain {
            let inner = self.inner.lock();
            (
                inner.generation,
                inner.domains.pool_bindings_channel(domain, name)?,
            )
        } else {
            let inner = self.inner.lock();
            let link = inner.link.as_ref().ok_or(native_abi::ERR_PEER_CLOSED)?;
            let index = link
                .network
                .interface_index(name)
                .map_err(configuration_status)?;
            if inner
                .interfaces
                .iter()
                .any(|interface| interface.index == index && interface.retry_at.is_some())
            {
                return Err(native_abi::ERR_SHOULD_WAIT);
            }
            (inner.generation, inner.pools.bindings_channel(index)?)
        };
        let (receive, reply) = Channel::create()?;
        control.write(&protocol::query(after), alloc::vec![reply.into_handle()])?;
        let page = receive.read_wait(deadline())?;
        if !page.handles.is_empty() || protocol::decode_page(&page.bytes).is_err() {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        if self.inner.lock().generation != generation {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        Ok((generation, page.bytes))
    }
}
