//! 块设备节点的字节范围读写：对齐、头尾 read-modify-write、设备末尾截断与失败行为。

use crate::fs::block_range::{BlockStore, RangeError, read_range, write_range};
use std::{cell::RefCell, vec, vec::Vec};

const BLOCK: usize = 8;

struct Disk {
    bytes: RefCell<Vec<u8>>,
    /// 第 N 次 write_block 起失败（`usize::MAX` 表示不失败）。
    fail_write_from: RefCell<usize>,
    writes: RefCell<usize>,
}

impl Disk {
    fn new(blocks: usize) -> Self {
        Self {
            bytes: RefCell::new((0..blocks * BLOCK).map(|byte| byte as u8).collect()),
            fail_write_from: RefCell::new(usize::MAX),
            writes: RefCell::new(0),
        }
    }
}

impl BlockStore for Disk {
    type Error = &'static str;

    fn block_size(&self) -> usize {
        BLOCK
    }

    fn block_count(&self) -> u64 {
        (self.bytes.borrow().len() / BLOCK) as u64
    }

    fn read_block(&self, block: usize, buffer: &mut [u8]) -> Result<(), Self::Error> {
        buffer.copy_from_slice(&self.bytes.borrow()[block * BLOCK..(block + 1) * BLOCK]);
        Ok(())
    }

    fn write_block(&self, block: usize, buffer: &[u8]) -> Result<(), Self::Error> {
        let mut writes = self.writes.borrow_mut();
        if *writes >= *self.fail_write_from.borrow() {
            return Err("io");
        }
        *writes += 1;
        self.bytes.borrow_mut()[block * BLOCK..(block + 1) * BLOCK].copy_from_slice(buffer);
        Ok(())
    }
}

#[test]
fn unaligned_read_spans_blocks_and_stops_at_the_end_of_the_device() {
    let disk = Disk::new(3);
    let mut scratch = [0u8; BLOCK];
    let mut out = [0u8; 10];
    assert_eq!(read_range(&disk, 5, &mut out, &mut scratch), Ok(10));
    assert_eq!(out, [5, 6, 7, 8, 9, 10, 11, 12, 13, 14]);
    let mut tail = [0u8; 10];
    assert_eq!(read_range(&disk, 20, &mut tail, &mut scratch), Ok(4));
    assert_eq!(&tail[..4], [20, 21, 22, 23]);
    assert_eq!(read_range(&disk, 24, &mut tail, &mut scratch), Ok(0));
}

#[test]
fn partial_block_write_preserves_neighbouring_bytes() {
    let disk = Disk::new(3);
    let mut scratch = [0u8; BLOCK];
    assert_eq!(write_range(&disk, 6, &[0xaa; 4], &mut scratch), Ok(4));
    let bytes = disk.bytes.borrow();
    assert_eq!(&bytes[4..12], [4, 5, 0xaa, 0xaa, 0xaa, 0xaa, 10, 11]);
    assert_eq!(bytes[3], 3);
    assert_eq!(bytes[12], 12);
}

#[test]
fn aligned_full_blocks_are_written_without_reading_first() {
    let disk = Disk::new(2);
    let mut scratch = [0xeeu8; BLOCK];
    assert_eq!(
        write_range(&disk, 0, &[7; 2 * BLOCK], &mut scratch),
        Ok(2 * BLOCK)
    );
    assert_eq!(
        scratch, [0xee; BLOCK],
        "aligned writes must not touch scratch"
    );
    assert_eq!(*disk.bytes.borrow(), vec![7u8; 2 * BLOCK]);
}

#[test]
fn write_is_clipped_at_the_end_and_starting_past_it_is_full() {
    let disk = Disk::new(2);
    let mut scratch = [0u8; BLOCK];
    assert_eq!(write_range(&disk, 12, &[1; 10], &mut scratch), Ok(4));
    assert_eq!(
        write_range(&disk, 16, &[1], &mut scratch),
        Err(RangeError::Full)
    );
    assert_eq!(write_range(&disk, 16, &[], &mut scratch), Ok(0));
}

#[test]
fn failure_after_progress_is_a_short_write_and_before_progress_is_an_error() {
    let disk = Disk::new(4);
    *disk.fail_write_from.borrow_mut() = 1;
    let mut scratch = [0u8; BLOCK];
    assert_eq!(
        write_range(&disk, 0, &[9; 3 * BLOCK], &mut scratch),
        Ok(BLOCK)
    );
    assert_eq!(
        write_range(&disk, BLOCK as u64, &[9; BLOCK], &mut scratch),
        Err(RangeError::Store("io"))
    );
}
