use super::*;

impl<'a> Socket<'a> {
    /// Apply one admitted, complete TCP segment to this connection.
    /// # Parameters
    /// `ip_repr` supplies validated IP endpoints and `repr` supplies TCP values after checksum
    /// validation and any authentication required by the selected connection policy.
    /// the caller must first use accepts and retain the same frozen input/path for this operation.
    /// # Returns
    /// At most one best-effort immediate ACK/reset; stream output remains pending for dispatch.
    /// Invalid sequence/window/control input is ignored or answered according to connection state,
    /// never copied into the receive buffer as application bytes.
    pub fn process(
        &mut self,
        cx: &mut impl Context,
        ip_repr: &IpRepr,
        repr: &TcpRepr,
    ) -> Option<(IpRepr, TcpRepr<'static>)> {
        debug_assert!(self.accepts(&*cx, ip_repr, repr));

        // Consider how much the sequence number space differs from the transmit buffer space.
        let (sent_syn, sent_fin) = match self.state {
            // In SYN-SENT or SYN-RECEIVED, we've just sent a SYN.
            State::SynSent | State::SynReceived => (true, false),
            // In FIN-WAIT-1, LAST-ACK, or CLOSING, we've just sent a FIN.
            State::FinWait1 | State::LastAck | State::Closing => (false, true),
            // In all other states we've already got acknowledgements for
            // all of the control flags we sent.
            _ => (false, false),
        };

        // Reject unacceptable acknowledgements.
        match (self.state, repr.control, repr.ack_number) {
            // An RST received in response to initial SYN is acceptable if it acknowledges
            // the initial SYN.
            (State::SynSent, TcpControl::Rst, None) => {
                net_debug!("unacceptable RST (expecting RST|ACK) in response to initial SYN");
                return None;
            }
            (State::SynSent, TcpControl::Rst, Some(ack_number)) => {
                if ack_number != self.local_seq_no + 1 || ack_number > self.send_seq_no {
                    net_debug!("unacceptable RST|ACK in response to initial SYN");
                    return None;
                }
            }
            // Any other RST need only have a valid sequence number.
            (_, TcpControl::Rst, _) => (),
            // The initial SYN cannot contain an acknowledgement.
            (State::Listen, _, None) => (),
            // This case is handled in `accepts()`.
            (State::Listen, _, Some(_)) => unreachable!(),
            // SYN|ACK in the SYN-SENT state must have the exact ACK number.
            (State::SynSent, TcpControl::Syn, Some(ack_number)) => {
                if ack_number != self.local_seq_no + 1 || ack_number > self.send_seq_no {
                    net_debug!("unacceptable SYN|ACK in response to initial SYN");
                    return Some(Self::rst_reply(ip_repr, repr));
                }
            }
            // TCP simultaneous open.
            // This is required by RFC 9293, which states "A TCP implementation MUST support
            // simultaneous open attempts (MUST-10)."
            (State::SynSent, TcpControl::Syn, None) => (),
            // ACKs in the SYN-SENT state are invalid.
            (State::SynSent, TcpControl::None, Some(ack_number)) => {
                // If the sequence number matches, ignore it instead of RSTing.
                // I'm not sure why, I think it may be a workaround for broken TCP
                // servers, or a defense against reordering. Either way, if Linux
                // does it, we do too.
                if ack_number == self.local_seq_no + 1 && ack_number <= self.send_seq_no {
                    net_debug!(
                        "expecting a SYN|ACK, received an ACK with the right ack_number, ignoring."
                    );
                    return None;
                }

                net_debug!(
                    "expecting a SYN|ACK, received an ACK with the wrong ack_number, sending RST."
                );
                return Some(Self::rst_reply(ip_repr, repr));
            }
            // Anything else in the SYN-SENT state is invalid.
            (State::SynSent, _, _) => {
                net_debug!("expecting a SYN|ACK");
                return None;
            }
            // Every packet after the initial SYN must be an acknowledgement.
            (_, _, None) => {
                net_debug!("expecting an ACK");
                return None;
            }
            // ACK in the SYN-RECEIVED state must have the exact ACK number, or we RST it.
            (State::SynReceived, _, Some(ack_number)) => {
                if ack_number != self.local_seq_no + 1 || ack_number > self.send_seq_no {
                    net_debug!("unacceptable ACK in response to SYN|ACK");
                    return Some(Self::rst_reply(ip_repr, repr));
                }
            }
            // Every acknowledgement must be for transmitted but unacknowledged data.
            (_, _, Some(ack_number)) => {
                // Acceptable ACK range (both inclusive)
                let mut ack_min = self.local_seq_no;
                let ack_max = self.send_seq_no;

                // If we have sent a SYN, it MUST be acknowledged.
                if sent_syn {
                    ack_min += 1;
                }

                if ack_number < ack_min {
                    net_debug!(
                        "duplicate ACK ({} not in {}...{})",
                        ack_number,
                        ack_min,
                        ack_max
                    );
                    return None;
                }

                if ack_number > ack_max {
                    net_debug!(
                        "unacceptable ACK ({} not in {}...{})",
                        ack_number,
                        ack_min,
                        ack_max
                    );
                    return self.challenge_ack_reply(cx, ip_repr, repr);
                }
            }
        }

        let window_start = self.remote_seq_no + self.rx_buffer.len();
        let window_end = if let Some(last_ack) = self.remote_last_ack {
            last_ack + ((self.remote_last_win as usize) << self.remote_win_shift)
        } else {
            window_start
        };
        let segment_start = repr.seq_number;
        let segment_end = repr.seq_number + repr.payload.len();

        let (payload, payload_offset) = match self.state {
            // In LISTEN and SYN-SENT states, we have not yet synchronized with the remote end.
            State::Listen | State::SynSent => (&[][..], 0),
            _ => {
                // https://www.rfc-editor.org/rfc/rfc9293.html#name-segment-acceptability-tests
                let segment_in_window = match (
                    segment_start == segment_end,
                    window_start == window_end,
                ) {
                    (true, _) if segment_end == window_start - 1 => {
                        net_debug!(
                            "received a keep-alive or window probe packet, will send an ACK"
                        );
                        false
                    }
                    (true, true) => {
                        if window_start == segment_start {
                            true
                        } else {
                            net_debug!(
                                "zero-length segment not inside zero-length window, will send an ACK."
                            );
                            false
                        }
                    }
                    (true, false) => {
                        if window_start <= segment_start && segment_start < window_end {
                            true
                        } else {
                            net_debug!("zero-length segment not inside window, will send an ACK.");
                            false
                        }
                    }
                    (false, true) => {
                        net_debug!(
                            "non-zero-length segment with zero receive window, will only send an ACK"
                        );
                        false
                    }
                    (false, false) => {
                        if (window_start <= segment_start && segment_start < window_end)
                            || (window_start < segment_end && segment_end <= window_end)
                        {
                            true
                        } else {
                            net_debug!(
                                "segment not in receive window ({}..{} not intersecting {}..{}), will send challenge ACK",
                                segment_start,
                                segment_end,
                                window_start,
                                window_end
                            );
                            false
                        }
                    }
                };

                if segment_in_window {
                    let overlap_start = window_start.max(segment_start);
                    let overlap_end = window_end.min(segment_end);

                    // the checks done above imply this.
                    debug_assert!(overlap_start <= overlap_end);

                    self.local_rx_last_seq = Some(repr.seq_number);

                    (
                        &repr.payload[overlap_start - segment_start..overlap_end - segment_start],
                        overlap_start - window_start,
                    )
                } else {
                    // If we're in the TIME-WAIT state, restart the TIME-WAIT timeout, since
                    // the remote end may not have realized we've closed the connection.
                    if self.state == State::TimeWait {
                        self.timer.set_for_close(cx.now());
                    }

                    return self.challenge_ack_reply(cx, ip_repr, repr);
                }
            }
        };

        // Compute the amount of acknowledged octets, removing the SYN and FIN bits
        // from the sequence space.
        let mut ack_len = 0;
        let mut ack_of_fin = false;
        let mut ack_all = false;
        // 只有真正推进SND.UNA的ACK才重置未完成sequence起点；窗口0应答不能延长USER_TIMEOUT。
        let ack_progress = repr.ack_number.is_some_and(|ack| ack > self.local_seq_no);
        if repr.control != TcpControl::Rst
            && let Some(ack_number) = repr.ack_number
        {
            // Sequence number corresponding to the first byte in `tx_buffer`.
            // This normally equals `local_seq_no`, but is 1 higher if we have sent a SYN,
            // as the SYN occupies 1 sequence number "before" the data.
            let tx_buffer_start_seq = self.local_seq_no + (sent_syn as usize);

            if ack_number >= tx_buffer_start_seq {
                ack_len = ack_number - tx_buffer_start_seq;

                // We could've sent data before the FIN, so only remove FIN from the sequence
                // space if all of that data is acknowledged.
                if sent_fin && self.tx_buffer.len() + 1 == ack_len {
                    ack_len -= 1;
                    net_trace!("received ACK of FIN");
                    ack_of_fin = true;
                }

                ack_all = self.send_seq_no <= ack_number;
            }

            self.rtte.on_ack(cx.now(), ack_number);
            if !self.sack.active() {
                self.congestion_controller.on_ack(ack_len);
            }
        }

        // Disregard control flags we don't care about or shouldn't act on yet.
        let mut control = repr.control;
        control = control.quash_psh();

        // If a FIN is received at the end of the current segment, but
        // we have a hole in the assembler before the current segment, disregard this FIN.
        if control == TcpControl::Fin && window_start < segment_start {
            net_trace!(
                "ignoring FIN because we don't have full data yet. window_start={} segment_start={}",
                window_start,
                segment_start
            );
            control = TcpControl::None;
        }

        // RFC1122/native RCV_SHUTDOWN：最后application owner退出后新数据不能被静默接收。
        if self.orphaned
            && matches!(self.state, State::FinWait1 | State::FinWait2)
            && !payload.is_empty()
        {
            self.abort();
            return self.take_reset();
        }

        // Validate and update the state.
        match (self.state, control) {
            // RSTs are not accepted in the LISTEN state.
            (State::Listen, TcpControl::Rst) => return None,

            // RSTs in SYN-RECEIVED flip the socket back to the LISTEN state.
            // Here we need to additionally check `listen_endpoint`, because we want to make sure
            // that SYN-RECEIVED was actually converted from the LISTEN state (another possible
            // reason is TCP simultaneous open).
            (State::SynReceived, TcpControl::Rst) if self.listen_endpoint.port != 0 => {
                net_trace!("received RST");
                self.tuple = None;
                self.set_state(State::Listen);
                return None;
            }

            // RSTs in any other state close the socket.
            (_, TcpControl::Rst) => {
                net_trace!("received RST");
                self.failure_reason = Some(FailureReason::Reset);
                self.set_state(State::Closed);
                self.tuple = None;
                return None;
            }

            // SYN packets in the LISTEN state change it to SYN-RECEIVED.
            (State::Listen, TcpControl::Syn) => {
                net_trace!("received SYN");
                if let Some(max_seg_size) = repr.max_seg_size.filter(|value| *value != 0) {
                    self.remote_mss = (max_seg_size as usize).max(48)
                } else {
                    // RFC 9293 section 3.7.1: the absent peer MSS is specific to its address family.
                    self.remote_mss = if ip_repr.src_addr().is_ipv4() {
                        536
                    } else {
                        1220
                    };
                }
                self.capture_mss(cx, ip_repr.dst_addr());
                if repr.timestamp.is_none() {
                    self.tsval_generator = None;
                }
                let mss = self.data_mss(cx);
                self.congestion_controller.set_mss(mss);

                self.tuple = Some(Tuple {
                    local: IpEndpoint::new(ip_repr.dst_addr(), repr.dst_port),
                    remote: IpEndpoint::new(ip_repr.src_addr(), repr.src_port),
                });
                self.local_seq_no = self.initial_seq(cx);
                self.remote_seq_no = repr.seq_number + 1;
                self.remote_last_seq = self.local_seq_no;
                self.send_seq_no = self.local_seq_no;
                self.remote_has_sack = repr.sack_permitted;
                self.remote_win_scale = repr.window_scale;
                // Remote doesn't support window scaling, don't do it.
                if self.remote_win_scale.is_none() {
                    self.remote_win_shift = 0;
                }
                self.set_state(State::SynReceived);
                self.pending_since = Some(cx.now());
                self.timer
                    .set_for_retransmit(cx.now(), self.rtte.retransmission_timeout());
            }

            // ACK packets in the SYN-RECEIVED state change it to ESTABLISHED.
            (State::SynReceived, TcpControl::None) => {
                self.set_state(State::Established);
                self.rtte.initialize_route(self.route_initial_rtt);
            }

            // FIN packets in the SYN-RECEIVED state change it to CLOSE-WAIT.
            // It's not obvious from RFC 793 that this is permitted, but
            // 7th and 8th steps in the "SEGMENT ARRIVES" event describe this behavior.
            (State::SynReceived, TcpControl::Fin) => {
                self.remote_seq_no += 1;
                self.rx_fin_received = true;
                self.set_state(State::CloseWait);
                self.rtte.initialize_route(self.route_initial_rtt);
            }

            // SYN|ACK packets in the SYN-SENT state change it to ESTABLISHED.
            // SYN packets in the SYN-SENT state change it to SYN-RECEIVED.
            (State::SynSent, TcpControl::Syn) => {
                if repr.ack_number.is_some() {
                    net_trace!("received SYN|ACK");
                } else {
                    net_trace!("received SYN");
                }
                self.remote_mss = repr.max_seg_size.filter(|value| *value != 0).map_or_else(
                    || {
                        if ip_repr.src_addr().is_ipv4() {
                            536
                        } else {
                            1220
                        }
                    },
                    |value| usize::from(value).max(48),
                );
                self.update_send_mss();
                if repr.timestamp.is_none() {
                    self.tsval_generator = None;
                }
                self.congestion_controller.set_mss(self.data_mss(cx));

                self.remote_seq_no = repr.seq_number + 1;
                // 同时打开需立即重发带 ACK 的原 SYN，不从接收事件伪造已发送序号。
                self.remote_last_seq = if repr.ack_number.is_some() {
                    self.local_seq_no + 1
                } else {
                    self.local_seq_no
                };
                self.remote_last_ack = Some(repr.seq_number);
                self.remote_has_sack = repr.sack_permitted;
                self.remote_win_scale = repr.window_scale;
                // Remote doesn't support window scaling, don't do it.
                if self.remote_win_scale.is_none() {
                    self.remote_win_shift = 0;
                }
                if repr.ack_number.is_some() {
                    self.set_state(State::Established);
                    self.rtte.initialize_route(self.route_initial_rtt);
                } else {
                    self.set_state(State::SynReceived);
                }
            }

            (State::Established, TcpControl::None) => {}

            // FIN packets in ESTABLISHED state indicate the remote side has closed.
            (State::Established, TcpControl::Fin) => {
                self.remote_seq_no += 1;
                self.rx_fin_received = true;
                self.set_state(State::CloseWait);
            }

            // ACK packets in FIN-WAIT-1 state change it to FIN-WAIT-2, if we've already
            // sent everything in the transmit buffer. If not, they reset the retransmit timer.
            (State::FinWait1, TcpControl::None) => {
                if ack_of_fin {
                    self.set_state(State::FinWait2);
                    if self.orphaned {
                        self.fin_wait2_since = Some(cx.now());
                        if self.linger2 < 0 {
                            self.abort();
                            return self.take_reset();
                        }
                    }
                }
            }

            // FIN packets in FIN-WAIT-1 state change it to CLOSING, or to TIME-WAIT
            // if they also acknowledge our FIN.
            (State::FinWait1, TcpControl::Fin) => {
                self.remote_seq_no += 1;
                self.rx_fin_received = true;
                if ack_of_fin {
                    self.set_state(State::TimeWait);
                    self.timer.set_for_close(cx.now());
                } else {
                    self.set_state(State::Closing);
                }
            }

            (State::FinWait2, TcpControl::None) => {}

            // FIN packets in FIN-WAIT-2 state change it to TIME-WAIT.
            (State::FinWait2, TcpControl::Fin) => {
                self.remote_seq_no += 1;
                self.rx_fin_received = true;
                self.set_state(State::TimeWait);
                self.timer.set_for_close(cx.now());
            }

            // ACK packets in CLOSING state change it to TIME-WAIT.
            (State::Closing, TcpControl::None) => {
                if ack_of_fin {
                    self.set_state(State::TimeWait);
                    self.timer.set_for_close(cx.now());
                }
            }

            (State::CloseWait, TcpControl::None) => {}

            // ACK packets in LAST-ACK state change it to CLOSED.
            (State::LastAck, TcpControl::None) => {
                if ack_of_fin {
                    // Clear the remote endpoint, or we'll send an RST there.
                    self.set_state(State::Closed);
                    self.tuple = None;
                } else if ack_len == 0 {
                    // Duplicate ACK; our FIN has not been acknowledged.
                    // Per RFC 9293 (3.10.7.4), send a challenge ACK.
                    return self.challenge_ack_reply(cx, ip_repr, repr);
                }
                // Partial ACK: fall through to advance SND.UNA normally.
            }

            _ => {
                net_debug!("unexpected packet {:?}", repr);
                return None;
            }
        }

        // Update remote state.
        self.remote_last_ts = Some(cx.now());
        self.record_activity(cx.now());

        // RFC 1323: The window field (SEG.WND) in the header of every incoming segment, with the
        // exception of SYN segments, is left-shifted by Snd.Wind.Scale bits before updating SND.WND.
        let scale = match repr.control {
            TcpControl::Syn => 0,
            _ => self.remote_win_scale.unwrap_or(0),
        };
        let new_remote_win_len = (repr.window_len as usize) << (scale as usize);
        let is_window_update = new_remote_win_len != self.remote_win_len;
        self.remote_win_len = new_remote_win_len;

        if ack_len > 0 {
            // Dequeue acknowledged octets.
            debug_assert!(self.tx_buffer.len() >= ack_len);
            net_trace!(
                "tx buffer: dequeueing {} octets (now {})",
                ack_len,
                self.tx_buffer.len() - ack_len
            );
            self.tx_buffer.dequeue_allocated(ack_len);

            // There's new room available in tx_buffer, wake the waiting task if any.
        }

        if let Some(ack_number) = repr.ack_number {
            let recovering = self.sack.active();
            let had_sack = self.sack.has_evidence();
            let new_sack = self.remote_has_sack
                && repr.control != TcpControl::Syn
                && self.sack.update(
                    ack_number,
                    ack_number + (self.send_seq_no - ack_number).min(self.tx_buffer.len()),
                    &repr.sack_ranges,
                );
            if recovering && !self.sack.active() {
                self.remote_last_seq = self.send_seq_no;
                self.local_rx_dup_acks = 0;
            }
            if self.remote_has_sack && (new_sack || self.sack.has_evidence() || self.sack.active())
            {
                // RFC 6675 counts new SACK evidence even with a window update, payload or advancing ACK.
                if ack_progress || !had_sack {
                    self.local_rx_dup_acks = 0;
                }
                if new_sack && !self.sack.active() {
                    self.local_rx_dup_acks = self.local_rx_dup_acks.saturating_add(1);
                }
                if self.sack.may_recover()
                    && self.send_seq_no > ack_number
                    && (new_sack && self.local_rx_dup_acks >= 3
                        || self.sack.is_lost(self.data_mss(cx)))
                {
                    self.sack.begin(ack_number, self.send_seq_no);
                    self.congestion_controller
                        .on_sack_loss(self.send_seq_no - ack_number);
                }
                self.local_rx_last_ack = Some(ack_number);
            } else {
                // Detect and react to duplicate ACKs by:
                // 1. Check if duplicate ACK and change self.local_rx_dup_acks accordingly
                // 2. If exactly 3 duplicate ACKs received, set for fast retransmit
                // 3. Update the last received ACK (self.local_rx_last_ack)
                match self.local_rx_last_ack {
                    // Duplicate ACK if payload empty and ACK doesn't move send window ->
                    // Increment duplicate ACK count and set for retransmit if we just received
                    // the third duplicate ACK
                    Some(last_rx_ack)
                        if repr.payload.is_empty()
                            && last_rx_ack == ack_number
                            && ack_number < self.send_seq_no
                            && !is_window_update =>
                    {
                        // Increment duplicate ACK count
                        self.local_rx_dup_acks = self.local_rx_dup_acks.saturating_add(1);

                        // Inform congestion controller of duplicate ACK
                        if !self.remote_has_sack || self.sack.may_recover() {
                            self.congestion_controller.on_duplicate_ack(
                                self.local_rx_dup_acks,
                                self.send_seq_no - self.local_seq_no,
                            );
                        }

                        net_debug!(
                            "received duplicate ACK for seq {} (duplicate nr {}{})",
                            ack_number,
                            self.local_rx_dup_acks,
                            if self.local_rx_dup_acks == u8::MAX {
                                "+"
                            } else {
                                ""
                            }
                        );

                        if self.local_rx_dup_acks == 3
                            && (!self.remote_has_sack || self.sack.may_recover())
                        {
                            self.timer.set_for_fast_retransmit();
                            net_debug!("started fast retransmit");
                        }
                    }
                    // No duplicate ACK -> Reset state and update last received ACK
                    _ => {
                        if self.local_rx_dup_acks > 0 {
                            self.local_rx_dup_acks = 0;
                            net_debug!("reset duplicate ACK count");
                        }
                        self.local_rx_last_ack = Some(ack_number);
                    }
                };
            }
            // We've processed everything in the incoming segment, so advance the local
            // sequence number past it.
            self.local_seq_no = ack_number;
            // During retransmission, if an earlier segment got lost but later was
            // successfully received, self.local_seq_no can move past self.remote_last_seq.
            // Do not attempt to retransmit the latter segments; not only this is pointless
            // in theory but also impossible in practice, since they have been already
            // deallocated from the buffer.
            if self.remote_last_seq < self.local_seq_no {
                self.remote_last_seq = self.local_seq_no
            }
        }

        // update last remote tsval
        if let Some(timestamp) = repr.timestamp {
            self.last_remote_tsval = timestamp.tsval;
        }

        if ack_progress {
            self.retransmits = 0;
            self.zero_window_since = None;
            self.pending_since =
                if self.tx_buffer.is_empty() && self.send_seq_no <= self.local_seq_no {
                    None
                } else {
                    Some(cx.now())
                };
        }

        // update timers.
        match self.timer {
            Timer::Retransmit { .. } | Timer::FastRetransmit => {
                if ack_all {
                    // RFC 6298: (5.2) ACK of all outstanding data turn off the retransmit timer.
                    self.timer.set_for_idle();
                } else if ack_len > 0 {
                    // (5.3) ACK of new data in ESTABLISHED state restart the retransmit timer.
                    let rto = self.rtte.retransmission_timeout();
                    self.timer.set_for_retransmit(cx.now(), rto);
                }
            }
            Timer::Idle => {
                // any packet on idle refresh the keepalive timer.
                self.timer.set_for_idle();
            }
            _ => {}
        }

        // start/stop the Zero Window Probe timer.
        if self.remote_win_len == 0
            && !self.tx_buffer.is_empty()
            && (self.timer.is_idle() || ack_len > 0)
        {
            let delay = self.rtte.retransmission_timeout();
            net_trace!("starting zero-window-probe timer for t+{}", delay);
            self.timer.set_for_zero_window_probe(Some(cx.now()), delay);
        }
        if self.remote_win_len != 0 && self.timer.is_zero_window_probe() {
            self.timer.set_for_idle();
            self.zero_window_since = None;
            self.zero_window_probes = 0;
        } else if self.timer.is_zero_window_probe() && repr.ack_number.is_some() {
            // tcp_ack_probe只刷新下次probe，保留probes_tstamp；持续窗口0不能躲过USER_TIMEOUT。
            self.zero_window_probes = 0;
            if let Timer::ZeroWindowProbe { delay, .. } = self.timer {
                self.timer = Timer::ZeroWindowProbe {
                    expires_at: Some(Timer::probe_deadline(
                        cx.now(),
                        delay,
                        self.probe_user_deadline(),
                    )),
                    delay,
                    pending: false,
                };
            }
        }

        let payload_len = payload.len();
        if payload_len == 0 {
            return None;
        }

        let assembler_was_empty = self.assembler.is_empty();

        // Try adding payload octets to the assembler.
        let Ok(contig_len) = self
            .assembler
            .add_then_remove_front(payload_offset, payload_len)
        else {
            net_debug!(
                "assembler: too many holes to add {} octets at offset {}",
                payload_len,
                payload_offset
            );
            return None;
        };

        // Place payload octets into the buffer.
        net_trace!(
            "rx buffer: receiving {} octets at offset {}",
            payload_len,
            payload_offset
        );
        let len_written = self.rx_buffer.write_unallocated(payload_offset, payload);
        debug_assert!(len_written == payload_len);

        if contig_len != 0 {
            // Enqueue the contiguous data octets in front of the buffer.
            net_trace!(
                "rx buffer: enqueueing {} octets (now {})",
                contig_len,
                self.rx_buffer.len() + contig_len
            );
            self.rx_buffer.enqueue_unallocated(contig_len);

            // There's new data in rx_buffer, notify waiting task if any.
        }

        if !self.assembler.is_empty() {
            // Print the ranges recorded in the assembler.
            net_trace!("assembler: {}", self.assembler);
        }

        // Handle delayed acks
        if let Some(ack_delay) = self.ack_delay
            && self.ack_to_transmit()
        {
            self.ack_delay_timer = match self.ack_delay_timer {
                AckDelayTimer::Idle => {
                    net_trace!("starting delayed ack timer");
                    AckDelayTimer::Waiting(cx.now() + ack_delay)
                }
                AckDelayTimer::Waiting(_) if self.immediate_ack_to_transmit() => {
                    net_trace!("delayed ack timer already started, forcing expiry");
                    AckDelayTimer::Immediate
                }
                timer @ AckDelayTimer::Waiting(_) => {
                    net_trace!("waiting until delayed ack timer expires");
                    timer
                }
                AckDelayTimer::Immediate => {
                    net_trace!("delayed ack timer already force-expired");
                    AckDelayTimer::Immediate
                }
            };
        }

        // Per RFC 5681, we should send an immediate ACK when either:
        //  1) an out-of-order segment is received, or
        //  2) a segment arrives that fills in all or part of a gap in sequence space.
        if !self.assembler.is_empty() || !assembler_was_empty {
            // Note that we change the transmitter state here.
            // The caller may submit this one immediate ACK; pending stream output remains
            // separately scheduled by dispatch and is not consumed by returning the reply.
            net_trace!("ACKing incoming segment");
            Some(self.ack_reply(ip_repr, repr))
        } else {
            None
        }
    }
}
