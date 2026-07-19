pub(crate) const EPHEMERAL_START: u16 = 49_152;
pub(crate) const EPHEMERAL_END: u16 = 65_535;
const EPHEMERAL_PORTS: usize = (EPHEMERAL_END - EPHEMERAL_START + 1) as usize;
const EPHEMERAL_WORDS: usize = EPHEMERAL_PORTS.div_ceil(u64::BITS as usize);

/// 完全空闲 ephemeral port 的有界 word-scan 投影。
pub(super) struct EphemeralPorts {
    occupied: [u64; EPHEMERAL_WORDS],
    next: u16,
}

impl EphemeralPorts {
    pub(super) const fn new() -> Self {
        Self {
            occupied: [0; EPHEMERAL_WORDS],
            next: EPHEMERAL_START,
        }
    }

    /// 返回 cursor 后第一个空闲 port 并推进 cursor，不发布占用 bit。
    pub(super) fn next_candidate(&mut self) -> Option<u16> {
        let port = self.first_free_from(self.next)?;
        self.next = if port == EPHEMERAL_END {
            EPHEMERAL_START
        } else {
            port + 1
        };
        Some(port)
    }

    pub(super) fn mark_occupied(&mut self, port: u16) {
        if (EPHEMERAL_START..=EPHEMERAL_END).contains(&port) {
            let bit = usize::from(port - EPHEMERAL_START);
            self.occupied[bit / 64] |= 1 << (bit % 64);
        }
    }

    pub(super) fn clear_occupied(&mut self, port: u16) {
        if (EPHEMERAL_START..=EPHEMERAL_END).contains(&port) {
            let bit = usize::from(port - EPHEMERAL_START);
            self.occupied[bit / 64] &= !(1 << (bit % 64));
        }
    }

    fn first_free_from(&self, cursor: u16) -> Option<u16> {
        let start = usize::from(cursor.clamp(EPHEMERAL_START, EPHEMERAL_END) - EPHEMERAL_START);
        let start_word = start / u64::BITS as usize;
        let start_bit = start % u64::BITS as usize;
        for offset in 0..EPHEMERAL_WORDS {
            let word = (start_word + offset) % EPHEMERAL_WORDS;
            let mut free = !self.occupied[word];
            if offset == 0 {
                free &= u64::MAX << start_bit;
            }
            if free != 0 {
                let bit = free.trailing_zeros() as usize;
                return Some(EPHEMERAL_START + (word * u64::BITS as usize + bit) as u16);
            }
        }
        if start_bit != 0 {
            let free = !self.occupied[start_word] & ((1u64 << start_bit) - 1);
            if free != 0 {
                return Some(
                    EPHEMERAL_START
                        + (start_word * u64::BITS as usize + free.trailing_zeros() as usize) as u16,
                );
            }
        }
        None
    }
}
