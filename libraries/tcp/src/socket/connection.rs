use super::*;

impl<'a> Socket<'a> {
    /// Whether a validated IP error's quoted TCP identity belongs to this live flow.
    /// `local`/`remote` are the original output tuple, `sequence` its quoted sequence and `interface` the receiving zone.
    /// Returns false for listeners, retired tuples, another device or a sequence outside SND.UNA..SND.NXT.
    /// No state, timer or sequence changes; checksum and IP quote validation belong to the caller.
    pub fn accepts_error(
        &self,
        local: IpEndpoint,
        remote: IpEndpoint,
        sequence: TcpSeqNumber,
        interface: u32,
    ) -> bool {
        !matches!(self.state, State::Closed | State::Listen | State::TimeWait)
            && self
                .tuple
                .is_some_and(|tuple| tuple.local == local && tuple.remote == remote)
            && self.bound_interface.is_none_or(|bound| bound == interface)
            && sequence >= self.local_seq_no
            && sequence < self.send_seq_no
    }

    pub(super) fn reset(&mut self) {
        let rx_cap_log2 =
            mem::size_of::<usize>() * 8 - self.rx_buffer.capacity().leading_zeros() as usize;

        self.state = State::Closed;
        self.failure_reason = None;
        self.pending_since = None;
        self.retransmits = 0;
        self.zero_window_probes = 0;
        self.zero_window_since = None;
        self.orphaned = false;
        self.fin_wait2_since = None;
        self.keep_alive_probes = 0;
        self.keep_alive_at = None;
        self.activity_at = None;
        self.timer = Timer::new();
        self.rtte = RttEstimator::default();
        self.congestion_controller = crate::congestion::Reno::new();
        self.assembler = Assembler::new();
        self.sack = crate::loss::Sack::new();
        self.remote_has_sack = false;
        self.local_rx_dup_acks = 0;
        self.local_rx_last_ack = None;
        self.tx_buffer.clear();
        self.rx_buffer.clear();
        self.rx_fin_received = false;
        self.listen_endpoint = IpListenEndpoint::default();
        self.tuple = None;
        self.local_seq_no = TcpSeqNumber::default();
        self.remote_seq_no = TcpSeqNumber::default();
        self.remote_last_seq = TcpSeqNumber::default();
        self.send_seq_no = TcpSeqNumber::default();
        self.remote_last_ack = None;
        self.remote_last_win = 0;
        self.remote_win_len = 0;
        self.remote_win_scale = None;
        self.remote_win_shift = rx_cap_log2.saturating_sub(16) as u8;
        self.remote_mss = DEFAULT_MSS;
        self.advertised_mss = None;
        self.route_send_mss = usize::MAX;
        self.sender_mss_limit = usize::MAX;
        self.receive_window = usize::MAX;
        self.route_initial_rtt = 0;
        self.remote_last_ts = None;
        self.ack_delay_timer = AckDelayTimer::Idle;
        self.challenge_ack_timer = Instant::from_secs(0);
    }

    /// Start listening on the given endpoint.
    ///
    /// This function returns `Err(Error::InvalidState)` if the socket was already open
    /// (see [is_open](#method.is_open)), and `Err(Error::Unaddressable)`
    /// if the port in the given endpoint is zero.
    pub fn listen<T>(&mut self, local_endpoint: T) -> Result<(), ListenError>
    where
        T: Into<IpListenEndpoint>,
    {
        let local_endpoint = local_endpoint.into();
        if local_endpoint.port == 0 {
            return Err(ListenError::Unaddressable);
        }

        if self.is_open() {
            // If we were already listening to same endpoint there is nothing to do; exit early.
            //
            // In the past listening on an socket that was already listening was an error,
            // however this makes writing an acceptor loop with multiple sockets impossible.
            // Without this early exit, if you tried to listen on a socket that's already listening you'll
            // immediately get an error. The only way around this is to abort the socket first
            // before listening again, but this means that incoming connections can actually
            // get aborted between the abort() and the next listen().
            if matches!(self.state, State::Listen) && self.listen_endpoint == local_endpoint {
                return Ok(());
            } else {
                return Err(ListenError::InvalidState);
            }
        }

        self.reset();
        self.listen_endpoint = local_endpoint;
        self.tuple = None;
        self.set_state(State::Listen);
        Ok(())
    }

