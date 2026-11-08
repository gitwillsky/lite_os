#![no_std]
#![feature(allocator_ext)]

//! 驱动宿主（driver-host）与设备使用者之间的设备线协议。
//!
//! 1. 驱动宿主在[`directory::Directory`]登记设备；使用者每次打开设备得到一个会话：一条请求 channel 与一个
//!    活动 Event（[`directory::OpenedDevice`]）；带载荷的设备回调（PCM 周期完成等）另有一条事件 channel。
//! 2. 使用者端每个设备类有一个实现 `device_model` trait 的远端 client：同步控制用 `channel_call`，
//!    有界 PCM 数据使用单向请求；活动 Event 与事件 channel 由一个 [`EventHub`] 线程统一等待并分派。
//! 3. 驱动宿主端每个设备类有一个 server 适配：若干工作线程读取请求、调用驱动 trait、写回同一事务号的回复。
//!
//! 线格式与请求/回复传输见 `service_rpc`。

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
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

/// Live 同时拥有回调和唯一关联的 peer 监听；Retired 禁止迟到的 bind/on_closed 重新登记。
/// peer 只弱引用活动 source，避免活动 owner 与关闭监听互相保活。
enum ActivityState {
    Live {
        callback: Option<device_model::Activity>,
        peer: Option<(u64, Arc<Source>)>,
    },
    Retired,
}

enum Source {
    /// 活动 Event：清除信号后调用绑定的通知。
    Activity(Event, Mutex<ActivityState>),
    /// 事件 channel：逐条交给处理函数。
    Events(
        Channel,
        Box<dyn Fn(Result<native_runtime::Message, Status>) + Send + Sync>,
    ),
    /// 会话对端关闭（驱动宿主终止）时调用一个活动登记的通知一次：活动 Event 不会再被置位，使用者只能经这条
    /// 通知发现断开并撤下设备。
    Closed(Channel, Weak<Source>),
}

/// 使用者端的事件分派：一个线程在 port 上等待全部活动 Event 与事件 channel。
///
/// 登记项以表中位置为 port key；[`EventHub::retire`] 摘除的位置由之后的登记复用。迟到的 packet 可能
/// 落到复用该位置的新登记项上，只会多触发一次通知（通知本身幂等）。
pub struct EventHub {
    port: Port,
    sources: Mutex<Vec<Option<Arc<Source>>>>,
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

    fn add(&self, source: Source) -> Result<(Arc<Source>, u64), Status> {
        let source = Arc::try_new(source).map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let key = {
            let mut sources = self.sources.lock();
            if let Some(index) = sources.iter().position(Option::is_none) {
                sources[index] = Some(source.clone());
                index as u64
            } else {
                sources
                    .try_reserve(1)
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                sources.push(Some(source.clone()));
                (sources.len() - 1) as u64
            }
        };
        if let Err(status) = self.arm(&source, key) {
            self.sources.lock()[key as usize] = None;
            return Err(status);
        }
        Ok((source, key))
    }

    fn arm(&self, source: &Source, key: u64) -> Result<(), Status> {
        match source {
            Source::Activity(event, _) => event.wait_async(&self.port, key, signals::USER_0),
            Source::Events(channel, _) => {
                channel.wait_async(&self.port, key, signals::READABLE | signals::PEER_CLOSED)
            }
            Source::Closed(peer, _) => peer.wait_async(&self.port, key, signals::PEER_CLOSED),
        }
    }

    /// 登记活动 Event，返回之后可绑定通知的句柄；绑定前到达的活动在绑定时补发一次。
    ///
    /// # Errors
    ///
    /// 分配或等待登记失败。
    pub fn activity(self: &Arc<Self>, event: Event) -> Result<ActivityBinding, Status> {
        let (source, key) = self.add(Source::Activity(
            event,
            Mutex::new(ActivityState::Live {
                callback: None,
                peer: None,
            }),
        ))?;
        let registration = match Arc::try_new(ActivityRegistration {
            hub: Arc::downgrade(self),
            source: Arc::downgrade(&source),
            key,
        }) {
            Ok(registration) => registration,
            Err(_) => {
                self.remove_source(key, &source);
                return Err(native_abi::ERR_NO_MEMORY);
            }
        };
        Ok(ActivityBinding { registration })
    }

