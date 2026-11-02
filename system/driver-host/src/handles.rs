//! 启动时取得、服务存活期间不变的全局句柄（角色约定见 `launch_protocol::driver-host`）。

use alloc::sync::Arc;

use device_protocol::directory::{ALL_CLASSES, Directory};
use native_runtime::sync::Once;
use native_runtime::{Channel, Resource, Startup, Vmo};

struct Handles {
    resource: Resource,
    directory: Arc<Directory>,
}

// OWNER: 启动时安装一次；之后只读。
static HANDLES: Once<Handles> = Once::new();

/// 从启动消息取出角色句柄，返回 DTB VMO（只在平台初始化时使用）。
pub(crate) fn install(startup: &mut Startup) -> Vmo {
    use launch_protocol::driver_host;
    let device_tree = Vmo::from_handle(startup.take(driver_host::DEVICE_TREE));
    HANDLES.call_once(|| {
        // 根服务持有完整目录连接，再按类派生给各使用者；目录随 driver-host 存续。
        let directory = Directory::new().expect("device directory allocation");
        directory
            .serve(
                Channel::from_handle(startup.take(driver_host::DEVICE_DIRECTORY)),
                ALL_CLASSES,
            )
            .expect("device directory service");
        Handles {
            resource: Resource::from_handle(startup.take(driver_host::RESOURCE)),
            directory,
        }
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

/// 设备目录：每个设备在此登记，使用者经目录连接打开会话。
pub(crate) fn directory() -> &'static Directory {
    &handles().directory
}
