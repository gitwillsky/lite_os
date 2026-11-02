//! 设备目录的使用者端：打开路由给 linuxd 的每个设备，把远端 client 登记到对应的设备类注册表。
//!
//! 1. 启动时枚举目录直到“启动枚举完成”，此后的块设备、console、显示、输入与音频装配都看到完整设备集；
//! 2. 之后由目录线程等待目录变化 Event，继续接入热插拔设备（USB 连接与其 PCM 输出），登记后请求一轮音频发现；
//! 3. 全部设备的活动 Event 与事件 channel 由同一个 [`EventHub`] 线程分派。

use alloc::sync::Arc;

use device_model::InputDevice;
use device_protocol::boot::{BootProgress, Milestone};
use device_protocol::directory::{Class, DirectoryClient, Entry, OpenedDevice};
use device_protocol::{EventHub, display, entropy, input, pcm, port, usb};
use native_runtime::native_abi::{self, Status, signals};
use native_runtime::{Channel, Event};

/// 启动阶段连接的设备集合。
pub(crate) struct DeviceDirectory {
    boot: Option<BootProgress>,
}

impl DeviceDirectory {
    /// 推进开机画面进度。
    pub(crate) fn advance(&self, milestone: Milestone) {
        if let Some(boot) = &self.boot {
            boot.advance(milestone);
        }
    }
}

fn attach(
    announcement: OpenedDevice,
    hub: &Arc<EventHub>,
    boot: &mut Option<BootProgress>,
) -> Result<(), Status> {
    match announcement.class {
        // 块设备与网卡由根服务分别路由给 fshost 与 netstack，不在 linuxd 的目录范围内。
        Class::Block | Class::Network => return Err(native_abi::ERR_NOT_SUPPORTED),
        Class::Display => {
            let _ = device_model::register_display_device(display::connect(announcement, hub)?);
        }
        Class::Input => {
            let device = input::connect(announcement, hub)?;
            let name = device.name().to_vec();
            // 注册顺序即 evdev minor：`/dev/input/event<index>` 绑定到这个设备。
            if let Ok(index) = device_model::register_input_device(device) {
                info!(
                    "input event{index}: {}",
                    core::str::from_utf8(&name).unwrap_or("<non-utf8>")
                );
            }
        }
        Class::Pcm => {
            let _ = device_model::register_pcm_output(pcm::connect(announcement, hub)?);
            super::audio::request_processing();
        }
        Class::Port => {
            let _ = device_model::register_port_device(port::connect(announcement, hub)?);
        }
        Class::Entropy => {
            let _ = device_model::register_entropy_source(entropy::connect(announcement)?);
        }
        Class::Usb => {
            let _ = device_model::register_usb_device(usb::connect(announcement)?);
        }
        Class::Boot => *boot = Some(device_protocol::boot::connect(announcement)?),
    }
    Ok(())
}

/// 接入 `last` 之后的新设备；返回（启动枚举是否已完成，是否接入了新设备）。
fn advance(
    directory: &DirectoryClient,
    last: &mut u64,
    hub: &Arc<EventHub>,
    boot: &mut Option<BootProgress>,
) -> Result<(bool, bool), Status> {
    let (enumerated, entries) = directory.list(*last)?;
    let progressed = !entries.is_empty();
    for entry in entries {
        *last = entry.id;
        if let Err(status) = open(directory, &entry, hub, boot) {
            error!(
                "device directory: {:?} attach failed: {status}",
                entry.class
            );
        }
    }
    Ok((enumerated, progressed))
}

fn open(
    directory: &DirectoryClient,
    entry: &Entry,
    hub: &Arc<EventHub>,
    boot: &mut Option<BootProgress>,
) -> Result<(), Status> {
    attach(directory.open(entry)?, hub, boot)
}

/// 先清除变化 Event 再读取，读取之后到达的变化不会丢失；没有新设备时等待下一次变化。
fn wait_change(watch: &Event) -> Result<(), Status> {
    watch.wait_one(signals::USER_0, native_abi::DEADLINE_INFINITE)?;
    watch.signal(signals::USER_0, 0)
}

/// 枚举目录直到启动枚举完成，并启动热插拔接收线程。
///
/// # Panics
///
/// 目录在枚举完成前关闭（driver-host 失败），或事件分派/目录线程创建失败：没有设备无法继续启动。
pub(crate) fn connect(channel: Channel) -> DeviceDirectory {
    let hub = EventHub::start().expect("device event hub");
    let directory = DirectoryClient::new(channel);
    let watch = directory.watch().expect("device directory watch");
    let mut boot = None;
    let mut last = 0;
    loop {
        let (enumerated, progressed) = advance(&directory, &mut last, &hub, &mut boot)
            .expect("device directory closed during enumeration");
        if progressed {
            continue;
        }
        if enumerated {
            break;
        }
        wait_change(&watch).expect("device directory closed during enumeration");
    }
    let thread = native_runtime::thread::spawn(b"device-directory", move || {
        let mut ignored = None;
        while wait_change(&watch).is_ok() {
            // 一次变化可能对应多台设备；读到没有新设备为止。
            while let Ok((_, true)) = advance(&directory, &mut last, &hub, &mut ignored) {}
        }
    })
    .expect("device directory thread");
    drop(thread);
    DeviceDirectory { boot }
}