    /// 会话 channel `peer`（使用者持有的副本）的对端关闭时调用 `binding` 的通知一次，随后撤销这条登记。
    ///
    /// # Errors
    ///
    /// 分配或等待登记失败；每个活动仅关联一个 peer，重复登记 ALREADY_BOUND，退休后 BAD_STATE。
    pub fn on_closed(
        self: &Arc<Self>,
        peer: Channel,
        binding: &ActivityBinding,
    ) -> Result<(), Status> {
        if !core::ptr::eq(binding.registration.hub.as_ptr(), self.as_ref()) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let source = binding
            .registration
            .source
            .upgrade()
            .ok_or(native_abi::ERR_BAD_STATE)?;
        let Source::Activity(_, slot) = &*source else {
            unreachable!("activity binding kind");
        };
        let mut state = slot.lock();
        let ActivityState::Live {
            peer: registered, ..
        } = &mut *state
        else {
            return Err(native_abi::ERR_BAD_STATE);
        };
        if registered.is_some() {
            return Err(native_abi::ERR_ALREADY_BOUND);
        }
        let (source, key) = self.add(Source::Closed(peer, Arc::downgrade(&source)))?;
        *registered = Some((key, source));
        Ok(())
    }

    /// 撤销本 hub 的活动与关联 peer；不等待已经领取的回调，迟到的登记/绑定无效。
    pub fn retire(&self, binding: &ActivityBinding) {
        if core::ptr::eq(binding.registration.hub.as_ptr(), self) {
            self.retire_activity(&binding.registration);
        }
    }

    fn retire_activity(&self, registration: &ActivityRegistration) {
        let Some(source) = registration.source.upgrade() else {
            return;
        };
        let Source::Activity(_, slot) = &*source else {
            unreachable!("activity binding kind");
        };
        let retired = core::mem::replace(&mut *slot.lock(), ActivityState::Retired);
        self.remove_source(registration.key, &source);
        if let ActivityState::Live {
            peer: Some((key, source)),
            ..
        } = &retired
        {
            self.remove_source(*key, source);
        }
        // 回调、Event 与 peer channel 的最终 Drop 均在两个状态锁外；binding 不强持 Event。
        drop(retired);
    }

    fn remove_source(&self, key: u64, source: &Arc<Source>) {
        let removed = {
            let mut sources = self.sources.lock();
            match sources.get_mut(key as usize) {
                Some(slot)
                    if slot
                        .as_ref()
                        .is_some_and(|installed| Arc::ptr_eq(installed, source)) =>
                {
                    slot.take()
                }
                _ => None,
            }
        };
        // 在表锁外释放：Event 关闭不在持锁时发生。
        drop(removed);
    }

    /// 登记事件 channel 与处理函数。
    ///
    /// # Parameters
    ///
    /// - `channel`: 由分派线程独占读取的事件端点。
    /// - `handler`: 接收消息；终止性读取/重登记错误交付一次 `Err`，随后撤销 source。
    ///
    /// # Returns
    ///
    /// 登记成功返回 `Ok(())`；之后的消息与断开由 handler 接收。
    ///
    /// # Errors
    ///
    /// 分配或等待登记失败。
    pub fn events(
        self: &Arc<Self>,
        channel: Channel,
        handler: Box<dyn Fn(Result<native_runtime::Message, Status>) + Send + Sync>,
    ) -> Result<(), Status> {
        self.add(Source::Events(channel, handler)).map(drop)
    }

