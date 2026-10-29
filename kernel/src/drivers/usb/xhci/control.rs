//! task-only xHCI/EP0 事务；等待时不持 ring 锁，超时后不重用仍可能被硬件访问的 DMA。
use super::*;
use crate::execution::sync::{TaskMutexGuard, WaitResult};

pub(super) struct Control<'a> {
    host: &'a Host,
    _guard: TaskMutexGuard<'a, ()>,
}
#[derive(Clone, Copy)]
pub(in crate::drivers::usb) struct Setup {
    pub(in crate::drivers::usb) kind: u8,
    pub(in crate::drivers::usb) request: u8,
    pub(in crate::drivers::usb) value: u16,
    pub(in crate::drivers::usb) index: u16,
}
impl Host {
    pub(super) fn control(&self) -> Result<Control<'_>> {
        if self.failed.load(Ordering::Acquire) {
            return Err(Error::Disconnected);
        }
        Ok(Control {
            host: self,
            _guard: self.operations.lock().map_err(|_| Error::Memory)?,
        })
    }
}
impl Control<'_> {
    pub(super) fn command(&self, trb: Trb) -> Result<Trb> {
        {
            let mut state = self.host.state.lock();
            if state.command.is_some() {
                return Err(Error::Busy);
            }
            let address = state.commands.push(trb)?;
            state.command = Some(Pending {
                address,
                result: None,
            });
        }
        self.host.doorbell(0, 0)?;
        let result = self.wait(true);
        if result.is_err() && self.host.failed.load(Ordering::Acquire) {
            self.host.fail();
        }
        result
    }
    fn wait(&self, command: bool) -> Result<Trb> {
        let deadline = get_time_ns().saturating_add(OP_TIMEOUT_NS);
        loop {
            if self.host.failed.load(Ordering::Acquire) {
                return Err(Error::Disconnected);
            }
            {
                let mut state = self.host.state.lock();
                let pending = if command {
                    &mut state.command
                } else {
                    &mut state.transfer
                };
                if let Some(result) = pending.as_ref().and_then(|p| p.result) {
                    *pending = None;
                    return result;
                }
            }
            let event = if command {
                &self.host.command_event
            } else {
                &self.host.transfer_event
            };
            if event.wait_until(deadline) != WaitResult::Woken {
                self.host.fail();
                return Err(Error::Timeout);
            }
        }
    }
    pub(super) fn transfer(&self, slot: u8, setup: Setup, bytes: &mut [u8]) -> Result<usize> {
        if bytes.len() > 4096 {
            return Err(Error::Unsupported);
        }
        let input = setup.kind & 0x80 != 0;
        {
            let mut state = self.host.state.lock();
            if state.transfer.is_some() {
                return Err(Error::Busy);
            }
            let device = state
                .devices
                .get_mut(usize::from(slot))
                .and_then(Option::as_mut)
                .ok_or(Error::Disconnected)?;
            if device.ep0.free() < 3 {
                return Err(Error::Busy);
            }
            if !input {
                device.control.write(0, bytes)?;
            }
            let packet = [
                setup.kind,
                setup.request,
                setup.value as u8,
                (setup.value >> 8) as u8,
                setup.index as u8,
                (setup.index >> 8) as u8,
                bytes.len() as u8,
                (bytes.len() >> 8) as u8,
            ];
            let trt = if bytes.is_empty() {
                0
            } else if input {
                3
            } else {
                2
            };
            device.ep0.push(Trb::new(
                u64::from_le_bytes(packet),
                8,
                2 << 10 | 1 << 6 | trt << 16,
            ))?;
            if !bytes.is_empty() {
                device.ep0.push(Trb::new(
                    device.control.physical,
                    bytes.len() as u32,
                    3 << 10 | u32::from(input) << 16,
                ))?;
            }
            let address = device.ep0.push(Trb::new(
                0,
                0,
                4 << 10 | 1 << 5 | u32::from(!input || bytes.is_empty()) << 16,
            ))?;
            state.transfer = Some(Pending {
                address,
                result: None,
            });
        }
        self.host.doorbell(slot, 1)?;
        self.wait(false)?;
        if input {
            let state = self.host.state.lock();
            state.devices[usize::from(slot)]
                .as_ref()
                .ok_or(Error::Disconnected)?
                .control
                .read(0, bytes)?;
        }
        Ok(bytes.len())
    }
    pub(super) fn interface(&self, slot: u8, interface: u8, alternate: u8) -> Result<()> {
        self.transfer(
            slot,
            Setup {
                kind: 1,
                request: 11,
                value: u16::from(alternate),
                index: u16::from(interface),
            },
            &mut [],
        )?;
        Ok(())
    }
    pub(super) fn remove(&self, slot: u8) -> Result<()> {
        self.command(Trb::new(0, 0, 10 << 10 | u32::from(slot) << 24))?;
        let mut state = self.host.state.lock();
        for port in &mut state.ports {
            if *port == PortState::Addressed(slot) {
                *port = PortState::Seen;
            }
        }
        if let Some(mut device) = state.devices[usize::from(slot)].take() {
            if let Some(audio) = &mut device.audio {
                audio.disconnect();
            }
            // Disable Slot completion 之后硬件不再拥有此 context/ring/payload，才允许析构。
            state.dcbaa.pointer(usize::from(slot) * 8, 0)?;
            drop(state);
            drop(device);
        }
        Ok(())
    }
}
