//! 块层：块设备注册表、分区表解析与分区视图（块设备 trait 在 `device_model`）。
//!
//! 硬件驱动实现 BlockDevice 并注册，fs 只消费块 I/O 与分区视图。
//! 分区表与字节范围属于块设备层；inode、目录、挂载及文件系统格式属于 fs。

use alloc::sync::Arc;

use device_model::AppendOnlyRegistry;

pub(crate) mod identity;
pub(crate) mod partition_device;
pub(crate) mod partition_table;
pub(crate) mod range;

pub(crate) use device_model::{BlockDevice, BlockError};

// OWNER: 块设备 adapter 的唯一发布点，只追加；index 是 adapter 的稳定 identity，根文件系统与 `fs`
// 的 `/dev` 发布都按它选择。缺失时第二块盘只能 panic 或被丢弃。
static DEVICES: AppendOnlyRegistry<dyn BlockDevice> = AppendOnlyRegistry::new();

/// 按发现顺序发布一个块设备。
///
/// # Returns
///
/// 设备的稳定 index。
///
/// # Errors
///
/// 注册表扩容失败时原样返回 adapter。
pub(crate) fn register(device: Arc<dyn BlockDevice>) -> Result<usize, Arc<dyn BlockDevice>> {
    DEVICES.register(device)
}

/// 从 fshost 的块设备清单接入全部块设备（全盘范围连接），按清单序号发布。
///
/// # Panics
///
/// fshost 不可用或某块设备无法连接：根文件系统所在的盘缺失时无法继续启动。
pub(crate) fn connect_storage() {
    let blocks = filesystem_protocol::BlocksClient::new(
        crate::handles::services()
            .connect(service_directory::BLOCKS)
            .expect("fshost block connection"),
    );
    let count = blocks.count().expect("fshost block count");
    for index in 0..count {
        let channel = blocks.open(index).expect("fshost block device");
        let device = device_protocol::block::from_channel(channel).expect("block device info");
        if register(device).is_err() {
            panic!("block device registry allocation failed");
        }
    }
}

/// 第 `index` 个已发布的块设备。
pub(crate) fn device(index: usize) -> Option<Arc<dyn BlockDevice>> {
    DEVICES.get(index)
}
