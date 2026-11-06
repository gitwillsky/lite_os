//! Linux ALSA playback 子流与 OFD lifecycle owner；混音与设备属于原生 audio-core（Fuchsia audio_core）。
//!
//! 每个 audio-core 输出对应一张卡，提供共享混音与原生格式独占两个互斥端点。共享端点按 control 偏好或
//! 首个空闲槽分配 [`SUBSTREAMS`] 个子流；每个已配置的子流对应 audio-core 的一个渲染流：样本在 OFD 的
//! 环形缓冲里，应用指针经共享控制区发布，硬件指针、欠载与断开从控制区同步回 ALSA 状态。

use crate::sync::Mutex;
use crate::sync::Once;
use alloc::{
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use audio_protocol::{
    AudioClient, Endpoint, OutputInfo, STATUS_DISCONNECTED, STATUS_UNDERRUN, StreamClient,
};
use device_protocol::{ActivityBinding, EventHub};

use crate::{
    ipc::{Pipe, PipeDirection, PipeEnd},
    memory::{DeviceMappingSource, PAGE_SIZE},
    namespace::PidIdentity,
    sync::TaskMutex,
};
use device_model::DeviceBacking;

mod codec;
mod control;
mod control_file;
mod endpoint;
mod ioctl;
mod pcm_file;
mod state;
use device_model::{MAX_PCM_PERIOD_BYTES, PcmCapabilities, PcmConfiguration};
use state::MAX_BUFFER_PERIODS;
pub(crate) use state::PcmState;
use state::PcmStateOwner;
include!("readiness.rs");
use audio_protocol::VOLUME_MAX;
pub(super) use endpoint::{PcmEndpoint, SUBSTREAMS};

pub(crate) use device_model::PcmOutputError as AudioError;

/// ALSA PCM hardware 参数的唯一受支持领域值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HardwareParameters {
    pub(crate) access: i32,
    pub(crate) format: i32,
    pub(crate) channels: u32,
    pub(crate) rate: u32,
    pub(crate) period_frames: u64,
    pub(crate) periods: u32,
    pub(crate) buffer_frames: u64,
}

/// ALSA PCM software 参数的受支持子集。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SoftwareParameters {
    pub(crate) timestamp_mode: u32,
    pub(crate) timestamp_type: Option<u32>,
    pub(crate) available_min: u64,
    pub(crate) start_threshold: u64,
    pub(crate) stop_threshold: u64,
    pub(crate) boundary: u64,
}

/// ioctl/status 与 mmap-control 共用的 live position 快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PcmStatus {
    pub(crate) state: PcmState,
    pub(crate) application_frames: u64,
    pub(crate) hardware_frames: u64,
    pub(crate) delay_frames: i64,
    pub(crate) buffer_frames: u64,
    pub(crate) timestamp_ns: u64,
}

/// audio-core 的一个输出：卡的名称与格式能力。
pub(super) struct RemoteOutput(OutputInfo);

impl RemoteOutput {
    pub(super) fn name(&self) -> &str {
        core::str::from_utf8(&self.0.name).unwrap_or("Audio")
    }

    pub(super) fn capabilities(&self) -> PcmCapabilities {
        self.0.capabilities
    }
}

/// 一个子流：ALSA 状态、OFD 环形缓冲与已配置时的渲染流。
struct Slot {
    state: PcmStateOwner,
    ring: Option<Arc<DeviceBacking>>,
    stream: Option<StreamClient>,
    binding: Option<ActivityBinding>,
    /// 已同步到 `state.hardware_frames` 的播放帧数（渲染流自停止起的计数）。
    played: u64,
}

impl Slot {
    const fn vacant() -> Self {
        Self {
            state: PcmStateOwner::new(),
            ring: None,
            stream: None,
            binding: None,
            played: 0,
        }
    }

    /// 从渲染流的控制区同步硬件指针、欠载与断开；返回状态或指针是否变化。
    fn sync(&mut self) -> bool {
        let Some(stream) = &self.stream else {
            return false;
        };
        let control = stream.control();
        let status = control.status();
        // 服务端关闭流（audio-core 终止）后控制区不再更新：按断开处理。
        if status & STATUS_DISCONNECTED != 0 || stream.closed() {
            let changed = self.state.state != PcmState::Disconnected;
            self.state.disconnect();
            return changed;
        }
        let played = control.played();
        let delta = played.wrapping_sub(self.played);
        let mut changed = false;
        if delta != 0 && delta <= self.state.buffer_frames {
            self.played = played;
            self.state.complete_period(delta);
            changed = true;
        }
        if status & STATUS_UNDERRUN != 0 && self.state.state == PcmState::Running {
            self.state.xrun();
            changed = true;
        }
        changed
    }
}

