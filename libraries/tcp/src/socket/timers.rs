use super::*;

impl<'a> Socket<'a> {
    pub(super) fn keep_alive_eligible(&self) -> bool {
        self.keep_alive.is_some()
            && matches!(self.state, State::Established | State::CloseWait)
            && self.tx_buffer.is_empty()
            && self.send_seq_no == self.local_seq_no
    }

    pub(super) fn keep_alive_due(&self, timestamp: Instant) -> bool {
        self.keep_alive_eligible()
            && self
                .keep_alive_at
                .is_some_and(|deadline| deadline <= timestamp)
    }

    pub(super) fn record_activity(&mut self, timestamp: Instant) {
        self.activity_at = Some(timestamp);
        self.keep_alive_probes = 0;
        self.keep_alive_at = self.keep_alive.map(|idle| timestamp + idle);
    }

    pub(super) fn finish_keep_alive(&mut self, timestamp: Instant, congested: bool) {
        if congested {
            self.keep_alive_at = Some(timestamp + Duration::from_millis(500));
        } else {
            self.keep_alive_probes = self.keep_alive_probes.saturating_add(1);
            self.keep_alive_at = Some(timestamp + self.keep_alive_interval);
        }
    }

    /// 将本次真实 FIB/邻居失败或资源拥塞交给同一探测 owner，不需要伪造发包。
    /// `timestamp` 是本轮时刻，`failure` 来自实际输出路径；非到期探测不改变状态。
    /// 不分配、不产生 socket 错误，也不推进 stream 序号。
    pub fn output_unavailable(&mut self, timestamp: Instant, failure: OutputFailure) {
        let congested = failure == OutputFailure::LocalCongestion;
        if self.keep_alive_due(timestamp) {
            self.finish_keep_alive(timestamp, congested);
        }
        if self.timer.should_zero_window_probe(timestamp) {
            self.timer
                .finish_zero_window_probe(timestamp, !congested, self.probe_user_deadline());
        }
    }

    pub(super) fn timed_out(&self, timestamp: Instant) -> bool {
        match (self.remote_last_ts, self.timeout) {
            (Some(remote_last_ts), Some(timeout)) => timestamp >= remote_last_ts + timeout,
            (_, _) => false,
        }
    }

    pub(super) fn pending_sequence(&self) -> bool {
        matches!(
            self.state,
            State::SynSent | State::SynReceived | State::FinWait1 | State::LastAck | State::Closing
        ) || !self.tx_buffer.is_empty()
            || self.send_seq_no > self.local_seq_no
    }

    pub(super) fn model_timeout(boundary: u8, base: u64) -> Duration {
        let mut delay = base;
        let mut total = 0u64;
        for _ in 0..=boundary {
            total += delay;
            delay = (delay * 2).min(u64::from(RTTE_MAX_RTO));
        }
        Duration::from_millis(total)
    }

    pub(super) fn retry_expired(&self, timestamp: Instant) -> bool {
        if self.retransmits == 0 {
            return false;
        }
        let Some(start) = self.pending_since else {
            return false;
        };
        let handshake = matches!(self.state, State::SynSent | State::SynReceived);
        let boundary = if self.state == State::SynReceived && self.listen_endpoint.port != 0 {
            5 // 原生被动request的sysctl_tcp_synack_retries，与主动SYNCNT不同。
        } else if handshake {
            self.syn_retries
        } else if self.orphaned {
            // native默认orphan_retries=0：RTO未到120s时使用安全预算8，否则0。
            if self.rtte.retransmission_timeout() < Duration::from_millis(RTTE_MAX_RTO as u64) {
                8
            } else {
                0
            }
        } else {
            15
        };
        let limit = if self.state == State::SynSent {
            u16::from(boundary) + 4
        } else {
            u16::from(boundary)
        };
        (handshake && u16::from(self.retransmits) >= limit)
            || timestamp
                >= start
                    + self.user_timeout.unwrap_or_else(|| {
                        Self::model_timeout(boundary, if handshake { 1000 } else { 200 })
                    })
    }

