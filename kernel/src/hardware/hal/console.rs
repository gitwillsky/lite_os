//! 平台串口 console：hardirq 发布的 RX ring 与 platform 提供的同步输出原语组成 `ConsoleDevice`。
//!
//! 与具体 UART register ABI 无关；platform 只提供 Linux 设备名与单字节输出，并从 hardirq 发布已
//! drain 的输入。

use alloc::{collections::VecDeque, sync::Arc};
use spin::Once;

use crate::sync::IrqMutex;

use super::InterruptError;
use super::registry::AppendOnlyRegistry;

const RX_CAPACITY: usize = 1024;

struct ConsoleInput {
    rx: VecDeque<u8>,
}

// OWNER: generic console input domain uniquely owns the fixed-capacity console RX ring. Concrete platform
// handlers only publish already-drained bytes and never retain a second software queue.
static CONSOLE_INPUT: Once<IrqMutex<ConsoleInput>> = Once::new();

/// 初始化唯一 console RX ring。
fn init_ring() -> Result<(), InterruptError> {
    let mut rx = VecDeque::new();
    rx.try_reserve_exact(RX_CAPACITY)
        .map_err(|_| InterruptError::NoMemory)?;
    CONSOLE_INPUT.call_once(|| IrqMutex::new(ConsoleInput { rx }));
    Ok(())
}

/// 发布 concrete platform handler 已从 hardware FIFO drain 的 byte batch。
///
/// ring 满时丢弃 batch 尾部；hardware FIFO 已由 platform drain，因此不会维持 level IRQ。
pub(crate) fn publish_received(bytes: &[u8]) {
    let mut input = CONSOLE_INPUT.wait().lock();
    let available = RX_CAPACITY.saturating_sub(input.rx.len());
    input
        .rx
        .extend(bytes[..bytes.len().min(available)].iter().copied());
}

/// 从唯一 console RX ring 非阻塞读取已有输入。
fn read(bytes: &mut [u8]) -> usize {
    let mut input = CONSOLE_INPUT.wait().lock();
    let count = bytes.len().min(input.rx.len());
    for byte in &mut bytes[..count] {
        *byte = input
            .rx
            .pop_front()
            .expect("console RX length changed under lock");
    }
    count
}

/// ring 非空；hardirq 据此决定是否发布 console deferred work。
pub(crate) fn input_ready() -> bool {
    !CONSOLE_INPUT.wait().lock().rx.is_empty()
}

fn discard_input() -> usize {
    let mut input = CONSOLE_INPUT.wait().lock();
    let count = input.rx.len();
    input.rx.clear();
    count
}

/// console 写出失败（底层 UART/firmware 拒绝）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConsoleError;

/// TTY 层使用的 console 设备 seam。
pub(crate) trait ConsoleDevice: Send + Sync {
    /// Linux 设备名（例如 `ttyAMA0`、`ttyS0`），供 `console=` 选择。
    fn name(&self) -> &[u8];

    /// 非阻塞读取已收到的输入，返回字节数。
    fn read(&self, bytes: &mut [u8]) -> usize;

    /// 是否有尚未读取的输入。
    fn input_ready(&self) -> bool;

    /// 丢弃尚未读取的输入，返回丢弃的字节数。
    fn discard_input(&self) -> usize;

    /// 同步写出全部字节，不睡眠、不保留发送队列。
    ///
    /// # Errors
    ///
    /// 底层输出拒绝时返回 [`ConsoleError`]。
    fn write(&self, bytes: &[u8]) -> Result<(), ConsoleError>;
}

/// platform UART 的同步单字节输出原语。
pub(crate) type SerialOutput = fn(u8) -> Result<(), ConsoleError>;

/// 唯一 RX ring 加 platform 输出原语组成的串口 console。
struct SerialConsole {
    name: &'static [u8],
    output: SerialOutput,
}

impl ConsoleDevice for SerialConsole {
    fn name(&self) -> &[u8] {
        self.name
    }

    fn read(&self, bytes: &mut [u8]) -> usize {
        read(bytes)
    }

    fn input_ready(&self) -> bool {
        input_ready()
    }

    fn discard_input(&self) -> usize {
        discard_input()
    }

    fn write(&self, bytes: &[u8]) -> Result<(), ConsoleError> {
        bytes.iter().try_for_each(|byte| (self.output)(*byte))
    }
}

/// 初始化 RX ring 并把平台串口发布为 console 设备。
///
/// # Parameters
///
/// - `name`: Linux 设备名。
/// - `output`: 同步单字节输出原语。
///
/// # Errors
///
/// ring 或注册表分配失败返回 `NoMemory`。
pub(crate) fn register_serial(
    name: &'static [u8],
    output: SerialOutput,
) -> Result<(), InterruptError> {
    init_ring()?;
    let console =
        Arc::try_new(SerialConsole { name, output }).map_err(|_| InterruptError::NoMemory)?;
    CONSOLE_DEVICES
        .register(console)
        .map_err(|_| InterruptError::NoMemory)?;
    Ok(())
}

// OWNER: console 设备的唯一发布点；TTY 按 `console=` 名称选择其一作为 `/dev/console`。缺失时 platform 的
// 串口与 tty 之间没有可枚举的 console 集合。
static CONSOLE_DEVICES: AppendOnlyRegistry<dyn ConsoleDevice> = AppendOnlyRegistry::new();

/// 第 `index` 个 console 设备。
pub(crate) fn console_device(index: usize) -> Option<Arc<dyn ConsoleDevice>> {
    CONSOLE_DEVICES.get(index)
}