/// 一张卡的子流表与子流音量；ALSA 子流编号是 Linux UAPI 概念，属于 linuxd。
struct Slots {
    endpoint: PcmEndpoint,
    slots: [Slot; SUBSTREAMS],
    volume: [[u32; 2]; SUBSTREAMS],
}

impl Slots {
    fn occupied(&self) -> [bool; SUBSTREAMS] {
        core::array::from_fn(|index| self.slots[index].ring.is_some())
    }

    /// 按 Linux control 偏好占用子流（规则见 [`endpoint::choose_substream`]）。
    fn allocate(
        &mut self,
        ring: Arc<DeviceBacking>,
        preferred: i32,
        endpoint: PcmEndpoint,
    ) -> Result<usize, Arc<DeviceBacking>> {
        let Some(index) =
            endpoint::choose_substream(self.occupied(), self.endpoint, endpoint, preferred)
        else {
            return Err(ring);
        };
        self.endpoint = endpoint;
        self.slots[index] = Slot::vacant();
        self.slots[index].ring = Some(ring);
        Ok(index)
    }

    fn endpoint_available(&self, endpoint: PcmEndpoint) -> usize {
        endpoint::endpoint_available(self.occupied(), self.endpoint, endpoint)
    }
}

/// 一块声卡：audio-core 输出、子流表与等待者通知。
struct Card {
    // OWNER: 子流配置事务（建立/撤销渲染流）的 task 锁；等待 audio-core 时不持子流自旋锁。
    configuration: TaskMutex<()>,
    index: u32,
    id: String,
    shared: bool,
    output: RemoteOutput,
    audio: Arc<AudioClient>,
    hub: Arc<EventHub>,
    // OWNER: composition root 安装的调用线程 identity 投影；只在 task context、且未持音频锁时调用。
    caller: fn() -> Option<Arc<PidIdentity>>,
    // OWNER: control open 顺序的唯一 registry；弱引用不延长 OFD，actual fput 由 ControlOpen.closed 撤销。
    // 锁序 registry → OFD state；不跨 registry 持子流锁或发布 readiness。仅 open/control 操作访问。
    controls: Mutex<Vec<Weak<control_file::ControlFile>>>,
    // OWNER: ALSA 子流状态与指针的唯一入口；同步只读渲染流的控制区，不调用 audio-core。
    slots: Mutex<Slots>,
    notification_read: Arc<PipeEnd>,
    notification_write: Arc<PipeEnd>,
    // OWNER: control 事件的独立唤醒源，避免 PCM 位置推进周期性唤醒空闲 mixer consumer。
    control_notification_read: Arc<PipeEnd>,
    control_notification_write: Arc<PipeEnd>,
}

// OWNER: discovery 是 audio-core 输出 → ALSA card 的唯一绑定者；新输出（含热插拔）由音频 deferred
// 工作绑定，事件分派线程不分配、不发布 devfs 节点。
struct Discovery {
    hub: Arc<EventHub>,
    // OWNER: 当前 audio-core 实例的连接与发现游标；实例重启后由音频连接线程整体替换。
    session: Mutex<Session>,
    // OWNER: 每卡的只追加索引；get 克隆 Arc 后立即释放 registry 锁，Card.process 不在 registry 锁下运行。
    cards: device_model::AppendOnlyRegistry<Card>,
    caller: fn() -> Option<Arc<PidIdentity>>,
}

/// 与一个 audio-core 实例的连接。
struct Session {
    audio: Arc<AudioClient>,
    /// 已绑定的最大输出 ID（同一实例内单调）。
    last: u64,
    // 输出集合变化的观察登记；随会话存续，旧实例的登记随替换撤销。
    watch: ActivityBinding,
}

