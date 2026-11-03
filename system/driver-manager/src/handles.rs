//! 启动时取得、服务存活期间不变的全局句柄（角色约定见 `launch_protocol::driver_manager`）。

use alloc::sync::Arc;

use device_protocol::directory::{ALL_CLASSES, Directory};
use native_runtime::native_abi;
use native_runtime::sync::Once;
use native_runtime::{Channel, Job, Resource, Startup, Vmo};

struct Handles {
    resource: Resource,
    directory: Arc<Directory>,
    job: Job,
    host_image: &'static [u8],
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
        // 根服务持有完整目录连接，再按类派生给各使用者；目录随 driver-manager 存续。
        let directory = Directory::new().expect("device directory allocation");
        directory
            .serve(
                Channel::from_handle(startup.take(driver_manager::DEVICE_DIRECTORY)),
                ALL_CLASSES,
            )
            .expect("device directory service");
        let image = Vmo::from_handle(startup.take(driver_manager::DRIVER_HOST_IMAGE));
        let size = image.size().expect("driver-host image size") as usize;
        let address = native_runtime::heap::vmar()
            .map(native_abi::vm::READ, 0, &image, 0, size)
            .expect("driver-host image mapping");
        // SAFETY: 只读映射永不解除；映像在服务存活期间不变，每次启动宿主都从中装载。
        let host_image = unsafe { core::slice::from_raw_parts(address as *const u8, size) };
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
pub(crate) fn directory() -> &'static Directory {
    &handles().directory
}

/// 驱动宿主所在的 Job。
pub(crate) fn job() -> &'static Job {
    &handles().job
}

/// 驱动宿主映像。
pub(crate) fn host_image() -> &'static [u8] {
    handles().host_image
}
