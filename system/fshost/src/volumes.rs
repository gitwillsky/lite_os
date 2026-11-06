//! 文件系统服务实例：fshost 为每块盘启动一个 ext4 实例（Fuchsia fshost 启动 fxfs/minfs）。
//!
//! 1. 实例在 fshost 的文件系统 Job 中运行，只拿到控制 channel；块设备连接由 fshost 在每次挂载时建立。
//! 2. 同一块盘的再次挂载（linuxd 重启后）交给同一实例：实例按超级块 UUID 串行化同一文件系统的会话，
//!    旧会话的日志与回写完成前新会话不会打开它。两个实例同时写同一文件系统会损坏它，因此每盘只有一个实例。
//! 3. 实例终止后，下一次挂载启动新实例；仍挂载着的使用者看到 FUSE channel `PEER_CLOSED`。

use alloc::sync::Arc;
use alloc::vec::Vec;

use device_model::BlockDevice;
use filesystem_protocol::{MountClient, RemoteMount, VolumeMountService};
use native_runtime::native_abi::{self, Status, rights, signals};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Job, Process, Vmar};

const EXT4_STACK_SIZE: usize = 256 * 1024;

/// 启动 ext4 实例所需的 Job 与映像。
pub(crate) struct Launcher {
    job: Job,
    image: &'static [u8],
}

/// 一个运行中的 ext4 实例。
pub(crate) struct Instance {
    pub(crate) process: Process,
    pub(crate) control: MountClient,
}

impl Launcher {
    pub(crate) fn new(job: Job, image: &'static [u8]) -> Self {
        Self { job, image }
    }

    /// 启动一个 ext4 实例。
    ///
    /// # Errors
    ///
    /// Job、进程或 channel 创建失败，或装载失败。
    pub(crate) fn launch(&self) -> Result<Instance, Status> {
        let job = Job::create(&self.job)?;
        let (process, vmar) = Process::create(&job, b"ext4", 0)?;
        let runtime_vmar: Vmar = vmar.duplicate(rights::SAME)?;
        let (client, server) = Channel::create()?;
        native_runtime::launch::launch(native_runtime::launch::Launch {
            process: &process,
            image_vmar: &vmar,
            runtime_vmar,
            name: b"ext4",
            image: self.image,
            arguments: &[],
            handles: alloc::vec![server.into_handle()],
            stack_size: EXT4_STACK_SIZE,
        })?;
        Ok(Instance {
            process,
            control: MountClient::new(client),
        })
    }
}

/// 交给 Linux 的块盘及其 ext4 实例。
pub(crate) struct Volumes {
    launcher: Arc<Launcher>,
    devices: Arc<Vec<Arc<dyn BlockDevice>>>,
    // OWNER: 每块盘至多一个实例；锁同时串行化同一盘的启动与挂载转发。
    instances: Mutex<Vec<Option<Instance>>>,
}

impl Volumes {
    pub(crate) fn new(launcher: Arc<Launcher>, devices: Arc<Vec<Arc<dyn BlockDevice>>>) -> Self {
        let instances = (0..devices.len()).map(|_| None).collect();
        Self {
            launcher,
            devices,
            instances: Mutex::new(instances),
        }
    }
}

fn terminated(process: &Process) -> bool {
    process.wait_one(signals::TERMINATED, 0).is_ok()
}

impl VolumeMountService for Volumes {
    fn mount_volume(
        &self,
        disk: usize,
        first: u64,
        count: u64,
        read_only: bool,
    ) -> Result<(Channel, RemoteMount), Status> {
        let device = self.devices.get(disk).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        if count == 0
            || first
                .checked_add(count)
                .is_none_or(|end| end > device.block_count())
        {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let block = device
            .share_range(first, count)
            .ok_or(native_abi::ERR_NOT_SUPPORTED)?;
        let mut instances = self.instances.lock();
        let slot = &mut instances[disk];
        if slot
            .as_ref()
            .is_none_or(|instance| terminated(&instance.process))
        {
            *slot = Some(self.launcher.launch()?);
        }
        let instance = slot.as_ref().expect("instance launched above");
        instance.control.mount(block, read_only)
    }
}