impl Session {
    /// 连接当前 audio-core 实例；路由在实例重启期间让连接等待新实例就绪。
    fn connect(hub: &Arc<EventHub>) -> Result<Self, ()> {
        let audio = Arc::try_new(AudioClient::new(
            crate::handles::services()
                .connect(service_directory::AUDIO)
                .map_err(|_| ())?,
        ))
        .map_err(|_| ())?;
        let watch = hub
            .activity(audio.watch().map_err(|_| ())?)
            .map_err(|_| ())?;
        watch.bind(processing_activity()?);
        Ok(Self {
            audio,
            last: 0,
            watch,
        })
    }
}
// OWNER: 唯一音频发现游标及 caller 绑定，init 发布后由音频 deferred 工作处理新增输出；缺失时
// 热插拔声卡不能发布 ALSA 节点。
static DISCOVERY: Once<Arc<Discovery>> = Once::new();
// OWNER: 音频 deferred 工作：渲染流位置推进、断开与新输出登记时发布，处理全部卡。init 前的发布被忽略，
// init 自身会处理一轮。
static AUDIO_WORK: Once<crate::deferred::DeferredWork> = Once::new();

/// 请求一轮音频处理（发现新输出并同步各卡）。
pub(crate) fn request_processing() {
    if let Some(work) = AUDIO_WORK.get() {
        crate::deferred::raise(*work);
    }
}

fn audio_work(_now_ns: u64) -> bool {
    if let Some(discovery) = DISCOVERY.get() {
        discovery.process();
    }
    false
}

/// 唤醒音频 deferred 工作的活动通知。
fn processing_activity() -> Result<device_model::Activity, ()> {
    Arc::try_new(request_processing)
        .map_err(|_| ())
        .map(|activity| activity as device_model::Activity)
}

/// 连接 audio-core 并发布已存在的输出；之后的新输出经音频 deferred 工作发现。
///
/// # Parameters
/// - `caller`: task owner 的实际 TID observer。
///
/// # Errors
/// 重复安装、连接、分配或通知登记失败时返回错误。
pub(crate) fn init(caller: fn() -> Option<Arc<PidIdentity>>) -> Result<(), ()> {
    if DISCOVERY.get().is_some() {
        return Err(());
    }
    let hub = EventHub::start().map_err(|_| ())?;
    let session = Session::connect(&hub)?;
    let discovery = Arc::try_new(Discovery {
        hub,
        session: Mutex::new(session),
        cards: device_model::AppendOnlyRegistry::new(),
        caller,
    })
    .map_err(|_| ())?;
    DISCOVERY.call_once(|| discovery.clone());
    let work = crate::deferred::register(audio_work)?;
    AUDIO_WORK.call_once(|| work);
    let keeper = discovery.clone();
    let thread =
        native_runtime::thread::spawn(b"audio-connection", move || keeper.keep_connected())
            .map_err(|_| ())?;
    drop(thread);
    discovery.process();
    Ok(())
}

impl Discovery {
    /// 音频连接线程：audio-core 终止后重连新实例，新实例的输出作为新卡登记（与声卡重新插入一致）；
    /// 旧卡的渲染流随旧实例关闭而断开。重连期间路由让连接等待，不占用音频 deferred 工作。
    fn keep_connected(&self) {
        loop {
            let audio = self.session.lock().audio.clone();
            audio.wait_closed();
            crate::warn!("audio-core connection closed; reconnecting");
            // 旧实例的流立即转为断开，等待者不必等到新实例就绪。
            request_processing();
            let Ok(session) = Session::connect(&self.hub) else {
                crate::error!("audio-core reconnection failed");
                return;
            };
            let previous = core::mem::replace(&mut *self.session.lock(), session);
            self.hub.retire(&previous.watch);
            drop(previous);
            request_processing();
        }
    }

    fn process(&self) {
        let mut session = self.session.lock();
        if let Ok(outputs) = session.audio.outputs(session.last) {
            for info in outputs {
                session.last = info.id;
                let index = self.cards.count();
                if attach(self, &session.audio, index, info).is_err() {
                    crate::error!("ALSA card {} initialization failed", index);
                }
            }
        }
        drop(session);
        let count = self.cards.count();
        for index in 0..count {
            if let Some(card) = self.cards.get(index) {
                card.process();
            }
        }
    }
}

