//! @description Linux ALSA playback state、position、poll 与 OFD lifecycle owner。

use alloc::sync::{Arc, Weak};
use spin::{Mutex, Once};

use crate::{
    drivers::{PcmCompletionObserver, PcmOutput},
    ipc::{Pipe, PipeDirection, PipeEnd},
    memory::{DeviceBacking, DeviceMappingSource, FrameAllocationClass, PAGE_SIZE},
};

#[path = "audio/state.rs"]
mod state;
pub(crate) use state::PcmState;
use state::PcmStateOwner;
include!("audio/readiness.rs");

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

struct AudioDevice {
    output: Arc<dyn PcmOutput>,
    notification_read: Arc<PipeEnd>,
    notification_write: Arc<PipeEnd>,
    // OWNER: first milestone 只有一个 hardware playback substream；Weak 允许 OFD Drop
    // 释放独占权且不让 device registry 反向保活已经关闭的 stream。
    opened: Mutex<Weak<PcmFile>>,
}

/// `/dev/snd/pcmC0D0p` 的单一 open-file-description backend。
pub(crate) struct PcmFile {
    device: Arc<AudioDevice>,
    // OWNER: serializes ALSA control/data transitions for one OFD.
    // Without it, concurrent HW_FREE/DROP and period submission can publish stale state.
    operation: Mutex<()>,
    state: Mutex<PcmStateOwner>,
    mmap_identity: u64,
    mmap_buffer: Arc<DeviceBacking>,
}

// OWNER: audio domain exclusively publishes and revokes the single playback adapter.
static AUDIO_DEVICE: Once<Arc<AudioDevice>> = Once::new();

/// @description 把 platform PCM adapter 与 task-aware readiness pipe 装配为 ALSA device。
/// @param output 唯一 physical output adapter。
/// @param notification read/write notification endpoints。
/// @return 首次初始化成功。
/// @errors 重复初始化、observer publication 或内存分配失败。
pub(crate) fn init(
    output: Arc<dyn PcmOutput>,
    notification: (Arc<PipeEnd>, Arc<PipeEnd>),
) -> Result<(), ()> {
    if AUDIO_DEVICE.get().is_some() {
        return Err(());
    }
    let device = Arc::try_new(AudioDevice {
        output: output.clone(),
        notification_read: notification.0,
        notification_write: notification.1,
        opened: Mutex::new(Weak::new()),
    })
    .map_err(|_| ())?;
    let observer: Arc<dyn PcmCompletionObserver> = device.clone();
    output.set_observer(observer).map_err(|_| ())?;
    AUDIO_DEVICE.call_once(|| device);
    Ok(())
}

/// @description platform 是否发布了可用 PCM output。
pub(crate) fn available() -> bool {
    AUDIO_DEVICE.get().is_some()
}

/// @description 独占打开第一个 playback substream。
/// @return 新 OFD-owned PCM file。
/// @errors 无设备、已有 live open 或分配失败。
pub(crate) fn open() -> Result<Arc<PcmFile>, AudioError> {
    let device = AUDIO_DEVICE.get().cloned().ok_or(AudioError::Device)?;
    let mut opened = device.opened.lock();
    if opened.upgrade().is_some() {
        return Err(AudioError::InvalidState);
    }
    let file = Arc::try_new(PcmFile {
        device: device.clone(),
        operation: Mutex::new(()),
        state: Mutex::new(PcmStateOwner::new()),
        mmap_identity: crate::id::next_runtime_object_id(),
        mmap_buffer: Arc::try_new(
            DeviceBacking::try_allocate(
                (PCM_BUFFER_FRAMES * PCM_FRAME_BYTES).div_ceil(PAGE_SIZE),
                FrameAllocationClass::Reclaimable,
            )
            .ok_or(AudioError::Device)?,
        )
        .map_err(|_| AudioError::Device)?,
    })
    .map_err(|_| AudioError::Device)?;
    *opened = Arc::downgrade(&file);
    crate::info!("[Audio] ALSA playback opened");
    Ok(file)
}

