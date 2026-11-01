#![no_std]
#![feature(allocator_ext)]

//! 驱动宿主（devmgr）与设备使用者之间的设备线协议。
//!
//! 1. 驱动宿主为每个设备创建一条请求 channel 与一个活动 Event，经目录 channel 把它们连同设备的静态
//!    描述[`directory::Announcement`]交给使用者；带载荷的设备回调（PCM 周期完成等）另有一条事件 channel。
//! 2. 使用者端每个设备类有一个实现 `device_model` trait 的远端 client：同步请求用 `channel_call`，
//!    调用线程直接阻塞；活动 Event 与事件 channel 由一个 [`EventHub`] 线程统一等待并分派。
//! 3. 驱动宿主端每个设备类有一个 server 适配：若干工作线程读取请求、调用驱动 trait、写回同一事务号的回复。
//!
//! 线格式与请求/回复传输见 `service_rpc`。

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use native_runtime::native_abi::{self, DEADLINE_INFINITE, PortPacket, Status, signals};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Event, Port};

pub use service_rpc::{Reader, Writer, call, serve, take_handle};

pub mod block;
pub mod boot;
pub mod directory;
pub mod display;
pub mod entropy;
pub mod input;
pub mod network;
pub mod pcm;
pub mod port;
pub mod usb;

/// 驱动宿主端的活动通知：置位交给使用者的 Event（多次置位自然合并）。
///
/// # Errors
///
/// 闭包分配失败。
pub fn activity_signal(event: Arc<Event>) -> Result<device_model::Activity, Status> {
    let activity: device_model::Activity = Arc::try_new(move || {
        let _ = event.signal(0, signals::USER_0);
    })
    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    Ok(activity)
}

enum Source {
    /// 活动 Event：清除信号后调用绑定的通知。
    Activity(Event, Mutex<Option<device_model::Activity>>),
    /// 事件 channel：逐条交给处理函数。
    Events(Channel, Box<dyn Fn(native_runtime::Message) + Send + Sync>),
}

/// 使用者端的设备事件分派：一个线程在 port 上等待全部活动 Event 与事件 channel。
pub struct EventHub {
    port: Port,
    sources: Mutex<Vec<Arc<Source>>>,
}

impl EventHub {
    /// 创建分派线程。
    ///
    /// # Errors
    ///
    /// port 或线程创建失败。
    pub fn start() -> Result<Arc<Self>, Status> {
        let hub = Arc::try_new(Self {
            port: Port::create()?,
            sources: Mutex::new(Vec::new()),
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let worker = hub.clone();
        let thread = native_runtime::thread::spawn(b"device-events", move || worker.run())?;
        drop(thread);
        Ok(hub)
    }

    fn add(&self, source: Source) -> Result<Arc<Source>, Status> {
        let source = Arc::try_new(source).map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let key = {
            let mut sources = self.sources.lock();
            sources.try_reserve(1).map_err(|_| native_abi::ERR_NO_MEMORY)?;
            sources.push(source.clone());
            (sources.len() - 1) as u64
        };
        self.arm(&source, key)?;
        Ok(source)
    }

    fn arm(&self, source: &Source, key: u64) -> Result<(), Status> {
        match source {
            Source::Activity(event, _) => event.wait_async(&self.port, key, signals::USER_0),
            Source::Events(channel, _) => {
                channel.wait_async(&self.port, key, signals::READABLE | signals::PEER_CLOSED)
            }
        }
    }

    /// 登记活动 Event，返回之后可绑定通知的句柄；绑定前到达的活动在绑定时补发一次。
    ///
    /// # Errors
    ///
    /// 分配或等待登记失败。
    pub fn activity(self: &Arc<Self>, event: Event) -> Result<ActivityBinding, Status> {
        let source = self.add(Source::Activity(event, Mutex::new(None)))?;
        Ok(ActivityBinding { source })
    }

    /// 登记事件 channel 与处理函数。
    ///
    /// # Errors
    ///
    /// 分配或等待登记失败。
    pub fn events(self: &Arc<Self>, channel: Channel, handler: Box<dyn Fn(native_runtime::Message) + Send + Sync>) -> Result<(), Status> {
        self.add(Source::Events(channel, handler)).map(|_| ())
    }

    fn run(&self) {
        loop {
            let packet: PortPacket = match self.port.wait(DEADLINE_INFINITE) {
                Ok(packet) => packet,
                Err(_) => continue,
            };
            let Some(source) = self.sources.lock().get(packet.key as usize).cloned() else {
                continue;
            };
            match &*source {
                Source::Activity(event, activity) => {
                    // 先清除再通知：通知之后置位的活动重新触发下一轮，不会丢失。
                    let _ = event.signal(signals::USER_0, 0);
                    if self.arm(&source, packet.key).is_err() {
                        continue;
                    }
                    if let Some(activity) = activity.lock().clone() {
                        activity();
                    }
                }
                Source::Events(channel, handler) => {
                    let mut closed = false;
                    loop {
                        match channel.read() {
                            Ok(message) => handler(message),
                            Err(native_abi::ERR_SHOULD_WAIT) => break,
                            Err(_) => {
                                closed = true;
                                break;
                            }
                        }
                    }
                    if !closed {
                        let _ = self.arm(&source, packet.key);
                    }
                }
            }
        }
    }
}

/// 已登记的活动 Event；绑定通知后由分派线程调用。
#[derive(Clone)]
pub struct ActivityBinding {
    source: Arc<Source>,
}

impl ActivityBinding {
    /// 绑定（或替换）通知，并补发一次以覆盖绑定前到达的活动。
    pub fn bind(&self, activity: device_model::Activity) {
        let Source::Activity(_, slot) = &*self.source else {
            unreachable!("activity binding refers to an activity source");
        };
        *slot.lock() = Some(activity.clone());
        activity();
    }
}
