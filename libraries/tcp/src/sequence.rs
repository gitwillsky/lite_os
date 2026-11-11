use core::{cmp, fmt, ops};
/// A TCP sequence number.
///
/// A sequence number is a monotonically advancing integer modulo 2<sup>32</sup>.
/// Sequence numbers do not have a discontiguity when compared pairwise across a signed overflow.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub struct Sequence(pub i32);

impl Sequence {
    pub fn max(self, rhs: Self) -> Self {
        if self > rhs { self } else { rhs }
    }

    pub fn min(self, rhs: Self) -> Self {
        if self < rhs { self } else { rhs }
    }
}

impl fmt::Display for Sequence {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0 as u32)
    }
}

impl ops::Add<usize> for Sequence {
    type Output = Sequence;

    fn add(self, rhs: usize) -> Sequence {
        if rhs > i32::MAX as usize {
            panic!("attempt to add to sequence number with unsigned overflow")
        }
        Sequence(self.0.wrapping_add(rhs as i32))
    }
}

impl ops::Sub<usize> for Sequence {
    type Output = Sequence;

    fn sub(self, rhs: usize) -> Sequence {
        if rhs > i32::MAX as usize {
            panic!("attempt to subtract to sequence number with unsigned overflow")
        }
        Sequence(self.0.wrapping_sub(rhs as i32))
    }
}

impl ops::AddAssign<usize> for Sequence {
    fn add_assign(&mut self, rhs: usize) {
        *self = *self + rhs;
    }
}

impl ops::Sub for Sequence {
    type Output = usize;

    fn sub(self, rhs: Sequence) -> usize {
        let result = self.0.wrapping_sub(rhs.0);
        if result < 0 {
            panic!("attempt to subtract sequence numbers with underflow")
        }
        result as usize
    }
}

impl cmp::PartialOrd for Sequence {
    fn partial_cmp(&self, other: &Sequence) -> Option<cmp::Ordering> {
        self.0.wrapping_sub(other.0).partial_cmp(&0)
    }
}
