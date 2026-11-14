use super::*;

impl<'a> Socket<'a> {
    /// RFC 6691: advertised MSS excludes options; the actual payload unit must deduct their space.
    pub(super) fn data_mss(&self, cx: &impl Context) -> usize {
        let local_mss = self.tuple.map_or(self.route_send_mss, |tuple| {
            let ip_header_len = if tuple.local.addr.is_ipv4() { 20 } else { 40 };
            cx.ip_mtu().saturating_sub(ip_header_len + TCP_HEADER_LEN)
        });
        local_mss
            .saturating_sub(TcpRepr::data_option_space(
                self.tsval_generator.is_some(),
                self.authentication,
            ))
            .min(self.effective_mss())
    }
    fn recovery_segment(&self, cx: &impl Context) -> Option<crate::loss::Transmit> {
        self.sack.next(
            self.local_seq_no,
            self.send_seq_no,
            self.remote_win_len,
            self.tx_buffer.len(),
            self.data_mss(cx).max(1),
            self.congestion_controller.window(),
        )
    }
    fn ordinary_segment(&self) -> Option<(TcpSeqNumber, usize)> {
        let right = self.local_seq_no
            + self
                .remote_win_len
                .min(self.tx_buffer.len())
                .min(self.congestion_controller.window());
        self.sack.unsacked(self.remote_last_seq, right)
    }
    pub(super) fn seq_to_transmit(&self, cx: &impl Context) -> bool {
        if self.sack.active() {
            return self.recovery_segment(cx).is_some();
        }
        let effective_mss = self.data_mss(cx);

        // Have we sent data that hasn't been ACKed yet?
        let data_in_flight = self.send_seq_no != self.local_seq_no;

        // If we want to send a SYN and we haven't done so, do it!
        if matches!(self.state, State::SynSent | State::SynReceived)
            && self.remote_last_seq == self.local_seq_no
        {
            return true;
        }

        let max_send = self.ordinary_segment().map_or(0, |(_, length)| length);

        // Can we send at least 1 octet?
        let mut can_send = max_send != 0;
        // Can we send at least 1 full segment?
        let can_send_full = max_send >= effective_mss;

        // Do we have to send a FIN?
        let want_fin = matches!(
            self.state,
            State::FinWait1 | State::Closing | State::LastAck
        );

        // If we're applying the Nagle algorithm we don't want to send more
        // until one of:
        // * There's no data in flight
        // * We can send a full packet
        // * We have all the data we'll ever send (we're closing send)
        if self.nagle
            && data_in_flight
            && self.remote_last_seq >= self.send_seq_no
            && !can_send_full
            && !want_fin
        {
            can_send = false;
        }

        // Can we actually send the FIN? We can send it if:
        // 1. We have unsent data that fits in the remote window.
        // 2. We have no unsent data.
        // This condition matches only if #2, because #1 is already covered by can_data and we're ORing them.
        let can_fin = want_fin && self.remote_last_seq == self.local_seq_no + self.tx_buffer.len();

        can_send || can_fin
    }

    pub(super) fn delayed_ack_expired(&self, timestamp: Instant) -> bool {
        match self.ack_delay_timer {
            AckDelayTimer::Idle => true,
            AckDelayTimer::Waiting(t) => t <= timestamp,
            AckDelayTimer::Immediate => true,
        }
    }

    pub(super) fn ack_to_transmit(&self) -> bool {
        if let Some(remote_last_ack) = self.remote_last_ack {
            remote_last_ack < self.remote_seq_no + self.rx_buffer.len()
        } else {
            false
        }
    }

    /// Return whether to send ACK immediately due to the amount of unacknowledged data.
    ///
    /// RFC 9293 states "An ACK SHOULD be generated for at least every second full-sized segment or
    /// 2*RMSS bytes of new data (where RMSS is the MSS specified by the TCP endpoint receiving the
    /// segments to be acknowledged, or the default value if not specified) (SHLD-19)."
    ///
    /// Note that the RFC above only says "at least 2*RMSS bytes", which is not a hard requirement.
    /// In practice, we follow the Linux kernel's empirical value of sending an ACK for every RMSS
    /// byte of new data. For details, see
    /// <https://elixir.bootlin.com/linux/v6.11.4/source/net/ipv4/tcp_input.c#L5747>.
    pub(super) fn immediate_ack_to_transmit(&self) -> bool {
        if let Some(remote_last_ack) = self.remote_last_ack {
            remote_last_ack + self.remote_mss < self.remote_seq_no + self.rx_buffer.len()
        } else {
            false
        }
    }

