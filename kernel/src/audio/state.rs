use crate::drivers::{PCM_BUFFER_FRAMES, PCM_PERIOD_FRAMES};

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
    pub(crate) state: PcmState,
    pub(crate) application_frames: u64,
    pub(crate) hardware_frames: u64,
    pub(crate) available_min: u64,
    pub(crate) start_threshold: u64,
    pub(crate) stop_threshold: u64,
    pub(crate) boundary: u64,
}

impl PcmStateOwner {
    pub(crate) const fn new() -> Self {
        Self {
            state: PcmState::Open,
            application_frames: 0,
            hardware_frames: 0,
            available_min: PCM_PERIOD_FRAMES as u64,
            start_threshold: PCM_BUFFER_FRAMES as u64,
            stop_threshold: PCM_BUFFER_FRAMES as u64,
            boundary: 1 << 31,
        }
    }

    pub(crate) fn configure(&mut self) -> Result<(), ()> {
        if self.state != PcmState::Open {
            return Err(());
        }
        self.state = PcmState::Setup;
        self.application_frames = 0;
        self.hardware_frames = 0;
        Ok(())
    }

    pub(crate) fn set_software(
        &mut self,
        available_min: u64,
        start_threshold: u64,
        stop_threshold: u64,
        boundary: u64,
    ) -> Result<(), ()> {
        if self.state != PcmState::Setup
            || available_min == 0
            || available_min > PCM_BUFFER_FRAMES as u64
            || start_threshold > boundary
            || stop_threshold > boundary
            || boundary < (PCM_BUFFER_FRAMES * 2) as u64
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
        if !matches!(self.state, PcmState::Setup | PcmState::Xrun) {
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

    pub(crate) fn submit_period(&mut self) -> Result<(), ()> {
        if !matches!(self.state, PcmState::Prepared | PcmState::Running)
            || self.delay() + PCM_PERIOD_FRAMES as u64 > PCM_BUFFER_FRAMES as u64
        {
            return Err(());
        }
        self.application_frames = self
            .application_frames
            .wrapping_add(PCM_PERIOD_FRAMES as u64);
        Ok(())
    }

    pub(crate) fn complete_period(&mut self) {
        if !matches!(self.state, PcmState::Prepared | PcmState::Running) {
            return;
        }
        let completed = self.hardware_frames.wrapping_add(PCM_PERIOD_FRAMES as u64);
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
            && self.delay() + PCM_PERIOD_FRAMES as u64 <= PCM_BUFFER_FRAMES as u64
    }
}