    pub(super) fn retry_delay(&self, timestamp: Instant) -> Duration {
        let delay = self.rtte.retransmission_timeout();
        if self.retransmits != 0
            && let (Some(start), Some(timeout)) = (self.pending_since, self.user_timeout)
        {
            return delay.min(Duration::from_millis(
                (start + timeout)
                    .total_millis()
                    .saturating_sub(timestamp.total_millis())
                    .max(2) as u64,
            ));
        }
        delay
    }

    pub(super) fn keep_alive_expired(&self, timestamp: Instant) -> bool {
        if !self.keep_alive_due(timestamp) {
            return false;
        }
        match self.user_timeout {
            Some(timeout) => {
                self.keep_alive_probes > 0
                    && self
                        .activity_at
                        .is_some_and(|last| timestamp >= last + timeout)
            }
            None => self.keep_alive_probes >= self.keep_alive_count,
        }
    }

    pub(super) fn probe_user_deadline(&self) -> Option<Instant> {
        self.zero_window_since
            .zip(self.user_timeout)
            .map(|(start, timeout)| start + timeout)
    }

    pub(super) fn zero_window_due(&self, timestamp: Instant) -> bool {
        matches!(self.timer, Timer::ZeroWindowProbe { expires_at: Some(deadline), .. } if timestamp >= deadline)
    }

    pub(super) fn zero_window_expired(&self, timestamp: Instant) -> bool {
        if !self.timer.is_zero_window_probe() {
            return false;
        }
        if self
            .probe_user_deadline()
            .is_some_and(|deadline| timestamp >= deadline)
        {
            return true;
        }
        let limit = if self.orphaned {
            let Timer::ZeroWindowProbe { delay, pending, .. } = self.timer else {
                unreachable!()
            };
            let delay = if pending {
                (delay * 2).min(Duration::from_millis(RTTE_MAX_RTO as _))
            } else {
                delay
            };
            // tcp_orphan_retries(alive)：退避到max且没有soft-error时默认预算0，否则安全预算8。
            if delay < Duration::from_millis(RTTE_MAX_RTO as _) {
                8
            } else {
                0
            }
        } else {
            15
        };
        self.zero_window_due(timestamp) && self.zero_window_probes >= limit
    }

    pub(super) fn fin_wait2_deadline(&self) -> Option<Instant> {
        if !self.orphaned || self.state != State::FinWait2 {
            return None;
        }
        let start = self.fin_wait2_since.unwrap_or(Instant::ZERO);
        if self.linger2 < 0 {
            return Some(start);
        }
        let seconds = if self.linger2 == 0 {
            60
        } else {
            self.linger2 as u64
        };
        // tcp_fin_time最短3.5*RTO；配置过短也不能越过真实未确认FIN的退避安全期。
        let timeout = Duration::from_millis(
            (seconds * 1000).max(self.rtte.retransmission_timeout().total_millis() * 7 / 2),
        );
        Some(start + timeout)
    }

    /// 与route/neighbor/设备独立的纯时间deadline；阻塞输出不得冻结退出与TIME_WAIT。
    pub fn maintenance_deadline(&self) -> Option<Instant> {
        if self.state == State::TimeWait {
            return match self.timer {
                Timer::Close { expires_at } => Some(expires_at),
                _ => None,
            };
        }
        if self.tuple.is_none() || matches!(self.state, State::Closed | State::Listen) {
            return None;
        }
        if self.orphaned && self.state == State::FinWait2 {
            return self.fin_wait2_deadline();
        }
        let timeout = self.timeout.map(|timeout| {
            self.remote_last_ts
                .map_or(Instant::ZERO, |last| last + timeout)
        });
        let keepalive = if self.keep_alive_eligible() {
            self.keep_alive_at
        } else {
            None
        };
        let retry = match self.timer {
            Timer::Retransmit { expires_at } => Some(expires_at),
            Timer::FastRetransmit => Some(Instant::ZERO),
            Timer::ZeroWindowProbe { expires_at, .. } => Some(expires_at.unwrap_or(Instant::ZERO))
                .into_iter()
                .chain(self.probe_user_deadline())
                .min(),
            _ if self.pending_sequence() => Some(Instant::ZERO),
            _ => None,
        };
        [timeout, keepalive, retry].into_iter().flatten().min()
    }