    /// Return whether we should send ACK immediately due to significant window updates.
    ///
    /// ACKs with significant window updates should be sent immediately to let the sender know that
    /// more data can be sent. According to the Linux kernel implementation, "significant" means
    /// doubling the receive window. The Linux kernel implementation can be found at
    /// <https://elixir.bootlin.com/linux/v6.9.9/source/net/ipv4/tcp.c#L1472>.
    pub(super) fn window_to_update(&self) -> bool {
        match self.state {
            State::SynSent
            | State::SynReceived
            | State::Established
            | State::FinWait1
            | State::FinWait2 => {
                let new_win = self.scaled_window();
                if let Some(last_win) = self.last_scaled_window() {
                    new_win > 0 && new_win / 2 >= last_win
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Submit at most one due segment using the current path and clock.
    /// # Parameters
    /// `cx` supplies the frozen path; `emit` must return success only after committing the whole segment.
    /// # Returns
    /// Ok when no output is due or the segment was committed; stream sequence advances only after success.
    /// # Errors
    /// Propagates the output error. LocalCongestion delays probes without charging a peer failure;
    /// PathUnavailable consumes the configured probe attempt. Active resets remain best-effort, once only.
    pub fn dispatch<C: Context, F, E>(&mut self, cx: &mut C, emit: F) -> Result<(), E>
    where
        F: FnOnce(&mut C, (IpRepr, TcpRepr)) -> Result<(), E>,
        E: Copy + Into<OutputFailure>,
    {
        self.maintain(cx.now());
        if self.tuple.is_none() {
            return Ok(());
        }

        if let Some(packet) = self.take_reset() {
            // standalone与外部有界队列使用同一reset_repr/Packet codec，不保留另一套RST构造。
            return emit(cx, packet);
        }

        // NOTE(unwrap): we check tuple is not None above.
        let tuple = self.tuple.unwrap();

        // 同一实际route guard刷新发送PMTU cache，不用后来改变的next-open user policy重协商。
        let ip_header = match tuple.local.addr {
            IpAddress::V4(_) => 20,

            IpAddress::V6(_) => 40,
        };
        let route_mss = cx
            .ip_mtu()
            .saturating_sub(ip_header + TCP_HEADER_LEN)
            .min(self.sender_mss_limit);
        if route_mss != self.route_send_mss {
            if route_mss < self.route_send_mss && self.send_seq_no > self.local_seq_no {
                // PMTU reduction is not congestion: resegment outstanding data without reducing cwnd or backing off RTO.
                self.remote_last_seq = self.local_seq_no;
                self.rtte.on_retransmit(false);
            }
            self.route_send_mss = route_mss;
            self.update_send_mss();
            if self.send_seq_no > self.local_seq_no {
                self.congestion_controller.resegment(self.data_mss(cx));
            } else {
                self.congestion_controller.set_mss(self.data_mss(cx));
            }
        }

        // Linux IPv4 weak-host routing允许bound source由另一设备发出，也不因地址修改重置TCP。
        // NET唯一route owner在dispatch前选择oif；tuple source不从当前iface重建。

        if !self.tx_buffer.is_empty() && self.send_seq_no == self.local_seq_no {
            self.congestion_controller
                .restart_after_idle(cx.now(), self.rtte.retransmission_timeout());
        }
        // Decide whether we're sending a packet.
        let transmit_sequence = self.seq_to_transmit(cx);
        let recovery_segment = self.recovery_segment(cx);
        let ordinary_segment = if self.sack.active() {
            None
        } else {
            self.ordinary_segment()
        };
        if transmit_sequence {
            // If we have data to transmit and it fits into partner's window, do it.
            net_trace!("outgoing segment will send data or flags");
        } else if self.ack_to_transmit() && self.delayed_ack_expired(cx.now()) {
            // If we have data to acknowledge, do it.
            net_trace!("outgoing segment will acknowledge");
        } else if self.window_to_update() {
            // If we have window length increase to advertise, do it.
            net_trace!("outgoing segment will update window");
        } else if self.keep_alive_due(cx.now()) {
            // If we need to transmit a keep-alive packet, do it.
            net_trace!("keep-alive timer expired");
        } else if self.timer.should_zero_window_probe(cx.now()) {
            net_trace!("sending zero-window probe");
        } else {
            return Ok(());
        }

        // Construct the lowered IP representation.
        // We might need this to calculate the MSS, so do it early.
        let mut ip_repr = IpRepr::new(tuple.local.addr, tuple.remote.addr, 6, 0, self.hop_limit);

        // Construct the basic TCP representation, an empty ACK packet.
        // We'll adjust this to be more specific as needed.
        let mut repr = TcpRepr {
            src_port: tuple.local.port,
            dst_port: tuple.remote.port,
            control: TcpControl::None,
            seq_number: self.send_seq_no,
            ack_number: Some(self.remote_seq_no + self.rx_buffer.len()),
            window_len: self.scaled_window(),
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None; 4],
            timestamp: TcpTimestampRepr::generate_reply_with_tsval(
                self.tsval_generator,
                self.last_remote_tsval,
            ),
            authentication: self.authentication,
            payload: &[],
        };

        let is_zero_window_probe = self.timer.should_zero_window_probe(cx.now());

        match self.state {
            State::Closed => unreachable!("closed TCP already retired its reset"),

            // We never transmit anything in the LISTEN state.
            State::Listen => return Ok(()),

            // We transmit a SYN in the SYN-SENT state.
            // We transmit a SYN|ACK in the SYN-RECEIVED state.
            State::SynSent | State::SynReceived => {
                repr.control = TcpControl::Syn;
                repr.seq_number = self.local_seq_no;
                // window len must NOT be scaled in SYNs.
                let mut window = self.rx_buffer.window().min(self.receive_window);
                let mss = self.advertised_mss.unwrap_or(self.send_mss);
                if window > mss {
                    window -= window % mss;
                }
                repr.window_len = u16::try_from(window).unwrap_or(u16::MAX);
                if self.state == State::SynSent {
                    repr.ack_number = None;
                    repr.window_scale = Some(self.remote_win_shift);
                    repr.sack_permitted = true;
                } else {
                    repr.sack_permitted = self.remote_has_sack;
                    repr.window_scale = self.remote_win_scale.map(|_| self.remote_win_shift);
                }
            }

            // We transmit data in all states where we may have data in the buffer,
            // or the transmit half of the connection is still open.
            State::Established
            | State::FinWait1
            | State::Closing
            | State::CloseWait
            | State::LastAck
                if transmit_sequence =>
            {
                repr.seq_number = recovery_segment.map_or_else(
                    || ordinary_segment.map_or(self.remote_last_seq, |(sequence, _)| sequence),
                    |packet| packet.sequence,
                );
                // Extract as much data as the remote side can receive in this packet
                // from the transmit buffer.

                // Right edge of window, ie the max sequence number we're allowed to send.
                let window = if self.sack.active() {
                    self.remote_win_len
                } else {
                    self.remote_win_len.min(self.congestion_controller.window())
                };
                let win_right_edge = self.local_seq_no + window;

                // Max amount of octets we're allowed to send according to the remote window.
                let win_limit = if win_right_edge >= repr.seq_number {
                    win_right_edge - repr.seq_number
                } else {
                    // This can happen if we've sent some data and later the remote side
                    // has shrunk its window so that data is no longer inside the window.
                    // This should be very rare and is strongly discouraged by the RFCs,
                    // but it does happen in practice.
                    // http://www.tcpipguide.com/free/t_TCPWindowManagementIssues.htm
                    0
                };

                // Maximum size we're allowed to send. This can be limited by 3 factors:
                // 1. remote window
                // 2. MSS the remote is willing to accept, probably determined by their MTU
                // 3. MSS we can send, determined by our MTU.
                let path_payload = cx
                    .ip_mtu()
                    .saturating_sub(ip_repr.header_len() + repr.header_len());
                let size = win_limit.min(self.data_mss(cx)).min(path_payload).min(
                    recovery_segment.map_or_else(
                        || ordinary_segment.map_or(usize::MAX, |(_, length)| length),
                        |packet| packet.length,
                    ),
                );

                let offset = repr.seq_number - self.local_seq_no;
                repr.payload = self.tx_buffer.get_allocated(offset, size);

                // If we've sent everything we had in the buffer, follow it with the PSH or FIN
                // flags, depending on whether the transmit half of the connection is open.
                if offset + repr.payload.len() == self.tx_buffer.len() {
                    match self.state {
                        State::FinWait1 | State::LastAck | State::Closing => {
                            repr.control = TcpControl::Fin
                        }
                        State::Established | State::CloseWait if !repr.payload.is_empty() => {
                            repr.control = TcpControl::Psh
                        }
                        _ => (),
                    }
                }
            }

            // In FIN-WAIT-2 and TIME-WAIT states we may only transmit ACKs for incoming data or FIN
            State::Established
            | State::FinWait1
            | State::Closing
            | State::CloseWait
            | State::LastAck
            | State::FinWait2
            | State::TimeWait => {}
        }

        // There might be more than one reason to send a packet. E.g. the keep-alive timer
        // has expired, and we also have data in transmit buffer. Since any packet that occupies
        // sequence space will elicit an ACK, we only need to send an explicit packet if we
        // couldn't fill the sequence space with anything.
        let is_keep_alive = if is_zero_window_probe {
            // native tcp_xmit_probe_skb：纯snd_una-1空ACK，不借payload或消耗stream序号。
            repr.seq_number = self.local_seq_no - 1;
            repr.payload = &[];
            repr.control = TcpControl::None;
            false
        } else if self.keep_alive_due(cx.now()) && repr.is_empty() {
            repr.seq_number = repr.seq_number - 1;
            // 原生tcp_xmit_probe_skb只发送snd_una-1的空ACK，不消耗stream sequence。
            true
        } else {
            false
        };

        // Trace a summary of what will be sent.
        if is_keep_alive {
            net_trace!("sending a keep-alive");
        } else if !repr.payload.is_empty() {
            net_trace!(
                "tx buffer: sending {} octets at offset {}",
                repr.payload.len(),
                self.remote_last_seq - self.local_seq_no
            );
        }
        if repr.control != TcpControl::None || repr.payload.is_empty() {
            let flags = match (repr.control, repr.ack_number) {
                (TcpControl::Syn, None) => "SYN",
                (TcpControl::Syn, Some(_)) => "SYN|ACK",
                (TcpControl::Fin, Some(_)) => "FIN|ACK",
                (TcpControl::Rst, Some(_)) => "RST|ACK",
                (TcpControl::Psh, Some(_)) => "PSH|ACK",
                (TcpControl::None, Some(_)) => "ACK",
                _ => "<unreachable>",
            };
            net_trace!("sending {}", flags);
        }

        if repr.control == TcpControl::Syn {
            // Fill the MSS option. See RFC 6691 for an explanation of this calculation.
            let mut max_segment_size = self
                .advertised_mss
                .expect("SYN must capture actual route MSS");
            // native tcp_advertise_mss可按当前route收紧已冻结广告，但不得用MTU压过min_advmss。
            if let Some(route) = cx.transport_route() {
                max_segment_size = max_segment_size.min(route.advertised_mss);
                self.advertised_mss = Some(max_segment_size);
            }
            repr.max_seg_size = Some(max_segment_size as u16);
        }

        // Actually send the packet. If this succeeds, it means the packet is in
        // the device buffer, and its transmission is imminent. If not, we might have
        // a number of problems, e.g. we need neighbor discovery.
        //
        // Bailing out if the packet isn't placed in the device buffer allows us
        // to not waste time waiting for the retransmit timer on packets that we know
        // for sure will not be successfully transmitted.
        ip_repr.set_payload_len(repr.buffer_len());
        if let Err(error) = emit(cx, (ip_repr, repr)) {
            if is_keep_alive || is_zero_window_probe {
                self.output_unavailable(cx.now(), error.into());
            }
            return Err(error);
        }
        // 提取本次提交的 scalar facts 后结束 payload borrow，再更新完整 socket owner。
        let segment_len = repr.segment_len();
        let payload_len = repr.payload.len();
        if segment_len != 0 {
            self.congestion_controller.on_send(cx.now());
        }
        let sequence = repr.seq_number;
        let sequence_end = sequence + segment_len;
        let ack_number = repr.ack_number;
        let window_len = repr.window_len;

        if is_keep_alive {
            self.finish_keep_alive(cx.now(), false);
        } else if !is_zero_window_probe {
            self.record_activity(cx.now());
        }

        // Reset delayed-ack timer
        match self.ack_delay_timer {
            AckDelayTimer::Idle => {}
            AckDelayTimer::Waiting(_) => {
                net_trace!("stop delayed ack timer")
            }
            AckDelayTimer::Immediate => {
                net_trace!("stop delayed ack timer (was force-expired)")
            }
        }
        self.ack_delay_timer = AckDelayTimer::Idle;

        // Leave the rest of the state intact if sending a zero-window probe.
        if is_zero_window_probe {
            self.timer
                .finish_zero_window_probe(cx.now(), true, self.probe_user_deadline());
            return Ok(());
        }

        // Leave the rest of the state intact if sending a keep-alive packet, since those
        // carry a fake segment.
        if is_keep_alive {
            return Ok(());
        }

        // We've sent a packet successfully, so we can update the internal state now.
        self.remote_last_ack = ack_number;
        self.remote_last_win = window_len;

        if segment_len > 0 {
            // 与旧 SND.NXT 比较。失败提交、ACK、探测及首次发送均不增加重传计数。
            if sequence < self.send_seq_no {
                self.total_retransmissions = self.total_retransmissions.saturating_add(1);
            }
            self.remote_last_seq = sequence_end;
            if self.remote_last_seq > self.send_seq_no {
                self.send_seq_no = self.remote_last_seq;
            }
            self.rtte.on_send(cx.now(), sequence_end);
            if let Some(packet) = recovery_segment {
                self.sack.sent(packet, payload_len);
                if packet.retransmitted() {
                    self.rtte.on_retransmit(false);
                }
                if packet.first() {
                    self.timer
                        .set_for_retransmit(cx.now(), self.rtte.retransmission_timeout());
                }
            }
        }

        if segment_len > 0 && !self.timer.is_retransmit() {
            // RFC 6298 (5.1) Every time a packet containing data is sent (including a
            // retransmission), if the timer is not running, start it running
            // so that it will expire after RTO seconds.
            let rto = self.rtte.retransmission_timeout();
            self.timer.set_for_retransmit(cx.now(), rto);
        }

        Ok(())
    }

    /// Next required work against the same path and monotonic clock, without changing connection state.
    /// Returns Ingress when only external input can make progress; no allocation or output occurs.
    pub fn poll_at(&self, cx: &impl Context) -> PollAt {
        self.poll_at_with(|| self.seq_to_transmit(cx))
    }

    /// Earliest possible output before choosing a path. A caller may defer route lookup until this time.
    /// Unsent or outstanding sequence space returns Now, including when a new path MTU could change segmentation.
    /// The selected path and actual transmit eligibility still come from `poll_at` and `dispatch`.
    pub fn output_interest(&self) -> PollAt {
        self.poll_at_with(|| self.pending_sequence())
    }

    fn poll_at_with(&self, sequence_ready: impl FnOnce() -> bool) -> PollAt {
        // The logic here mirrors the beginning of dispatch() closely.
        if self.tuple.is_none() {
            // No one to talk to, nothing to transmit.
            PollAt::Ingress
        } else if self.remote_last_ts.is_none() {
            // Socket stopped being quiet recently, we need to acquire a timestamp.
            PollAt::Now
        } else if self.state == State::Closed {
            // Socket was aborted, we have an RST packet to transmit.
            PollAt::Now
        } else if sequence_ready() {
            // We have a data or flag packet to transmit.
            PollAt::Now
        } else if self.window_to_update() {
            // The receive window has been raised significantly.
            PollAt::Now
        } else {
            let want_ack = self.ack_to_transmit();
            let keep_alive_poll_at = if matches!(self.state, State::Established | State::CloseWait)
                && self.tx_buffer.is_empty()
                && self.send_seq_no == self.local_seq_no
            {
                self.keep_alive_at.map_or(PollAt::Ingress, PollAt::Time)
            } else {
                PollAt::Ingress
            };

            let delayed_ack_poll_at = match (want_ack, self.ack_delay_timer) {
                (false, _) => PollAt::Ingress,
                (true, AckDelayTimer::Idle) => PollAt::Now,
                (true, AckDelayTimer::Waiting(t)) => PollAt::Time(t),
                (true, AckDelayTimer::Immediate) => PollAt::Now,
            };

            let timeout_poll_at = match (self.remote_last_ts, self.timeout) {
                // If we're transmitting or retransmitting data, we need to poll at the moment
                // when the timeout would expire.
                (Some(remote_last_ts), Some(timeout)) => PollAt::Time(remote_last_ts + timeout),
                // Otherwise we have no timeout.
                (_, _) => PollAt::Ingress,
            };

            // We wait for the earliest of our timers to fire.
            *[
                self.timer.poll_at(),
                timeout_poll_at,
                delayed_ack_poll_at,
                keep_alive_poll_at,
            ]
            .iter()
            .min()
            .unwrap_or(&PollAt::Ingress)
        }
    }
}
