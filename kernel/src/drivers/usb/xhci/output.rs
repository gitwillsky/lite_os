//! ALSA PCM adapter：冷路径串行 USB control，热路径只发布预分配的等时请求。
use super::super::audio::{Endpoint, stream::Lifecycle};
use super::control::Setup;
use super::*;
use crate::devices::model::{
    MAX_PCM_PERIOD_BYTES, PCM_RATES, PcmCapabilities, PcmCompletionObserver, PcmConfiguration,
    PcmOutput, PcmOutputError,
};
use alloc::string::String;

pub(super) struct Output {
    pub(in crate::drivers::usb) host: Arc<Host>,
    pub(in crate::drivers::usb) slot: u8,
    pub(in crate::drivers::usb) generation: u64,
    pub(in crate::drivers::usb) capabilities: PcmCapabilities,
    pub(in crate::drivers::usb) name: String,
    pub(in crate::drivers::usb) control: u8,
}
impl Output {
    fn device<'a>(&self, state: &'a mut State) -> Result<&'a mut Device> {
        if self.host.failed.load(Ordering::Acquire) {
            return Err(Error::Disconnected);
        }
        state
            .devices
            .get_mut(usize::from(self.slot))
            .and_then(Option::as_mut)
            .filter(|device| device.generation == self.generation)
            .ok_or(Error::Disconnected)
    }
    fn complete_operation(&self, result: Result<()>) -> core::result::Result<(), PcmOutputError> {
        if matches!(result, Err(Error::Controller(_))) {
            {
                let mut state = self.host.state.lock();
                if let Ok(device) = self.device(&mut state)
                    && let Some(audio) = device.audio.as_mut()
                {
                    audio.disconnect();
                }
            }
            self.host.port_event.signal();
        }
        result.map_err(pcm_error)
    }
    fn configure_pcm(&self, configuration: PcmConfiguration) -> Result<()> {
        if !self.capabilities.supports(configuration)
            || configuration.period_frames == 0
            || configuration.period_bytes() > MAX_PCM_PERIOD_BYTES
        {
            return Err(Error::Unsupported);
        }
        let transaction = self.host.control()?;
        let alt = {
            let mut state = self.host.state.lock();
            let audio = self
                .device(&mut state)?
                .audio
                .as_ref()
                .ok_or(Error::Protocol)?;
            if audio.state != Lifecycle::Unconfigured {
                return Err(Error::Busy);
            }
            *audio
                .alternates
                .iter()
                .find(|alt| {
                    alt.format == configuration.format
                        && PCM_RATES.iter().enumerate().any(|(i, rate)| {
                            *rate == configuration.rate && alt.rates & (1 << i) != 0
                        })
                })
                .ok_or(Error::Unsupported)?
        };
        let index = u16::from(alt.clock) << 8 | u16::from(self.control);
        if alt.clock_controls & 3 == 3 {
            transaction.transfer(
                self.slot,
                Setup {
                    kind: 0x21,
                    request: 1,
                    value: 0x100,
                    index,
                },
                &mut configuration.rate.to_le_bytes(),
            )?;
        }
        let mut rate = [0; 4];
        transaction.transfer(
            self.slot,
            Setup {
                kind: 0xa1,
                request: 1,
                value: 0x100,
                index,
            },
            &mut rate,
        )?;
        if u32::from_le_bytes(rate) != configuration.rate {
            return Err(Error::Unsupported);
        }
        if alt.clock_controls & 12 != 0 {
            let deadline = get_time_ns().saturating_add(OP_TIMEOUT_NS);
            loop {
                let mut valid = [0];
                transaction.transfer(
                    self.slot,
                    Setup {
                        kind: 0xa1,
                        request: 1,
                        value: 0x200,
                        index,
                    },
                    &mut valid,
                )?;
                if valid[0] == 1 {
                    break;
                }
                if valid[0] != 0 || get_time_ns() >= deadline {
                    return Err(Error::Protocol);
                }
            }
        }
        let address = {
            let mut state = self.host.state.lock();
            let device = self.device(&mut state)?;
            let audio = device.audio.as_mut().ok_or(Error::Protocol)?;
            audio.selected = Some(alt);
            audio.configuration = Some(configuration);
            audio.reset()?;
            clear_input(&device.input)?;
            let mut slot = [0u8; 64];
            device.output.read(0, &mut slot[..self.host.stride])?;
            let entries = alt
                .feedback
                .map_or_else(|| alt.output.dci(), |ep| ep.dci().max(alt.output.dci()));
            let word = u32::from_le_bytes(slot[..4].try_into().unwrap());
            slot[..4]
                .copy_from_slice(&(word & !(31 << 27) | u32::from(entries) << 27).to_le_bytes());
            device
                .input
                .write(self.host.stride, &slot[..self.host.stride])?;
            let add = 1 | 1u32 << alt.output.dci() | alt.feedback.map_or(0, |ep| 1u32 << ep.dci());
            device.input.set_word(4, add)?;
            endpoint_context(
                &device.input,
                self.host.stride,
                alt.output,
                audio.output.memory.physical,
            )?;
            if let Some(feedback) = alt.feedback {
                endpoint_context(
                    &device.input,
                    self.host.stride,
                    feedback,
                    audio.feedback.memory.physical,
                )?;
            }
            device.input.physical
        };
        transaction.command(Trb::new(address, 0, 12 << 10 | u32::from(self.slot) << 24))?;
        if let Err(error) = transaction.interface(self.slot, alt.interface, alt.alternate) {
            self.host.fail();
            return Err(error);
        }
        self.device(&mut self.host.state.lock())?
            .audio
            .as_mut()
            .ok_or(Error::Protocol)?
            .state = Lifecycle::Configured;
        Ok(())
    }
    fn stop_pcm(&self) -> Result<()> {
        let transaction = self.host.control()?;
        let alt = {
            let mut state = self.host.state.lock();
            let audio = self
                .device(&mut state)?
                .audio
                .as_mut()
                .ok_or(Error::Protocol)?;
            if audio.state == Lifecycle::Failed {
                return Err(Error::Disconnected);
            }
            if audio.state != Lifecycle::Running {
                return Ok(());
            }
            // 先禁止 deferred refill，再停止硬件；State 锁外等待 Stop Endpoint completion。
            audio.state = Lifecycle::Stopped;
            audio.selected.ok_or(Error::Protocol)?
        };
        for endpoint in [Some(alt.output), alt.feedback].into_iter().flatten() {
            transaction.command(Trb::new(
                0,
                0,
                15 << 10 | u32::from(endpoint.dci()) << 16 | u32::from(self.slot) << 24,
            ))?;
        }
        let (out, feedback) = {
            let mut state = self.host.state.lock();
            let audio = self
                .device(&mut state)?
                .audio
                .as_mut()
                .ok_or(Error::Protocol)?;
            audio.reset()?;
            (audio.output.memory.physical, audio.feedback.memory.physical)
        };
        for (endpoint, address) in [(Some(alt.output), out), (alt.feedback, feedback)] {
            if let Some(endpoint) = endpoint {
                transaction.command(Trb::new(
                    address | 1,
                    0,
                    16 << 10 | u32::from(endpoint.dci()) << 16 | u32::from(self.slot) << 24,
                ))?;
            }
        }
        Ok(())
    }
    fn release_pcm(&self) -> Result<()> {
        self.stop_pcm()?;
        let transaction = self.host.control()?;
        let (alt, address) = {
            let mut state = self.host.state.lock();
            let device = self.device(&mut state)?;
            let audio = device.audio.as_mut().ok_or(Error::Protocol)?;
            if audio.state == Lifecycle::Unconfigured {
                return Ok(());
            }
            let alt = audio.selected.ok_or(Error::Protocol)?;
            clear_input(&device.input)?;
            device.input.set_word(
                0,
                1u32 << alt.output.dci() | alt.feedback.map_or(0, |ep| 1u32 << ep.dci()),
            )?;
            device.input.set_word(4, 1)?;
            let mut slot = [0u8; 64];
            device.output.read(0, &mut slot[..self.host.stride])?;
            let value = u32::from_le_bytes(slot[..4].try_into().unwrap());
            slot[..4].copy_from_slice(&(value & !(31 << 27) | 1 << 27).to_le_bytes());
            device
                .input
                .write(self.host.stride, &slot[..self.host.stride])?;
            (alt, device.input.physical)
        };
        transaction.interface(self.slot, alt.interface, 0)?;
        transaction.command(Trb::new(address, 0, 12 << 10 | u32::from(self.slot) << 24))?;
        let mut state = self.host.state.lock();
        let audio = self
            .device(&mut state)?
            .audio
            .as_mut()
            .ok_or(Error::Protocol)?;
        audio.selected = None;
        audio.configuration = None;
        audio.state = Lifecycle::Unconfigured;
        Ok(())
    }
}
fn clear_input(input: &Dma) -> Result<()> {
    for offset in (0..input.bytes).step_by(64) {
        input.write(offset, &[0; 64])?;
    }
    Ok(())
}
fn endpoint_context(input: &Dma, stride: usize, endpoint: Endpoint, ring: u64) -> Result<()> {
    let offset = (usize::from(endpoint.dci()) + 1) * stride;
    let payload = endpoint.payload() as u32;
    input.set_word(
        offset,
        u32::from(endpoint.interval - 1) << 16 | (payload >> 16) << 24,
    )?;
    let kind = if endpoint.address & 0x80 == 0 { 1 } else { 5 };
    input.set_word(
        offset + 4,
        kind << 3
            | u32::from(endpoint.packet >> 11 & 3) << 8
            | u32::from(endpoint.packet & 0x7ff) << 16,
    )?;
    input.pointer(offset + 8, ring | 1)?;
    input.set_word(offset + 16, payload | payload << 16)?;
    Ok(())
}
fn pcm_error(error: Error) -> PcmOutputError {
    match error {
        Error::Busy => PcmOutputError::WouldBlock,
        Error::Unsupported | Error::Protocol => PcmOutputError::InvalidState,
        _ => PcmOutputError::Device,
    }
}
impl PcmOutput for Output {
    fn name(&self) -> &str {
        &self.name
    }
    fn capabilities(&self) -> PcmCapabilities {
        self.capabilities
    }
    fn set_observer(
        &self,
        observer: Arc<dyn PcmCompletionObserver>,
    ) -> core::result::Result<(), PcmOutputError> {
        (|| -> Result<()> {
            let mut state = self.host.state.lock();
            let audio = self
                .device(&mut state)?
                .audio
                .as_mut()
                .ok_or(Error::Protocol)?;
            if audio.observer.is_some() {
                return Err(Error::Busy);
            }
            audio.observer = Some(observer);
            Ok(())
        })()
        .map_err(pcm_error)
    }
    fn configure(
        &self,
        configuration: PcmConfiguration,
    ) -> core::result::Result<(), PcmOutputError> {
        self.complete_operation(self.configure_pcm(configuration))
    }
    fn release(&self) -> core::result::Result<(), PcmOutputError> {
        self.complete_operation(self.release_pcm())
    }
    fn prepare(&self) -> core::result::Result<(), PcmOutputError> {
        (|| -> Result<()> {
            let mut state = self.host.state.lock();
            let audio = self
                .device(&mut state)?
                .audio
                .as_mut()
                .ok_or(Error::Protocol)?;
            if !matches!(audio.state, Lifecycle::Configured | Lifecycle::Stopped) {
                return Err(Error::Protocol);
            }
            audio.state = Lifecycle::Prepared;
            Ok(())
        })()
        .map_err(pcm_error)
    }
    fn start(&self) -> core::result::Result<(), PcmOutputError> {
        (|| -> Result<()> {
            let mut state = self.host.state.lock();
            self.device(&mut state)?
                .audio
                .as_mut()
                .ok_or(Error::Protocol)?
                .start(&self.host)
        })()
        .map_err(pcm_error)
    }
    fn stop(&self) -> core::result::Result<(), PcmOutputError> {
        self.complete_operation(self.stop_pcm())
    }
    fn submit_period(&self, bytes: &[u8]) -> core::result::Result<(), PcmOutputError> {
        (|| -> Result<()> {
            let mut state = self.host.state.lock();
            self.device(&mut state)?
                .audio
                .as_mut()
                .ok_or(Error::Protocol)?
                .submit(&self.host, bytes)
        })()
        .map_err(pcm_error)
    }
    fn writable(&self) -> bool {
        self.device(&mut self.host.state.lock())
            .ok()
            .and_then(|device| device.audio.as_ref())
            .is_some_and(audio::Stream::writable)
    }
}
