use alloc::{sync::Arc, sync::Weak, vec::Vec};
use spin::{Mutex, Once};

use crate::{
    drivers::{InputDevice, InputId, RawInputEvent},
    ipc::{Pipe, PipeDirection, PipeEnd},
};

#[path = "input/client_queue.rs"]
mod client_queue;
use client_queue::{ClientQueue, EventTimes, InputClock};

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const EV_MSC: u16 = 0x04;
const EV_SW: u16 = 0x05;
const EV_LED: u16 = 0x11;
const EV_SND: u16 = 0x12;
const EV_REP: u16 = 0x14;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const SYN_MAX: u16 = 0x0f;
const KEY_BITMAP_BYTES: usize = 96;
const ABS_COUNT: usize = 64;
const EVENT_BATCH: usize = 64;

/// @description 一个 Linux RV64 native `struct input_event` 的领域值。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct InputEvent {
    seconds: i64,
    microseconds: i64,
    event_type: u16,
    code: u16,
    value: i32,
}

impl InputEvent {
    /// @description 编码 RV64 24-byte native-endian `struct input_event`。
    /// @return 可直接 copyout 的 ABI bytes。
    pub(crate) fn encode(self) -> [u8; 24] {
        let mut bytes = [0u8; 24];
        bytes[..8].copy_from_slice(&self.seconds.to_ne_bytes());
        bytes[8..16].copy_from_slice(&self.microseconds.to_ne_bytes());
        bytes[16..18].copy_from_slice(&self.event_type.to_ne_bytes());
        bytes[18..20].copy_from_slice(&self.code.to_ne_bytes());
        bytes[20..24].copy_from_slice(&self.value.to_ne_bytes());
        bytes
    }
}

