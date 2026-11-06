//! Linux character-device projection for the selected VirtIO Console byte stream.

use crate::sync::{Mutex, Once};
use alloc::sync::Arc;
use linux_abi::errno;

use crate::{
    fs::{
        FileSystemError,
        {
            DeviceNumber,
            chardev::{
                self, CharacterDriver, DeviceError, DeviceFile, DeviceWaitSources, OpenRequest,
                UserFault, UserInput, UserOutput,
            },
            devnode,
        },
    },
    ipc::{Pipe, PipeDirection, PipeEnd},
};
use device_model::{PortDevice, PortError};

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

/// One attached VirtIO port session and its task-aware readiness pipe.
pub(crate) struct Port {
    device: Arc<dyn PortDevice>,
    notification_read: Arc<PipeEnd>,
    notification_write: Arc<PipeEnd>,
}

/// devfs projection of the standard named port; the session behind it is replaced on re-attach.
struct Projection {
    // OWNER: 唯一的当前会话；open 只取它，旧 OFD 继续持有旧 `Port` 并在其断开后报告 ERR|HUP。
    // 缺失单一 owner 时新旧会话的 readiness 会发到不同 Pipe，且驱动宿主重启后新端口无法接管节点。
    current: Mutex<Option<Arc<Port>>>,
    work: crate::deferred::DeferredWork,
}

// OWNER: driver/deferred work 只登记一次；之后每次 attach 只替换 `current`。
// 缺失时重复 attach 会重复注册设备号区间而失败。
static PROJECTION: Once<Projection> = Once::new();

/// 打开当前 port 的 driver。
struct PortDriver;

impl CharacterDriver for PortDriver {
    fn open(&self, _request: &OpenRequest<'_>) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
        PROJECTION
            .get()
            .and_then(|projection| projection.current.lock().clone())
            .map(|port| port as Arc<dyn DeviceFile>)
            .ok_or(FileSystemError::NoDevice)
    }
}

fn projection() -> Result<&'static Projection, ()> {
    if let Some(projection) = PROJECTION.get() {
        return Ok(projection);
    }
    let work = crate::deferred::register(port_work)?;
    let driver = Arc::try_new(PortDriver).map_err(|_| ())?;
    chardev::register_driver(PORT_NUMBER, 1, driver).map_err(|_| ())?;
    Ok(PROJECTION.call_once(|| Projection {
        current: Mutex::new(None),
        work,
    }))
}

/// 接入新的 port 会话：替换当前会话并（重新）发布 devfs 节点。
///
/// 驱动宿主重启后端口以新会话重新出现；旧 OFD 仍绑定旧会话，读写返回 EIO、poll 报告 ERR|HUP，
/// 与 Linux 设备热拔后旧 fd 的行为一致，新 open 得到新会话。
///
/// # Parameters
///
/// - `device`: 设备目录中新出现的 port adapter。
///
/// # Errors
///
/// Pipe、deferred work、driver 或节点登记失败返回 unit error。
pub(crate) fn attach(device: Arc<dyn PortDevice>) -> Result<(), ()> {
    // 驱动 open/deferred 只在首次接入时由 linuxd 的设备目录线程登记，此后不再分配。
    let projection = projection()?;
    let notification = crate::ipc::Pipe::notification_pair()?;
    let port = Arc::try_new(Port {
        device,
        notification_read: notification.0,
        notification_write: notification.1,
    })
    .map_err(|_| ())?;
    port.device
        .bind_activity(crate::deferred::activity(projection.work)?);
    let previous = projection.current.lock().replace(port);
    if let Some(previous) = previous {
        // 旧会话的等待者不会再收到 work 派发；唤醒它们以观察断开。
        previous.notification_write.signal_readiness();
    } else {
        devnode::register_node(PORT_PATH, PORT_NUMBER, 0o600).map_err(|_| ())?;
    }
    Ok(())
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
                Err(PortError::WouldBlock) => chardev::wait_ready(self, POLLIN, nonblocking)?,
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
                    Err(PortError::WouldBlock) => chardev::wait_ready(self, POLLOUT, nonblocking)?,
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
    let Some(projection) = PROJECTION.get() else {
        return false;
    };
    let Some(port) = projection.current.lock().clone() else {
        return false;
    };
    let activity = port.device.dispatch();
    if activity.readable_changed || activity.writable_changed {
        port.notification_write.signal_readiness();
    } else if !port.device.connected() {
        // 会话关闭通知只派发一次：撤下节点和当前会话，唤醒旧 OFD 的等待者观察 ERR|HUP。
        let mut current = projection.current.lock();
        if current
            .as_ref()
            .is_some_and(|live| Arc::ptr_eq(live, &port))
        {
            *current = None;
            drop(current);
            devnode::unregister_node(PORT_PATH);
        }
        port.notification_write.signal_readiness();
        return false;
    }
    activity.backlog
}
