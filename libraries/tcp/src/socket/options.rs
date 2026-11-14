use super::*;

impl<'a> Socket<'a> {
    /// 实际bound-device机制；passive child与所有补位handle由调用者唯一policy初始化。
    pub fn set_bound_interface(&mut self, index: Option<u32>) {
        self.bound_interface = index;
    }

    /// Select the output header required by the current endpoint key policy.
    /// The caller supplies the key when encoding each output segment.
    pub fn set_authentication(&mut self, authentication: Authentication) {
        self.authentication = authentication;
    }

    /// Return SND.UNA and RCV.NXT for authentication sequence checkpoints.
    /// Only use RCV.NXT after a peer SYN establishes its sequence space.
    pub fn authentication_checkpoints(&self) -> (TcpSeqNumber, TcpSeqNumber) {
        (self.local_seq_no, self.remote_seq_no + self.rx_buffer.len())
    }

    /// actual bound-device mechanism的只读投影，供出口route选择。
    pub fn bound_interface(&self) -> Option<u32> {
        self.bound_interface
    }

    /// Create a socket using the given buffers.
    pub fn new<T>(rx_buffer: T, tx_buffer: T) -> Socket<'a>
    where
        T: Into<SocketBuffer<'a>>,
    {
        let (rx_buffer, tx_buffer) = (rx_buffer.into(), tx_buffer.into());
        let rx_capacity = rx_buffer.capacity();

        // From RFC 1323:
        // [...] the above constraints imply that 2 * the max window size must be less
        // than 2**31 [...] Thus, the shift count must be limited to 14 (which allows
        // windows of 2**30 = 1 Gbyte).
        // Prevent overflow
        if rx_capacity > (1 << 30) {
            panic!("receiving buffer too large, cannot exceed 1 GiB")
        }
        let rx_cap_log2 = mem::size_of::<usize>() * 8 - rx_capacity.leading_zeros() as usize;

        Socket {
            state: State::Closed,
            failure_reason: None,
            syn_retries: 6,
            user_timeout: None,
            pending_since: None,
            retransmits: 0,
            total_retransmissions: 0,
            zero_window_probes: 0,
            zero_window_since: None,
            orphaned: false,
            fin_wait2_since: None,
            linger2: 0,
            timer: Timer::new(),
            rtte: RttEstimator::default(),
            assembler: Assembler::new(),
            sack: crate::loss::Sack::new(),
            tx_buffer,
            rx_buffer,
            rx_fin_received: false,
            timeout: None,
            keep_alive: None,
            keep_alive_interval: Duration::from_secs(75),
            keep_alive_count: 9,
            keep_alive_probes: 0,
            keep_alive_at: None,
            activity_at: None,
            hop_limit: None,
            bound_interface: None,
            user_mss: None,
            advertised_mss: None,
            route_send_mss: usize::MAX,
            sender_mss_limit: usize::MAX,
            receive_window: usize::MAX,
            route_initial_rtt: 0,
            send_mss: DEFAULT_MSS,
            listen_endpoint: IpListenEndpoint::default(),
            tuple: None,
            local_seq_no: TcpSeqNumber::default(),
            remote_seq_no: TcpSeqNumber::default(),
            remote_last_seq: TcpSeqNumber::default(),
            send_seq_no: TcpSeqNumber::default(),
            remote_last_ack: None,
            remote_last_win: 0,
            remote_win_len: 0,
            remote_win_shift: rx_cap_log2.saturating_sub(16) as u8,
            remote_win_scale: None,
            remote_has_sack: false,
            authentication: Authentication::None,
            remote_mss: DEFAULT_MSS,
            remote_last_ts: None,
            local_rx_last_ack: None,
            local_rx_last_seq: None,
            local_rx_dup_acks: 0,
            ack_delay: Some(ACK_DELAY_DEFAULT),
            ack_delay_timer: AckDelayTimer::Idle,
            challenge_ack_timer: Instant::from_secs(0),
            nagle: true,
            tsval_generator: None,
            last_remote_tsval: 0,
            congestion_controller: crate::congestion::Reno::new(),
        }
    }

    /// Enable or disable TCP Timestamp.
    pub fn set_tsval_generator(&mut self, generator: Option<TcpTimestampGenerator>) {
        self.tsval_generator = generator;
    }

    /// Return whether TCP Timestamp is enabled.
    pub fn timestamp_enabled(&self) -> bool {
        self.tsval_generator.is_some()
    }

    /// 冻结发送估计器的 RTO、平滑 RTT 和 RTT 偏差，单位为微秒。
    /// 尚未采样时 RTT 与偏差为零。
    pub fn round_trip_times(&self) -> [u32; 3] {
        [self.rtte.rto, self.rtte.srtt, self.rtte.rttvar].map(|millis| millis.saturating_mul(1000))
    }

    /// 返回当前发送窗口和慢启动阈值，以拥塞控制实际使用的段大小为单位。
    /// 阈值缺席时仍处于无阈值的初始慢启动。
    pub fn congestion_windows(&self) -> (u32, Option<u32>) {
        self.congestion_controller.windows()
    }

