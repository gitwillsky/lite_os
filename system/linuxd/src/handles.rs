//! 启动时取得、服务存活期间不变的全局句柄（角色约定见 `launch_protocol::linuxd`）。

use native_runtime::sync::Once;
use native_runtime::{Job, Process, Resource, Startup, Vmo};

struct Handles {
    shared_process: Process,
    job: Job,
    resource: Resource,
}

// OWNER: 启动时安装一次；之后只读。
static HANDLES: Once<Handles> = Once::new();

/// 从启动消息取出角色句柄，返回 DTB VMO（只在平台初始化时使用）。
pub(crate) fn install(startup: &mut Startup) -> Vmo {
    use launch_protocol::linuxd;
    let device_tree = Vmo::from_handle(startup.take(linuxd::DEVICE_TREE));
    HANDLES.call_once(|| Handles {
        shared_process: Process::from_handle(startup.take(linuxd::SHARED_PROCESS)),
        job: Job::from_handle(startup.take(linuxd::JOB)),
        resource: Resource::from_handle(startup.take(linuxd::RESOURCE)),
    });
    device_tree
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

