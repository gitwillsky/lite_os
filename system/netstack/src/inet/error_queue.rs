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
    // 两族分别记录 RFC 4884 选项。只在错误入队时决定是否提供扩展位置，关闭选项不会改写旧记录。
    extensions: [bool; 2],
}

impl ErrorQueue {
    pub(super) fn new() -> Self {
        Self {
            pending: None,
            records: VecDeque::new(),
            bytes: 0,
            ipv4: false,
            ipv6: false,
            extensions: [false; 2],
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
    pub(super) fn report(&mut self, mut error: InternetError, quote: &[u8], hard: bool) -> bool {
        let ipv6 = match error.origin {
            InternetErrorOrigin::Local => error.destination.ip().to_canonical().is_ipv6(),
            InternetErrorOrigin::Icmpv4 => false,
            InternetErrorOrigin::Icmpv6 => true,
        };
        let enabled = self.enabled(ipv6);
        if hard || enabled && error.origin != InternetErrorOrigin::Local {
            self.pending = Some(error.error);
        }
        if !enabled || self.records.len() == MAX_RECORDS || quote.len() > MAX_BYTES - self.bytes {
            return false;
        }
        if !self.extensions[usize::from(ipv6)] {
            error.extension = None;
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
        self.local_error(destination, SocketError::MessageTooLarge, mtu as u32, false)
    }

    /// 将本机发送失败记录到原目的。延迟失败可同时通知与原目的仍关联的 socket。
    pub(super) fn local_error(
        &mut self,
        destination: InetAddress,
        error: SocketError,
        info: u32,
        connected: bool,
    ) -> bool {
        self.report(
            InternetError {
                error,
                origin: InternetErrorOrigin::Local,
                kind: 0,
                code: 0,
                info,
                destination,
                offender: None,
                extension: None,
                packet_info: None,
            },
            &[],
            connected,
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
        let (queue, options) = network.error_queue_mut(self.endpoint)?;
        let (length, mut error) = queue.receive(output)?;
        error.packet_info = options.receive_packet_info(error.packet_info);
        Ok((length, error))
    }

    pub(crate) fn set_receive_error(&self, ipv6: bool, enabled: bool) -> Result<(), SocketError> {
        self.set_error_option(ipv6, enabled, false)
    }

    /// 控制同族错误记录的扩展位置及校验标志；不启用或清除错误队列。
    /// # Errors
    /// 端点关闭、地址族不适用或传输不支持数据报错误。
    pub(crate) fn set_receive_error_extensions(
        &self,
        ipv6: bool,
        enabled: bool,
    ) -> Result<(), SocketError> {
        self.set_error_option(ipv6, enabled, true)
    }

    fn set_error_option(
        &self,
        ipv6: bool,
        enabled: bool,
        extensions: bool,
    ) -> Result<(), SocketError> {
        let _operation = self.open_operation()?;
        let mut network = self.stack()?.lock()?;
        network.check_receive_error_family(self.endpoint, ipv6)?;
        let (queue, _) = network.error_queue_mut(self.endpoint)?;
        if extensions {
            queue.extensions[usize::from(ipv6)] = enabled;
        } else {
            queue.set_enabled(ipv6, enabled);
        }
        Ok(())
    }
}

impl NetworkStack {
    /// 每接口同一批失败只扫描一次发送队列，保留当前仍有其他路径的报文。
    /// # Returns
    /// 有探测失败时返回 true，让轮询者继续推进等待备用路径的报文。
    pub(super) fn retire_failed_neighbor_datagrams<A: Into<core::net::IpAddr>, const N: usize>(
        &mut self,
        position: usize,
        failed: [Option<A>; N],
        timestamp: Instant,
    ) -> bool {
        if !failed.iter().any(Option::is_some) {
            return false;
        }
        let failed = failed.map(|address| address.map(Into::into));
        let interface = self.interfaces[position].index;
        let dropped = super::udp::neighbor_failed(self, interface, &failed, timestamp)
            + self.raw_neighbor_failed(interface, &failed, timestamp);
        for _ in 0..dropped {
            self.interfaces[position].device.record_transmit_drop();
        }
        true
    }

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
    ) -> Result<(&mut ErrorQueue, &super::options::InetSocketOptions), SocketError> {
        match endpoint {
            InetEndpoint::Udp(id) => self
                .udp_endpoints
                .get_mut(&id)
                .map(|state| (&mut state.errors, &state.options)),
            InetEndpoint::Raw(id) => self
                .raw_endpoints
                .get_mut(&id)
                .map(|state| (&mut state.errors, &state.options)),
            InetEndpoint::Tcp(_) => return Err(SocketError::OperationNotSupported),
        }
        .ok_or(SocketError::NotConnected)
    }

    pub(super) fn receive_error_enabled(
        &self,
        endpoint: InetEndpoint,
        ipv6: bool,
        extensions: bool,
    ) -> Result<bool, SocketError> {
        self.check_receive_error_family(endpoint, ipv6)?;
        let errors = match endpoint {
            InetEndpoint::Udp(id) => &self.udp_endpoints[&id].errors,
            InetEndpoint::Raw(id) => &self.raw_endpoints[&id].errors,
            InetEndpoint::Tcp(_) => return Err(SocketError::OperationNotSupported),
        };
        Ok(if extensions {
            errors.extensions[usize::from(ipv6)]
        } else {
            errors.enabled(ipv6)
        })
    }
}
