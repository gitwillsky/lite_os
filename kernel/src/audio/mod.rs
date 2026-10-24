//! Linux ALSA playback 子流、软件混音与 OFD lifecycle owner。
//!
//! 物理上只有一个 playback 流（VirtIO Sound）。本领域在它上面提供 [`SUBSTREAMS`] 个互相独立的子流：每次
//! `open("/dev/snd/pcmC0D0p")` 占用第一个空闲子流，所有运行中的子流由 [`engine::Engine`] 混成同一个物理流。
//! 与 Linux 里带硬件混音的声卡（例如 emu10k1）的对外形态一致，因此任何 ALSA 客户端都不需要用户态混音服务。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use spin::{Mutex, Once};

use crate::{
    drivers::{PcmCompletionObserver, PcmOutput, io_completion::CompletionSource},
    ipc::{Pipe, PipeDirection, PipeEnd},
    memory::{DeviceBacking, DeviceMappingSource, FrameAllocationClass, PAGE_SIZE},
};

mod codec;
mod control;
mod control_file;
mod engine;
mod ioctl;
mod mixer;
mod pcm_file;
mod state;
use engine::{Engine, Events, SUBSTREAMS};
pub(crate) use state::PcmState;
include!("readiness.rs");

pub(crate) use crate::drivers::{
    PCM_BUFFER_FRAMES, PCM_FRAME_BYTES, PCM_PERIOD_BYTES, PCM_PERIOD_FRAMES, PCM_RATE,
    PcmOutputError as AudioError,
};

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
}

/// 声卡在 `snd_pcm_info` / `snd_ctl_card_info` 里的短 id，以及 PCM 设备的名字。
const CARD_ID: &[u8] = b"LiteOS";
const PCM_NAME: &[u8] = b"LiteOS Playback";

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
    output: Arc<dyn PcmOutput>,
    // OWNER: 引擎状态（子流指针、在途 period 记账、主音量）的唯一入口；持锁期间会调用 `output`，所以设备回调
    // 绝不取它。缺少这条纪律，`submit_period` 内部触发的 reclaim 回调会在同一个线程上自旋等待自己。
    engine: Mutex<Engine<Arc<DeviceBacking>>>,
    events: EventCounters,
    notification_read: Arc<PipeEnd>,
    notification_write: Arc<PipeEnd>,
}

// OWNER: audio domain 独占发布唯一的声卡；init 之后只读。
static CARD: Once<Arc<Card>> = Once::new();

/// 把 platform PCM adapter 装配成带软件混音的 ALSA 声卡。
///
/// # Parameters
///
/// - `output`: 唯一物理 playback adapter。
///
/// # Errors
///
/// 重复初始化、物理流配置失败、observer/completion source 发布失败或内存不足。
pub(crate) fn init(output: Arc<dyn PcmOutput>) -> Result<(), ()> {
    if CARD.get().is_some() {
        return Err(());
    }
    // 物理流只配置并 prepare 一次，此后归混音引擎独占；子流的 hw_params/prepare 只是软件状态转换。
    output.configure().map_err(|_| ())?;
    output.prepare().map_err(|_| ())?;
    let notification = Pipe::notification_pair()?;
    let card = Arc::try_new(Card {
        output: output.clone(),
        engine: Mutex::new(Engine::new()),
        events: EventCounters::default(),
        notification_read: notification.0,
        notification_write: notification.1,
    })
    .map_err(|_| ())?;
    let observer: Arc<dyn PcmCompletionObserver> = card.clone();
    output.set_observer(observer).map_err(|_| ())?;
    let source: Arc<dyn CompletionSource> = card.clone();
    crate::drivers::register_completion_source(source).map_err(|_| ())?;
    CARD.call_once(|| card);
    let driver = Arc::try_new(pcm_file::PcmDriver).map_err(|_| ())?;
    crate::fs::chardev::register_driver(pcm_file::PCM_NUMBER, 1, driver).map_err(|_| ())?;
    crate::fs::devnode::register_node(pcm_file::PCM_PATH, pcm_file::PCM_NUMBER, 0o600)
        .map_err(|_| ())?;
    let control = Arc::try_new(control_file::ControlDriver).map_err(|_| ())?;
    crate::fs::chardev::register_driver(control_file::CONTROL_NUMBER, 1, control).map_err(|_| ())?;
    crate::fs::devnode::register_node(control_file::CONTROL_PATH, control_file::CONTROL_NUMBER, 0o600)
        .map_err(|_| ())
}