fn attach(
    discovery: &Discovery,
    audio: &Arc<AudioClient>,
    index: usize,
    info: OutputInfo,
) -> Result<(), ()> {
    let index = u32::try_from(index).map_err(|_| ())?;
    let minor = index.checked_mul(32).ok_or(())?;
    let shared = info.shared();
    let notification = Pipe::notification_pair()?;
    let control_notification = Pipe::notification_pair()?;
    let card = Arc::try_new(Card {
        configuration: TaskMutex::new(()),
        index,
        id: if index == 0 {
            String::from("LiteOS")
        } else {
            format!("LiteOS{index}")
        },
        shared,
        output: RemoteOutput(info),
        audio: audio.clone(),
        hub: discovery.hub.clone(),
        caller: discovery.caller,
        controls: Mutex::new(Vec::new()),
        slots: Mutex::new(Slots {
            endpoint: PcmEndpoint::Shared,
            slots: core::array::from_fn(|_| Slot::vacant()),
            volume: [[VOLUME_MAX; 2]; SUBSTREAMS],
        }),
        notification_read: notification.0,
        notification_write: notification.1,
        control_notification_read: control_notification.0,
        control_notification_write: control_notification.1,
    })
    .map_err(|_| ())?;
    discovery.cards.register(card.clone()).map_err(|_| ())?;
    for endpoint in [PcmEndpoint::Shared, PcmEndpoint::Direct] {
        if endpoint == PcmEndpoint::Shared && !shared {
            continue;
        }
        let number =
            crate::fs::DeviceNumber::new(116, minor.checked_add(16 + endpoint.device()).ok_or(())?);
        let driver = Arc::try_new(pcm_file::PcmDriver {
            card: card.clone(),
            endpoint,
        })
        .map_err(|_| ())?;
        crate::fs::chardev::register_driver(number, 1, driver).map_err(|_| ())?;
        crate::fs::devnode::register_node(
            format!("snd/pcmC{index}D{}p", endpoint.device()).as_bytes(),
            number,
            0o600,
        )
        .map_err(|_| ())?;
    }
    let number = crate::fs::DeviceNumber::new(116, minor);
    let control = Arc::try_new(control_file::ControlDriver { card }).map_err(|_| ())?;
    crate::fs::chardev::register_driver(number, 1, control).map_err(|_| ())?;
    crate::fs::devnode::register_node(format!("snd/controlC{index}").as_bytes(), number, 0o600)
        .map_err(|_| ())
}

impl Card {
    /// 输出设备已失败或断开（audio-core 拒绝其控制请求）。只在 open 与 control ioctl 上查询。
    fn disconnected(&self) -> bool {
        self.audio.controls(self.output.0.id).is_err()
    }

    /// 从各渲染流的控制区同步子流状态；有变化时唤醒所有等待者。
    fn process(&self) {
        let changed = self
            .slots
            .lock()
            .slots
            .iter_mut()
            .fold(false, |changed, slot| slot.sync() | changed);
        if changed {
            self.notification_write.signal_readiness();
        }
    }

    /// 主音量、静音或子流音量的当前值：主控件属于 audio-core 的输出。
    fn control_values(&self, element: control::Element) -> [u32; 2] {
        match element {
            control::Element::PcmVolume(index) => self.slots.lock().volume[index],
            element => {
                let controls =
                    self.audio
                        .controls(self.output.0.id)
                        .unwrap_or(audio_protocol::Controls {
                            volume: [0; 2],
                            enabled: [false; 2],
                        });
                match element {
                    control::Element::MasterVolume => controls.volume,
                    _ => controls.enabled.map(u32::from),
                }
            }
        }
    }

    /// 应用 codec 已校验的值，返回是否发生真实变化，供 control 事件通知使用。
    fn set_control(&self, element: control::Element, values: [u32; 2]) -> bool {
        match element {
            control::Element::PcmVolume(index) => {
                let mut slots = self.slots.lock();
                if slots.volume[index] == values {
                    return false;
                }
                slots.volume[index] = values;
                if let Some(stream) = &slots.slots[index].stream {
                    let _ = stream.set_gain(values);
                }
                true
            }
            element => {
                let Ok(mut controls) = self.audio.controls(self.output.0.id) else {
                    return false;
                };
                if element == control::Element::MasterVolume {
                    controls.volume = values;
                } else {
                    controls.enabled = values.map(|value| value != 0);
                }
                self.audio
                    .set_controls(self.output.0.id, controls)
                    .unwrap_or(false)
            }
        }
    }
}

/// 一个 PCM OFD 的时钟选择；默认不采集 timestamp，TTSTAMP 只选择源，SW_PARAMS 决定启用。
struct PcmClock {
    kind: u32,
    enabled: bool,
}

