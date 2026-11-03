//! 启动时取得、服务存活期间不变的设备目录（角色约定见 `launch_protocol::driver_host`）。

use alloc::sync::Arc;

use device_protocol::directory::{ALL_CLASSES, Directory};
use native_runtime::sync::Once;
use native_runtime::{Channel, Startup};

// OWNER: 启动时安装一次；之后只读。
static DIRECTORY: Once<Arc<Directory>> = Once::new();

/// 以本宿主的全部设备类服务目录 channel；driver-manager 持有客户端并汇聚。
///
/// # Panics
///
/// 目录分配或服务线程创建失败。
pub(crate) fn install(startup: &mut Startup) {
    DIRECTORY.call_once(|| {
        let directory = Directory::new().expect("device directory allocation");
        directory
            .serve(
                Channel::from_handle(startup.take(launch_protocol::driver_host::DIRECTORY)),
                ALL_CLASSES,
            )
            .expect("device directory service");
        directory
    });
}

/// 设备目录：本宿主的驱动在此登记设备。
pub(crate) fn directory() -> &'static Directory {
    DIRECTORY
        .get()
        .expect("driver-host directory read before startup")
}
