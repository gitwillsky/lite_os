#![no_std]
#![no_main]
#![feature(allocator_ext)]

//! fshost：存储宿主（对应 Fuchsia fshost）。
//!
//! 1. 从根服务路由来的块设备目录打开启动时发现的全部块设备，持有各设备唯一的驱动会话；
//! 2. 为每块盘启动一个 ext4 实例（[`volumes`]）；使用者不直接接触块设备驱动会话或 ext4；
//! 3. 卷标为 `liteos-data` 的 ext4 是原生数据卷：不交给 Linux，由 fshost 读写挂载，并为各原生服务派生私有目录
//!    （存储能力，见 [`storage`]）；
//! 4. 经 outgoing 提供协议：块设备清单（按序号交出全盘范围连接，Linux 块设备 UAPI 与挂载在其上实现）、
//!    按盘序号的 ext4 挂载与存储管理（只路由给根服务）。使用者（linuxd）重启后重新连接即可，
//!    块设备会话、ext4 实例与数据卷不受影响。

extern crate alloc;
#[macro_use]
extern crate native_runtime;

mod storage;
mod volumes;

use alloc::sync::Arc;
use alloc::vec::Vec;

use device_model::BlockDevice;
use device_protocol::directory::{Class, DirectoryClient};
use native_runtime::native_abi::{self, Status, signals};
use native_runtime::{Channel, Startup};

native_runtime::entry!(main);

/// 启动时发现的块设备：交给 Linux 的块设备与原生数据卷（若存在）。
struct Devices {
    linux: Vec<Arc<dyn BlockDevice>>,
    data: Option<Arc<dyn BlockDevice>>,
}

/// 枚举块设备直到启动枚举完成；块设备没有热插拔，之后不再观察目录。
fn open_devices(directory: &DirectoryClient) -> Result<Devices, Status> {
    let mut data: Option<Arc<dyn BlockDevice>> = None;
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
                Ok(device) if data.is_none() && storage::is_data_volume(&*device) => {
                    data = Some(device);
                }
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
            return Ok(Devices {
                linux: devices,
                data,
            });
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
    let source = native_runtime::Vmo::from_handle(startup.take(fshost::EXT4_IMAGE));
    let size = source.size().expect("ext4 image size") as usize;
    let address = native_runtime::heap::vmar()
        .map(native_abi::vm::READ, 0, &source, 0, size)
        .expect("ext4 image mapping");
    // SAFETY: 完整只读映射在 parse 期间有效；映像不保留输入借用。
    let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, size) };
    let image = native_runtime::elf::Image::parse(bytes).expect("ext4 ELF");
    // SAFETY: parse 已复制段内容且不保留 bytes；此映射没有其他使用者。
    unsafe {
        native_runtime::heap::vmar()
            .unmap(address, size.next_multiple_of(native_abi::PAGE_SIZE))
            .expect("ext4 image unmapping");
    }
    drop(source);
    let launcher = Arc::new(volumes::Launcher::new(
        native_runtime::Job::from_handle(startup.take(fshost::FILESYSTEM_JOB)),
        image,
    ));
    let Devices {
        linux: devices,
        data,
    } = open_devices(&directory).expect("fshost block devices");
    let devices = Arc::new(devices);
    let volumes = Arc::new(volumes::Volumes::new(launcher.clone(), devices.clone()));
    // 数据卷缺失时存储管理返回 NOT_FOUND，需要私有目录的服务因此无法启动：镜像总是带数据卷。
    let volume = data.and_then(|device| {
        storage::DataVolume::mount(&launcher, &*device)
            .inspect_err(|status| error!("fshost: data volume mount failed: {status}"))
            .ok()
    });
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
                filesystem_protocol::serve_volume_mounts(channel, volumes.clone())
            } else if protocol == service_directory::STORAGE_ADMIN {
                let volume = volume.clone().ok_or(native_abi::ERR_NOT_FOUND)?;
                filesystem_protocol::serve_storage_admin(
                    channel,
                    Arc::new(move |name: &[u8]| {
                        let directory = volume.directory(name)?;
                        let (client, server) = Channel::create()?;
                        filesystem_protocol::serve_storage(
                            server,
                            Arc::new(storage::ServiceStorage {
                                volume: volume.clone(),
                                directory,
                            }),
                        )?;
                        Ok(client)
                    }),
                )
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
