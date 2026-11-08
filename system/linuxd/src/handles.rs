//! 启动时取得、服务存活期间不变的全局句柄（角色约定见 `launch_protocol::linuxd`）。

use alloc::vec::Vec;

use crate::sync::Once;
use filesystem_protocol::VolumeMountClient;
use native_runtime::{Channel, Job, Process, Resource, Startup};
use service_directory::ServiceDirectory;

struct Handles {
    shared_process: Process,
    job: Job,
    resource: Resource,
    command_line: Vec<u8>,
    ext4: VolumeMountClient,
    services: ServiceDirectory,
}

// OWNER: 启动时安装一次；之后只读。
static HANDLES: Once<Handles> = Once::new();

/// 从启动消息取出角色句柄与启动参数，返回设备目录 channel（只在设备装配时使用）。
pub(crate) fn install(startup: &mut Startup) -> Channel {
    use launch_protocol::linuxd;
    let directory = Channel::from_handle(startup.take(linuxd::DEVICE_DIRECTORY));
    let command_line = core::mem::take(&mut startup.arguments);
    let services = ServiceDirectory::new(Channel::from_handle(startup.take(linuxd::SERVICES)));
    HANDLES.call_once(|| Handles {
        shared_process: Process::from_handle(startup.take(linuxd::SHARED_PROCESS)),
        job: Job::from_handle(startup.take(linuxd::JOB)),
        resource: Resource::from_handle(startup.take(linuxd::RESOURCE)),
        command_line,
        ext4: VolumeMountClient::new(
            services
                .connect(service_directory::EXT4)
                .expect("fshost ext4 connection"),
        ),
        services,
    });
    directory
}

fn handles() -> &'static Handles {
    HANDLES.get().expect("linuxd handles read before startup")
}

/// 共享进程：每个 Linux 进程以它为原型创建成员进程。
pub(crate) fn shared_process() -> &'static Process {
    &handles().shared_process
}

/// 容纳全部 Linux 进程的 Job。
pub(crate) fn job() -> &'static Job {
    &handles().job
}

/// 覆盖日志、串口、UTC、电源、内存观测与调度的 system resource，不含设备 authority。
pub(crate) fn resource() -> &'static Resource {
    &handles().resource
}

/// 根服务转交的启动参数。
pub(crate) fn command_line() -> &'static [u8] {
    &handles().command_line
}

/// fshost 的卷挂载：`ext4` 类型的每次挂载经它按盘序号打开文件系统。
pub(crate) fn ext4() -> &'static VolumeMountClient {
    &handles().ext4
}

/// 根服务的服务路由目录：按协议名连接 netstack 等服务的当前实例。
pub(crate) fn services() -> &'static ServiceDirectory {
    &handles().services
}
