use super::*;

impl<'a> Socket<'a> {
    /// Form the semantic reverse tuple and a control-free empty reply to an admitted segment.
    /// Returns values only; the caller still owns IP route admission and output commitment.
    pub fn reply(ip_repr: &IpRepr, repr: &TcpRepr) -> (IpRepr, TcpRepr<'static>) {
        let reply_repr = TcpRepr {
            src_port: repr.dst_port,
            dst_port: repr.src_port,
            control: TcpControl::None,
            seq_number: TcpSeqNumber(0),
            ack_number: None,
            window_len: 0,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None; 4],
            timestamp: None,
            md5: false,
            payload: &[],
        };
        let ip_reply_repr = IpRepr::new(
            ip_repr.dst_addr(),
            ip_repr.src_addr(),
            6,
            reply_repr.buffer_len(),
            None,
        );
        (ip_reply_repr, reply_repr)
    }

    /// Form the RFC 9293 reset sequence/acknowledgment for an admitted unexpected segment.
    /// The caller must suppress resets to resets and invalid/broadcast/multicast IP sources.
    pub fn rst_reply(ip_repr: &IpRepr, repr: &TcpRepr) -> (IpRepr, TcpRepr<'static>) {
        debug_assert!(repr.control != TcpControl::Rst);

        let (ip_reply_repr, mut reply_repr) = Self::reply(ip_repr, repr);

        // See https://www.snellman.net/blog/archive/2016-02-01-tcp-rst/ for explanation
        // of why we sometimes send an RST and sometimes an RST|ACK
        reply_repr.control = TcpControl::Rst;
        reply_repr.md5 = repr.md5;
        reply_repr.seq_number = repr.ack_number.unwrap_or_default();
        if repr.control == TcpControl::Syn && repr.ack_number.is_none() {
            reply_repr.ack_number = Some(repr.seq_number + repr.segment_len());
        }

        (ip_reply_repr, reply_repr)
    }

    pub(super) fn ack_reply(
        &mut self,
        ip_repr: &IpRepr,
        repr: &TcpRepr,
    ) -> (IpRepr, TcpRepr<'static>) {
        let (mut ip_reply_repr, mut reply_repr) = Self::reply(ip_repr, repr);
        reply_repr.md5 = self.md5;
        reply_repr.timestamp = repr
            .timestamp
            .and_then(|tcp_ts| tcp_ts.generate_reply(self.tsval_generator));

        // From RFC 793:
        // [...] an empty acknowledgment segment containing the current send-sequence number
        // and an acknowledgment indicating the next sequence number expected
        // to be received.
        reply_repr.seq_number = self.send_seq_no;
        reply_repr.ack_number = Some(self.remote_seq_no + self.rx_buffer.len());
        self.remote_last_ack = reply_repr.ack_number;

        // From RFC 1323:
        // The window field [...] of every outgoing segment, with the exception of SYN
        // segments, is right-shifted by [advertised scale value] bits[...]
        reply_repr.window_len = self.scaled_window();
        self.remote_last_win = reply_repr.window_len;

        // If the remote supports selective acknowledgement, add the option to the outgoing
        // segment.
        if self.remote_has_sack {
            net_debug!("sending sACK option with current assembler ranges");

            // RFC 2018: The first SACK block (i.e., the one immediately following the kind and
            // length fields in the option) MUST specify the contiguous block of data containing
            // the segment which triggered this ACK, unless that segment advanced the
            // Acknowledgment Number field in the header.
            reply_repr.sack_ranges[0] = None;

            let ack = reply_repr.ack_number.unwrap_or(TcpSeqNumber(0));

            if let Some(last_seg_seq) = self.local_rx_last_seq {
                reply_repr.sack_ranges[0] = self
                    .assembler
                    .iter_data()
                    .map(|(left, right)| (ack + left, ack + right))
                    .find(|&(left, right)| left <= last_seg_seq && right >= last_seg_seq)
                    .map(|(left, right)| (left.0 as u32, right.0 as u32));
            }

            if reply_repr.sack_ranges[0].is_none() {
                // The matching segment was removed from the assembler, meaning the acknowledgement
                // number has advanced, or there was no previous sACK.
                //
                // While the RFC says we SHOULD keep a list of reported sACK ranges, and iterate
                // through those, that is currently infeasible. Instead, we offer the range with
                // the lowest sequence number (if one exists) to hint at what segments would
                // most quickly advance the acknowledgement number.
                reply_repr.sack_ranges[0] = self
                    .assembler
                    .iter_data()
                    .map(|(left, right)| (ack + left, ack + right))
                    .next()
                    .map(|(left, right)| (left.0 as u32, right.0 as u32));
            }
            // Fit remaining blocks alongside the actual timestamp and authentication options.
            let first = reply_repr.sack_ranges[0];
            let mut slot = 1;
            for (left, right) in self.assembler.iter_data() {
                let block = Some(((ack + left).0 as u32, (ack + right).0 as u32));
                if block != first && slot < reply_repr.sack_ranges.len() {
                    reply_repr.sack_ranges[slot] = block;
                    if reply_repr.header_len() > 60 {
                        reply_repr.sack_ranges[slot] = None;
                        break;
                    }
                    slot += 1;
                }
            }
        }

        // Since the sACK option may have changed the length of the payload, update that.
        ip_reply_repr.set_payload_len(reply_repr.buffer_len());
        (ip_reply_repr, reply_repr)
    }

    pub(super) fn challenge_ack_reply(
        &mut self,
        cx: &mut impl Context,
        ip_repr: &IpRepr,
        repr: &TcpRepr,
    ) -> Option<(IpRepr, TcpRepr<'static>)> {
        if cx.now() < self.challenge_ack_timer {
            return None;
        }

        // Rate-limit to 1 per second max.
        self.challenge_ack_timer = cx.now() + Duration::from_secs(1);

        Some(self.ack_reply(ip_repr, repr))
    }

    /// Match this live tuple/listener and its bound interface, without consuming input or entropy.
    /// IP local-delivery, family/port demultiplexing and checksum admission remain caller obligations.
    pub fn accepts(&self, cx: &impl Context, ip_repr: &IpRepr, repr: &TcpRepr) -> bool {
        if self
            .bound_interface
            .is_some_and(|index| index != cx.interface_index())
        {
            return false;
        }
        if self.state == State::Closed {
            return false;
        }

        // If we're still listening for SYNs and the packet has an ACK or a RST,
        // it cannot be destined to this socket, but another one may well listen
        // on the same local endpoint.
        if self.state == State::Listen
            && (repr.ack_number.is_some() || repr.control == TcpControl::Rst)
        {
            return false;
        }

        if let Some(tuple) = &self.tuple {
            // Reject packets not matching the 4-tuple
            ip_repr.dst_addr() == tuple.local.addr
                && repr.dst_port == tuple.local.port
                && ip_repr.src_addr() == tuple.remote.addr
                && repr.src_port == tuple.remote.port
        } else {
            // We're listening, reject packets not matching the listen endpoint.
            let addr_ok = match self.listen_endpoint.addr {
                Some(addr) => ip_repr.dst_addr() == addr,
                None => true,
            };
            addr_ok && repr.dst_port != 0 && repr.dst_port == self.listen_endpoint.port
        }
    }

    /// Match a checked wire packet without accepting its authentication or changing state.
    /// IncomingSegment::authenticate must accept input before it is delivered to process().
    pub fn accepts_incoming(
        &self,
        cx: &impl Context,
        ip: &IpRepr,
        packet: &crate::IncomingSegment<'_>,
    ) -> bool {
        self.accepts(cx, ip, &packet.segment)
    }
}
