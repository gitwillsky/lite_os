#![no_std]
#![no_main]

//! fshost：存储宿主（对应 Fuchsia fshost）。
//!
//! 1. 从根服务路由来的块设备目录打开启动时发现的全部块设备，持有各设备唯一的驱动会话；
//! 2. 持有 ext4d 的控制连接；使用者不直接接触块设备驱动会话或 ext4d；
//! 3. 经 outgoing 提供两个协议：块设备清单（按序号交出全盘范围连接，Linux 块设备 UAPI 与挂载在其上实现）
//!    与 ext4 挂载（原样转发给 ext4d）。使用者（linuxd）重启后重新连接即可，块设备会话与 ext4d 不受影响。

extern crate alloc;
#[macro_use]
extern crate native_runtime;

use alloc::sync::Arc;
use alloc::vec::Vec;

use device_model::BlockDevice;
use device_protocol::directory::{Class, DirectoryClient};
use filesystem_protocol::MountClient;
use native_runtime::native_abi::{self, Status, signals};
use native_runtime::{Channel, Startup};

native_runtime::entry!(main);

/// 枚举块设备直到启动枚举完成；块设备没有热插拔，之后不再观察目录。
fn open_devices(directory: &DirectoryClient) -> Result<Vec<Arc<dyn BlockDevice>>, Status> {
    let watch = directory.watch()?;
    let mut devices: Vec<Arc<dyn BlockDevice>> = Vec::new();
    let mut last = 0;
    loop {
        let _ = watch.signal(signals::USER_0, 0);
        let (enumerated, entries) = directory.list(last)?;
        let progressed = !entries.is_empty();
        for entry in entries {
            last = entry.id;
            if entry.class != Class::Block {
                continue;
            }
            match directory
                .open(&entry)
                .and_then(device_protocol::block::connect)
            {
                Ok(device) => {
                    devices
                        .try_reserve(1)
                        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                    devices.push(device);
                }
                Err(status) => error!("fshost: block device {} open failed: {status}", entry.id),
            }
        }
        if !progressed && enumerated {
            return Ok(devices);
        }
        if !progressed {
            watch.wait_one(signals::USER_0, native_abi::DEADLINE_INFINITE)?;
        }
    }
}

fn main(mut startup: Startup) -> i64 {
    use launch_protocol::fshost;
    let outgoing = Channel::from_handle(startup.take(fshost::OUTGOING));
    let directory =
        DirectoryClient::new(Channel::from_handle(startup.take(fshost::DEVICE_DIRECTORY)));
    let ext4 = Arc::new(MountClient::new(Channel::from_handle(
        startup.take(fshost::EXT4),
    )));
    let devices = Arc::new(open_devices(&directory).expect("fshost block devices"));
    service_directory::serve_directory(
        outgoing,
        b"fshost-outgoing",
        Arc::new(move |protocol: &[u8], channel: Channel| {
            if protocol == service_directory::BLOCKS {
                let devices = devices.clone();
                filesystem_protocol::serve_blocks(
                    channel,
                    devices.len(),
                    Arc::new(move |index: usize| {
                        let device = devices.get(index).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
                        device
                            .share_range(0, device.block_count())
                            .ok_or(native_abi::ERR_NOT_SUPPORTED)
                    }),
                )
            } else if protocol == service_directory::EXT4 {
                ext4.serve_proxy(channel)
            } else {
                Err(native_abi::ERR_NOT_FOUND)
            }
        }),
    )
    .expect("fshost outgoing service");
    info!("fshost ready");
    // 服务都在服务线程上；主线程只结束自己。
    native_runtime::task::exit_thread()
}
