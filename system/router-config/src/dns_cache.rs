//! DNS 的持久意图归 saved，实际执行会话与重试归配置事件循环。

use super::*;
use core::net::{IpAddr, SocketAddr, SocketAddrV6};
use network_protocol::InterfaceSnapshot;
use router_protocol::{
    configuration::{InterfaceReference, Name},
    dns_cache::{self as protocol, Binding, Settings},
};

#[derive(Default)]
pub(super) struct DnsExecution {
    session: Option<Session>,
    pub(super) retry_at: Option<i64>,
}

struct Session {
    key: u64,
    control: Channel,
    applied: Settings,
    // 结果不明时持续持有暂停能力，不能让旧服务在接口变化后继续应答。
    pending_pause: Option<Channel>,
}

impl Session {
    fn closed(&self) -> bool {
        self.control.wait_one(signals::PEER_CLOSED, 0) != Err(native_abi::ERR_TIMED_OUT)
    }

    fn command(&self, command: u32, body: &[u8], token: Option<Channel>) -> Result<(), Status> {
        let (receive, reply) = Channel::create()?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(4 + body.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        bytes.extend_from_slice(&command.to_le_bytes());
        bytes.extend_from_slice(body);
        let mut handles = Vec::new();
        handles
            .try_reserve_exact(1 + usize::from(token.is_some()))
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        handles.push(reply.into_handle());
        handles.extend(token.map(Channel::into_handle));
        self.control.write(&bytes, handles)?;
        let result =
            receive.read_wait(native_runtime::system::monotonic().saturating_add(5_000_000_000))?;
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

    fn pause(&self) -> Result<Channel, Status> {
        if let Some(token) = &self.pending_pause {
            return token.duplicate(native_abi::rights::SAME);
        }
        let (token, peer) = Channel::create()?;
        self.command(protocol::PAUSE, &[], Some(peer))?;
        Ok(token)
    }

    fn uses(&self, index: u32) -> bool {
        self.applied
            .listen
            .iter()
            .any(|binding| binding.interface == index)
            || self.applied.upstream.iter().any(
                |address| matches!(address, SocketAddr::V6(address) if address.scope_id() == index),
            )
    }
}

impl RouterConfig {
    /// 暂存批次改变服务配置或其接口时，先停止旧实例的应答。
    pub(super) fn pause_dns(
        &self,
        inner: &mut Inner,
        configuration: Option<&Configuration>,
        positions: impl Iterator<Item = usize>,
    ) -> Result<Option<Channel>, Status> {
        if inner.dns.session.as_ref().is_some_and(Session::closed) {
            inner.dns.session = None;
        }
        let Some(session) = &inner.dns.session else {
            return Ok(None);
        };
        let saved = inner.saved.as_deref();
        let previous = saved.and_then(|saved| saved.dns.forwarding.as_ref());
        let changed = previous != configuration.and_then(|saved| saved.dns.forwarding.as_ref());
        let uses_vlan = saved.zip(previous).is_some_and(|(saved, forwarding)| {
            forwarding
                .listen
                .iter()
                .map(|listener| &listener.interface)
                .chain(
                    forwarding
                        .upstream
                        .iter()
                        .filter_map(|server| server.interface.as_ref()),
                )
                .any(|name| {
                    matches!(
                        saved.interface_reference(name),
                        Some(InterfaceReference::Vlan(_))
                    )
                })
        });
        let topology_changed =
            uses_vlan && configuration.is_some_and(|next| routing::bridges_changed(saved, next));
        if session.pending_pause.is_none()
            && !changed
            && !topology_changed
            && !positions.into_iter().any(|position| {
                let interface = &inner.interfaces[position];
                session.uses(interface.index)
                    || uses_vlan
                        && [saved, configuration].into_iter().flatten().any(|config| {
                            config
                                .bridged_profile(&interface.name, interface.mac)
                                .is_some()
                        })
            })
        {
            return Ok(None);
        }
        session.pause().map(Some)
    }

    /// 把暂存暂停交给执行重放 owner，成功确认前保持旧服务暂停。
    pub(super) fn retain_dns_pause(inner: &mut Inner, token: Option<Channel>) {
        if let (Some(session), Some(token)) = (&mut inner.dns.session, token) {
            session.pending_pause = Some(token);
        }
    }

    pub(super) fn queue_dns(&self, inner: &mut Inner) {
        if inner.dns.session.is_some()
            || inner
                .saved
                .as_ref()
                .is_some_and(|saved| saved.dns.forwarding.is_some())
        {
            if inner.dns.retry_at.is_none() {
                inner.dns.retry_at = Some(transaction::retry_at());
                self.wake_configuration();
            }
        } else {
            inner.dns.retry_at = None;
        }
    }

    /// 从唯一持久决定重建实际监听和上游，不持有接口索引的持久副本。
    pub(super) fn apply_dns(&self, inner: &mut Inner) -> Result<(), Status> {
        let result = self.apply_dns_decision(inner);
        inner.dns.retry_at = result.as_ref().err().map(|_| transaction::retry_at());
        result
    }

    fn apply_dns_decision(&self, inner: &mut Inner) -> Result<(), Status> {
        let Some(settings) = compile(inner)? else {
            if let Some(session) = &mut inner.dns.session
                && !session.closed()
            {
                if session.pending_pause.is_none() {
                    session.pending_pause = Some(session.pause()?);
                }
                session.command(protocol::APPLY, &[], None)?;
            }
            inner.dns.session = None;
            return Ok(());
        };
        if inner.dns.session.as_ref().is_some_and(Session::closed) {
            inner.dns.session = None;
        }
        if let Some(session) = &mut inner.dns.session {
            if session.applied == settings && session.pending_pause.is_none() {
                return Ok(());
            }
            let bytes = settings.encode().map_err(|_| native_abi::ERR_NO_MEMORY)?;
            if session.pending_pause.is_none() {
                session.pending_pause = Some(session.pause()?);
            }
            session.command(protocol::APPLY, &bytes, None)?;
            session.applied = settings;
            session.pending_pause = None;
            return Ok(());
        }
        let key = inner.next_key;
        inner.next_key = key.checked_add(1).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        let bytes = settings.encode().map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let service = self.services.connect(service_directory::DNS_CACHE)?;
        let mut request = service_rpc::Writer::request(protocol::START);
        request.try_reserve(4 + bytes.len())?;
        request.bytes(&bytes);
        let mut reply = service_rpc::call(&service, &request, Vec::new())?;
        reply.reader().finish()?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let control = Channel::from_handle(service_rpc::take_handle(&mut reply.handles, 0)?);
        control.wait_async(&self.port, key, signals::PEER_CLOSED)?;
        inner.dns.session = Some(Session {
            key,
            control,
            applied: settings,
            pending_pause: None,
        });
        Ok(())
    }

    pub(super) fn dns_closed(&self, key: u64) -> bool {
        let mut inner = self.inner.lock();
        if !inner
            .dns
            .session
            .as_ref()
            .is_some_and(|session| session.key == key && session.closed())
        {
            return false;
        }
        inner.dns.session = None;
        self.queue_dns(&mut inner);
        Self::changed(&mut inner);
        true
    }
}

fn compile(inner: &Inner) -> Result<Option<Settings>, Status> {
    let Some(saved) = inner.saved.as_deref() else {
        return Ok(None);
    };
    let Some(forwarding) = &saved.dns.forwarding else {
        return Ok(None);
    };
    let network = &inner
        .link
        .as_ref()
        .ok_or(native_abi::ERR_BAD_STATE)?
        .network;
    let interfaces = network
        .interface_snapshots()
        .map_err(configuration_status)?;
    let addresses = network.address_snapshot(0).map_err(configuration_status)?;
    let usable = || {
        (0..addresses.len())
            .filter_map(|at| addresses.get(at))
            .filter(|row| row.usable)
    };
    let mut listen = Vec::new();
    listen
        .try_reserve_exact(forwarding.listen.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for listener in &forwarding.listen {
        let Some(interface) = resolve(saved, &interfaces, &listener.interface)? else {
            continue;
        };
        if usable()
            .any(|row| row.interface_index == interface.index && row.address == listener.address)
        {
            listen.push(Binding {
                interface: interface.index,
                address: listener.address,
            });
        }
    }
    if listen.is_empty() {
        return Ok(None);
    }
    let mut upstream = Vec::new();
    upstream
        .try_reserve_exact(forwarding.upstream.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for server in &forwarding.upstream {
        let address = if let Some(name) = &server.interface {
            let Some(interface) = resolve(saved, &interfaces, name)? else {
                continue;
            };
            if !usable().any(|row| {
                row.interface_index == interface.index
                    && matches!(row.address, IpAddr::V6(ip) if ip.is_unicast_link_local())
            }) {
                continue;
            }
            let IpAddr::V6(address) = server.address else {
                return Err(native_abi::ERR_INVALID_ARGS);
            };
            SocketAddrV6::new(address, protocol::DNS_PORT, 0, interface.index).into()
        } else {
            SocketAddr::new(server.address, protocol::DNS_PORT)
        };
        upstream.push(address);
    }
    Ok(Some(Settings { listen, upstream }))
}

fn resolve<'a>(
    configuration: &Configuration,
    interfaces: &'a [InterfaceSnapshot],
    name: &Name,
) -> Result<Option<&'a InterfaceSnapshot>, Status> {
    let reference = configuration
        .interface_reference(name)
        .ok_or(native_abi::ERR_INVALID_ARGS)?;
    let mut matching = interfaces.iter().filter(|snapshot| {
        if snapshot.loopback {
            return false;
        }
        let name = snapshot
            .name
            .split(|&byte| byte == 0)
            .next()
            .unwrap_or_default();
        match reference {
            InterfaceReference::Device(device) => {
                !snapshot.vlan && device.matches(name, snapshot.mac)
            }
            InterfaceReference::Vlan(vlan) => snapshot.vlan && vlan.as_str().as_bytes() == name,
        }
    });
    let first = matching.next();
    if matching.next().is_some() {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    Ok(first.filter(|snapshot| snapshot.up))
}
