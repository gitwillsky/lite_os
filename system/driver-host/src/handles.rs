//! 启动时取得、服务存活期间不变的全局句柄（角色约定见 `launch_protocol::driver-host`）。

use native_runtime::sync::Once;
use native_runtime::{Channel, Resource, Startup, Vmo};

struct Handles {
    resource: Resource,
    directory: Channel,
}

// OWNER: 启动时安装一次；之后只读。
static HANDLES: Once<Handles> = Once::new();

/// 从启动消息取出角色句柄，返回 DTB VMO（只在平台初始化时使用）。
pub(crate) fn install(startup: &mut Startup) -> Vmo {
    use launch_protocol::driver_host;
    let device_tree = Vmo::from_handle(startup.take(driver_host::DEVICE_TREE));
    HANDLES.call_once(|| Handles {
        resource: Resource::from_handle(startup.take(driver_host::RESOURCE)),
        directory: Channel::from_handle(startup.take(driver_host::DEVICE_DIRECTORY)),
    });
    device_tree
}

fn handles() -> &'static Handles {
    HANDLES
        .get()
        .expect("driver-host handles read before startup")
}

/// 根 resource。
pub(crate) fn resource() -> &'static Resource {
    &handles().resource
}

/// 设备目录（写端）：每个设备经它发布给使用者。
pub(crate) fn directory() -> &'static Channel {
    &handles().directory
}