    /// 累计成功提交的重传段数与当前连续超时次数。
    pub fn retransmissions(&self) -> (u32, u8) {
        (self.total_retransmissions, self.retransmits)
    }

    /// 返回协商的 SACK 支持以及收发窗口缩放值。窗口缩放缺席时返回 None。
    pub fn peer_window_options(&self) -> (bool, Option<[u8; 2]>) {
        (
            self.remote_has_sack,
            self.remote_win_scale
                .map(|scale| [scale, self.remote_win_shift]),
        )
    }

    /// 返回握手选定的本地接收 MSS 广告。
    pub fn advertised_mss(&self) -> Option<usize> {
        self.advertised_mss
    }

    /// Return the timeout duration.
    ///
    /// See also the [set_timeout](#method.set_timeout) method.
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// Return the ACK delay duration.
    ///
    /// See also the [set_ack_delay](#method.set_ack_delay) method.
    pub fn ack_delay(&self) -> Option<Duration> {
        self.ack_delay
    }

    /// Return whether Nagle's Algorithm is enabled.
    ///
    /// See also the [set_nagle_enabled](#method.set_nagle_enabled) method.
    pub fn nagle_enabled(&self) -> bool {
        self.nagle
    }

    /// Return the current window field value, including scaling according to RFC 1323.
    ///
    /// Used in internal calculations as well as packet generation.
    #[inline]
    pub(super) fn scaled_window(&self) -> u16 {
        u16::try_from(self.rx_buffer.window().min(self.receive_window) >> self.remote_win_shift)
            .unwrap_or(u16::MAX)
    }

    /// Return the last window field value, including scaling according to RFC 1323.
    ///
    /// Used in internal calculations as well as packet generation.
    ///
    /// Unlike `remote_last_win`, we take into account new packets received (but not acknowledged)
    /// since the last window update and adjust the window length accordingly. This ensures a fair
    /// comparison between the last window length and the new window length we're going to
    /// advertise.
    #[inline]
    pub(super) fn last_scaled_window(&self) -> Option<u16> {
        let last_ack = self.remote_last_ack?;
        let next_ack = self.remote_seq_no + self.rx_buffer.len();

        let last_win = (self.remote_last_win as usize) << self.remote_win_shift;
        let last_win_adjusted = last_ack + last_win - next_ack;

        Some(u16::try_from(last_win_adjusted >> self.remote_win_shift).unwrap_or(u16::MAX))
    }

    /// Set the timeout duration.
    ///
    /// A socket with a timeout duration set will abort the connection if either of the following
    /// occurs:
    ///
    ///   * After a [connect](#method.connect) call, the remote endpoint does not respond within
    ///     the specified duration;
    ///   * After establishing a connection, there is data in the transmit buffer and the remote
    ///     endpoint exceeds the specified duration between any two packets it sends;
    ///   * After enabling [keep-alive](#method.set_keep_alive), the remote endpoint exceeds
    ///     the specified duration between any two packets it sends.
    pub fn set_timeout(&mut self, duration: Option<Duration>) {
        self.timeout = duration
    }

    /// 设置原生主动SYN重试预算；setter不清除已有握手的实际重试次数。
    pub fn set_syn_retries(&mut self, retries: u8) {
        assert!((1..=127).contains(&retries));
        self.syn_retries = retries;
    }

    /// 设置原生未确认数据/zero-window/user keepalive最大期限；idle连接不因此关闭。
    pub fn set_user_timeout(&mut self, timeout: Option<Duration>) {
        self.user_timeout = timeout;
    }

    /// Set the ACK delay duration.
    ///
    /// By default, the ACK delay is set to 10ms.
    pub fn set_ack_delay(&mut self, duration: Option<Duration>) {
        self.ack_delay = duration
    }

    /// Enable or disable Nagle's Algorithm.
    ///
    /// Also known as "tinygram prevention". By default, it is enabled.
    /// Disabling it is equivalent to Linux's TCP_NODELAY flag.
    ///
    /// When enabled, Nagle's Algorithm prevents sending segments smaller than MSS if
    /// there is data in flight (sent but not acknowledged). In other words, it ensures
    /// at most only one segment smaller than MSS is in flight at a time.
    ///
    /// It ensures better network utilization by preventing sending many very small packets,
    /// at the cost of increased latency in some situations, particularly when the remote peer
    /// has ACK delay enabled.
    pub fn set_nagle_enabled(&mut self, enabled: bool) {
        self.nagle = enabled
    }

    /// Return the keep-alive interval.
    ///
    /// See also the [set_keep_alive](#method.set_keep_alive) method.
    pub fn keep_alive(&self) -> Option<Duration> {
        self.keep_alive
    }

