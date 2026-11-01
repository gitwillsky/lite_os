//! 启动时取得、服务存活期间不变的全局句柄（角色约定见 `launch_protocol::linuxd`）。

use alloc::vec::Vec;

use crate::sync::Once;
use filesystem_protocol::MountClient;
use native_runtime::{Channel, Job, Process, Resource, Startup};
use network_protocol::NetworkClient;

struct Handles {
    shared_process: Process,
    job: Job,
    resource: Resource,
    command_line: Vec<u8>,
    ext4: MountClient,
    network: NetworkClient,
}

// OWNER: 启动时安装一次；之后只读。
static HANDLES: Once<Handles> = Once::new();

/// 从启动消息取出角色句柄与启动参数，返回设备目录 channel（只在设备装配时使用）。
pub(crate) fn install(startup: &mut Startup) -> Channel {
    use launch_protocol::linuxd;
    let directory = Channel::from_handle(startup.take(linuxd::DEVICE_DIRECTORY));
    let command_line = core::mem::take(&mut startup.arguments);
    HANDLES.call_once(|| Handles {
        shared_process: Process::from_handle(startup.take(linuxd::SHARED_PROCESS)),
        job: Job::from_handle(startup.take(linuxd::JOB)),
        resource: Resource::from_handle(startup.take(linuxd::RESOURCE)),
        command_line,
        ext4: MountClient::new(Channel::from_handle(startup.take(linuxd::EXT4))),
        network: NetworkClient::new(Channel::from_handle(startup.take(linuxd::NETWORK))),
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

/// 根 resource。
pub(crate) fn resource() -> &'static Resource {
    &handles().resource
}

/// 根服务转交的启动参数。
pub(crate) fn command_line() -> &'static [u8] {
    &handles().command_line
}

/// ext4d 的挂载控制：`ext4` 类型的每次挂载经它打开块设备上的文件系统。
pub(crate) fn ext4() -> &'static MountClient {
    &handles().ext4
}

/// netd 的控制：转交网卡、创建网络域。
pub(crate) fn network() -> &'static NetworkClient {
    &handles().network
}
