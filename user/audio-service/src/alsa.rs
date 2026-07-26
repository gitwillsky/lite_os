use std::io;

use audio_proto::CHANNELS;
use linux_uapi::alsa::{self, PlaybackPcm};

use crate::mixer::{DEVICE_PERIODS, DeviceError, PERIOD_FRAMES, PlaybackDevice};

/// The sole production playback path: fixed-format Linux ALSA PCM.
pub(crate) struct AlsaDevice {
    pcm: PlaybackPcm,
    // Counts the only four pre-start periods. Without it START would run an
    // empty device buffer and immediately XRUN.
    prefill_remaining: usize,
}

impl AlsaDevice {
    pub(crate) fn open() -> io::Result<Self> {
        if alsa::RATE != audio_proto::SAMPLE_RATE
            || alsa::CHANNELS != CHANNELS
            || alsa::PERIOD_FRAMES != PERIOD_FRAMES
            || alsa::BUFFER_FRAMES != crate::mixer::DEVICE_BUFFER_FRAMES
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ALSA wrapper does not match the system PCM normal form",
            ));
        }
        Ok(Self {
            pcm: PlaybackPcm::open()?,
            prefill_remaining: 0,
        })
    }

    fn classify(error: io::Error) -> DeviceError {
        if error.raw_os_error() == Some(32) {
            DeviceError::Xrun
        } else {
            DeviceError::Fatal
        }
    }
}

impl PlaybackDevice for AlsaDevice {
    fn activate(&mut self) -> Result<(), DeviceError> {
        self.pcm.prepare().map_err(Self::classify)?;
        self.prefill_remaining = DEVICE_PERIODS;
        Ok(())
    }

    fn wait_period(&mut self) -> Result<(), DeviceError> {
        if self.prefill_remaining == 0 {
            self.pcm.wait_period().map_err(Self::classify)
        } else {
            Ok(())
        }
    }

    fn write_period(
        &mut self,
        frames: &[[f32; CHANNELS]; PERIOD_FRAMES],
    ) -> Result<bool, DeviceError> {
        self.pcm.write_period(frames).map_err(Self::classify)?;
        let mut started = false;
        if self.prefill_remaining != 0 {
            self.prefill_remaining -= 1;
            if self.prefill_remaining == 0 {
                self.pcm.start().map_err(Self::classify)?;
                started = true;
            }
        }
        Ok(started)
    }

    fn delay_frames(&mut self) -> Result<u32, DeviceError> {
        self.pcm.delay_frames().map_err(Self::classify)
    }

    fn recover_xrun(&mut self) -> Result<(), DeviceError> {
        self.pcm.recover_xrun().map_err(Self::classify)?;
        self.prefill_remaining = DEVICE_PERIODS;
        Ok(())
    }

    fn stop(&mut self) {
        self.pcm.stop();
        self.prefill_remaining = 0;
    }
}
