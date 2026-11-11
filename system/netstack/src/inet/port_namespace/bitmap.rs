pub(crate) const EPHEMERAL_START: u16 = 49_152;
pub(crate) const EPHEMERAL_END: u16 = 65_535;
const EPHEMERAL_PORTS: usize = (EPHEMERAL_END - EPHEMERAL_START + 1) as usize;
const EPHEMERAL_WORDS: usize = EPHEMERAL_PORTS.div_ceil(u64::BITS as usize);

/// 按 IPv4、IPv6 或双族地址空间查找空闲 ephemeral port 的有界位图投影。
pub(super) struct EphemeralPorts {
    // OWNER: 三个投影均来自同一端口 owner 的实际 family counts；第三项为两族的并集。
    // 预存并集让双栈申请仍最多读取 257 个 word，而非临时合并两次扫描；不拥有另一份租约。
    occupied: [[u64; EPHEMERAL_WORDS]; 3],
    next: [u16; 3],
}

impl EphemeralPorts {
    pub(super) const fn new() -> Self {
        Self {
            occupied: [[0; EPHEMERAL_WORDS]; 3],
            next: [EPHEMERAL_START; 3],
        }
    }

    /// 返回所选地址空间 cursor 后第一个空闲端口并推进 cursor，不发布占用 bit。
    pub(super) fn next_candidate(&mut self, families: u8) -> Option<u16> {
        let index = usize::from(families.checked_sub(1).expect("empty port address space"));
        let port = self.first_free_from(index, self.next[index])?;
        self.next[index] = if port == EPHEMERAL_END {
            EPHEMERAL_START
        } else {
            port + 1
        };
        Some(port)
    }

    /// 在实际 membership 发布/迁移/退休后替换该端口的全部族投影，不累加独立计数。
    pub(super) fn publish(&mut self, port: u16, families: u8) {
        if (EPHEMERAL_START..=EPHEMERAL_END).contains(&port) {
            let bit = usize::from(port - EPHEMERAL_START);
            let mask = 1 << (bit % 64);
            for (index, occupied) in self.occupied.iter_mut().enumerate() {
                let covers = families & (index as u8 + 1) != 0;
                if covers {
                    occupied[bit / 64] |= mask;
                } else {
                    occupied[bit / 64] &= !mask;
                }
            }
        }
    }

    fn first_free_from(&self, index: usize, cursor: u16) -> Option<u16> {
        let occupied = &self.occupied[index];
        let start = usize::from(cursor.clamp(EPHEMERAL_START, EPHEMERAL_END) - EPHEMERAL_START);
        let start_word = start / u64::BITS as usize;
        let start_bit = start % u64::BITS as usize;
        for offset in 0..EPHEMERAL_WORDS {
            let word = (start_word + offset) % EPHEMERAL_WORDS;
            let mut free = !occupied[word];
            if offset == 0 {
                free &= u64::MAX << start_bit;
            }
            if free != 0 {
                let bit = free.trailing_zeros() as usize;
                return Some(EPHEMERAL_START + (word * u64::BITS as usize + bit) as u16);
            }
        }
        if start_bit != 0 {
            let free = !occupied[start_word] & ((1u64 << start_bit) - 1);
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
