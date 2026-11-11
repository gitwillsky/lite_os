//! DNS 的持久意图归 saved，实际执行会话与重试归配置事件循环。

use super::*;
use core::net::{IpAddr, SocketAddr, SocketAddrV6};
use router_protocol::dns_cache::{self as protocol, Binding, Settings};

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
        let changed = inner
            .saved
            .as_deref()
            .and_then(|saved| saved.dns.forwarding.as_ref())
            != configuration.and_then(|saved| saved.dns.forwarding.as_ref());
        if session.pending_pause.is_none()
            && !changed
            && !positions
                .into_iter()
                .any(|position| session.uses(inner.interfaces[position].index))
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
    let mut listen = Vec::new();
    listen
        .try_reserve_exact(forwarding.listen.len())
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    for listener in &forwarding.listen {
        let profile = &saved.interfaces[&listener.interface];
        let Some(interface) = inner
            .interfaces
            .iter()
            .find(|interface| profile.device.matches(&interface.name, interface.mac))
        else {
            continue;
        };
        if interface.config.method != Method::Disabled {
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
            let profile = &saved.interfaces[name];
            let interface = inner
                .interfaces
                .iter()
                .find(|interface| {
                    profile.device.matches(&interface.name, interface.mac)
                        && interface.config.method != Method::Disabled
                })
                .ok_or(native_abi::ERR_SHOULD_WAIT)?;
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
