//! ECMP 递归展开用精确有理数；不能表示时返回 Capacity，不改变权重比例。
use super::{Adjacency, Error, Member};
use alloc::vec::Vec;

#[derive(Clone, Copy)]
pub(super) struct Weight {
    numerator: u128,
    denominator: u128,
}
impl Weight {
    pub(super) fn direct(weight: u32) -> Self {
        Self {
            numerator: u128::from(weight),
            denominator: 1,
        }
    }
    pub(super) fn scale(self, outer: u32, total: u64) -> Result<Self, Error> {
        // 两个因子都先约分，再交叉消去；否则 2/4 等中间值会破坏 relative 的最简不变量。
        let divisor = gcd(u128::from(outer), u128::from(total));
        let outer = u128::from(outer) / divisor;
        let total = u128::from(total) / divisor;
        let first = gcd(self.numerator, total);
        let second = gcd(outer, self.denominator);
        Ok(Self {
            numerator: (self.numerator / first)
                .checked_mul(outer / second)
                .ok_or(Error::Capacity)?,
            denominator: (self.denominator / second)
                .checked_mul(total / first)
                .ok_or(Error::Capacity)?,
        })
    }
    fn add(self, other: Self) -> Result<Self, Error> {
        let common = gcd(self.denominator, other.denominator);
        let left = other.denominator / common;
        let right = self.denominator / common;
        let numerator = self
            .numerator
            .checked_mul(left)
            .and_then(|value| {
                other
                    .numerator
                    .checked_mul(right)
                    .and_then(|other| value.checked_add(other))
            })
            .ok_or(Error::Capacity)?;
        let denominator = self.denominator.checked_mul(left).ok_or(Error::Capacity)?;
        let divisor = gcd(numerator, denominator);
        Ok(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }
    fn relative(self, base: Self) -> Result<Self, Error> {
        // 先交叉约分，公共因子不应让最终可表达的比例误溢出。
        let first = gcd(self.numerator, base.numerator);
        let second = gcd(base.denominator, self.denominator);
        Ok(Self {
            numerator: (self.numerator / first)
                .checked_mul(base.denominator / second)
                .ok_or(Error::Capacity)?,
            denominator: (self.denominator / second)
                .checked_mul(base.numerator / first)
                .ok_or(Error::Capacity)?,
        })
    }
}
fn gcd(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}
#[derive(Clone, Copy)]
pub(super) struct Part {
    pub target: Adjacency,
    pub weight: Weight,
}

pub(super) fn normalize(mut parts: Vec<Part>) -> Result<Vec<Member>, Error> {
    parts.sort_unstable_by_key(|part| part.target);
    let mut count = 0;
    for index in 0..parts.len() {
        let part = parts[index];
        if count != 0 && parts[count - 1].target == part.target {
            parts[count - 1].weight = parts[count - 1].weight.add(part.weight)?;
        } else {
            parts[count] = part;
            count += 1;
        }
    }
    parts.truncate(count);
    u32::try_from(count).map_err(|_| Error::Capacity)?;
    let mut members = Vec::new();
    members
        .try_reserve_exact(count)
        .map_err(|_| Error::NoMemory)?;
    let Some(base) = parts.first().map(|part| part.weight) else {
        return Ok(members);
    };
    // 相对首项的分母 LCM 是最终首项的最小整数权重，超过 u32 即没有精确 UAPI 表示。
    let mut common = 1u128;
    for part in &mut parts {
        part.weight = part.weight.relative(base)?;
        common = (common / gcd(common, part.weight.denominator))
            .checked_mul(part.weight.denominator)
            .filter(|value| *value <= u128::from(u32::MAX))
            .ok_or(Error::Capacity)?;
    }
    for part in parts {
        let weight = part
            .weight
            .numerator
            .checked_mul(common / part.weight.denominator)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or(Error::Capacity)?;
        members.push(Member {
            target: part.target,
            weight,
        });
    }
    Ok(members)
}