impl PcmClock {
    fn sample(&self) -> u64 {
        if !self.enabled {
            0
        } else if self.kind == 0 {
            crate::time::get_realtime_ns()
        } else {
            // 当前 timer 没有频率校正；MONOTONIC 与 MONOTONIC_RAW 使用同一 counter。
            crate::time::get_time_ns()
        }
    }
}

/// PCM 节点的单个 open-file-description：对所选卡、端点与子流的句柄。
pub(crate) struct PcmFile {
    endpoint: PcmEndpoint,
    card: Arc<Card>,
    index: usize,
    // OWNER: serializes ALSA control/data transitions for one OFD.
    // Without it, concurrent HW_FREE/DROP and period submission can publish stale state.
    operation: Mutex<PcmClock>,
    ring: Arc<DeviceBacking>,
}

/// 按同一 opener TID 的 control 偏好或首个空闲槽选择子流。
///
/// # Returns
///
/// 新 OFD-owned PCM file。
///
/// # Errors
///
/// 无设备、全部子流都被占用（`InvalidState`，映射成 `EBUSY`）或分配失败。
fn open(card: Arc<Card>, endpoint: PcmEndpoint) -> Result<Arc<PcmFile>, AudioError> {
    // 每次 open 分配全新的环形缓冲与 mmap identity：旧 OFD 留下的映射不能和新子流的缓冲别名。
    let ring = Arc::try_new(
        DeviceBacking::try_allocate(
            // 整个 OFD 生命周期共享同一 backing；预留有界上限，协商及混音热路径不重分配，
            // 旧 mmap 即使跨 HW_FREE 仍有效，也不会与其他 OFD 的 ring 共享物理页。
            (match endpoint {
                PcmEndpoint::Shared => PcmConfiguration::SHARED.period_bytes() * MAX_BUFFER_PERIODS,
                PcmEndpoint::Direct => MAX_PCM_PERIOD_BYTES * MAX_BUFFER_PERIODS,
            })
            .div_ceil(PAGE_SIZE),
        )
        .ok_or(AudioError::Device)?,
    )
    .map_err(|_| AudioError::Device)?;
    let preferred = card.preferred_subdevice();
    let index = card
        .slots
        .lock()
        .allocate(ring.clone(), preferred, endpoint)
        .map_err(|_| AudioError::InvalidState)?;
    let file = Arc::try_new(PcmFile {
        endpoint,
        card: card.clone(),
        index,
        operation: Mutex::new(PcmClock {
            kind: 0,
            enabled: false,
        }),
        ring,
    });
    match file {
        Ok(file) => {
            crate::info!("ALSA playback substream {} opened", index);
            Ok(file)
        }
        Err(_) => {
            // 分配失败：把已占用的子流还回去，否则它永远空闲不下来。
            card.slots.lock().slots[index] = Slot::vacant();
            Err(AudioError::Device)
        }
    }
}

impl PcmFile {
    /// 撤销子流的渲染流；流 channel 在锁外关闭，audio-core 随之摘掉它的在途归属。
    fn retire_stream(&self) {
        let (stream, binding) = {
            let mut slots = self.card.slots.lock();
            let slot = &mut slots.slots[self.index];
            (slot.stream.take(), slot.binding.take())
        };
        if let Some(binding) = &binding {
            self.card.hub.retire(binding);
        }
        drop(stream);
    }