    /// Connect to a given endpoint.
    ///
    /// The caller must reserve a nonzero local port before opening. If the local address
    /// is omitted, Context selects it from the same authoritative route decision.
    /// The local address may optionally be provided.
    ///
    /// This function returns an error if the socket was open; see [is_open](#method.is_open).
    /// It also returns an error if the local or remote port is zero, or if the remote address
    /// is unspecified.
    pub fn connect<T, U>(
        &mut self,
        cx: &mut impl Context,
        remote_endpoint: T,
        local_endpoint: U,
    ) -> Result<(), ConnectError>
    where
        T: Into<IpEndpoint>,
        U: Into<IpListenEndpoint>,
    {
        let remote_endpoint: IpEndpoint = remote_endpoint.into();
        let local_endpoint: IpListenEndpoint = local_endpoint.into();

        if self.is_open() {
            return Err(ConnectError::InvalidState);
        }
        if remote_endpoint.port == 0 || remote_endpoint.addr.is_unspecified() {
            return Err(ConnectError::Unaddressable);
        }
        if local_endpoint.port == 0 {
            return Err(ConnectError::Unaddressable);
        }

        // If local address is not provided, choose it automatically.
        let local_endpoint = IpEndpoint {
            addr: match local_endpoint.addr {
                Some(addr) => {
                    if addr.is_unspecified() {
                        return Err(ConnectError::Unaddressable);
                    }
                    addr
                }
                None => cx
                    .source_address(remote_endpoint.addr)
                    .ok_or(ConnectError::Unaddressable)?,
            },
            port: local_endpoint.port,
        };

        if local_endpoint.addr.is_ipv4() != remote_endpoint.addr.is_ipv4() {
            return Err(ConnectError::Unaddressable);
        }

        self.reset();
        self.tuple = Some(Tuple {
            local: local_endpoint,
            remote: remote_endpoint,
        });
        self.remote_mss = if local_endpoint.addr.is_ipv4() {
            536
        } else {
            1220
        };
        self.capture_mss(cx, local_endpoint.addr);
        self.set_state(State::SynSent);
        self.remote_last_ts = Some(cx.now());
        self.record_activity(cx.now());
        self.pending_since = Some(cx.now());
        self.timer
            .set_for_retransmit(cx.now(), self.rtte.retransmission_timeout());

        let seq = self.initial_seq(cx);
        self.local_seq_no = seq;
        self.remote_last_seq = seq;
        self.send_seq_no = seq;
        Ok(())
    }

    pub(super) fn initial_seq(&self, cx: &mut impl Context) -> TcpSeqNumber {
        let tuple = self
            .tuple
            .expect("initial sequence requires a frozen tuple");
        cx.initial_sequence(tuple.local, tuple.remote)
    }

    /// Close the transmit half of the full-duplex connection.
    ///
    /// Note that there is no corresponding function for the receive half of the full-duplex
    /// connection; only the remote end can close it. If you no longer wish to receive any
    /// data and would like to reuse the socket right away, use [abort](#method.abort).
    pub fn close(&mut self) {
        match self.state {
            // In the LISTEN state there is no established connection.
            State::Listen => self.set_state(State::Closed),
            // In the SYN-SENT state the remote endpoint is not yet synchronized and, upon
            // receiving an RST, will abort the connection.
            State::SynSent => self.set_state(State::Closed),
            // In the SYN-RECEIVED, ESTABLISHED and CLOSE-WAIT states the transmit half
            // of the connection is open, and needs to be explicitly closed with a FIN.
            State::SynReceived | State::Established => self.set_state(State::FinWait1),
            State::CloseWait => self.set_state(State::LastAck),
            // In the FIN-WAIT-1, FIN-WAIT-2, CLOSING, LAST-ACK, TIME-WAIT and CLOSED states,
            // the transmit half of the connection is already closed, and no further
            // action is needed.
            State::FinWait1
            | State::FinWait2
            | State::Closing
            | State::TimeWait
            | State::LastAck
            | State::Closed => (),
        }
    }

    /// 消费最后application owner，关闭写侧；FIN_WAIT2时钟只约束已orphan的连接。
    /// timestamp为本次实际fput事件时间。无分配，不丢失待发FIN或已开始的TIME_WAIT。
    pub fn close_orphan(&mut self, timestamp: Instant) {
        self.orphaned = true;
        self.close();
        if self.state == State::FinWait2 && self.fin_wait2_since.is_none() {
            self.fin_wait2_since = Some(timestamp);
        }
        if self.state == State::FinWait2 {
            self.maintain(timestamp);
        }
    }

    /// 设置原生TCP_LINGER2配置，caller已归一化为-1..120秒；不改变活连接SHUT_WR权限。
    pub fn set_linger2(&mut self, seconds: i32) {
        assert!((-1..=120).contains(&seconds));
        self.linger2 = seconds;
    }

    /// Aborts the connection, if any.
    ///
    /// This function instantly closes the socket. One reset packet will be sent to the remote
    /// endpoint.
    ///
    /// In terms of the TCP state machine, the socket may be in any state and is moved to
    /// the `CLOSED` state.
    pub fn abort(&mut self) {
        self.set_state(State::Closed);
    }

    /// 取走唯一待发reset并退休实际tuple；发送队列不得继续持有TCP或端口身份。
    /// 未关闭或没有待发tuple时返回None。caller负责有界报文准入；失败不恢复连接。
    pub fn take_reset(&mut self) -> Option<(IpRepr, TcpRepr<'static>)> {
        let packet = self.reset_packet()?;
        self.tuple = None;
        self.timer = Timer::new();

        Some(packet)
    }

