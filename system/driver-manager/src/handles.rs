//! 启动时取得、服务存活期间不变的全局句柄（角色约定见 `launch_protocol::driver_manager`）。

use device_protocol::directory::Publisher;
use native_runtime::sync::Once;
use native_runtime::{Channel, Job, Resource, Startup, Vmo};

struct Handles {
    resource: Resource,
    directory: Publisher,
    job: Job,
    host_image: native_runtime::elf::Image,
}

// OWNER: 启动时安装一次；之后只读。
static HANDLES: Once<Handles> = Once::new();

/// 从启动消息取出角色句柄，返回 DTB VMO（只在平台初始化时使用）。
///
/// # Panics
///
/// 驱动宿主映像无法映射时 fail-stop：没有它任何驱动都无法运行。
pub(crate) fn install(startup: &mut Startup) -> Vmo {
    use launch_protocol::driver_manager;
    let device_tree = Vmo::from_handle(startup.take(driver_manager::DEVICE_TREE));
    HANDLES.call_once(|| {
        let directory = Publisher::new(Channel::from_handle(
            startup.take(driver_manager::DEVICE_PUBLISHER),
        ));
        let image = Vmo::from_handle(startup.take(driver_manager::DRIVER_HOST_IMAGE));
        // 映像在 bootfs 子对象里：各段成为它的写时复制子对象，不复制字节。
        let host_image = native_runtime::elf::Image::from_vmo(&image, 0).expect("driver-host ELF");
        Handles {
            resource: Resource::from_handle(startup.take(driver_manager::RESOURCE)),
            directory,
            job: Job::from_handle(startup.take(driver_manager::DRIVER_JOB)),
            host_image,
        }
    });
    device_tree
}

fn handles() -> &'static Handles {
    HANDLES
        .get()
        .expect("driver-manager handles read before startup")
}

/// 根 resource。
pub(crate) fn resource() -> &'static Resource {
    &handles().resource
}

/// 汇聚各宿主设备的目录。
pub(crate) fn directory() -> &'static Publisher {
    &handles().directory
}

/// 驱动宿主所在的 Job。
pub(crate) fn job() -> &'static Job {
    &handles().job
}

/// 驱动宿主映像。
pub(crate) fn host_image() -> &'static native_runtime::elf::Image {
    &handles().host_image
}