    /// 配置协商格式与软件缓冲，并在 audio-core 上建立渲染流。
    ///
    /// # Parameters
    ///
    /// - `parameters`: 从 Linux `snd_pcm_hw_params` 解码的领域值。
    pub(crate) fn hardware_parameters(
        &self,
        parameters: HardwareParameters,
    ) -> Result<(), AudioError> {
        let _configuration = self
            .card
            .configuration
            .lock()
            .map_err(|_| AudioError::Device)?;
        let configuration = PcmConfiguration {
            rate: parameters.rate,
            format: codec::pcm_format(parameters.format).ok_or(AudioError::InvalidState)?,
            period_frames: usize::try_from(parameters.period_frames)
                .map_err(|_| AudioError::InvalidState)?,
        };
        if !matches!(parameters.access, 0 | 3)
            || parameters.channels != 2
            || !self.card.output.capabilities().supports(configuration)
            || parameters.buffer_frames != u64::from(parameters.periods) * parameters.period_frames
            || configuration.period_bytes() > MAX_PCM_PERIOD_BYTES
            || self.endpoint == PcmEndpoint::Shared && configuration != PcmConfiguration::SHARED
        {
            return Err(AudioError::InvalidState);
        }
        if self.card.slots.lock().slots[self.index].state.state != PcmState::Open {
            return Err(AudioError::InvalidState);
        }
        self.retire_stream();
        let ring = self
            .ring
            .vmo()
            .duplicate(native_runtime::native_abi::rights::SAME)
            .map_err(|_| AudioError::Device)?;
        let endpoint = match self.endpoint {
            PcmEndpoint::Shared => Endpoint::Shared,
            PcmEndpoint::Direct => Endpoint::Exclusive,
        };
        let stream = self
            .card
            .audio
            .create_stream(
                self.card.output.0.id,
                endpoint,
                configuration,
                parameters.buffer_frames,
                ring,
            )
            .map_err(|status| {
                if status == native_runtime::native_abi::ERR_ALREADY_BOUND {
                    AudioError::InvalidState
                } else {
                    AudioError::Device
                }
            })?;
        let binding = self
            .card
            .hub
            .activity(
                stream
                    .event()
                    .duplicate(native_runtime::native_abi::rights::SAME)
                    .map_err(|_| AudioError::Device)?,
            )
            .map_err(|_| AudioError::Device)?;
        binding.bind(processing_activity().map_err(|()| AudioError::Device)?);
        let mut slots = self.card.slots.lock();
        let volume = slots.volume[self.index];
        if volume != [VOLUME_MAX; 2] {
            let _ = stream.set_gain(volume);
        }
        let slot = &mut slots.slots[self.index];
        slot.state
            .configure(parameters.buffer_frames, configuration)
            .map_err(|_| AudioError::InvalidState)?;
        slot.stream = Some(stream);
        slot.binding = Some(binding);
        slot.played = 0;
        crate::info!("ALSA playback substream {} configured", self.index);
        Ok(())
    }

    /// 设置 wake/start/stop/boundary 和参考时钟 software parameters。
    ///
    /// # Errors
    ///
    /// Open/Disconnected 返回 EBADFD；已配置流的非法参数返回 EINVAL。
    pub(crate) fn software_parameters(&self, parameters: SoftwareParameters) -> Result<(), isize> {
        use linux_abi::errno;
        let mut clock = self.operation.lock();
        let mut slots = self.card.slots.lock();
        let state = &mut slots.slots[self.index].state;
        if matches!(state.state, PcmState::Open | PcmState::Disconnected) {
            return Err(errno::EBADFD);
        }
        if parameters.timestamp_mode > 1 || parameters.timestamp_type.is_some_and(|kind| kind > 2) {
            return Err(errno::EINVAL);
        }
        state
            .set_software(
                parameters.available_min,
                parameters.start_threshold,
                parameters.stop_threshold,
                parameters.boundary,
            )
            .map_err(|_| errno::EINVAL)?;
        clock.enabled = parameters.timestamp_mode == 1;
        if let Some(kind) = parameters.timestamp_type {
            clock.kind = kind;
        }
        Ok(())
    }

    /// 选择 Linux timestamp clock；0 为 realtime，1/2 为本平台未校频的 monotonic counter。
    fn timestamp_type(&self, kind: u32) -> Result<(), isize> {
        if kind > 2 {
            return Err(linux_abi::errno::EINVAL);
        }
        self.operation.lock().kind = kind;
        Ok(())
    }

    /// 让渲染流停止并清零计数（prepare 与 drop 之后的指针都从零开始）。
    fn stop_stream(&self) -> Result<(), AudioError> {
        let slots = self.card.slots.lock();
        if let Some(stream) = &slots.slots[self.index].stream {
            stream.stop().map_err(|_| AudioError::Device)?;
        }
        Ok(())
    }

    pub(crate) fn prepare(&self) -> Result<(), AudioError> {
        let _operation = self.operation.lock();
        {
            let mut slots = self.card.slots.lock();
            let slot = &mut slots.slots[self.index];
            slot.state.prepare().map_err(|_| AudioError::InvalidState)?;
            slot.played = 0;
        }
        self.stop_stream()?;
        self.card.process();
        Ok(())
    }

    pub(crate) fn free_hardware(&self) -> Result<(), AudioError> {
        let _configuration = self
            .card
            .configuration
            .lock()
            .map_err(|_| AudioError::Device)?;
        let _operation = self.operation.lock();
        self.card.slots.lock().slots[self.index]
            .state
            .free_hardware()
            .map_err(|_| AudioError::InvalidState)?;
        self.retire_stream();
        Ok(())
    }

