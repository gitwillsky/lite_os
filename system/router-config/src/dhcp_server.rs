//! 下游 DHCPv4 会话随已确认的接口和持久配置生效。

use super::*;
use router_protocol::dhcp_server::{self as protocol, Pool};

pub(super) struct PoolSession {
    key: u64,
    control: Channel,
    // 当前服务已确认应用的池，None 表示停用但仍保留未过期的地址占用。
    applied: Option<Pool>,
    // Unconfirmed execution holds this pause until the durable decision is replayed.
    // Releasing it early would allow an obsolete pool to answer on a changed interface.
    pub(super) pending_pause: Option<Channel>,
}

fn deadline() -> i64 {
    native_runtime::system::monotonic().saturating_add(5_000_000_000)
}

impl PoolSession {
    pub(super) fn closed(&self) -> bool {
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
    pub(super) fn pause_for(&self, desired: Option<&Pool>) -> Result<Option<Channel>, Status> {
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

impl RouterConfig {
    pub(super) fn apply_pool(
        &self,
        inner: &mut Inner,
        position: usize,
        pool: Option<Pool>,
    ) -> Result<(), Status> {
        let interface = &mut inner.interfaces[position];
        if let Some(session) = &mut interface.dhcp_server {
            if !session.closed() {
                return session.apply(pool);
            }
            interface.dhcp_server = None;
        }
        let Some(pool) = pool else { return Ok(()) };
        let key = inner.next_key;
        inner.next_key = key.checked_add(1).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        let service = self.services.connect(service_directory::DHCP_SERVER)?;
        let mut request = service_rpc::Writer::request(protocol::START);
        let bytes = pool.encode().map_err(|_| native_abi::ERR_NO_MEMORY)?;
        request.try_reserve(8 + bytes.len())?;
        request.u32(interface.index).bytes(&bytes);
        let mut reply = service_rpc::call(&service, &request, Vec::new())?;
        reply.reader().finish()?;
        if reply.handles.len() != 1 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let control = Channel::from_handle(service_rpc::take_handle(&mut reply.handles, 0)?);
        control.wait_async(&self.port, key, signals::PEER_CLOSED)?;
        interface.dhcp_server = Some(PoolSession {
            key,
            control,
            applied: Some(pool),
            pending_pause: None,
        });
        Ok(())
    }

    pub(super) fn pool_closed(&self, key: u64) -> bool {
        let mut inner = self.inner.lock();
        let Some(interface) = inner.interfaces.iter_mut().find(|interface| {
            interface
                .dhcp_server
                .as_ref()
                .is_some_and(|session| session.key == key)
        }) else {
            return false;
        };
        let Some(session) = interface.dhcp_server.take() else {
            return false;
        };
        if session.applied.is_some() {
            self.queue_application(interface);
            Self::changed(&mut inner);
        }
        true
    }

    /// Read a page through the authorized settings endpoint without holding the configuration lock.
    pub(super) fn pool_bindings(
        &self,
        name: &[u8],
        after: Ipv4Addr,
    ) -> Result<(u64, Vec<u8>), Status> {
        let (generation, control) = {
            let inner = self.inner.lock();
            let interface = inner
                .interfaces
                .iter()
                .find(|interface| interface.name == name)
                .ok_or(native_abi::ERR_NOT_FOUND)?;
            if interface.retry_at.is_some() {
                return Err(native_abi::ERR_SHOULD_WAIT);
            }
            let session = interface
                .dhcp_server
                .as_ref()
                .filter(|session| session.applied.is_some())
                .ok_or(native_abi::ERR_NOT_FOUND)?;
            (
                inner.generation,
                session.control.duplicate(native_abi::rights::SAME)?,
            )
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
