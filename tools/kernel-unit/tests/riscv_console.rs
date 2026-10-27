//! 执行实际 RISC-V console owner，注入 SBI 失败，保证后续批次与尾部仍被冲出。

use std::cell::RefCell;

mod firmware {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct FirmwareError;
}

mod sync {
    pub(crate) struct IrqMutex<T>(std::sync::Mutex<T>);
    impl<T> IrqMutex<T> {
        pub(crate) const fn new(value: T) -> Self {
            Self(std::sync::Mutex::new(value))
        }
        pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, T> {
            self.0.lock().unwrap()
        }
    }
}

thread_local! {
    static BATCHES: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
}

fn debug_console_write_bytes(bytes: &[u8]) -> Result<(), firmware::FirmwareError> {
    BATCHES.with(|batches| {
        let mut batches = batches.borrow_mut();
        batches.push(bytes.to_vec());
        if batches.len() == 1 {
            Err(firmware::FirmwareError)
        } else {
            Ok(())
        }
    })
}

fn debug_console_write(_: u8) -> Result<(), firmware::FirmwareError> {
    unreachable!("tty byte writes must use SBI DBCN batching")
}

#[path = "../../../kernel/src/platform/qemu_virt/riscv64/console.rs"]
#[expect(
    dead_code,
    reason = "host fixture calls the tty entry; the production logging and panic entrypoints remain compiled"
)]
mod console;

#[test]
fn firmware_failure_keeps_flushing_full_batches_and_the_partial_tail() {
    // 两个完整批次先裁决错误后的 flush，避免错误短路实现进入后续零进展循环。
    for length in [512, 600] {
        BATCHES.with(|batches| batches.borrow_mut().clear());
        let bytes: Vec<_> = (0..length).map(|index| (index / 256) as u8).collect();
        assert_eq!(console::write_bytes(&bytes), Err(firmware::FirmwareError));
        BATCHES.with(|batches| {
            assert_eq!(
                *batches.borrow(),
                bytes.chunks(256).map(<[u8]>::to_vec).collect::<Vec<_>>()
            );
        });
        assert_eq!(console::write_bytes(b"next write"), Ok(()));
        BATCHES.with(|batches| assert_eq!(batches.borrow().last().unwrap(), b"next write"));
    }
}
