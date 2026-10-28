//! `/dev/fuse`（misc 10:229）：每次 open 得到一个未绑定端点，挂载的 `fd=` 或 `FUSE_DEV_IOC_CLONE`
//! 把它绑定到连接（Linux `fuse_dev`）。
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use spin::Once;
use syscall_abi::{errno, fuse};

use super::connection::Connection;
use crate::{
    fs::{
        DeviceNumber, FileSystemError,
        chardev::{
            self, CharacterDriver, DeviceError, DeviceFile, DeviceWaitSources, IoctlCall,
            OpenRequest, UserInput, UserOutput, wait_ready,
        },
        devnode,
    },
    ipc::Pipe,
};

/// Linux `FUSE_MINOR` 在 misc major 下的设备号。
const NUMBER: DeviceNumber = DeviceNumber::new(10, 229);
const POLLIN: i16 = 0x001;
const POLLERR: i16 = 0x008;

struct Binding {
    connection: Arc<Connection>,
    device: u64,
}

/// 一个 `/dev/fuse` 打开文件。
pub(super) struct FuseDevice {
    binding: Once<Binding>,
    // OWNER: release 的幂等标记；OFD 最后释放与构造失败的 Drop 都可能调用 release，缺失时连接的
    // 设备计数会被重复扣减并提前中止仍有守护进程的挂载。
    released: AtomicBool,
}

impl FuseDevice {
    /// 把本端点绑定到 `connection`。
    ///
    /// # Errors
    ///
    /// 已绑定返回 `InvalidOperation`（Linux 对重复挂载同一 fd 返回 `EINVAL`）。
    pub(super) fn bind(&self, connection: &Arc<Connection>) -> Result<(), FileSystemError> {
        let mut bound = false;
        self.binding.call_once(|| {
            bound = true;
            Binding {
                connection: connection.clone(),
                device: connection.attach_device(),
            }
        });
        bound.then_some(()).ok_or(FileSystemError::InvalidOperation)
    }

    /// 从调用者 fd 取得 `/dev/fuse` 端点；其他文件返回 `None`。
    pub(super) fn from_file(file: &dyn DeviceFile) -> Option<&Self> {
        (file as &dyn core::any::Any).downcast_ref()
    }

    fn bound(&self) -> Result<&Binding, DeviceError> {
        self.binding.get().ok_or(DeviceError::Errno(errno::EPERM))
    }

    /// FUSE_DEV_IOC_CLONE：参数是已绑定端点的 fd，本端点加入同一连接（多线程守护进程）。
    fn clone_from(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        let mut bytes = [0u8; 4];
        call.user
            .read(call.argument, &mut bytes)
            .map_err(|_| DeviceError::Errno(errno::EFAULT))?;
        let source = call.user.device_file(i32::from_ne_bytes(bytes))?;
        let connection = Self::from_file(source.as_ref())
            .and_then(|device| device.binding.get())
            .map(|binding| binding.connection.clone())
            .ok_or(DeviceError::Errno(errno::EINVAL))?;
        self.bind(&connection)
            .map(|()| 0)
            .map_err(|_| DeviceError::Errno(errno::EINVAL))
    }
}

impl DeviceFile for FuseDevice {
    fn uring_command(
        &self,
        command: &chardev::UringCommand,
        user: &dyn chardev::UserMemory,
    ) -> Result<alloc::boxed::Box<dyn chardev::PendingCommand>, DeviceError> {
        self.bound()?.connection.uring_command(command, user)
    }

    fn release(&self) {
        if !self.released.swap(true, Ordering::AcqRel)
            && let Some(binding) = self.binding.get()
        {
            binding.connection.detach_device(binding.device);
        }
    }

    fn anonymous_name(&self) -> &'static [u8] {
        b"anon_inode:[fuse]"
    }

    fn read(&self, output: &mut dyn UserOutput, nonblocking: bool) -> Result<(), DeviceError> {
        let binding = self.bound()?;
        binding
            .connection
            .read(binding.device, output, nonblocking, || {
                wait_ready(self, POLLIN, false)
            })
    }

    fn write(&self, input: &mut dyn UserInput, _nonblocking: bool) -> Result<(), DeviceError> {
        let binding = self.bound()?;
        binding
            .connection
            .write(binding.device, input, |code, payload| {
                super::filesystem::notify(&binding.connection, code, payload)
            })
    }

    fn poll(&self, events: i16) -> i16 {
        match self.binding.get() {
            Some(binding) => binding.connection.poll(events),
            None => POLLERR,
        }
    }

    fn wait_sources(&self, events: i16) -> DeviceWaitSources {
        self.binding
            .get()
            .map_or_else(DeviceWaitSources::new, |binding| {
                DeviceWaitSources::pipe(binding.connection.readiness_pipe(), events | POLLERR)
            })
    }

    fn readiness_generation(&self) -> u64 {
        self.binding.get().map_or(0, |binding| {
            binding
                .connection
                .readiness_pipe()
                .readiness_generation(crate::ipc::PipeDirection::Read)
        })
    }

    fn prepare_wait(&self, events: i16) -> Option<Arc<Pipe>> {
        let binding = self.binding.get()?;
        if self.poll(events) != 0 {
            return None;
        }
        binding.connection.drain_readiness();
        (self.poll(events) == 0).then(|| binding.connection.readiness_pipe())
    }

    fn ioctl(&self, call: &IoctlCall<'_>) -> Result<isize, DeviceError> {
        match call.request {
            fuse::DEV_IOC_CLONE => self.clone_from(call),
            fuse::DEV_IOC_BACKING_OPEN | fuse::DEV_IOC_BACKING_CLOSE => {
                self.bound()?.connection.backing_ioctl(call)
            }
            _ => Err(DeviceError::Errno(errno::ENOTTY)),
        }
    }
}

impl Drop for FuseDevice {
    fn drop(&mut self) {
        self.release();
    }
}

struct Driver;

impl CharacterDriver for Driver {
    fn open(&self, _request: &OpenRequest<'_>) -> Result<Arc<dyn DeviceFile>, FileSystemError> {
        Arc::try_new(FuseDevice {
            binding: Once::new(),
            released: AtomicBool::new(false),
        })
        .map(|device| device as Arc<dyn DeviceFile>)
        .map_err(|_| FileSystemError::OutOfMemory)
    }
}

/// 登记 driver 与 `/dev/fuse` 节点（Linux 为 `crw-rw-rw-`，挂载权限由 setuid fusermount3 把关）。
pub(super) fn register() -> Result<(), FileSystemError> {
    let driver = Arc::try_new(Driver).map_err(|_| FileSystemError::OutOfMemory)?;
    chardev::register_driver(NUMBER, 1, driver)?;
    devnode::register_node(b"fuse", NUMBER, 0o666)
}