impl Card {
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
    fn period_completed(&self, frames: usize) {
        assert_eq!(frames, PCM_PERIOD_FRAMES);
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

/// `/dev/snd/pcmC0D0p` 的单个 open-file-description：对一个子流的句柄。
pub(crate) struct PcmFile {
    card: Arc<Card>,
    index: usize,
    // OWNER: serializes ALSA control/data transitions for one OFD.
    // Without it, concurrent HW_FREE/DROP and period submission can publish stale state.
    operation: Mutex<()>,
    mmap_identity: u64,
    ring: Arc<DeviceBacking>,
}

/// 占用第一个空闲子流。
///
/// # Returns
///
/// 新 OFD-owned PCM file。
///
/// # Errors
///
/// 无设备、全部子流都被占用（`InvalidState`，映射成 `EBUSY`）或分配失败。
fn open() -> Result<Arc<PcmFile>, AudioError> {
    let card = CARD.get().cloned().ok_or(AudioError::Device)?;
    // 每次 open 分配全新的环形缓冲与 mmap identity：旧 OFD 留下的映射不能和新子流的缓冲别名。
    let ring = Arc::try_new(
        DeviceBacking::try_allocate(
            (PCM_BUFFER_FRAMES * PCM_FRAME_BYTES).div_ceil(PAGE_SIZE),
            FrameAllocationClass::Reclaimable,
        )
        .ok_or(AudioError::Device)?,
    )
    .map_err(|_| AudioError::Device)?;
    let index = card
        .engine
        .lock()
        .allocate(ring.clone())
        .map_err(|_| AudioError::InvalidState)?;
    let file = Arc::try_new(PcmFile {
        card,
        index,
        operation: Mutex::new(()),
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
            let _ = CARD.get().map(|card| card.engine.lock().release(index));
            Err(AudioError::Device)
        }
    }
}

impl PcmFile {
    /// 配置唯一 48kHz/stereo/F32_LE/MMAP-or-RW-interleaved hardware contract。
    ///
    /// # Parameters
    ///
    /// - `parameters`: 从 Linux `snd_pcm_hw_params` 解码的领域值。
    pub(crate) fn hardware_parameters(
        &self,
        parameters: HardwareParameters,
    ) -> Result<(), AudioError> {
        let _operation = self.operation.lock();
        if !matches!(parameters.access, 0 | 3)
            || parameters.format != 14
            || parameters.channels != 2
            || parameters.rate != PCM_RATE
            || parameters.period_frames != PCM_PERIOD_FRAMES as u64
            || parameters.periods != 4
            || parameters.buffer_frames != PCM_BUFFER_FRAMES as u64
        {
            return Err(AudioError::InvalidState);
        }
        self.card
            .engine
            .lock()
            .configure(self.index)
            .map_err(|()| AudioError::InvalidState)?;
        crate::info!("ALSA playback substream {} configured", self.index);
        Ok(())
    }

    /// 设置 wake/start/stop/boundary software parameters。
    pub(crate) fn software_parameters(
        &self,
        parameters: SoftwareParameters,
    ) -> Result<(), AudioError> {
        let _operation = self.operation.lock();
        self.card
            .engine
            .lock()
            .set_software(
                self.index,
                parameters.available_min,
                parameters.start_threshold,
                parameters.stop_threshold,
                parameters.boundary,
            )
            .map_err(|()| AudioError::InvalidState)
    }

    pub(crate) fn prepare(&self) -> Result<(), AudioError> {
        let _operation = self.operation.lock();
        self.card
            .engine
            .lock()
            .prepare(self.index)
            .map_err(|()| AudioError::InvalidState)
    }

    pub(crate) fn free_hardware(&self) -> Result<(), AudioError> {
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
        let _operation = self.operation.lock();
        self.card
            .engine
            .lock()
            .drop_stream(self.index)
            .map_err(|()| AudioError::InvalidState)?;
        crate::info!("ALSA playback substream {} stopped", self.index);
        self.card.process();
        Ok(())
    }

    /// 写入一个完整 period 并推进应用指针；混音由随后的 `process` 完成。
    ///
    /// # Parameters
    ///
    /// - `bytes`: 256 frame 的 native little-endian float bytes。
    ///
    /// # Errors
    ///
    /// 环形缓冲没有一个 period 的空位返回 `WouldBlock`；状态不允许写入返回 `InvalidState`。
    pub(crate) fn write_period(&self, bytes: &[u8]) -> Result<(), AudioError> {
        let _operation = self.operation.lock();
        {
            let mut engine = self.card.engine.lock();
            let state = engine.state(self.index);
            if !state.writable() {
                return Err(AudioError::WouldBlock);
            }
            // 引擎只读取应用指针之前的帧，所以必须先写数据、再推进指针，两步在同一把锁里完成。
            let frame = state.application_frames as usize % PCM_BUFFER_FRAMES;
            self.ring
                .write(frame * PCM_FRAME_BYTES, bytes)
                .map_err(|()| AudioError::Device)?;
            engine
                .application_period(self.index)
                .map_err(|()| AudioError::InvalidState)?;
        }
        self.card.process();
        Ok(())
    }

    pub(crate) fn status(&self) -> PcmStatus {
        let engine = self.card.engine.lock();
        let state = engine.state(self.index);
        PcmStatus {
            state: state.state,
            application_frames: state.application_frames % state.boundary,
            hardware_frames: state.hardware_frames % state.boundary,
            delay_frames: state.delay() as i64,
        }
    }

    /// 返回 ALSA data mmap 的固定 1024-frame backing。
    ///
    /// # Parameters
    ///
    /// - `offset`: 只接受 `SNDRV_PCM_MMAP_OFFSET_DATA=0`。
    /// - `length`: 必须覆盖完整 hardware buffer。
    pub(crate) fn mapping(
        &self,
        offset: u64,
        length: usize,
    ) -> Result<DeviceMappingSource, AudioError> {
        if offset != 0 || length != PCM_BUFFER_FRAMES * PCM_FRAME_BYTES {
            return Err(AudioError::InvalidState);
        }
        Ok(DeviceMappingSource::new(
            self.mmap_identity,
            self.ring.clone(),
        ))
    }

    /// 从 ALSA mmap ring 提交 application pointer 的整 period 增量。数据已经由应用直接写进环形缓冲，
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
            if !delta.is_multiple_of(PCM_PERIOD_FRAMES as u64) || delta > PCM_BUFFER_FRAMES as u64 {
                return Err(AudioError::InvalidState);
            }
            for _ in 0..delta as usize / PCM_PERIOD_FRAMES {
                engine
                    .application_period(self.index)
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

    /// 本子流的编号与卡上总子流数、仍空闲的子流数，供 `SNDRV_PCM_IOCTL_INFO` 使用。
    pub(crate) fn subdevices(&self) -> (usize, usize, usize) {
        (
            self.index,
            SUBSTREAMS,
            self.card.engine.lock().available(),
        )
    }
}

impl Drop for PcmFile {
    fn drop(&mut self) {
        // 引擎摘掉本子流在在途 period 里的归属并交还环形缓冲；缓冲在锁外销毁。
        let ring = self.card.engine.lock().release(self.index);
        drop(ring);
        self.card.process();
        self.card.notification_write.signal_readiness();
    }
}
