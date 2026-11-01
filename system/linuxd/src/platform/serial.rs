//! 内核拥有的平台串口作为 Linux console：输出经 `SERIAL_WRITE` 原样写出，输入由专属线程以
//! `SERIAL_READ` 阻塞读取后发布到 console RX ring，并唤醒 tty 读取者。

use native_runtime::native_abi::{self, DEADLINE_INFINITE};

use crate::deferred::{self, DeferredWork};
use crate::devices::console::{self, ConsoleError};

/// Linux 为该 UART 使用的设备名，`console=` 按它选择。
#[cfg(target_arch = "aarch64")]
const DEVICE_NAME: &[u8] = b"ttyAMA0";
#[cfg(target_arch = "riscv64")]
const DEVICE_NAME: &[u8] = b"ttyS0";

/// 一次读取的最大字节数（与 RX ring 的发布批次相同量级）。
const READ_BATCH: usize = 64;

fn write(bytes: &[u8]) -> Result<(), ConsoleError> {
    native_runtime::system::serial_write(crate::handles::resource(), bytes).map_err(|_| ConsoleError)
}

/// 注册串口 console 并启动输入线程。
///
/// # Panics
///
/// 注册或线程创建失败时 fail-stop：没有 console 就无法给 init 提供 `/dev/console`。
pub(crate) fn register_console() {
    console::register_serial(DEVICE_NAME, write).expect("serial console registration failed");
    let reader = native_runtime::thread::spawn(b"serial-rx", || {
        let mut bytes = [0u8; READ_BATCH];
        loop {
            match native_runtime::system::serial_read(crate::handles::resource(), &mut bytes, DEADLINE_INFINITE) {
                Ok(0) => {}
                Ok(count) => {
                    console::publish_received(&bytes[..count]);
                    deferred::raise(DeferredWork::CONSOLE);
                }
                Err(native_abi::ERR_CANCELED) => return,
                Err(status) => panic!("serial console read failed: {status}"),
            }
        }
    })
    .expect("serial console reader thread");
    // 输入线程与服务同寿命。
    drop(reader);
}
