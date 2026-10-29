//! Linux ALSA playback 子流、软件混音与 OFD lifecycle owner。
//!
//! 物理上只有一个 playback 流（VirtIO Sound）。本领域在它上面提供 [`SUBSTREAMS`] 个互相独立的子流：每次
//! `open("/dev/snd/pcmC0D0p")` 按 control 偏好或首个空闲槽选择子流，所有运行中的子流由 [`engine::Engine`] 混成同一个物理流。
//! 与 Linux 里带硬件混音的声卡（例如 emu10k1）的对外形态一致，因此任何 ALSA 客户端都不需要用户态混音服务。

use alloc::{
    format,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use spin::{Mutex, Once};

use crate::{
    drivers::{PcmCompletionObserver, PcmOutput, io_completion::CompletionSource},
    ipc::{Pipe, PipeDirection, PipeEnd},
    memory::{
        DeviceBacking, DeviceMappingSource, DeviceMemoryKind, FrameAllocationClass, PAGE_SIZE,
    },
    namespace::PidIdentity,
    sync::TaskMutex,
};

mod codec;
mod control;
mod control_file;
mod engine;
mod ioctl;
mod mixer;
mod pcm_file;
mod state;
use crate::drivers::{MAX_PCM_PERIOD_BYTES, PcmConfiguration};
use engine::{Engine, Events, PcmEndpoint, SUBSTREAMS};
use state::MAX_BUFFER_PERIODS;
pub(crate) use state::PcmState;
include!("readiness.rs");

pub(crate) use crate::drivers::PcmOutputError as AudioError;

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

/// 子流环形缓冲：引擎按字节偏移读取整个 period。
impl engine::Ring for Arc<DeviceBacking> {
    fn read(&self, offset: usize, output: &mut [u8]) -> Result<(), ()> {
        DeviceBacking::read(self, offset, output)
    }
}

/// 设备回调累计的事件。回调运行在设备 `reclaim` 内部，而 `reclaim` 又会被引擎自己的 `submit_period`/`writable`
/// 触发，所以回调只能写这些原子量，不能取引擎的锁——否则就是持锁自等的死锁。
#[derive(Default)]
struct EventCounters {
    completed: AtomicUsize,
    disconnected: AtomicBool,
}

impl EventCounters {
    fn take(&self) -> Events {
        Events {
            completed: self.completed.swap(0, Ordering::AcqRel),
            disconnected: self.disconnected.load(Ordering::Acquire),
        }
    }

    fn pending(&self) -> bool {
        self.completed.load(Ordering::Acquire) != 0
    }
}

/// 一块声卡：物理输出、混音引擎与等待者通知。
struct Card {
    // OWNER: 冷配置事务的唯一 task 锁；等待 USB 控制传输时不持 Engine/operation 自旋锁。
    configuration: TaskMutex<()>,
    index: u32,
    id: String,
    shared: bool,
    output: Arc<dyn PcmOutput>,
    // OWNER: composition root 安装的调用线程 identity 投影；只在 task context、且未持音频锁时调用。
    caller: fn() -> Option<Arc<PidIdentity>>,
    // OWNER: control open 顺序的唯一 registry；弱引用不延长 OFD，actual fput 由 ControlOpen.closed 撤销。
    // 锁序 registry → OFD state；不跨 registry 持引擎锁或发布 readiness。仅 open/control 操作访问，非混音热路径。
    controls: Mutex<Vec<Weak<control_file::ControlFile>>>,
    // OWNER: 引擎状态（子流指针、在途 period 记账、主音量）的唯一入口；持锁期间会调用 `output`，所以设备回调
    // 绝不取它。缺少这条纪律，`submit_period` 内部触发的 reclaim 回调会在同一个线程上自旋等待自己。
    engine: Mutex<Engine<Arc<DeviceBacking>>>,
    events: EventCounters,
    notification_read: Arc<PipeEnd>,
    notification_write: Arc<PipeEnd>,
    // OWNER: control 事件的独立唤醒源，避免 PCM period completion 周期性唤醒空闲 mixer consumer。
    control_notification_read: Arc<PipeEnd>,
    control_notification_write: Arc<PipeEnd>,
}

// OWNER: discovery 是 drivers PCM index → ALSA card 的唯一绑定者。next 只在本锁下推进；
// 硬件热插入发布 DRIVER_IO 后在 task/idle safe point 绑定，IRQ 不分配、不发布 devfs 节点。
struct Discovery {
    next: Mutex<usize>,
    caller: fn() -> Option<Arc<PidIdentity>>,
}
static DISCOVERY: Once<Arc<Discovery>> = Once::new();

/// 安装 ALSA 设备发现并发布已存在的 PCM adapter；之后的新 adapter 经 DRIVER_IO 发现。
///
/// # Parameters
/// - `caller`: task owner 的实际 TID observer。
///
/// # Errors
/// 重复安装、分配或 completion source 发布失败时返回错误。
pub(crate) fn init(caller: fn() -> Option<Arc<PidIdentity>>) -> Result<(), ()> {
    if DISCOVERY.get().is_some() {
        return Err(());
    }
    let discovery = Arc::try_new(Discovery {
        next: Mutex::new(0),
        caller,
    })
    .map_err(|_| ())?;
    crate::drivers::register_completion_source(discovery.clone()).map_err(|_| ())?;
    DISCOVERY.call_once(|| discovery.clone());
    discovery.dispatch_completions();
    Ok(())
}

impl CompletionSource for Discovery {
    fn dispatch_completions(&self) -> bool {
        let mut next = self.next.lock();
        while let Some(output) = crate::drivers::pcm_output(*next) {
            let index = *next;
            *next += 1;
            if attach(index, output, self.caller).is_err() {
                crate::error!("ALSA card {} initialization failed", index);
            }
        }
        false
    }
}

fn attach(
    index: usize,
    output: Arc<dyn PcmOutput>,
    caller: fn() -> Option<Arc<PidIdentity>>,
) -> Result<(), ()> {
    let index = u32::try_from(index).map_err(|_| ())?;
    let minor = index.checked_mul(32).ok_or(())?;
    let shared = output.capabilities().supports(PcmConfiguration::SHARED);
    if shared {
        output.configure(PcmConfiguration::SHARED).map_err(|_| ())?;
        output.prepare().map_err(|_| ())?;
    }
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
        output: output.clone(),
        caller,
        controls: Mutex::new(Vec::new()),
        engine: Mutex::new(Engine::new(shared.then_some(PcmConfiguration::SHARED))),
        events: EventCounters::default(),
        notification_read: notification.0,
        notification_write: notification.1,
        control_notification_read: control_notification.0,
        control_notification_write: control_notification.1,
    })
    .map_err(|_| ())?;
    output.set_observer(card.clone()).map_err(|_| ())?;
    crate::drivers::register_completion_source(card.clone()).map_err(|_| ())?;
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
    /// 配置锁由调用方持有；先冻结提交，再在全部自旋锁之外完成 stop/release/configure/prepare。
    fn switch_output(&self, target: Option<PcmConfiguration>) -> Result<(), AudioError> {
        let (previous, running) = {
            let mut engine = self.engine.lock();
            let previous = engine.configuration();
            (previous, engine.begin_transition()?)
        };
        let result = (|| {
            if running {
                self.output.stop()?;
            }
            if previous.is_some() {
                self.output.release()?;
            }
            // RELEASE 是旧 DMA 与回调的完成屏障，新的 stream 尚未提交任何 I/O。
            let _ = self.events.take();
            if let Some(configuration) = target {
                self.output.configure(configuration)?;
                self.output.prepare()?;
            }
            Ok(())
        })();
        let mut engine = self.engine.lock();
        if let Err(error) = result {
            engine.fail();
            return Err(error);
        }
        engine.finish_transition(target)
    }

    /// 让引擎消化设备事件并补填队列；有变化时唤醒所有等待者。
    ///
    /// 补填期间设备 reclaim 可能又累计出新的完成事件，所以循环到事件取空为止。
    fn process(&self) {
        let mut changed = false;
        for _ in 0..16 {
            let events = self.events.take();
            let tick_changed = self.engine.lock().tick(events, &*self.output);
            changed |= tick_changed;
            if !self.events.pending() {
                break;
            }
        }
        if changed {
            self.notification_write.signal_readiness();
        }
    }
}