impl PcmFile {
    /// @description 配置唯一 48kHz/stereo/F32_LE/MMAP-or-RW-interleaved hardware contract。
    /// @param parameters 从 Linux `snd_pcm_hw_params` 解码的领域值。
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
        self.device.output.configure()?;
        self.state
            .lock()
            .configure()
            .map_err(|_| AudioError::InvalidState)?;
        crate::info!("[Audio] ALSA playback configured");
        Ok(())
    }

    /// @description 设置 wake/start/stop/boundary software parameters。
    pub(crate) fn software_parameters(
        &self,
        parameters: SoftwareParameters,
    ) -> Result<(), AudioError> {
        let _operation = self.operation.lock();
        self.state
            .lock()
            .set_software(
                parameters.available_min,
                parameters.start_threshold,
                parameters.stop_threshold,
                parameters.boundary,
            )
            .map_err(|_| AudioError::InvalidState)
    }

    pub(crate) fn prepare(&self) -> Result<(), AudioError> {
        let _operation = self.operation.lock();
        let previous = self.state.lock().state;
        if previous == PcmState::Xrun {
            self.device.output.stop()?;
        }
        self.device.output.prepare()?;
        if let Err(()) = self.state.lock().prepare() {
            if previous != PcmState::Disconnected {
                let _ = self.device.output.release();
            }
            return Err(AudioError::InvalidState);
        }
        Ok(())
    }

    pub(crate) fn free_hardware(&self) -> Result<(), AudioError> {
        let _operation = self.operation.lock();
        if self.state.lock().state != PcmState::Setup {
            return Err(AudioError::InvalidState);
        }
        self.device.output.release()?;
        self.state
            .lock()
            .free_hardware()
            .map_err(|_| AudioError::InvalidState)
    }

    pub(crate) fn start(&self) -> Result<(), AudioError> {
        let _operation = self.operation.lock();
        if self.state.lock().state != PcmState::Prepared {
            return Err(AudioError::InvalidState);
        }
        self.device.output.start()?;
        self.state
            .lock()
            .start()
            .map_err(|_| AudioError::InvalidState)?;
        crate::info!("[Audio] ALSA playback started");
        Ok(())
    }

    pub(crate) fn drop_stream(&self) -> Result<(), AudioError> {
        let _operation = self.operation.lock();
        let state = self.state.lock().state;
        if matches!(state, PcmState::Running | PcmState::Xrun) {
            self.device.output.stop()?;
        }
        self.device.output.release()?;
        self.device.output.configure()?;
        self.state
            .lock()
            .drop_stream()
            .map_err(|_| AudioError::InvalidState)?;
        crate::info!("[Audio] ALSA playback stopped");
        Ok(())
    }

    /// @description 提交一个完整 hardware period。
    /// @param bytes 256 frame 的 native little-endian float bytes。
    pub(crate) fn write_period(&self, bytes: &[u8]) -> Result<(), AudioError> {
        let _operation = self.operation.lock();
        if !self.state.lock().writable() {
            return Err(AudioError::WouldBlock);
        }
        self.device.output.submit_period(bytes)?;
        self.state
            .lock()
            .submit_period()
            .map_err(|_| AudioError::InvalidState)
    }

    pub(crate) fn status(&self) -> PcmStatus {
        let state = self.state.lock();
        PcmStatus {
            state: state.state,
            application_frames: state.application_frames % state.boundary,
            hardware_frames: state.hardware_frames % state.boundary,
            delay_frames: state.delay() as i64,
        }
    }

    /// @description 返回 ALSA data mmap 的固定 1024-frame backing。
    /// @param offset 只接受 `SNDRV_PCM_MMAP_OFFSET_DATA=0`。
    /// @param length 必须覆盖完整 hardware buffer。
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
            self.mmap_buffer.clone(),
        ))
    }

    /// @description 从 ALSA mmap ring 提交 application pointer 的整 period 增量。
    /// @param new_application_pointer 以 software boundary 为模的用户指针。
    pub(crate) fn commit_application_pointer(
        &self,
        new_application_pointer: u64,
    ) -> Result<(), AudioError> {
        let (current, boundary) = {
            let state = self.state.lock();
            (state.application_frames % state.boundary, state.boundary)
        };
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
        let mut period = [0u8; PCM_PERIOD_BYTES];
        let periods = delta as usize / PCM_PERIOD_FRAMES;
        for _ in 0..periods {
            let frame = self.state.lock().application_frames as usize % PCM_BUFFER_FRAMES;
            self.mmap_buffer
                .read(frame * PCM_FRAME_BYTES, &mut period)
                .map_err(|_| AudioError::Device)?;
            self.write_period(&period)?;
        }
        Ok(())
    }

    /// @description level-triggered PCM poll projection。
    pub(crate) fn poll_events(&self, events: i16) -> i16 {
        const ERROR: i16 = 0x008;
        const HANGUP: i16 = 0x010;
        let (state, writable) = {
            let owner = self.state.lock();
            (owner.state, owner.writable())
        };
        match state {
            PcmState::Disconnected => ERROR | HANGUP,
            PcmState::Xrun => ERROR,
            // Physical reclaim may call the completion observer, which takes `self.state`.
            // Snapshot then release that lock first; holding it here deadlocks when poll itself
            // discovers a TX completion before deferred work consumes it.
            PcmState::Prepared | PcmState::Running => {
                project_playback_events(events, writable && self.device.output.writable())
            }
            _ => 0,
        }
    }

    pub(crate) fn readiness_generation(&self) -> u64 {
        self.device
            .notification_read
            .pipe()
            .readiness_generation(PipeDirection::Read)
    }

    pub(crate) fn notification_pipe(&self) -> Arc<Pipe> {
        self.device.notification_read.pipe()
    }
}

