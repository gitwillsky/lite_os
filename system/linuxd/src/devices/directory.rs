//! driver-host 设备目录的使用者端：连接目录中的每个设备，把远端 client 登记到对应的设备类注册表；
//! 网卡属于 netstack，其目录记录原样转交给 netstack。
//!
//! 1. 启动时读取目录直到“枚举完成”，此后的块设备、console、显示、输入与音频装配都看到完整设备集；
//! 2. 之后由目录线程继续接收热插拔设备（USB 连接与其 PCM 输出），登记后请求一轮音频发现；
//! 3. 全部设备的活动 Event 与事件 channel 由同一个 [`EventHub`] 线程分派。

use alloc::sync::Arc;

use device_model::InputDevice;
use device_protocol::boot::{BootProgress, Milestone};
use device_protocol::directory::{self, Announcement, Class};
use device_protocol::{EventHub, block, display, entropy, input, pcm, port, usb};
use native_runtime::Channel;
use native_runtime::native_abi::Status;

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
    announcement: Announcement,
    hub: &Arc<EventHub>,
    boot: &mut Option<BootProgress>,
) -> Result<(), Status> {
    match announcement.class {
        Class::Block => {
            let _ = super::block::register(block::connect(announcement)?);
        }
        Class::Network => {
            crate::handles::network()
                .attach_device(&announcement.description, announcement.handles)?;
        }
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
        Class::EnumerationComplete => {}
    }
    Ok(())
}

/// 读取目录直到枚举完成，并启动热插拔接收线程。
///
/// # Panics
///
/// 目录在枚举完成前关闭（driver-host 失败），或事件分派/目录线程创建失败：没有设备无法继续启动。
pub(crate) fn connect(channel: Channel) -> DeviceDirectory {
    let hub = EventHub::start().expect("device event hub");
    let mut boot = None;
    loop {
        let announcement =
            directory::receive(&channel).expect("device directory closed during enumeration");
        if announcement.class == Class::EnumerationComplete {
            break;
        }
        let class = announcement.class;
        if let Err(status) = attach(announcement, &hub, &mut boot) {
            error!("device directory: {class:?} attach failed: {status}");
        }
    }
    let thread = native_runtime::thread::spawn(b"device-directory", move || {
        let mut ignored = None;
        while let Ok(announcement) = directory::receive(&channel) {
            let class = announcement.class;
            if let Err(status) = attach(announcement, &hub, &mut ignored) {
                error!("device directory: hot-plugged {class:?} attach failed: {status}");
            }
        }
    })
    .expect("device directory thread");
    drop(thread);
    DeviceDirectory { boot }
}