impl PcmCompletionObserver for Card {
    fn period_completed(&self, _frames: usize) {
        self.events.completed.fetch_add(1, Ordering::AcqRel);
    }

    fn xrun(&self) {
        // 设备 underrun 的后果（队列排空）由 `tick` 通过完成记账发现；这里只负责让它尽快被处理。
        self.notification_write.signal_readiness();
    }

    fn disconnected(&self) {
        self.events.disconnected.store(true, Ordering::Release);
        crate::info!("ALSA playback reset");
        self.notification_write.signal_readiness();
    }
}

impl CompletionSource for Card {
    fn dispatch_completions(&self) -> bool {
        self.process();
        false
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
            crate::timer::get_realtime_ns()
        } else {
            // 当前 timer 没有频率校正；MONOTONIC 与 MONOTONIC_RAW 使用同一 counter。
            crate::timer::get_time_ns()
        }
    }
}

/// `/dev/snd/pcmC0D0p` 的单个 open-file-description：对一个子流的句柄。
pub(crate) struct PcmFile {
    endpoint: PcmEndpoint,
    card: Arc<Card>,
    index: usize,
    // OWNER: serializes ALSA control/data transitions for one OFD.
    // Without it, concurrent HW_FREE/DROP and period submission can publish stale state.
    operation: Mutex<PcmClock>,
    mmap_identity: u64,
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
        // virtio-snd 的 SNDRV_DMA_TYPE_VMALLOC ring 经 remap_vmalloc_range 插入普通页。
        DeviceBacking::try_allocate(
            // 整个 OFD 生命周期共享同一 backing；预留有界上限，协商及混音热路径不重分配，
            // 旧 mmap 即使跨 HW_FREE 仍有效，也不会与其他 OFD 的 ring 共享物理页。
            (match endpoint {
                PcmEndpoint::Shared => PcmConfiguration::SHARED.period_bytes() * MAX_BUFFER_PERIODS,
                PcmEndpoint::Direct => MAX_PCM_PERIOD_BYTES * MAX_BUFFER_PERIODS,
            })
            .div_ceil(PAGE_SIZE),
            FrameAllocationClass::Reclaimable,
            DeviceMemoryKind::InsertedPages,
        )
        .ok_or(AudioError::Device)?,
    )
    .map_err(|_| AudioError::Device)?;
    let preferred = card.preferred_subdevice();
    let index = card
        .engine
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
        mmap_identity: crate::id::next_runtime_object_id(),
        ring,
    });
    match file {
        Ok(file) => {
            crate::info!("ALSA playback substream {} opened", index);
            Ok(file)
        }
        Err(_) => {
            // 分配失败：把已占用的子流还回去，否则它永远空闲不下来。
            let _ = card.engine.lock().release(index);
            Err(AudioError::Device)
        }
    }
}