impl PcmCompletionObserver for AudioDevice {
    fn period_completed(&self, frames: usize) {
        assert_eq!(frames, PCM_PERIOD_FRAMES);
        if let Some(file) = self.opened.lock().upgrade() {
            let completed_periods = {
                let mut state = file.state.lock();
                let previous = state.hardware_frames;
                state.complete_period();
                (state.hardware_frames != previous)
                    .then_some(state.hardware_frames / PCM_PERIOD_FRAMES as u64)
            };
            if let Some(completed_periods) = completed_periods
                && completed_periods.is_multiple_of(256)
            {
                crate::info!(
                    "[Audio] ALSA playback periods completed: {}",
                    completed_periods
                );
            }
            self.notification_write.signal_readiness();
        }
    }

    fn xrun(&self) {
        if let Some(file) = self.opened.lock().upgrade() {
            let mut state = file.state.lock();
            let previous = state.state;
            state.xrun();
            if previous != state.state {
                crate::info!("[Audio] ALSA playback XRUN");
            }
            drop(state);
            self.notification_write.signal_readiness();
        }
    }

    fn disconnected(&self) {
        if let Some(file) = self.opened.lock().upgrade() {
            file.state.lock().disconnect();
            crate::info!("[Audio] ALSA playback reset");
            self.notification_write.signal_readiness();
        }
    }
}

impl Drop for PcmFile {
    fn drop(&mut self) {
        let state = self.state.lock().state;
        if matches!(state, PcmState::Running | PcmState::Xrun) {
            let _ = self.device.output.stop();
        }
        if matches!(
            state,
            PcmState::Setup | PcmState::Prepared | PcmState::Running | PcmState::Xrun
        ) {
            let _ = self.device.output.release();
        }
        *self.device.opened.lock() = Weak::new();
        self.device.notification_write.signal_readiness();
    }
}