    pub(crate) fn start(&self) -> Result<(), AudioError> {
        let _operation = self.operation.lock();
        let mut slots = self.card.slots.lock();
        let slot = &mut slots.slots[self.index];
        slot.state.start().map_err(|_| AudioError::InvalidState)?;
        if let Some(stream) = &slot.stream {
            stream.start().map_err(|_| AudioError::Device)?;
        }
        drop(slots);
        crate::info!("ALSA playback substream {} started", self.index);
        self.card.process();
        Ok(())
    }

    pub(crate) fn drop_stream(&self) -> Result<(), AudioError> {
        let _operation = self.operation.lock();
        {
            let mut slots = self.card.slots.lock();
            let slot = &mut slots.slots[self.index];
            slot.state
                .drop_stream()
                .map_err(|_| AudioError::InvalidState)?;
            slot.played = 0;
        }
        self.stop_stream()?;
        self.card.process();
        Ok(())
    }

    /// 写入至多一个 period 的完整音频帧并推进应用指针；混音由 audio-core 完成。
    ///
    /// # Parameters
    ///
    /// - `bytes`: 1..=256 帧的双声道 native little-endian float bytes。
    ///
    /// # Returns
    ///
    /// 实际接收的帧数；空间不足时允许短写，完全无空间才返回 EAGAIN。
    ///
    /// # Errors
    ///
    /// 无可用空间为 EAGAIN，XRUN 为 EPIPE，断连为 ENODEV，未准备的流为 EBADFD，非法帧长度为 EINVAL。
    pub(crate) fn write_frames(
        &self,
        bytes: &[u8],
        configuration: PcmConfiguration,
    ) -> Result<usize, isize> {
        use linux_abi::errno;
        let frame_bytes = configuration.frame_bytes();
        if bytes.is_empty()
            || bytes.len() > configuration.period_bytes()
            || !bytes.len().is_multiple_of(frame_bytes)
        {
            return Err(errno::EINVAL);
        }
        let requested = bytes.len() / frame_bytes;
        let _operation = self.operation.lock();
        let frames;
        {
            let mut slots = self.card.slots.lock();
            let slot = &mut slots.slots[self.index];
            slot.sync();
            let state = &mut slot.state;
            if state.configuration != configuration {
                return Err(errno::EBADFD);
            }
            // XRUN 的 poll 立即报告错误；若仍把它当成满缓冲，WRITEI 会在关中断的 syscall 中
            // 不断重试，阻断远端 TLB ack，令其他 CPU 的 munmap 与整个桌面一起停住。
            match state.state {
                PcmState::Xrun => return Err(errno::EPIPE),
                PcmState::Disconnected => return Err(errno::ENODEV),
                PcmState::Open | PcmState::Setup => return Err(errno::EBADFD),
                PcmState::Prepared | PcmState::Running => {}
            }
            frames = requested.min((state.buffer_frames - state.delay()) as usize);
            if frames == 0 {
                return Err(errno::EAGAIN);
            }
            let bytes = &bytes[..frames * frame_bytes];
            // audio-core 只读取已发布帧数之前的数据，所以先写数据、再发布，两步在同一把锁里完成。
            let frame = (state.application_frames % state.buffer_frames) as usize;
            let first = bytes
                .len()
                .min((state.buffer_frames as usize - frame) * frame_bytes);
            self.ring
                .write(frame * frame_bytes, &bytes[..first])
                .map_err(|_| errno::EIO)?;
            if first < bytes.len() {
                self.ring
                    .write(0, &bytes[first..])
                    .map_err(|_| errno::EIO)?;
            }
            state
                .submit_frames(frames as u64)
                .map_err(|_| errno::EBADFD)?;
            let written = state.application_frames;
            if let Some(stream) = &slot.stream {
                stream.commit(written);
            }
        }
        Ok(frames)
    }

    pub(crate) fn status(&self) -> PcmStatus {
        let clock = self.operation.lock();
        let mut slots = self.card.slots.lock();
        let slot = &mut slots.slots[self.index];
        slot.sync();
        let state = &slot.state;
        PcmStatus {
            state: state.state,
            application_frames: state.application_frames % state.boundary,
            hardware_frames: state.hardware_frames % state.boundary,
            delay_frames: state.delay() as i64,
            buffer_frames: state.buffer_frames,
            timestamp_ns: clock.sample(),
        }
    }