impl PcmFile {
    /// 配置 48kHz/stereo/F32_LE/MMAP-or-RW-interleaved 和协商的软件缓冲。
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
        {
            let engine = self.card.engine.lock();
            if engine.state(self.index).state != PcmState::Open {
                return Err(AudioError::InvalidState);
            }
        }
        if self.card.engine.lock().configuration() != Some(configuration) {
            self.card.switch_output(Some(configuration))?;
        }
        self.card
            .engine
            .lock()
            .configure(self.index, parameters.buffer_frames, configuration)
            .map_err(|()| AudioError::InvalidState)?;
        crate::info!("ALSA playback substream {} configured", self.index);
        Ok(())
    }

    /// 设置 wake/start/stop/boundary 和参考时钟 software parameters。
    ///
    /// # Errors
    ///
    /// Open/Disconnected 返回 EBADFD；已配置流的非法参数返回 EINVAL。
    pub(crate) fn software_parameters(&self, parameters: SoftwareParameters) -> Result<(), isize> {
        use syscall_abi::errno;
        let mut clock = self.operation.lock();
        let mut engine = self.card.engine.lock();
        if matches!(
            engine.state(self.index).state,
            PcmState::Open | PcmState::Disconnected
        ) {
            return Err(errno::EBADFD);
        }
        if parameters.timestamp_mode > 1 || parameters.timestamp_type.is_some_and(|kind| kind > 2) {
            return Err(errno::EINVAL);
        }
        engine
            .set_software(
                self.index,
                parameters.available_min,
                parameters.start_threshold,
                parameters.stop_threshold,
                parameters.boundary,
            )
            .map_err(|()| errno::EINVAL)?;
        clock.enabled = parameters.timestamp_mode == 1;
        if let Some(kind) = parameters.timestamp_type {
            clock.kind = kind;
        }
        Ok(())
    }

    /// 选择 Linux timestamp clock；0 为 realtime，1/2 为本平台未校频的 monotonic counter。
    fn timestamp_type(&self, kind: u32) -> Result<(), isize> {
        if kind > 2 {
            return Err(syscall_abi::errno::EINVAL);
        }
        self.operation.lock().kind = kind;
        Ok(())
    }

    pub(crate) fn prepare(&self) -> Result<(), AudioError> {
        let _configuration = self
            .card
            .configuration
            .lock()
            .map_err(|_| AudioError::Device)?;
        let (configuration, restart) = {
            let _operation = self.operation.lock();
            let mut engine = self.card.engine.lock();
            let restart = self.endpoint == PcmEndpoint::Direct && engine.running();
            engine
                .prepare(self.index)
                .map_err(|()| AudioError::InvalidState)?;
            (engine.state(self.index).configuration, restart)
        };
        if restart {
            self.card.switch_output(Some(configuration))?;
        }
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
        self.card
            .engine
            .lock()
            .free_hardware(self.index)
            .map_err(|()| AudioError::InvalidState)
    }

    pub(crate) fn start(&self) -> Result<(), AudioError> {
        let _operation = self.operation.lock();
        self.card
            .engine
            .lock()
            .start(self.index)
            .map_err(|()| AudioError::InvalidState)?;
        crate::info!("ALSA playback substream {} started", self.index);
        self.card.process();
        Ok(())
    }

    pub(crate) fn drop_stream(&self) -> Result<(), AudioError> {
        let _configuration = self
            .card
            .configuration
            .lock()
            .map_err(|_| AudioError::Device)?;
        let configuration = {
            let _operation = self.operation.lock();
            let mut engine = self.card.engine.lock();
            engine
                .drop_stream(self.index)
                .map_err(|()| AudioError::InvalidState)?;
            engine.state(self.index).configuration
        };
        if self.endpoint == PcmEndpoint::Direct {
            self.card.switch_output(Some(configuration))?;
        }
        self.card.process();
        Ok(())
    }

    /// 写入至多一个 period 的完整音频帧并推进应用指针；混音由随后的 `process` 完成。
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
        use syscall_abi::errno;
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
            let mut engine = self.card.engine.lock();
            let state = engine.state(self.index);
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
            // 引擎只读取应用指针之前的帧，所以必须先写数据、再推进指针，两步在同一把锁里完成。
            let frame = (state.application_frames % state.buffer_frames) as usize;
            let first = bytes
                .len()
                .min((state.buffer_frames as usize - frame) * frame_bytes);
            self.ring
                .write(frame * frame_bytes, &bytes[..first])
                .map_err(|()| errno::EIO)?;
            if first < bytes.len() {
                self.ring
                    .write(0, &bytes[first..])
                    .map_err(|()| errno::EIO)?;
            }
            engine
                .application_frames(self.index, frames as u64)
                .map_err(|()| errno::EBADFD)?;
        }
        self.card.process();
        Ok(frames)
    }

    pub(crate) fn status(&self) -> PcmStatus {
        let clock = self.operation.lock();
        let engine = self.card.engine.lock();
        let state = engine.state(self.index);
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
        let engine = self.card.engine.lock();
        let state = engine.state(self.index);
        let bytes = state.buffer_frames as usize * state.configuration.frame_bytes();
        if matches!(state.state, PcmState::Open | PcmState::Disconnected)
            || offset != 0
            || length < bytes
            || length > bytes.div_ceil(PAGE_SIZE) * PAGE_SIZE
        {
            return Err(AudioError::InvalidState);
        }
        Ok(DeviceMappingSource::new(
            self.mmap_identity,
            self.ring.clone(),
        ))
    }

    /// 从 ALSA mmap ring 提交 application pointer 的帧增量。数据已经由应用直接写进环形缓冲，
    /// 所以这里只推进指针，不拷贝。
    ///
    /// # Parameters
    ///
    /// - `new_application_pointer`: 以 software boundary 为模的用户指针。
    pub(crate) fn commit_application_pointer(
        &self,
        new_application_pointer: u64,
    ) -> Result<(), AudioError> {
        let _operation = self.operation.lock();
        {
            let mut engine = self.card.engine.lock();
            let state = engine.state(self.index);
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
                engine
                    .application_frames(self.index, delta)
                    .map_err(|()| AudioError::InvalidState)?;
            }
        }
        self.card.process();
        Ok(())
    }

    /// level-triggered PCM poll projection。
    pub(crate) fn poll_events(&self, events: i16) -> i16 {
        const ERROR: i16 = 0x008;
        const HANGUP: i16 = 0x010;
        let (state, writable) = {
            let engine = self.card.engine.lock();
            let owner = engine.state(self.index);
            (owner.state, owner.writable())
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
        self.card.engine.lock().state(self.index).configuration
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
            self.card.engine.lock().endpoint_available(self.endpoint),
        )
    }
}

impl Drop for PcmFile {
    fn drop(&mut self) {
        // 引擎摘掉本子流在在途 period 里的归属并交还环形缓冲；缓冲在锁外销毁。
        if self.endpoint == PcmEndpoint::Direct {
            match self.card.configuration.lock() {
                Ok(_configuration) => {
                    if self.card.switch_output(None).is_err() {
                        crate::warn!("native PCM release failed");
                    }
                }
                Err(_) => self.card.engine.lock().fail(),
            }
        }
        let ring = self.card.engine.lock().release(self.index);
        drop(ring);
        self.card.process();
        self.card.notification_write.signal_readiness();
    }
}