    /// 推进实际socket纯时间状态，无Context/NIC/分配；返回是否发生可观察transition。
    pub fn maintain(&mut self, timestamp: Instant) -> bool {
        if self.tuple.is_none() || matches!(self.state, State::Closed | State::Listen) {
            return false;
        }
        if self.state == State::TimeWait {
            if self.timer.should_close(timestamp) {
                // 只退休transport tuple/timer，保留已排队RX与FIN事实供存活OFD读取。
                self.set_state(State::Closed);
                self.tuple = None;
                self.timer = Timer::new();
                return true;
            }
            return false;
        }
        if self.orphaned && self.state == State::FinWait2 {
            let deadline = self
                .fin_wait2_deadline()
                .expect("orphan FIN_WAIT2 lost clock");
            if timestamp >= deadline {
                self.set_state(State::Closed);
                if self.linger2 >= 0 {
                    // native FIN_WAIT2 timewait-slot自然到期静默退休；负linger2才发active reset。
                    self.tuple = None;
                }
                self.timer = Timer::new();
                return true;
            }
            return false;
        }
        if self.remote_last_ts.is_none() {
            self.remote_last_ts = Some(timestamp);
        }
        if self.activity_at.is_none() {
            self.record_activity(timestamp);
        }
        if self.pending_sequence() && self.pending_since.is_none() {
            self.pending_since = Some(timestamp);
        }
        if let Timer::ZeroWindowProbe {
            expires_at: None,
            delay,
            ..
        } = self.timer
        {
            self.timer.set_for_zero_window_probe(Some(timestamp), delay);
        }
        if self.pending_sequence() && self.timer.is_idle() {
            self.timer
                .set_for_retransmit(timestamp, self.rtte.retransmission_timeout());
        }
        let retransmit = self.timer.should_retransmit(timestamp);
        let zero_window_expired = self.zero_window_expired(timestamp);
        if self.timed_out(timestamp)
            || self.keep_alive_expired(timestamp)
            || (retransmit && self.retry_expired(timestamp))
            || zero_window_expired
        {
            // tcp_write_err/tcp_done退休tuple；只有keepalive明确先发送active reset。
            let reset = self.keep_alive_expired(timestamp);
            self.failure_reason = Some(FailureReason::TimedOut);
            self.set_state(State::Closed);
            if !reset {
                self.tuple = None;
                self.timer = Timer::new();
            }
            return true;
        }
        if self.zero_window_due(timestamp) {
            self.zero_window_since.get_or_insert(timestamp);
            self.zero_window_probes = self.zero_window_probes.saturating_add(1);
            self.timer
                .schedule_zero_window_probe(timestamp, self.probe_user_deadline());
        }
        if retransmit {
            let fast = matches!(self.timer, Timer::FastRetransmit);
            if !fast {
                self.retransmits = self.retransmits.saturating_add(1);
                self.sack.timeout(self.send_seq_no);
                self.local_rx_dup_acks = 0;
            }
            self.remote_last_seq = self.local_seq_no;
            // 原生active SYN前4次linear RTO；fast-retransmit只撤销样本，不增加RTO退避。
            let backoff = !fast && (self.state != State::SynSent || self.retransmits > 4);
            self.rtte.on_retransmit(backoff);
            if !fast {
                self.congestion_controller
                    .on_timeout(self.local_seq_no, self.send_seq_no - self.local_seq_no);
            }
            self.timer
                .set_for_retransmit(timestamp, self.retry_delay(timestamp));
        }
        false
    }
}
