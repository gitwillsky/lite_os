/// Time specification structure
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimeSpec {
    pub sec: i64,
    pub nsec: i64,
}

impl TimeSpec {
    pub const fn new(sec: i64, nsec: i64) -> Self {
        Self { sec, nsec }
    }

    pub fn zero() -> Self {
        Self::new(0, 0)
    }

    pub fn to_microseconds(&self) -> u64 {
        (self.sec as u64 * 1_000_000) + (self.nsec as u64 / 1000)
    }

    pub fn from_microseconds(us: u64) -> Self {
        Self {
            sec: (us / 1_000_000) as i64,
            nsec: ((us % 1_000_000) * 1000) as i64,
        }
    }
}