    pub(super) fn reset_packet(&self) -> Option<(IpRepr, TcpRepr<'static>)> {
        if self.state != State::Closed {
            return None;
        }
        let tuple = self.tuple?;
        let window_end = self.local_seq_no + self.remote_win_len;
        // Linux tcp_acceptable_seq允许窗口缩小时一个scale单位的舍入误差。
        let seq_number = if window_end >= self.send_seq_no
            || self
                .remote_win_scale
                .is_some_and(|scale| self.send_seq_no - window_end < 1usize << scale)
        {
            self.send_seq_no
        } else {
            window_end
        };
        let repr = TcpRepr {
            src_port: tuple.local.port,
            dst_port: tuple.remote.port,
            control: TcpControl::Rst,
            seq_number,
            ack_number: Some(self.remote_seq_no + self.rx_buffer.len()),
            window_len: 0,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None; 4],
            timestamp: None,
            md5: self.md5,
            payload: &[],
        };
        Some((
            IpRepr::new(
                tuple.local.addr,
                tuple.remote.addr,
                6,
                repr.buffer_len(),
                self.hop_limit,
            ),
            repr,
        ))
    }

    /// Return whether the socket is passively listening for incoming connections.
    ///
    /// In terms of the TCP state machine, the socket must be in the `LISTEN` state.
    #[inline]
    pub fn is_listening(&self) -> bool {
        self.state == State::Listen
    }

    /// Return whether the socket is open.
    ///
    /// This function returns true if the socket will process incoming or dispatch outgoing
    /// packets. Note that this does not mean that it is possible to send or receive data through
    /// the socket; for that, use [can_send](#method.can_send) or [can_recv](#method.can_recv).
    ///
    /// In terms of the TCP state machine, the socket must not be in the `CLOSED`
    /// or `TIME-WAIT` states.
    #[inline]
    pub fn is_open(&self) -> bool {
        !matches!(self.state, State::Closed | State::TimeWait)
    }

    /// Return whether a connection is active.
    ///
    /// This function returns true if the socket is actively exchanging packets with
    /// a remote endpoint. Note that this does not mean that it is possible to send or receive
    /// data through the socket; for that, use [can_send](#method.can_send) or
    /// [can_recv](#method.can_recv).
    ///
    /// If a connection is established, [abort](#method.close) will send a reset to
    /// the remote endpoint.
    ///
    /// In terms of the TCP state machine, the socket must not be in the `CLOSED`, `TIME-WAIT`,
    /// or `LISTEN` state.
    #[inline]
    pub fn is_active(&self) -> bool {
        !matches!(self.state, State::Closed | State::TimeWait | State::Listen)
    }

    /// Return whether the transmit half of the full-duplex connection is open.
    ///
    /// This function returns true if it's possible to send data and have it arrive
    /// to the remote endpoint. However, it does not make any guarantees about the state
    /// of the transmit buffer, and even if it returns true, [send](#method.send) may
    /// not be able to enqueue any octets.
    ///
    /// In terms of the TCP state machine, the socket must be in the `ESTABLISHED` or
    /// `CLOSE-WAIT` state.
    #[inline]
    pub fn may_send(&self) -> bool {
        match self.state {
            State::Established => true,
            // In CLOSE-WAIT, the remote endpoint has closed our receive half of the connection
            // but we still can transmit indefinitely.
            State::CloseWait => true,
            _ => false,
        }
    }

    /// Return whether the receive half of the full-duplex connection is open.
    ///
    /// This function returns true if it's possible to receive data from the remote endpoint.
    /// It will return true while there is data in the receive buffer, and if there isn't,
    /// as long as the remote endpoint has not closed the connection.
    ///
    /// In terms of the TCP state machine, the socket must be in the `ESTABLISHED`,
    /// `FIN-WAIT-1`, or `FIN-WAIT-2` state, or have data in the receive buffer instead.
    #[inline]
    pub fn may_recv(&self) -> bool {
        match self.state {
            State::Established => true,
            // In FIN-WAIT-1/2, we have closed our transmit half of the connection but
            // we still can receive indefinitely.
            State::FinWait1 | State::FinWait2 => true,
            // If we have something in the receive buffer, we can receive that.
            _ if self.can_recv() => true,
            _ => false,
        }
    }

    /// Check whether the transmit half of the full-duplex connection is open
    /// (see [may_send](#method.may_send)), and the transmit buffer is not full.
    #[inline]
    pub fn can_send(&self) -> bool {
        if !self.may_send() {
            return false;
        }

        !self.tx_buffer.is_full()
    }

    /// Return the maximum number of bytes inside the recv buffer.
    #[inline]
    pub fn recv_capacity(&self) -> usize {
        self.rx_buffer.capacity()
    }

    /// Return the maximum number of bytes inside the transmit buffer.
    #[inline]
    pub fn send_capacity(&self) -> usize {
        self.tx_buffer.capacity()
    }

    /// Check whether the receive buffer is not empty.
    #[inline]
    pub fn can_recv(&self) -> bool {
        !self.rx_buffer.is_empty()
    }

    pub(super) fn set_state(&mut self, state: State) {
        if self.state != state {
            net_trace!("state={}=>{}", self.state, state);
        }

        if matches!(state, State::Closed | State::Listen | State::TimeWait) {
            // No data recovery survives tuple retirement; a live descriptor may still retain RX/EOF.
            self.sack = crate::loss::Sack::new();
        }
        self.state = state;
    }
}