    /// Set the keep-alive interval.
    ///
    /// An idle socket with a keep-alive interval set will transmit a "keep-alive ACK" packet
    /// every time it receives no communication during that interval. As a result, three things
    /// may happen:
    ///
    ///   * The remote endpoint is fine and answers with an ACK packet.
    ///   * The remote endpoint has rebooted and answers with an RST packet.
    ///   * The remote endpoint has crashed and does not answer.
    ///
    /// The keep-alive functionality together with the timeout functionality allows to react
    /// to these error conditions.
    pub fn set_keep_alive(&mut self, idle: Option<Duration>) {
        self.set_keep_alive_policy(idle, self.keep_alive_interval, self.keep_alive_count);
    }

    /// 同一实际keepalive机制接受native idle/interval/count；更新policy不改重传timer。
    pub fn set_keep_alive_policy(&mut self, idle: Option<Duration>, interval: Duration, count: u8) {
        assert!(count != 0 && interval.total_millis() > 0);
        let idle_changed = self.keep_alive != idle;
        self.keep_alive = idle;
        self.keep_alive_interval = interval;
        self.keep_alive_count = count;
        if idle_changed {
            self.keep_alive_at =
                idle.map(|idle| self.activity_at.map_or(Instant::ZERO, |last| last + idle));
        }
    }

    /// Return the time-to-live (IPv4) or hop limit (IPv6) value used in outgoing packets.
    ///
    /// See also the [set_hop_limit](#method.set_hop_limit) method
    pub fn hop_limit(&self) -> Option<u8> {
        self.hop_limit
    }

    /// Set the time-to-live (IPv4) or hop limit (IPv6) value used in outgoing packets.
    ///
    /// `hop_limit` is None for the default value 64, or the explicit value for the next output.
    /// IPv4 TTL validity belongs to the caller's IP policy; IPv6 permits zero.
    /// Returns unit without allocating or changing connection sequence state.
    pub fn set_hop_limit(&mut self, hop_limit: Option<u8>) {
        self.hop_limit = hop_limit
    }

    /// Set the MSS cap for the next connection. The ABI caller validates its native range.
    pub fn set_user_mss(&mut self, value: Option<u16>) {
        assert!(value != Some(0));
        self.user_mss = value;
    }

    /// Return native configured MSS for CLOSED/LISTEN, otherwise actual negotiated send MSS.
    pub fn max_segment_size(&self) -> usize {
        if matches!(self.state, State::Closed | State::Listen)
            && let Some(value) = self.user_mss
        {
            return value as usize;
        }
        self.effective_mss()
    }

    pub(super) fn effective_mss(&self) -> usize {
        // RFC 6691: the peer's MSS excludes options, whereas the payload limit does not.
        self.send_mss.saturating_sub(TcpRepr::data_option_space(
            self.tsval_generator.is_some(),
            self.authentication,
        ))
    }

    /// Negotiated sender ceiling before TCP options, independently of a next-open user cap.
    pub fn send_mss(&self) -> usize {
        self.send_mss
    }

    /// Transfer the actual closed/listener sender cache during transport storage replacement.
    pub fn set_send_mss(&mut self, value: usize) {
        assert!(value != 0);
        self.send_mss = value;
    }

    pub(super) fn update_send_mss(&mut self) {
        self.send_mss = self.remote_mss.min(self.route_send_mss);
    }

    pub(super) fn capture_mss(&mut self, cx: &impl Context, address: IpAddress) {
        let header = match address {
            IpAddress::V4(_) => 20,

            IpAddress::V6(_) => 40,
        };
        // 1. 广告来自actual FIB ADVMSS，默认下限256；不能用它增大发送方PMTU上限。
        let mtu_mss = cx.ip_mtu().saturating_sub(header + TCP_HEADER_LEN);
        let route = cx.transport_route();
        self.advertised_mss = Some(
            route
                .map_or(mtu_mss, |route| route.advertised_mss)
                .min(self.user_mss.map_or(usize::MAX, usize::from)),
        );
        self.sender_mss_limit = self.user_mss.map_or(usize::MAX, usize::from);
        self.route_send_mss = mtu_mss.min(self.sender_mss_limit);
        self.receive_window = route.map_or(usize::MAX, |route| {
            if route.receive_window == 0 {
                usize::MAX
            } else {
                route.receive_window
            }
        });
        self.route_initial_rtt = route.map_or(0, |route| route.initial_rtt);
        self.update_send_mss();
    }

    /// Return the listen endpoint
    #[inline]
    pub fn listen_endpoint(&self) -> IpListenEndpoint {
        self.listen_endpoint
    }

    /// Return the local endpoint, or None if not connected.
    #[inline]
    pub fn local_endpoint(&self) -> Option<IpEndpoint> {
        Some(self.tuple?.local)
    }

    /// Return the remote endpoint, or None if not connected.
    #[inline]
    pub fn remote_endpoint(&self) -> Option<IpEndpoint> {
        Some(self.tuple?.remote)
    }

    /// Return the connection state, in terms of the TCP state machine.
    #[inline]
    pub fn state(&self) -> State {
        self.state
    }

    /// 返回实际关闭事件的原因；读操作不消费，唯一endpoint owner负责应用错误的一次交付。
    pub fn failure_reason(&self) -> Option<FailureReason> {
        self.failure_reason
    }
}
