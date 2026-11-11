//! UDP/RAW 的独立错误记录及一次性 SO_ERROR。错误队列与普通接收 FIFO 各自消费。
use super::*;
use alloc::{collections::VecDeque, vec::Vec};
use network_protocol::{InternetError, InternetErrorOrigin};

const MAX_RECORDS: usize = 8;
const MAX_BYTES: usize = 64 * 1024;

/// 仅启用且实际收到错误时分配。限制记录数和载荷字节，防止引用流量无限占用端点存储。
pub(super) struct ErrorQueue {
    pub(super) pending: Option<SocketError>,
    records: VecDeque<(InternetError, Vec<u8>)>,
    bytes: usize,
    ipv4: bool,
    ipv6: bool,
}

impl ErrorQueue {
    pub(super) fn new() -> Self {
        Self {
            pending: None,
            records: VecDeque::new(),
            bytes: 0,
            ipv4: false,
            ipv6: false,
        }
    }
    pub(super) fn enabled(&self, ipv6: bool) -> bool {
        if ipv6 { self.ipv6 } else { self.ipv4 }
    }
    pub(super) fn set_enabled(&mut self, ipv6: bool, enabled: bool) {
        if ipv6 {
            self.ipv6 = enabled;
        } else {
            self.ipv4 = enabled;
        }
        if !enabled {
            self.records = VecDeque::new();
            self.bytes = 0;
        }
    }
    pub(super) fn ready(&self) -> bool {
        self.pending.is_some() || !self.records.is_empty()
    }
    /// 记录已匹配的发送错误。未启用扩展错误时只保留原有的关联硬错误语义。
    pub(super) fn report(&mut self, error: InternetError, quote: &[u8], hard: bool) -> bool {
        let enabled = self.enabled(match error.origin {
            InternetErrorOrigin::Local => error.destination.ip().to_canonical().is_ipv6(),
            InternetErrorOrigin::Icmpv4 => false,
            InternetErrorOrigin::Icmpv6 => true,
        });
        if hard || enabled && error.origin != InternetErrorOrigin::Local {
            self.pending = Some(error.error);
        }
        if !enabled || self.records.len() == MAX_RECORDS || quote.len() > MAX_BYTES - self.bytes {
            return false;
        }
        let mut bytes = Vec::new();
        if bytes.try_reserve_exact(quote.len()).is_err()
            || bytes.capacity() > MAX_BYTES - self.bytes
            || self.records.try_reserve_exact(1).is_err()
        {
            return false;
        }
        bytes.extend_from_slice(quote);
        self.bytes += bytes.capacity();
        self.records.push_back((error, bytes));
        true
    }
    /// 本机长度检查失败。同步 errno 仍由发送调用返回，队列记录携带实际上限。
    pub(super) fn local_mtu(&mut self, destination: InetAddress, mtu: usize) -> bool {
        self.report(
            InternetError {
                error: SocketError::MessageTooLarge,
                origin: InternetErrorOrigin::Local,
                kind: 0,
                code: 0,
                info: mtu as u32,
                destination,
                offender: None,
            },
            &[],
            false,
        )
    }
    /// 取队首及实际可见长度。弹出 ICMP 后用下一条 ICMP 更新 SO_ERROR。
    /// # Errors
    /// 无记录返回 Again。接收缓冲在取得队列前已完整准备。
    pub(super) fn receive(
        &mut self,
        output: &mut ReceiveBuffer,
    ) -> Result<(usize, InternetError), SocketError> {
        let (error, bytes) = self.records.pop_front().ok_or(SocketError::Again)?;
        self.bytes -= bytes.capacity();
        if let Some((next, _)) = self
            .records
            .front()
            .filter(|(next, _)| next.origin != InternetErrorOrigin::Local)
        {
            self.pending = Some(next.error);
        } else if error.origin != InternetErrorOrigin::Local {
            self.pending = None;
        }
        output.append(&bytes);
        Ok((bytes.len(), error))
    }
}

impl InetSocket {
    /// 非阻塞消费 UDP/RAW 的一条错误。
    /// # Errors
    /// 空队列返回 Again。流式端点不提供数据报错误队列。
    pub(crate) fn receive_error(
        &self,
        output: &mut ReceiveBuffer,
    ) -> Result<(usize, InternetError), SocketError> {
        let _operation = self.open_operation()?;
        let mut network = self.stack()?.lock()?;
        network.error_queue_mut(self.endpoint)?.receive(output)
    }

    pub(crate) fn set_receive_error(&self, ipv6: bool, enabled: bool) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        let mut network = self.stack()?.lock()?;
        network.check_receive_error_family(self.endpoint, ipv6)?;
        network
            .error_queue_mut(self.endpoint)?
            .set_enabled(ipv6, enabled);
        Ok(())
    }
}

impl NetworkStack {
    pub(super) fn check_receive_error_family(
        &self,
        endpoint: InetEndpoint,
        ipv6: bool,
    ) -> Result<(), SocketError> {
        let family = match endpoint {
            InetEndpoint::Udp(id) => self
                .udp_endpoints
                .get(&id)
                .ok_or(SocketError::NotConnected)?
                .is_ipv6(),
            InetEndpoint::Raw(id) => self
                .raw_endpoints
                .get(&id)
                .ok_or(SocketError::NotConnected)?
                .is_ipv6(),
            InetEndpoint::Tcp(_) => return Err(SocketError::OperationNotSupported),
        };
        if ipv6 && !family {
            return Err(SocketError::OperationNotSupported);
        }
        Ok(())
    }

    pub(super) fn error_queue_mut(
        &mut self,
        endpoint: InetEndpoint,
    ) -> Result<&mut ErrorQueue, SocketError> {
        match endpoint {
            InetEndpoint::Udp(id) => self
                .udp_endpoints
                .get_mut(&id)
                .map(|state| &mut state.errors),
            InetEndpoint::Raw(id) => self
                .raw_endpoints
                .get_mut(&id)
                .map(|state| &mut state.errors),
            InetEndpoint::Tcp(_) => return Err(SocketError::OperationNotSupported),
        }
        .ok_or(SocketError::NotConnected)
    }

    pub(super) fn receive_error_enabled(
        &self,
        endpoint: InetEndpoint,
        ipv6: bool,
    ) -> Result<bool, SocketError> {
        self.check_receive_error_family(endpoint, ipv6)?;
        let errors = match endpoint {
            InetEndpoint::Udp(id) => &self.udp_endpoints[&id].errors,
            InetEndpoint::Raw(id) => &self.raw_endpoints[&id].errors,
            InetEndpoint::Tcp(_) => return Err(SocketError::OperationNotSupported),
        };
        Ok(errors.enabled(ipv6))
    }
}
