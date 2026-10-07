//! Validate the production HAL window against host-owned aligned storage.
#[path = "../../architecture-bench/src/mmio_fixture.rs"]
mod arch;
#[allow(dead_code)]
#[path = "../../../kernel/src/hardware/hal/bus.rs"]
mod bus;

use bus::{BusError, MmioBus};

#[test]
fn invalid_windows_and_subwindows_are_rejected() {
    for (base, size) in [(0, 8), (8, 0), (usize::MAX - 3, 8)] {
        assert!(matches!(
            MmioBus::new(base, size),
            Err(BusError::InvalidAddress)
        ));
    }
    let parent = MmioBus::new(0x1000, 16).unwrap();
    for (offset, size) in [(0, 0), (16, 1), (8, 9), (usize::MAX, 8)] {
        assert!(matches!(
            parent.subwindow(offset, size),
            Err(BusError::InvalidAddress)
        ));
    }
}

#[test]
fn all_widths_check_end_alignment_and_overflow_before_access() {
    let mut storage = [0u64; 2];
    let base = storage.as_mut_ptr() as usize;
    let window = MmioBus::new(base, size_of_val(&storage)).unwrap();
    macro_rules! width {
        ($read:ident, $write:ident, $bytes:expr, $value:expr) => {
            window.$write(16 - $bytes, $value).unwrap();
            assert_eq!(window.$read(16 - $bytes), Ok($value));
            for offset in [16, 17 - $bytes, usize::MAX] {
                assert_eq!(window.$read(offset), Err(BusError::InvalidAddress));
                assert_eq!(window.$write(offset, $value), Err(BusError::InvalidAddress));
            }
            if $bytes > 1 {
                assert_eq!(window.$read(1), Err(BusError::InvalidAddress));
                let unaligned = MmioBus::new(base + 1, 8).unwrap();
                assert_eq!(unaligned.$write(0, $value), Err(BusError::InvalidAddress));
            }
        };
    }
    width!(read_u8, write_u8, 1, 0xa5);
    width!(read_u16, write_u16, 2, 0x1234);
    width!(read_u32, write_u32, 4, 0x1234_5678);
    width!(read_u64, write_u64, 8, 0x1234_5678_9abc_def0);
    let tiny = MmioBus::new(base, 1).unwrap();
    assert_eq!(tiny.read_u64(0), Err(BusError::InvalidAddress));
}

#[test]
fn subwindow_uses_parent_storage_and_cannot_reach_adjacent_registers() {
    let mut storage = [0u64; 2];
    let parent = MmioBus::new(storage.as_mut_ptr() as usize, size_of_val(&storage)).unwrap();
    let child = parent.subwindow(8, 8).unwrap();
    child.write_u64(0, 0x1234_5678).unwrap();
    assert_eq!(parent.read_u64(8), Ok(0x1234_5678));
    assert_eq!(parent.read_u64(0), Ok(0));
    assert_eq!(child.read_u8(8), Err(BusError::InvalidAddress));
    assert_eq!(child.write_u32(8, 1), Err(BusError::InvalidAddress));
}
