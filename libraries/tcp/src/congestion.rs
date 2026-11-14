//! RFC 5681 Reno, byte counting for additive increase, and the RFC 6928 initial window.
//! The connection retains SND.UNA/SND.NXT/rwnd; only congestion-control history lives here.
use crate::{
    Sequence,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub(super) struct Reno {
    cwnd: usize,
    mss: usize,
    ssthresh: usize,
    // ACKed bytes toward the next additive step; incrementing a whole MSS per ACK is exponential growth.
    acknowledged: usize,
    // Fast recovery survives replacement of FastRetransmit by the normal retransmit timer.
    // Without this phase, a new ACK cannot deflate the artificially inflated window.
    recovering: bool,
    // The threshold is reduced once per timed-out SND.UNA, not on each retry of the same lost data.
    timeout_sequence: Option<Sequence>,
    // Actual successful sequence-space output, not keepalives/ACKs; idle restart cannot use peer RX activity.
    last_transmit: Option<Instant>,
}
fn initial_window(mss: usize) -> usize {
    mss.saturating_mul(10).min(mss.saturating_mul(2).max(14600))
}
impl Reno {
    pub(super) fn new() -> Self {
        Self {
            cwnd: initial_window(536),
            mss: 536,
            ssthresh: usize::MAX,
            acknowledged: 0,
            recovering: false,
            timeout_sequence: None,
            last_transmit: None,
        }
    }
    pub(super) fn window(&self) -> usize {
        self.cwnd
    }
    pub(super) fn windows(&self) -> (u32, Option<u32>) {
        let segments = |bytes: usize| (bytes / self.mss).min(u32::MAX as usize) as u32;
        (
            segments(self.cwnd),
            (self.ssthresh != usize::MAX).then(|| segments(self.ssthresh)),
        )
    }
    pub(super) fn set_mss(&mut self, mss: usize) {
        let mss = mss.max(1);
        let initial = self.ssthresh == usize::MAX && self.cwnd == initial_window(self.mss);
        self.resegment(mss);
        if initial {
            self.cwnd = initial_window(mss);
        }
    }
    /// Change the segment unit of a live flight without treating PMTU feedback as congestion.
    pub(super) fn resegment(&mut self, mss: usize) {
        let mss = mss.max(1);
        self.mss = mss;
        self.cwnd = self.cwnd.max(mss);
        if self.ssthresh != usize::MAX {
            self.ssthresh = self.ssthresh.max(mss.saturating_mul(2));
        }
    }
    pub(super) fn on_ack(&mut self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.timeout_sequence = None;
        if self.recovering {
            self.cwnd = self.ssthresh;
            self.recovering = false;
            self.acknowledged = 0;
        } else if self.cwnd < self.ssthresh {
            self.cwnd = self.cwnd.saturating_add(bytes.min(self.mss));
        } else {
            self.acknowledged = self.acknowledged.saturating_add(bytes);
            if self.acknowledged >= self.cwnd {
                self.acknowledged -= self.cwnd;
                self.cwnd = self.cwnd.saturating_add(self.mss);
            }
        }
    }
    pub(super) fn on_duplicate_ack(&mut self, duplicates: u8, flight: usize) {
        if duplicates == 3 {
            self.ssthresh = (flight / 2).max(self.mss.saturating_mul(2));
            self.cwnd = self.ssthresh.saturating_add(self.mss.saturating_mul(3));
            self.acknowledged = 0;
            self.recovering = true;
        } else if duplicates > 3 && self.recovering {
            // RFC 5681 permits bounding inflation by outstanding segments against ACK duplication attacks.
            self.cwnd = self.cwnd.saturating_add(self.mss).min(
                flight
                    .saturating_add(self.mss.saturating_mul(3))
                    .max(self.ssthresh),
            );
        }
    }
    /// RFC 6675 uses Pipe instead of Reno's duplicate-ACK inflation during SACK recovery.
    pub(super) fn on_sack_loss(&mut self, flight: usize) {
        self.ssthresh = (flight / 2).max(self.mss.saturating_mul(2));
        self.cwnd = self.ssthresh;
        self.acknowledged = 0;
        self.recovering = false;
    }
    pub(super) fn on_timeout(&mut self, sequence: Sequence, flight: usize) {
        if self.timeout_sequence != Some(sequence) {
            self.ssthresh = (flight / 2).max(self.mss.saturating_mul(2));
            self.timeout_sequence = Some(sequence);
        }
        self.cwnd = self.mss;
        self.acknowledged = 0;
        self.recovering = false;
    }
    pub(super) fn restart_after_idle(&mut self, now: Instant, rto: Duration) {
        if self
            .last_transmit
            .is_some_and(|last| now >= last && now - last >= rto)
        {
            self.cwnd = self.cwnd.min(initial_window(self.mss));
            self.acknowledged = 0;
        }
    }
    pub(super) fn on_send(&mut self, now: Instant) {
        self.last_transmit = Some(now);
    }
}
