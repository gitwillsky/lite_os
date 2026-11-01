//! 启动时取得、服务存活期间不变的全局句柄（角色约定见 `launch_protocol::devmgr`）。

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
    use launch_protocol::devmgr;
    let device_tree = Vmo::from_handle(startup.take(devmgr::DEVICE_TREE));
    HANDLES.call_once(|| Handles {
        resource: Resource::from_handle(startup.take(devmgr::RESOURCE)),
        directory: Channel::from_handle(startup.take(devmgr::DEVICE_DIRECTORY)),
    });
    device_tree
}

fn handles() -> &'static Handles {
    HANDLES.get().expect("devmgr handles read before startup")
}

/// 根 resource。
pub(crate) fn resource() -> &'static Resource {
    &handles().resource
}

/// 设备目录（写端）：每个设备经它发布给使用者。
pub(crate) fn directory() -> &'static Channel {
    &handles().directory
}
