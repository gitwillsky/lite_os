//! RFC 6675 sender scoreboard and recovery selection. Ranges are half-open stream positions;
//! the socket alone owns SND.UNA, SND.NXT, payload storage, congestion control and output commitment.
use crate::Sequence;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug)]
struct Range {
    start: Sequence,
    end: Sequence,
}

#[derive(Debug)]
struct Recovery {
    high_rxt: Sequence,
    rescue_rxt: Sequence,
    // The mandatory first retransmission bypasses Pipe once, and is consumed only by committed TX.
    first: bool,
    rescued: Option<Range>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    First,
    Retransmit,
    New,
    Rescue,
}
/// A borrowed decision, not a transmission record; failed output must leave recovery unchanged.
#[derive(Clone, Copy, Debug)]
pub(super) struct Transmit {
    pub(super) sequence: Sequence,
    pub(super) length: usize,
    kind: Kind,
}
impl Transmit {
    pub(super) fn retransmitted(self) -> bool {
        self.kind != Kind::New
    }
    pub(super) fn first(self) -> bool {
        self.kind == Kind::First
    }
}
/// Only advisory receive evidence and RFC 6675 recovery history live here. No payload is released
/// by a SACK, and the empty/common case has no allocation. One range per disjoint acknowledged run
/// bounds metadata by the actual transmitted buffer, rather than a second per-byte payload copy.
#[derive(Debug)]
pub(super) struct Sack {
    ranges: Vec<Range>,
    recovery: Option<Recovery>,
    // RecoveryPoint also survives an RTO: no second fast recovery until this flight is ACKed.
    limit: Option<Sequence>,
}
impl Sack {
    pub(super) const fn new() -> Self {
        Self {
            ranges: Vec::new(),
            recovery: None,
            limit: None,
        }
    }
    pub(super) fn active(&self) -> bool {
        self.recovery.is_some()
    }
    pub(super) fn has_evidence(&self) -> bool {
        !self.ranges.is_empty()
    }
    /// Locate the next unsacked run, including fresh evidence received after RTO (section 5.1).
    /// This does not re-enter fast recovery or release bytes; the caller still enforces cwnd/rwnd.
    pub(super) fn unsacked(
        &self,
        mut sequence: Sequence,
        right: Sequence,
    ) -> Option<(Sequence, usize)> {
        for range in &self.ranges {
            if sequence >= right {
                return None;
            }
            if sequence < range.start {
                return Some((sequence, range.start.min(right) - sequence));
            }
            sequence = sequence.max(range.end);
        }
        (sequence < right).then(|| (sequence, right - sequence))
    }
    pub(super) fn may_recover(&self) -> bool {
        self.limit.is_none()
    }
    /// Retire cumulative evidence and merge only SACKs inside the actual sent data window.
    /// Returns whether any previously unSACKed bytes were learned. DSACK, future/empty/reversed
    /// ranges cannot prove loss. On allocation failure the new advisory range is ignored;
    /// retained evidence, payload and the normal retransmission timer remain valid.
    pub(super) fn update(
        &mut self,
        ack: Sequence,
        high: Sequence,
        blocks: &[Option<(u32, u32)>],
    ) -> bool {
        if self.limit.is_some_and(|limit| ack >= limit) {
            self.limit = None;
            self.recovery = None;
        }
        self.ranges.retain_mut(|range| {
            range.start = range.start.max(ack);
            range.end > range.start
        });
        let flight = high - ack;
        let mut changed = false;
        for &(start, end) in blocks.iter().flatten() {
            let left = start.wrapping_sub(ack.0 as u32) as usize;
            let right = end.wrapping_sub(ack.0 as u32) as usize;
            if left == 0 || left >= right || right > flight {
                continue;
            }
            changed |= self.insert(Range {
                start: ack + left,
                end: ack + right,
            });
        }
        if self.ranges.is_empty() {
            self.ranges = Vec::new();
        }
        changed
    }
    fn insert(&mut self, range: Range) -> bool {
        let first = self.ranges.partition_point(|old| old.end < range.start);
        let end = first + self.ranges[first..].partition_point(|old| old.start <= range.end);
        if first == end {
            if self.ranges.try_reserve(1).is_err() {
                return false;
            }
            self.ranges.insert(first, range);
        } else {
            if self.ranges[first].start <= range.start && self.ranges[first].end >= range.end {
                return false;
            }
            self.ranges[first] = Range {
                start: self.ranges[first].start.min(range.start),
                end: self.ranges[end - 1].end.max(range.end),
            };
            self.ranges.drain(first + 1..end);
        }
        true
    }
    pub(super) fn is_lost(&self, mss: usize) -> bool {
        self.ranges.len() >= 3
            || self
                .ranges
                .iter()
                .map(|range| range.end - range.start)
                .sum::<usize>()
                > mss.saturating_mul(2)
    }
    pub(super) fn begin(&mut self, ack: Sequence, high: Sequence) {
        debug_assert!(self.may_recover() && high > ack);
        self.limit = Some(high);
        self.recovery = Some(Recovery {
            high_rxt: ack,
            rescue_rxt: ack,
            first: true,
            rescued: None,
        });
    }
    /// RFC 6675 section 5.1: all old SACK evidence may have been reneged after a timeout.
    pub(super) fn timeout(&mut self, high: Sequence) {
        self.ranges = Vec::new();
        self.recovery = None;
        self.limit = Some(high);
    }
    /// Iterate unsacked runs with IsLost evidence in one pass. Suffix byte totals and disjoint
    /// block counts avoid re-scanning the whole scoreboard for every hole (linear SetPipe/NextSeg).
    fn holes(
        &self,
        ack: Sequence,
        high: Sequence,
        mss: usize,
    ) -> impl Iterator<Item = (Range, bool)> + '_ {
        let mut above = self
            .ranges
            .iter()
            .map(|range| range.end - range.start)
            .sum::<usize>();
        (0..=self.ranges.len()).filter_map(move |index| {
            let start = if index == 0 {
                ack
            } else {
                self.ranges[index - 1].end
            }
            .max(ack);
            let end = self
                .ranges
                .get(index)
                .map_or(high, |range| range.start)
                .min(high);
            let lost = self.ranges.len() - index >= 3 || above > mss.saturating_mul(2);
            if let Some(range) = self.ranges.get(index) {
                above -= range.end - range.start;
            }
            (start < end).then_some((Range { start, end }, lost))
        })
    }
    fn pipe(&self, ack: Sequence, high: Sequence, mss: usize, recovery: &Recovery) -> usize {
        self.holes(ack, high, mss)
            .map(|(range, lost)| {
                let original = if lost { 0 } else { range.end - range.start };
                let retransmitted = if range.start < recovery.high_rxt {
                    range.end.min(recovery.high_rxt) - range.start
                } else {
                    0
                };
                let rescue = recovery.rescued.map_or(0, |rescue| {
                    let start = range.start.max(rescue.start).max(recovery.high_rxt);
                    let end = range.end.min(rescue.end);
                    if start < end { end - start } else { 0 }
                });
                original + retransmitted + rescue
            })
            .sum()
    }
    /// RFC 6675 NextSeg: lost hole, new data, a speculative hole, then one highest-byte rescue.
    /// The inputs are current socket facts; PMTU and receiver-window changes apply immediately.
    pub(super) fn next(
        &self,
        ack: Sequence,
        high: Sequence,
        window: usize,
        buffered: usize,
        mss: usize,
        cwnd: usize,
    ) -> Option<Transmit> {
        let recovery = self.recovery.as_ref()?;
        let high = ack + (high - ack).min(buffered);
        let right = ack + window.min(buffered);
        let segment = |start: Sequence, end: Sequence, kind| {
            let end = end.min(right);
            (start < end).then(|| Transmit {
                sequence: start,
                length: (end - start).min(mss),
                kind,
            })
        };
        if recovery.first {
            let end = self
                .ranges
                .first()
                .map_or(high, |range| range.start)
                .min(high);
            return segment(ack, end, Kind::First);
        }
        if cwnd.saturating_sub(self.pipe(ack, high, mss, recovery)) < mss {
            return None;
        }
        let highest_sack = self.ranges.last().map_or(ack, |range| range.end);
        let hole = |range: Range| {
            segment(
                range.start.max(recovery.high_rxt),
                range.end.min(highest_sack),
                Kind::Retransmit,
            )
        };
        if let Some(packet) = self
            .holes(ack, high, mss)
            .filter(|(_, lost)| *lost)
            .find_map(|(range, _)| hole(range))
        {
            return Some(packet);
        }
        if let Some(packet) = segment(high, right, Kind::New) {
            return Some(packet);
        }
        if let Some(packet) = self
            .holes(ack, high, mss)
            .find_map(|(range, _)| hole(range))
        {
            return Some(packet);
        }
        if ack >= recovery.rescue_rxt {
            let (range, _) = self.holes(ack, high, mss).last()?;
            let end = range.end.min(right);
            let start = if end > range.start {
                end - (end - range.start).min(mss)
            } else {
                return None;
            };
            return segment(start, end, Kind::Rescue);
        }
        None
    }
    pub(super) fn sent(&mut self, packet: Transmit, length: usize) {
        let recovery = self
            .recovery
            .as_mut()
            .expect("committed recovery selection");
        let end = packet.sequence + length;
        match packet.kind {
            Kind::First => {
                recovery.first = false;
                recovery.high_rxt = end;
                recovery.rescue_rxt = end;
            }
            Kind::Retransmit => recovery.high_rxt = end,
            Kind::Rescue => {
                recovery.rescue_rxt = self.limit.expect("active RecoveryPoint");
                recovery.rescued = Some(Range {
                    start: packet.sequence,
                    end,
                });
            }
            Kind::New => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sequence wrap and an RTO during recovery cannot be validated by a normal successful stream.
    #[test]
    fn invalid_sacks_do_not_prove_loss_and_timeout_evidence_cannot_restart_the_old_recovery() {
        let ack = Sequence(-16);
        let high = ack + 100;
        let mut sack = Sack::new();
        let blocks = [Some((0, 10)), Some((20, 30)), Some((40, 50))];
        assert!(sack.update(ack, high, &blocks));
        assert!(sack.is_lost(16));
        assert!(
            !sack.update(ack, high, &blocks),
            "duplicate options add no loss evidence"
        );
        assert!(!sack.update(
            ack,
            high,
            &[
                Some((ack.0 as u32, 10)), // SACK cannot cover the cumulative acknowledgment.
                Some((80, 90)),           // End exceeds actual HighData (84).
                Some((30, 20)),           // Reversed range.
                Some((u32::MAX - 30, u32::MAX - 20)), // DSACK below SND.UNA.
            ]
        ));
        sack.begin(ack, high);
        let packet = sack.next(ack, high, 100, 100, 16, 50).unwrap();
        assert_eq!(packet.sequence, ack);
        assert_eq!(packet.length, 16);
        sack.sent(packet, 16);
        sack.timeout(high);
        assert!(!sack.active() && !sack.has_evidence() && !sack.may_recover());
        assert_eq!(sack.unsacked(ack + 16, high), Some((ack + 16, 84)));
        assert!(sack.update(ack, high, &blocks));
        assert_eq!(sack.unsacked(ack + 16, high), Some((ack + 26, 10)));
        assert!(
            !sack.may_recover(),
            "fresh SACKs cannot reopen the timed-out flight"
        );
        assert!(!sack.update(high, high, &[]));
        assert!(sack.may_recover() && !sack.has_evidence());
    }
}
