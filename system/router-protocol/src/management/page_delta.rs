//! 采样页共用的变化位图，只编码变化的位置。

use crate::{Decoder, Invalid};
use alloc::vec::Vec;

pub(crate) struct Delta<T> {
    count: usize,
    mask: u64,
    rows: Vec<T>,
}

pub(crate) fn changed<T: PartialEq>(before: &[T], after: &[T]) -> u64 {
    after.iter().enumerate().fold(0u64, |mask, (index, row)| {
        if before.get(index) == Some(row) {
            mask
        } else {
            mask | (1 << index)
        }
    })
}

pub(crate) fn write<T: Copy>(
    output: &mut Vec<u8>,
    mask: u64,
    rows: &[T],
    mut encode: impl FnMut(T, &mut Vec<u8>) -> Result<(), Invalid>,
) -> Result<(), Invalid> {
    output.extend_from_slice(&mask.to_le_bytes());
    for (index, row) in rows.iter().enumerate() {
        if mask & (1 << index) != 0 {
            encode(*row, output)?;
        }
    }
    Ok(())
}

impl<T: Copy> Delta<T> {
    pub(crate) fn read(
        input: &mut Decoder<'_>,
        count: usize,
        mut decode: impl FnMut(&mut Decoder<'_>) -> Result<T, Invalid>,
    ) -> Result<Self, Invalid> {
        let mask = input.u64()?;
        if count > 64 || count < 64 && mask >> count != 0 {
            return Err(Invalid);
        }
        let mut rows = Vec::new();
        rows.try_reserve_exact(mask.count_ones() as usize)
            .map_err(|_| Invalid)?;
        for _ in 0..mask.count_ones() {
            rows.push(decode(input)?);
        }
        Ok(Self { count, mask, rows })
    }

    pub(crate) fn apply(self, previous: &[T]) -> Result<Vec<T>, Invalid> {
        let mut rows = Vec::new();
        rows.try_reserve_exact(self.count).map_err(|_| Invalid)?;
        let mut changed = self.rows.into_iter();
        for index in 0..self.count {
            let row = if self.mask & (1 << index) != 0 {
                changed.next()
            } else {
                previous.get(index).copied()
            };
            rows.push(row.ok_or(Invalid)?);
        }
        Ok(rows)
    }
}