    fn run(&self) {
        loop {
            let packet: PortPacket = match self.port.wait(DEADLINE_INFINITE) {
                Ok(packet) => packet,
                Err(_) => continue,
            };
            let Some(source) = self
                .sources
                .lock()
                .get(packet.key as usize)
                .cloned()
                .flatten()
            else {
                continue;
            };
            match &*source {
                Source::Activity(event, activity) => {
                    // 先清除再通知：通知之后置位的活动重新触发下一轮，不会丢失。
                    let _ = event.signal(signals::USER_0, 0);
                    if self.arm(&source, packet.key).is_err() {
                        continue;
                    }
                    let callback = match &*activity.lock() {
                        ActivityState::Live { callback, .. } => callback.clone(),
                        ActivityState::Retired => None,
                    };
                    if let Some(callback) = callback {
                        callback();
                    }
                }
                Source::Events(channel, handler) => {
                    let mut closed = false;
                    loop {
                        match channel.read() {
                            Ok(message) => handler(Ok(message)),
                            Err(native_abi::ERR_SHOULD_WAIT) => break,
                            Err(status) => {
                                handler(Err(status));
                                closed = true;
                                break;
                            }
                        }
                    }
                    if !closed && let Err(status) = self.arm(&source, packet.key) {
                        handler(Err(status));
                        closed = true;
                    }
                    if closed {
                        self.remove_source(packet.key, &source);
                    }
                }
                Source::Closed(peer, activity) => {
                    // key 的槽位可能已复用；PEER_CLOSED 是永久电平，旧 packet 不能关闭新会话。
                    // 未关闭说明真实等待仍然挂着，不重复武装，也不制造另一条关闭通知。
                    if peer.wait_one(signals::PEER_CLOSED, 0).is_err() {
                        continue;
                    }
                    self.remove_source(packet.key, &source);
                    if let Some(activity) = activity.upgrade()
                        && let Source::Activity(_, slot) = &*activity
                    {
                        let (callback, peer) = match &mut *slot.lock() {
                            ActivityState::Live { callback, peer }
                                if peer.as_ref().is_some_and(|(key, registered)| {
                                    *key == packet.key && Arc::ptr_eq(registered, &source)
                                }) =>
                            {
                                (callback.clone(), peer.take())
                            }
                            ActivityState::Live { .. } => (None, None),
                            ActivityState::Retired => (None, None),
                        };
                        drop(peer);
                        if let Some(callback) = callback {
                            callback();
                        }
                    }
                }
            }
        }
    }
}

/// 活动登记的共享寿命凭据；最后一个克隆或显式 retire 撤销活动与关联 peer 等待，迟到 bind 无效。
#[derive(Clone)]
pub struct ActivityBinding {
    registration: Arc<ActivityRegistration>,
}

/// 只有 binding 的克隆计入此 owner；hub 表不持有它，构造失败或最后一个 binding 退出会准确撤销登记。
struct ActivityRegistration {
    hub: Weak<EventHub>,
    source: Weak<Source>,
    key: u64,
}
impl Drop for ActivityRegistration {
    fn drop(&mut self) {
        if let Some(hub) = self.hub.upgrade() {
            hub.retire_activity(self);
        }
    }
}

impl ActivityBinding {
    /// 绑定（或替换）通知，并补发一次以覆盖绑定前到达的活动。
    pub fn bind(&self, activity: device_model::Activity) {
        let Some(source) = self.registration.source.upgrade() else {
            return;
        };
        let Source::Activity(_, slot) = &*source else {
            unreachable!("activity binding refers to an activity source");
        };
        let old = {
            let mut state = slot.lock();
            let ActivityState::Live { callback, .. } = &mut *state else {
                return;
            };
            callback.replace(activity.clone())
        };
        // 任意回调捕获的 owner 都可能在最后 Drop 时反入事件登记，不能在状态锁内释放它。
        drop(old);
        activity();
    }
}
