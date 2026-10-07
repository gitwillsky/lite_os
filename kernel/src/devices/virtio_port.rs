//! Linux character-device projection for the selected VirtIO Console byte stream.

use alloc::sync::Arc;
use spin::Once;
use syscall_abi::errno;

use crate::{
    drivers::{PortDevice, PortError},
    fs::{
        FileSystemError,
        device::{
            self, CharacterDriver, DeviceError, DeviceFile, DeviceNumber, DeviceWaitSources,
            OpenRequest, UserFault, UserInput, UserOutput,
        },
    },
    ipc::{Pipe, PipeDirection, PipeEnd},
};

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;
/// 单次 adapter 读写的 kernel 中转上限。
const TRANSFER_BYTES: usize = 4096;

/// VirtIO ports use a dynamically allocated Linux character major; LiteOS reserves this local
/// identity because the pathname/protocol, not the number, is the ABI.
const PORT_NUMBER: DeviceNumber = DeviceNumber::new(253, 1);
/// SPICE agent 的标准 named-port 路径。
const PORT_PATH: &[u8] = b"virtio-ports/com.redhat.spice.0";

/// System-wide projection of one standard VirtIO port.
pub(crate) struct Port {
    device: Arc<dyn PortDevice>,
    notification_read: Arc<PipeEnd>,
    notification_write: Arc<PipeEnd>,
}

// OWNER: virtio_port retains the only task-aware readiness projection for the physical port.
// Without one publication, separate devfs opens could signal different Pipes and lose wakeups.
static PORT: Once<Arc<Port>> = Once::new();

/// 打开唯一 port 的 driver。
struct PortDriver;

impl CharacterDriver for PortDriver {
    fn open(&self, _request: &OpenRequest<'_>) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
        PORT.get()
            .cloned()
            .map(|port| port as Arc<dyn DeviceFile>)
            .ok_or(FileSystemError::NotFound)
    }
}

/// Publish the selected adapter and register its character device.
///
/// # Parameters
///
/// - `device`: Platform-owned physical adapter.
///
/// # Returns
///
/// The first complete publication succeeds.
///
/// # Errors
///
/// 重复初始化、Pipe/注册表分配失败返回 unit error。
pub(crate) fn init(device: Arc<dyn PortDevice>) -> Result<(), ()> {
    if PORT.get().is_some() {
        return Err(());
    }
    // 只承载合并 readiness edge 的 read/write endpoints。
    let notification = crate::ipc::Pipe::notification_pair()?;
    let port = Arc::try_new(Port {
        device,
        notification_read: notification.0,
        notification_write: notification.1,
    })
    .map_err(|_| ())?;
    let driver = Arc::try_new(PortDriver).map_err(|_| ())?;
    let work = crate::deferred::register(port_work)?;
    let adapter = port.device.clone();
    PORT.call_once(|| port);
    adapter.bind_completion_work(work);
    device::register_driver(PORT_NUMBER, 1, driver).map_err(|_| ())?;
    device::register_node(PORT_PATH, PORT_NUMBER, 0o600).map_err(|_| ())
}

fn port_error(error: PortError) -> DeviceError {
    match error {
        PortError::WouldBlock => DeviceError::WouldBlock,
        PortError::Disconnected => DeviceError::Errno(errno::EIO),
    }
}

fn fault(_: UserFault) -> DeviceError {
    DeviceError::Errno(errno::EFAULT)
}

impl DeviceFile for Port {
    /// 字节流：一次 read 交付一批当前可得数据；无数据时按 `nonblocking` 等待。
    fn read(&self, output: &mut dyn UserOutput, nonblocking: bool) -> Result<(), DeviceError> {
        let mut buffer = [0u8; TRANSFER_BYTES];
        let requested = output.remaining().min(buffer.len());
        // adapter read 会出队数据；先证明目标可写，避免 fault 丢弃已出队字节。
        output.reserve(requested).map_err(fault)?;
        loop {
            match self.device.read(&mut buffer[..requested]) {
                Err(PortError::WouldBlock) => device::wait_ready(self, POLLIN, nonblocking)?,
                result => {
                    let count = result.map_err(port_error)?;
                    return output.write(&buffer[..count]).map_err(fault);
                }
            }
        }
    }

    /// 写出全部输入；只在尚未写出任何字节时阻塞，已有进度后队列满即返回部分完成。
    fn write(&self, input: &mut dyn UserInput, nonblocking: bool) -> Result<(), DeviceError> {
        let mut buffer = [0u8; TRANSFER_BYTES];
        let mut progressed = false;
        while input.remaining() != 0 {
            let requested = input.remaining().min(buffer.len());
            input.copy(&mut buffer[..requested]).map_err(fault)?;
            let written = loop {
                match self.device.write(&buffer[..requested]) {
                    Err(PortError::WouldBlock) if progressed => return Ok(()),
                    Err(PortError::WouldBlock) => device::wait_ready(self, POLLOUT, nonblocking)?,
                    result => break result.map_err(port_error)?,
                }
            };
            input.consume(written);
            progressed = true;
            if written < requested {
                break;
            }
        }
        Ok(())
    }

    fn poll(&self, events: i16) -> i16 {
        if !self.device.connected() {
            return POLLERR | POLLHUP;
        }
        let mut ready = 0;
        if self.device.readable() {
            ready |= events & POLLIN;
        }
        if self.device.writable() {
            ready |= events & POLLOUT;
        }
        ready
    }

    fn wait_sources(&self, events: i16) -> DeviceWaitSources {
        DeviceWaitSources::pipe(self.notification_read.pipe(), events)
    }

    fn readiness_generation(&self) -> u64 {
        self.notification_read
            .pipe()
            .readiness_generation(PipeDirection::Read)
    }

    fn prepare_wait(&self, events: i16) -> Option<Arc<Pipe>> {
        if self.poll(events) != 0 {
            return None;
        }
        self.notification_read.drain_readiness();
        (self.poll(events) == 0).then(|| self.notification_read.pipe())
    }
}

/// Drain adapter completions and publish one merged task readiness edge.
///
/// # Returns
///
/// `true` when a bounded pass left queue backlog.
fn port_work(_now_ns: u64) -> bool {
    dispatch_work()
}

fn dispatch_work() -> bool {
    let Some(port) = PORT.get() else {
        return false;
    };
    let activity = port.device.dispatch();
    if activity.readable_changed || activity.writable_changed {
        port.notification_write.signal_readiness();
    }
    activity.backlog
}
