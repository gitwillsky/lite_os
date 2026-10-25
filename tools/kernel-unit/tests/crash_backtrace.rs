//! Exercise the production fail-stop walker with owned, initialized frame records.
//! The memory seam supplies the current stack extent; no live host/guest call-chain record is dereferenced.

use std::cell::Cell;

thread_local! {
    static STACK: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

mod memory {
    pub const KERNEL_STACK_SIZE: usize = 128 * 1024;

    pub fn kernel_stack_bounds(_: usize) -> Option<(usize, usize)> {
        Some(super::STACK.get())
    }
}

mod arch {
    pub mod backtrace {
        pub fn stack_pointer() -> usize {
            super::super::STACK.get().0
        }

        pub unsafe fn read_frame(frame: usize) -> (usize, usize) {
            let record = frame as *const usize;
            // SAFETY: the production walker checks the complete record lies in the owned arena.
            unsafe { (record.read(), record.add(1).read()) }
        }
    }
}

#[path = "../../../kernel/src/crash/backtrace.rs"]
mod production;

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct Frame {
    next: usize,
    address: usize,
}

#[test]
fn walker_stays_in_the_current_stack_and_stops_at_corrupt_links() {
    let mut arena = [Frame {
        next: 0,
        address: 0,
    }; 8];
    let base = arena.as_ptr() as usize;
    let end = base + std::mem::size_of_val(&arena);
    STACK.set((base, end));
    arena[1] = Frame {
        next: base + 32,
        address: 0x1234,
    };
    arena[2] = Frame {
        next: base + 48,
        address: 0x5678,
    };
    let mut out = [0; production::MAX_FRAMES];
    assert_eq!(production::capture(base + 16, &mut out), 2);
    assert_eq!(&out[..2], &[0x1234, 0x5678]);

    // A valid record in another allocated stack is still outside the current SP's extent.
    let foreign = [Frame {
        next: 0,
        address: 0x9999,
    }; 4];
    let foreign_frame = foreign.as_ptr() as usize + 16;
    assert_eq!(production::capture(foreign_frame, &mut out), 0);

    arena[1].next = base + 16;
    assert_eq!(production::capture(base + 16, &mut out), 1);
    arena[1].next = usize::MAX - 15;
    assert_eq!(production::capture(base + 16, &mut out), 1);
    assert_eq!(production::capture(base, &mut out), 0);
    assert_eq!(production::capture(base + 17, &mut out), 0);
}