/// @description `EVIOCGABS` 返回的 live axis value 与 immutable limits。
#[derive(Debug, Clone, Copy)]
pub(crate) struct AbsoluteInfo {
    pub(crate) value: i32,
    pub(crate) minimum: i32,
    pub(crate) maximum: i32,
    pub(crate) fuzz: i32,
    pub(crate) flat: i32,
    pub(crate) resolution: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputError {
    NotFound,
    OutOfMemory,
    Busy,
    Invalid,
    Revoked,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum InputString {
    Name,
    PhysicalPath,
    Serial,
}

struct InputDeviceState {
    clients: Vec<Weak<InputFile>>,
    grabbed: Option<Weak<InputFile>>,
    keys: [u8; KEY_BITMAP_BYTES],
    absolute_values: [i32; ABS_COUNT],
}

struct InputClientState {
    queue: ClientQueue,
    revoked: bool,
}

struct EvdevDevice {
    adapter: Arc<dyn InputDevice>,
    notification_read: Arc<PipeEnd>,
    notification_write: Arc<PipeEnd>,
    // OWNER: 一个 lock 原子维护 live state、client registry 与 grab owner。拆分会让 ioctl
    // state snapshot 与事件 fanout 观察到不同 generation，或向 grab 之外的 client 泄漏事件。
    state: Mutex<InputDeviceState>,
}

/// @description 一个 open evdev OFD 的独立 packet queue 与 clock policy。
pub(crate) struct InputFile {
    device: Arc<EvdevDevice>,
    // OWNER: queue 与 revoked 必须由同一 lock 线性化。拆分会让 read 在 revoke 已返回后
    // 继续消费旧事件，或让 fanout 在强制撤销后重新发布 readiness。
    client: Mutex<InputClientState>,
}

impl InputFile {
    fn new(device: Arc<EvdevDevice>) -> Result<Arc<Self>, InputError> {
        let file = Arc::try_new(Self {
            device: device.clone(),
            client: Mutex::new(InputClientState {
                queue: ClientQueue::new(),
                revoked: false,
            }),
        })
        .map_err(|_| InputError::OutOfMemory)?;
        let mut state = device.state.lock();
        if let Some(slot) = state
            .clients
            .iter_mut()
            .find(|slot| slot.upgrade().is_none())
        {
            *slot = Arc::downgrade(&file);
        } else {
            state
                .clients
                .try_reserve(1)
                .map_err(|_| InputError::OutOfMemory)?;
            state.clients.push(Arc::downgrade(&file));
        }
        drop(state);
        Ok(file)
    }

    /// @description 返回完整 packet 中当前可读的 event 数。
    /// @return 零表示 read 必须阻塞或返回 EAGAIN。
    /// @errors 已撤销 OFD 返回 Revoked。
    pub(crate) fn readable_count(&self) -> Result<usize, InputError> {
        let client = self.client.lock();
        if client.revoked {
            return Err(InputError::Revoked);
        }
        Ok(client.queue.readable_count())
    }

    /// @description 原子消费不超过 output 长度的完整 packet events。
    /// @param output kernel stack staging event slice。
    /// @return 实际消费 event 数；不会越过最后一个 SYN_REPORT。
    /// @errors 已撤销 OFD 返回 Revoked。
    pub(crate) fn read(&self, output: &mut [InputEvent]) -> Result<usize, InputError> {
        let mut client = self.client.lock();
        if client.revoked {
            return Err(InputError::Revoked);
        }
        Ok(client.queue.read(output))
    }

    /// @description 排空旧 notification token 后复查当前 OFD 的 level readiness。
    /// @return 仍需阻塞时返回共享 device Pipe；已有完整 packet 返回 None。
    pub(crate) fn prepare_to_block(&self) -> Option<Arc<Pipe>> {
        if !matches!(self.readable_count(), Ok(0)) {
            return None;
        }
        self.device.notification_read.drain_readiness();
        self.readable_count()
            .is_ok_and(|readable| readable == 0)
            .then(|| self.device.notification_read.pipe())
    }

    /// @description 返回设备级 notification source 的最新 generation。
    /// @return 可供 epoll ET 比较的单调 generation。
    pub(crate) fn readiness_generation(&self) -> u64 {
        self.device
            .notification_read
            .pipe()
            .readiness_generation(PipeDirection::Read)
    }

    /// @description 取得 poll registration 使用的共享 notification Pipe。
    /// @return device read-side Pipe Arc。
    pub(crate) fn notification_pipe(&self) -> Arc<Pipe> {
        self.device.notification_read.pipe()
    }

    /// @description 投影 Linux evdev read/hangup readiness。
    /// @param events caller 关注的 poll mask。
    /// @return live client 的完整 packet readiness；撤销后无条件包含 HUP 与 ERR。
    pub(crate) fn poll_events(&self, events: i16) -> i16 {
        const INPUT: i16 = 0x001;
        const ERROR: i16 = 0x008;
        const HANGUP: i16 = 0x010;
        let client = self.client.lock();
        let readable = if client.queue.readable_count() != 0 {
            events & INPUT
        } else {
            0
        };
        readable | if client.revoked { ERROR | HANGUP } else { 0 }
    }

    /// @description 查询该 evdev OFD 是否已被不可逆撤销。
    /// @return `EVIOCREVOKE` 已完成时为 true。
    pub(crate) fn is_revoked(&self) -> bool {
        self.client.lock().revoked
    }

    /// @description 复制 Linux `input_id` 值。
    /// @return immutable adapter identity。
    pub(crate) fn id(&self) -> InputId {
        self.device.adapter.id()
    }

    /// @description 复制 NUL-terminated identity string，遵循 evdev variable ioctl 截断。
    /// @param kind name、physical path 或 serial selector。
    /// @param output kernel-owned ioctl staging buffer。
    /// @return copied bytes（非零 capacity 时包含 NUL）。
    pub(crate) fn copy_string(&self, kind: InputString, output: &mut [u8]) -> usize {
        let value = match kind {
            InputString::Name => self.device.adapter.name(),
            InputString::PhysicalPath => self.device.adapter.physical_path(),
            InputString::Serial => self.device.adapter.serial(),
        };
        let count = output.len().min(value.len().saturating_add(1));
        let content = count.min(value.len());
        output[..content].copy_from_slice(&value[..content]);
        if count > content {
            output[content] = 0;
        }
        count
    }

    /// @description 复制 property 或 event capability bitmap，并补齐 Linux RV64 word shape。
    /// @param event_type None 选择 properties；Some(0) 选择 EV type bitmap；其他选择 code bitmap。
    /// @param output kernel-owned ioctl staging buffer。
    /// @return Linux 对应 bitmap 的截断 byte count。
    /// @errors 未知 event type 返回 Invalid。
    pub(crate) fn copy_bitmap(
        &self,
        event_type: Option<u16>,
        output: &mut [u8],
    ) -> Result<usize, InputError> {
        let (source, full_length) = match event_type {
            None => (self.device.adapter.properties(), 8),
            Some(EV_SYN) => (self.device.adapter.event_types(), 8),
            Some(kind @ (EV_KEY | EV_REL | EV_ABS | EV_MSC | EV_SW | EV_LED | EV_SND | EV_REP)) => {
                let length = if kind == EV_KEY { KEY_BITMAP_BYTES } else { 8 };
                (self.device.adapter.event_codes(kind), length)
            }
            Some(_) => return Err(InputError::Invalid),
        };
        let count = output.len().min(full_length);
        output[..count].fill(0);
        let copied = count.min(source.len());
        output[..copied].copy_from_slice(&source[..copied]);
        Ok(count)
    }

    /// @description 复制当前 device-wide key state bitmap。
    /// @param output kernel-owned ioctl staging buffer。
    /// @return 截断到 Linux KEY bitmap 的 byte count。
    pub(crate) fn copy_key_state(&self, output: &mut [u8]) -> usize {
        let state = self.device.state.lock();
        let count = output.len().min(state.keys.len());
        output[..count].copy_from_slice(&state.keys[..count]);
        // 与 Linux evdev_handle_get_val 一致：state snapshot 后清除该 client 已排队的
        // EV_KEY，避免调用者先取得最新 bitmap、随后又重复应用旧 key transition。
        self.client.lock().queue.flush_type(EV_KEY);
        count
    }

    /// @description 在 state ioctl copyout 失败后标记 client event stream 已失步。
    /// @return 无返回值；下一 SYN_REPORT 前该 marker 不可读。
    pub(crate) fn mark_sync_lost(&self) {
        self.client.lock().queue.pass(
            RawInputEvent {
                event_type: EV_SYN,
                code: SYN_DROPPED,
                value: 0,
            },
            current_times(),
        );
    }

    /// @description 读取 absolute axis 的 live value 与 immutable limits。
    /// @param code Linux ABS code。
    /// @return 完整 `input_absinfo` 领域值。
    /// @errors 设备不支持该 axis 返回 Invalid。
    pub(crate) fn absolute_info(&self, code: u16) -> Result<AbsoluteInfo, InputError> {
        let limits = self
            .device
            .adapter
            .abs_info(code)
            .ok_or(InputError::Invalid)?;
        let value = *self
            .device
            .state
            .lock()
            .absolute_values
            .get(code as usize)
            .ok_or(InputError::Invalid)?;
        Ok(AbsoluteInfo {
            value,
            minimum: limits.minimum,
            maximum: limits.maximum,
            fuzz: limits.fuzz,
            flat: limits.flat,
            resolution: limits.resolution,
        })
    }

    /// @description 设置该 OFD 的 event timestamp clock。
    /// @param clock_id Linux CLOCK_REALTIME/MONOTONIC/BOOTTIME value。
    /// @return 支持的 clock 成功切换。
    /// @errors 其他 clock 返回 Invalid。
    pub(crate) fn set_clock(&self, clock_id: i32) -> Result<(), InputError> {
        let clock = match clock_id {
            0 => InputClock::Realtime,
            1 => InputClock::Monotonic,
            7 => InputClock::Boottime,
            _ => return Err(InputError::Invalid),
        };
        self.client.lock().queue.set_clock(clock, current_times());
        Ok(())
    }

    /// @description 建立或释放该 device 的 Linux EVIOCGRAB exclusive owner。
    /// @param file 当前 ioctl 所属 InputFile Arc。
    /// @param grab true 建立，false 释放。
    /// @return owner 转换成功。
    /// @errors 其他 live client 已 grab 返回 Busy；非 owner release 返回 Invalid。
    pub(crate) fn set_grab(file: &Arc<Self>, grab: bool) -> Result<(), InputError> {
        let mut state = file.device.state.lock();
        if file.client.lock().revoked {
            return Err(InputError::Revoked);
        }
        let current = state.grabbed.as_ref().and_then(Weak::upgrade);
        if grab {
            if current
                .as_ref()
                .is_some_and(|owner| !Arc::ptr_eq(owner, file))
            {
                return Err(InputError::Busy);
            }
            state.grabbed = Some(Arc::downgrade(file));
        } else if current
            .as_ref()
            .is_some_and(|owner| Arc::ptr_eq(owner, file))
        {
            state.grabbed = None;
        } else {
            return Err(InputError::Invalid);
        }
        Ok(())
    }

    /// @description 不可逆撤销当前 evdev OFD，并释放其 exclusive grab。
    /// @param file 当前 ioctl 所属 InputFile Arc。
    /// @return live client 成功转为 revoked。
    /// @errors 已撤销 client 返回 Revoked。
    pub(crate) fn revoke(file: &Arc<Self>) -> Result<(), InputError> {
        let mut state = file.device.state.lock();
        let mut client = file.client.lock();
        if client.revoked {
            return Err(InputError::Revoked);
        }
        client.revoked = true;
        if state
            .grabbed
            .as_ref()
            .and_then(Weak::upgrade)
            .as_ref()
            .is_some_and(|owner| Arc::ptr_eq(owner, file))
        {
            state.grabbed = None;
        }
        drop(client);
        drop(state);
        // 发布现有 device notification；缺失该 wakeup 会让 revoke 前已阻塞的 read/poll
        // 永久睡眠，无法观察 Linux 要求的 ENODEV/EPOLLHUP。
        file.device.notification_write.signal_readiness();
        Ok(())
    }
}

impl EvdevDevice {
    fn dispatch(&self, raw: RawInputEvent, times: EventTimes) {
        let recognized = if raw.event_type == EV_SYN {
            raw.code <= SYN_MAX
        } else {
            bit_is_set(self.adapter.event_types(), raw.event_type)
                && bit_is_set(self.adapter.event_codes(raw.event_type), raw.code)
        };
        if !recognized {
            return;
        }
        let mut state = self.state.lock();
        if raw.event_type == EV_KEY
            && let Some(byte) = state.keys.get_mut(raw.code as usize / 8)
        {
            let mask = 1 << (raw.code % 8);
            if raw.value == 0 {
                *byte &= !mask;
            } else {
                *byte |= mask;
            }
        } else if raw.event_type == EV_ABS
            && let Some(value) = state.absolute_values.get_mut(raw.code as usize)
        {
            *value = raw.value;
        }

        let mut notify = false;
        if let Some(grabbed) = state.grabbed.as_ref().and_then(Weak::upgrade) {
            let mut client = grabbed.client.lock();
            if !client.revoked {
                notify |= client.queue.pass(raw, times);
            }
        } else {
            state.grabbed = None;
            let mut index = 0;
            while index < state.clients.len() {
                if let Some(client) = state.clients[index].upgrade() {
                    let mut client = client.client.lock();
                    if !client.revoked {
                        notify |= client.queue.pass(raw, times);
                    }
                    index += 1;
                } else {
                    state.clients.swap_remove(index);
                }
            }
        }
        drop(state);
        if notify {
            self.notification_write.signal_readiness();
        }
    }
}

fn bit_is_set(bits: &[u8], bit: u16) -> bool {
    bits.get(bit as usize / 8)
        .is_some_and(|byte| byte & (1 << (bit % 8)) != 0)
}

fn current_times() -> EventTimes {
    EventTimes {
        realtime_ns: crate::timer::get_realtime_ns(),
        monotonic_ns: crate::timer::get_time_ns(),
    }
}

// OWNER: input core 永久拥有按 raw adapter index 排列的 evdev devices；devfs 只投影 index，
// OFD 只持 Arc。缺失该 immutable owner 会让 event minor、client registry 与 hardware 分裂。
static INPUT_DEVICES: Once<Vec<Arc<EvdevDevice>>> = Once::new();

/// @description 将全部 DTB input adapters 与 task-aware notification Pipe 装配为 evdev devices。
/// @param create_notification 为每个 device 创建一对 read/write notification endpoints。
/// @return 全部 adapter 原子发布成功返回 unit。
/// @errors Pipe、device control block 或 registry allocation 失败返回 unit。
pub(crate) fn init(
    mut create_notification: impl FnMut() -> Result<(Arc<PipeEnd>, Arc<PipeEnd>), ()>,
) -> Result<(), ()> {
    if INPUT_DEVICES.get().is_some() {
        return Err(());
    }
    let count = crate::drivers::input_device_count();
    let mut devices = Vec::new();
    devices.try_reserve_exact(count).map_err(|_| ())?;
    for index in 0..count {
        let adapter = crate::drivers::input_device(index).ok_or(())?;
        let notification = create_notification()?;
        devices.push(
            Arc::try_new(EvdevDevice {
                adapter,
                notification_read: notification.0,
                notification_write: notification.1,
                state: Mutex::new(InputDeviceState {
                    clients: Vec::new(),
                    grabbed: None,
                    keys: [0; KEY_BITMAP_BYTES],
                    absolute_values: [0; ABS_COUNT],
                }),
            })
            .map_err(|_| ())?,
        );
    }
    INPUT_DEVICES.call_once(|| devices);
    Ok(())
}

/// @description 返回已发布 evdev device 数量。
/// @return 初始化前为零，之后与 raw adapter count 恒等。
pub(crate) fn device_count() -> usize {
    INPUT_DEVICES.get().map_or(0, Vec::len)
}

/// @description 为 `/dev/input/eventN` 创建独立 client queue。
/// @param index devfs event minor index。
/// @return 新 InputFile Arc。
/// @errors index 不存在或 allocation 失败返回精确错误。
pub(crate) fn open(index: usize) -> Result<Arc<InputFile>, InputError> {
    let device = INPUT_DEVICES
        .get()
        .and_then(|devices| devices.get(index))
        .cloned()
        .ok_or(InputError::NotFound)?;
    InputFile::new(device)
}

/// @description 在 deferred context 有界消费所有 input eventq 并 fanout 到 evdev clients。
/// @return 任一 adapter budget 用尽且仍有 completion 时返回 true。
/// @errors queue/transport 损坏直接 fail-stop，禁止在 owner 不确定后继续 DMA。
pub(crate) fn dispatch_input_work() -> bool {
    let Some(devices) = INPUT_DEVICES.get() else {
        return false;
    };
    let mut backlog = false;
    for device in devices {
        for _ in 0..EVENT_BATCH {
            let Some(event) = device
                .adapter
                .receive_event()
                .unwrap_or_else(|_| panic!("VirtIO input eventq corrupted"))
            else {
                break;
            };
            device.dispatch(event, current_times());
        }
        device
            .adapter
            .finish_receive_batch()
            .unwrap_or_else(|_| panic!("VirtIO input repost notification failed"));
        backlog |= device.adapter.has_pending_event();
    }
    backlog
}
