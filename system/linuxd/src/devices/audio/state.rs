use crate::devices::model::{PCM_BUFFER_FRAMES, PCM_PERIOD_FRAMES, PcmConfiguration};

/// 软件 ring 最多 32 个已协商 period；共享格式上限 64 KiB，可覆盖约 100ms 调度余量。
/// 物理 DMA 队列仍由 adapter 固定；若软件 ring 也限为它的 21ms，普通调度抖动即可耗尽音频。
pub(super) const MAX_BUFFER_PERIODS: usize = 32;

/// ALSA playback stream state number，固定到 Linux `snd_pcm_state_t`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub(crate) enum PcmState {
    Open = 0,
    Setup = 1,
    Prepared = 2,
    Running = 3,
    Xrun = 4,
    Disconnected = 8,
}

/// 单一 playback OFD 的 ALSA state 与 position owner。
pub(crate) struct PcmStateOwner {
    pub(crate) configuration: PcmConfiguration,
    pub(crate) state: PcmState,
    pub(crate) application_frames: u64,
    pub(crate) hardware_frames: u64,
    pub(crate) buffer_frames: u64,
    pub(crate) available_min: u64,
    pub(crate) start_threshold: u64,
    pub(crate) stop_threshold: u64,
    pub(crate) boundary: u64,
}

impl PcmStateOwner {
    pub(crate) const fn new() -> Self {
        Self {
            configuration: PcmConfiguration::SHARED,
            state: PcmState::Open,
            application_frames: 0,
            hardware_frames: 0,
            buffer_frames: PCM_BUFFER_FRAMES as u64,
            available_min: PCM_PERIOD_FRAMES as u64,
            start_threshold: PCM_BUFFER_FRAMES as u64,
            stop_threshold: PCM_BUFFER_FRAMES as u64,
            boundary: 1 << 31,
        }
    }

    /// 将 Open 流配置为指定的软件 ring 容量。
    ///
    /// # Parameters
    /// - `buffer_frames`: 4..=32 个完整 256-frame period。
    ///
    /// # Errors
    /// 非 Open 状态、容量越界或不含完整 period 时返回错误，原状态不变。
    pub(crate) fn configure(
        &mut self,
        buffer_frames: u64,
        configuration: PcmConfiguration,
    ) -> Result<(), ()> {
        let period = configuration.period_frames as u64;
        if self.state != PcmState::Open
            || period == 0
            || !(period * 4..=period * MAX_BUFFER_PERIODS as u64).contains(&buffer_frames)
            || !buffer_frames.is_multiple_of(period)
        {
            return Err(());
        }
        self.state = PcmState::Setup;
        self.configuration = configuration;
        self.application_frames = 0;
        self.hardware_frames = 0;
        self.buffer_frames = buffer_frames;
        self.available_min = period;
        self.start_threshold = buffer_frames;
        self.stop_threshold = buffer_frames;
        // Linux 的默认 boundary 是 buffer 的二次幂倍数；非二次幂 ring 跨 boundary 后仍须落在相同位置。
        self.boundary = buffer_frames;
        while self.boundary <= (i64::MAX as u64 - buffer_frames) / 2 {
            self.boundary *= 2;
        }
        Ok(())
    }

    pub(crate) fn set_software(
        &mut self,
        available_min: u64,
        start_threshold: u64,
        stop_threshold: u64,
        boundary: u64,
    ) -> Result<(), ()> {
        if matches!(self.state, PcmState::Open | PcmState::Disconnected)
            || available_min == 0
            || available_min > self.buffer_frames
            || start_threshold > boundary
            || stop_threshold > boundary
            || boundary < self.buffer_frames * 2
        {
            return Err(());
        }
        self.available_min = available_min;
        self.start_threshold = start_threshold;
        self.stop_threshold = stop_threshold;
        self.boundary = boundary;
        Ok(())
    }

    pub(crate) fn prepare(&mut self) -> Result<(), ()> {
        if !matches!(
            self.state,
            PcmState::Setup | PcmState::Prepared | PcmState::Xrun
        ) {
            return Err(());
        }
        self.state = PcmState::Prepared;
        self.application_frames = 0;
        self.hardware_frames = 0;
        Ok(())
    }

    pub(crate) fn free_hardware(&mut self) -> Result<(), ()> {
        if self.state != PcmState::Setup {
            return Err(());
        }
        self.state = PcmState::Open;
        Ok(())
    }

    pub(crate) fn start(&mut self) -> Result<(), ()> {
        if self.state != PcmState::Prepared {
            return Err(());
        }
        self.state = PcmState::Running;
        Ok(())
    }

    pub(crate) fn drop_stream(&mut self) -> Result<(), ()> {
        if !matches!(
            self.state,
            PcmState::Prepared | PcmState::Running | PcmState::Xrun
        ) {
            return Err(());
        }
        self.state = PcmState::Setup;
        self.application_frames = 0;
        self.hardware_frames = 0;
        Ok(())
    }

    pub(crate) fn submit_frames(&mut self, frames: u64) -> Result<(), ()> {
        if !matches!(self.state, PcmState::Prepared | PcmState::Running)
            || frames == 0
            || frames > self.buffer_frames - self.delay()
        {
            return Err(());
        }
        self.application_frames = self.application_frames.wrapping_add(frames);
        Ok(())
    }

    pub(crate) fn complete_period(&mut self, frames: u64) {
        if !matches!(self.state, PcmState::Prepared | PcmState::Running) {
            return;
        }
        let completed = self.hardware_frames.wrapping_add(frames);
        if completed > self.application_frames {
            self.state = PcmState::Xrun;
            return;
        }
        self.hardware_frames = completed;
    }

    pub(crate) fn xrun(&mut self) {
        if matches!(self.state, PcmState::Prepared | PcmState::Running) {
            self.state = PcmState::Xrun;
        }
    }

    pub(crate) fn disconnect(&mut self) {
        self.state = PcmState::Disconnected;
    }

    pub(crate) fn delay(&self) -> u64 {
        self.application_frames.saturating_sub(self.hardware_frames)
    }

    pub(crate) fn writable(&self) -> bool {
        matches!(self.state, PcmState::Prepared | PcmState::Running)
            && self.delay() + self.available_min <= self.buffer_frames
    }
}
