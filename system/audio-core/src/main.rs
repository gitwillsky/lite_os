#![no_std]
#![no_main]
#![feature(allocator_ext)]

//! audio-core：音频服务（Fuchsia audio_core）。
//!
//! 1. 从路由来的 PCM 设备目录接入每个输出（含热插拔的 USB 声卡），独占其设备会话；
//! 2. 每个输出一个混音引擎：共享渲染流按共享格式混音，独占渲染流以协商格式直通设备；
//! 3. 经 outgoing 提供音频协议（`audio_protocol`）：使用者（linuxd 的 ALSA 翻译）列出输出、建立渲染流、
//!    读写主音量。样本在使用者提供的环形缓冲里，位置经共享控制区交换，热路径不走 RPC。

extern crate alloc;
#[macro_use]
extern crate native_runtime;

mod engine;
mod mix;
mod output;

use alloc::sync::Arc;
use alloc::vec::Vec;

use audio_protocol::{AudioService, Controls, CreatedStream, Endpoint, OutputInfo};
use device_protocol::EventHub;
use device_protocol::directory::{Class, DirectoryClient};
use native_runtime::native_abi::{self, Status, rights, signals};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Event, Startup, Vmo};

use output::Output;

#[allow(
    unused_imports,
    reason = "日志宏经 `crate::` 路径调用的地方需要根级名字"
)]
pub(crate) use native_runtime::{debug, error, info, warn};

native_runtime::entry!(main);

/// 已接入的输出与观察者。
struct Outputs {
    // OWNER: 按接入顺序的输出；ID 单调递增，热插拔只追加。
    outputs: Mutex<Vec<Arc<Output>>>,
    // OWNER: 每个 `watch` 调用者的变化 Event；输出集合变化时全部置位。数量受使用者连接数约束。
    watchers: Mutex<Vec<Event>>,
}

impl Outputs {
    fn output(&self, id: u64) -> Result<Arc<Output>, Status> {
        self.outputs
            .lock()
            .iter()
            .find(|output| output.info.id == id)
            .cloned()
            .ok_or(native_abi::ERR_NOT_FOUND)
    }

    fn add(&self, output: Arc<Output>) {
        self.outputs.lock().push(output);
        for watcher in self.watchers.lock().iter() {
            let _ = watcher.signal(0, signals::USER_0);
        }
    }
}

impl AudioService for Outputs {
    fn outputs(&self, after: u64) -> Result<Vec<OutputInfo>, Status> {
        Ok(self
            .outputs
            .lock()
            .iter()
            .filter(|output| output.info.id > after)
            .map(|output| output.info.clone())
            .collect())
    }

    fn watch(&self) -> Result<Event, Status> {
        let event = Event::create()?;
        let peer = event.duplicate(rights::SAME)?;
        self.watchers.lock().push(event);
        Ok(peer)
    }

    fn create_stream(
        &self,
        output: u64,
        endpoint: Endpoint,
        configuration: audio_protocol::PcmConfiguration,
        buffer_frames: u64,
        ring: Vmo,
    ) -> Result<CreatedStream, Status> {
        self.output(output)?
            .create_stream(endpoint, configuration, buffer_frames, ring)
    }

    fn controls(&self, output: u64) -> Result<Controls, Status> {
        self.output(output)?.controls()
    }

    fn set_controls(&self, output: u64, controls: Controls) -> Result<bool, Status> {
        Ok(self.output(output)?.set_controls(controls))
    }
}

/// 接入目录中 ID 大于 `last` 的 PCM 设备；返回启动枚举是否已完成。
fn discover(
    directory: &DirectoryClient,
    hub: &Arc<EventHub>,
    outputs: &Outputs,
    last: &mut u64,
    scheduler: &Arc<native_runtime::Resource>,
) -> Result<bool, Status> {
    loop {
        let (enumerated, entries) = directory.list(*last)?;
        if entries.is_empty() {
            return Ok(enumerated);
        }
        for entry in entries {
            *last = entry.id;
            if entry.class != Class::Pcm {
                continue;
            }
            let attached = directory
                .open(&entry, &[])
                .and_then(|opened| device_protocol::pcm::connect(opened, hub))
                .and_then(|device| Output::attach(entry.id, device, scheduler.clone()));
            match attached {
                Ok(output) => outputs.add(output),
                Err(status) => error!(
                    "audio-core: PCM device {} attach failed: {status}",
                    entry.id
                ),
            }
        }
    }
}

fn main(mut startup: Startup) -> i64 {
    use launch_protocol::audio_core;
    let outgoing = Channel::from_handle(startup.take(audio_core::OUTGOING));
    let scheduler = Arc::new(native_runtime::Resource::from_handle(
        startup.take(audio_core::SCHEDULER),
    ));
    let directory = DirectoryClient::new(Channel::from_handle(
        startup.take(audio_core::DEVICE_DIRECTORY),
    ));
    let hub = EventHub::start().expect("audio event hub");
    let outputs = Arc::new(Outputs {
        outputs: Mutex::new(Vec::new()),
        watchers: Mutex::new(Vec::new()),
    });
    let watch = directory.watch().expect("PCM device watch");
    let mut last = 0;
    // 启动枚举完成后才提供服务：使用者第一次列出输出就看到启动时的全部声卡。
    loop {
        let _ = watch.signal(signals::USER_0, 0);
        if discover(&directory, &hub, &outputs, &mut last, &scheduler)
            .expect("PCM device directory")
        {
            break;
        }
        let _ = watch.wait_one(signals::USER_0, native_abi::DEADLINE_INFINITE);
    }
    let service: Arc<dyn AudioService> = outputs.clone();
    service_directory::serve_directory(
        outgoing,
        b"audio-core-outgoing",
        Arc::new(move |protocol: &[u8], channel: Channel| {
            if protocol == service_directory::AUDIO {
                audio_protocol::serve_audio(channel, service.clone())
            } else {
                Err(native_abi::ERR_NOT_FOUND)
            }
        }),
    )
    .expect("audio-core outgoing service");
    info!("audio-core ready");
    // 主线程继续接入热插拔的 USB 声卡。
    loop {
        let _ = watch.wait_one(signals::USER_0, native_abi::DEADLINE_INFINITE);
        let _ = watch.signal(signals::USER_0, 0);
        if discover(&directory, &hub, &outputs, &mut last, &scheduler).is_err() {
            native_runtime::task::exit_thread();
        }
    }
}