    /// 返回 ALSA data mmap 的 OFD backing，只允许映射已协商的完整软件缓冲。
    ///
    /// # Parameters
    ///
    /// - `offset`: 只接受 `SNDRV_PCM_MMAP_OFFSET_DATA=0`。
    /// - `length`: 覆盖完整软件 buffer，允许向上取整到所在页末尾。
    pub(crate) fn mapping(
        &self,
        offset: u64,
        length: usize,
    ) -> Result<DeviceMappingSource, AudioError> {
        let _operation = self.operation.lock();
        let slots = self.card.slots.lock();
        let state = &slots.slots[self.index].state;
        let bytes = state.buffer_frames as usize * state.configuration.frame_bytes();
        if matches!(state.state, PcmState::Open | PcmState::Disconnected)
            || offset != 0
            || length < bytes
            || length > bytes.div_ceil(PAGE_SIZE) * PAGE_SIZE
        {
            return Err(AudioError::InvalidState);
        }
        DeviceMappingSource::new(self.ring.clone()).map_err(|_| AudioError::Device)
    }

    /// 从 ALSA mmap ring 提交 application pointer 的帧增量。数据已经由应用直接写进环形缓冲，
    /// 所以这里只推进并发布指针，不拷贝。
    ///
    /// # Parameters
    ///
    /// - `new_application_pointer`: 以 software boundary 为模的用户指针。
    pub(crate) fn commit_application_pointer(
        &self,
        new_application_pointer: u64,
    ) -> Result<(), AudioError> {
        let _operation = self.operation.lock();
        let mut slots = self.card.slots.lock();
        let slot = &mut slots.slots[self.index];
        let state = &mut slot.state;
        let (current, boundary) = (state.application_frames % state.boundary, state.boundary);
        if new_application_pointer >= boundary {
            return Err(AudioError::InvalidState);
        }
        let delta = if new_application_pointer >= current {
            new_application_pointer - current
        } else {
            boundary - current + new_application_pointer
        };
        if delta > state.buffer_frames {
            return Err(AudioError::InvalidState);
        }
        if delta != 0 {
            state
                .submit_frames(delta)
                .map_err(|_| AudioError::InvalidState)?;
            let written = state.application_frames;
            if let Some(stream) = &slot.stream {
                stream.commit(written);
            }
        }
        Ok(())
    }

    /// level-triggered PCM poll projection。
    pub(crate) fn poll_events(&self, events: i16) -> i16 {
        const ERROR: i16 = 0x008;
        const HANGUP: i16 = 0x010;
        let (state, writable) = {
            let mut slots = self.card.slots.lock();
            let slot = &mut slots.slots[self.index];
            slot.sync();
            (slot.state.state, slot.state.writable())
        };
        match state {
            PcmState::Disconnected => ERROR | HANGUP,
            PcmState::Xrun => ERROR,
            PcmState::Prepared | PcmState::Running => project_playback_events(events, writable),
            _ => 0,
        }
    }

    pub(crate) fn readiness_generation(&self) -> u64 {
        self.card
            .notification_read
            .pipe()
            .readiness_generation(PipeDirection::Read)
    }

    pub(crate) fn notification_pipe(&self) -> Arc<Pipe> {
        self.card.notification_read.pipe()
    }

    pub(crate) fn transfer_configuration(&self) -> PcmConfiguration {
        self.card.slots.lock().slots[self.index].state.configuration
    }

    /// 本子流的编号与卡上总子流数、仍空闲的子流数，供 `SNDRV_PCM_IOCTL_INFO` 使用。
    pub(crate) fn subdevices(&self) -> (usize, usize, usize) {
        (
            self.index,
            if self.endpoint == PcmEndpoint::Shared {
                SUBSTREAMS
            } else {
                1
            },
            self.card.slots.lock().endpoint_available(self.endpoint),
        )
    }
}

impl Drop for PcmFile {
    fn drop(&mut self) {
        // 关闭渲染流（audio-core 摘掉它的在途归属；独占流离开时回到共享格式），交还子流与环形缓冲。
        self.retire_stream();
        let slot = core::mem::replace(
            &mut self.card.slots.lock().slots[self.index],
            Slot::vacant(),
        );
        drop(slot);
        self.card.notification_write.signal_readiness();
    }
}
