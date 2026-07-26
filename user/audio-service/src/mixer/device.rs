use audio_proto::CHANNELS;

use super::PERIOD_FRAMES;

pub(crate) const DEVICE_PERIODS: usize = 4;
pub(crate) const DEVICE_BUFFER_FRAMES: usize = PERIOD_FRAMES * DEVICE_PERIODS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeviceError {
    Xrun,
    Fatal,
}

/// Physical playback seam; production has exactly one ALSA implementation.
pub(crate) trait PlaybackDevice: Send + 'static {
    fn activate(&mut self) -> Result<(), DeviceError>;
    fn wait_period(&mut self) -> Result<(), DeviceError>;
    /// Submits one period and reports the exact device-start transition.
    fn write_period(
        &mut self,
        frames: &[[f32; CHANNELS]; PERIOD_FRAMES],
    ) -> Result<bool, DeviceError>;
    fn delay_frames(&mut self) -> Result<u32, DeviceError>;
    fn recover_xrun(&mut self) -> Result<(), DeviceError>;
    fn stop(&mut self);
}
