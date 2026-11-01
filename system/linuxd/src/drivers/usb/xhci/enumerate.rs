//! root-port 热插拔、标准设备请求与 UAC2 时钟发现；由独立内核线程执行可睡眠事务。
use super::control::{Control, Setup};
use super::output::Output;
use super::*;
use alloc::format;

impl Host {
    /// 运行控制器设备枚举线程；IRQ 发布端口事件，空闲时睡眠，不轮询桌面线程。
    /// # Returns
    /// 控制器不可恢复失败后返回；其注册 Arc 继续隔离可能仍被硬件访问的 DMA。
    pub(crate) fn run(self: Arc<Self>) {
        loop {
            if self.failed.load(Ordering::Acquire) {
                return;
            }
            for port in 1..=self.ports {
                if let Err(error) = self.scan_port(port) {
                    crate::warn!("USB port {} enumeration failed: {:?}", port, error);
                    if matches!(error, Error::Timeout | Error::Disconnected) {
                        self.fail();
                        return;
                    }
                }
            }
            self.port_event.wait_idle();
        }
    }
    fn scan_port(self: &Arc<Self>, port: usize) -> Result<()> {
        let control = self.control()?;
        let offset = 0x400 + (port - 1) * 16;
        let status = self.op.read_u32(offset)?;
        let previous = self.state.lock().ports[port - 1];
        let failed = if let PortState::Addressed(slot) = previous {
            self.state.lock().devices[usize::from(slot)]
                .as_ref()
                .and_then(|d| d.audio.as_ref())
                .is_some_and(|audio| audio.state == audio::stream::Lifecycle::Failed)
        } else {
            false
        };
        if !failed && previous != PortState::Empty && status & 1 != 0 && status & (1 << 17) == 0 {
            return Ok(());
        }
        if let PortState::Addressed(slot) = previous {
            control.remove(slot)?;
        }
        self.state.lock().ports[port - 1] = if status & 1 != 0 {
            PortState::Seen
        } else {
            PortState::Empty
        };
        self.op
            .write_u32(offset, (status & 0x0e00) | (status & PORT_CHANGE))?;
        if status & 1 == 0 || failed {
            return Ok(());
        }
        // USB2 7.1.7.3 的稳定连接去抖；非音频设备同样只处理一次，防止 reset 事件自激重枚举。
        let mut stable = get_time_ns().saturating_add(100_000_000);
        loop {
            let value = self.op.read_u32(offset)?;
            if value & 1 == 0 {
                return Ok(());
            }
            if value & (1 << 17) != 0 {
                self.op
                    .write_u32(offset, (value & 0x0e00) | (value & PORT_CHANGE))?;
                stable = get_time_ns().saturating_add(100_000_000);
            }
            match self.port_event.wait_until(stable) {
                crate::execution::sync::WaitResult::Woken => {}
                crate::execution::sync::WaitResult::TimedOut => break,
                _ => return Err(Error::Memory),
            }
        }
        self.op.write_u32(offset, (status & 0x0e00) | (1 << 4))?;
        let deadline = get_time_ns().saturating_add(OP_TIMEOUT_NS);
        loop {
            let value = self.op.read_u32(offset)?;
            if value & 1 == 0 {
                return Ok(());
            }
            if value & 2 != 0 && value & 16 == 0 {
                break;
            }
            if self.port_event.wait_until(deadline) != crate::execution::sync::WaitResult::Woken {
                return Err(Error::Timeout);
            }
        }
        let status = self.op.read_u32(offset)?;
        self.op
            .write_u32(offset, (status & 0x0e00) | (status & PORT_CHANGE))?;
        let recovery = get_time_ns().saturating_add(10_000_000);
        while self.port_event.wait_until(recovery) == crate::execution::sync::WaitResult::Woken {}
        // 当前 UAC2 数据/反馈 codec 对应 USB2 high-speed（125us，16.16 feedback）。
        // FS/LS 需要 TT/10.14，SuperSpeed 需要 companion/extended burst，不能套用 HS 布局。
        if status >> 10 & 15 != 3 {
            return Err(Error::Unsupported);
        }
        let slot = control.command(Trb::new(0, 0, 9 << 10))?.slot();
        self.state.lock().ports[port - 1] = PortState::Addressed(slot);
        let result = control.enumerate(self, slot, port);
        if result.is_err() && control.remove(slot).is_err() {
            self.fail();
        }
        result
    }
}
impl Control<'_> {
    fn enumerate(&self, host: &Arc<Host>, slot: u8, port: usize) -> Result<()> {
        let generation = {
            let mut state = host.state.lock();
            state.next_generation = state
                .next_generation
                .checked_add(1)
                .ok_or(Error::Protocol)?;
            state.next_generation
        };
        let device = Device {
            generation,
            output: Dma::allocate(1)?,
            input: Dma::allocate(1)?,
            ep0: Ring::new(1)?,
            control: Dma::allocate(1)?,
            audio: None,
        };
        device.input.set_word(4, 3)?;
        device.input.set_word(host.stride, 3 << 20 | 1 << 27)?;
        device
            .input
            .set_word(host.stride + 4, (port as u32) << 16)?;
        device
            .input
            .set_word(host.stride * 2 + 4, 3 << 1 | 4 << 3 | 64 << 16)?;
        device
            .input
            .pointer(host.stride * 2 + 8, device.ep0.memory.physical | 1)?;
        device.input.set_word(host.stride * 2 + 16, 8)?;
        let address = device.input.physical;
        {
            let mut state = host.state.lock();
            if slot == 0
                || usize::from(slot) >= state.devices.len()
                || state.devices[usize::from(slot)].is_some()
            {
                return Err(Error::Protocol);
            }
            state
                .dcbaa
                .pointer(usize::from(slot) * 8, device.output.physical)?;
            state.devices[usize::from(slot)] = Some(device);
        }
        self.command(Trb::new(address, 0, 11 << 10 | u32::from(slot) << 24))?;
        let mut descriptor = [0u8; 18];
        self.transfer(
            slot,
            Setup {
                kind: 0x80,
                request: 6,
                value: 0x100,
                index: 0,
            },
            &mut descriptor,
        )?;
        if descriptor[0] != 18 || descriptor[1] != 1 || descriptor[7] != 64 {
            return Err(Error::Protocol);
        }
        let mut chosen = None;
        for configuration in 0..descriptor[17] {
            let mut header = [0u8; 9];
            let setup = Setup {
                kind: 0x80,
                request: 6,
                value: 0x200 | u16::from(configuration),
                index: 0,
            };
            self.transfer(slot, setup, &mut header)?;
            let length = usize::from(u16::from_le_bytes([header[2], header[3]]));
            if !(9..=4096).contains(&length) {
                continue;
            }
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(length).map_err(|_| Error::Memory)?;
            bytes.resize(length, 0);
            self.transfer(slot, setup, &mut bytes)?;
            match audio::parse(&bytes) {
                Ok(config) => {
                    chosen = Some((bytes, config));
                    break;
                }
                Err(Error::Unsupported) => {}
                Err(error) => return Err(error),
            }
        }
        let (bytes, mut config) = chosen.ok_or(Error::Unsupported)?;
        self.transfer(
            slot,
            Setup {
                kind: 0,
                request: 9,
                value: u16::from(config.value),
                index: 0,
            },
            &mut [],
        )?;
        for alt in &mut config.alternates {
            let terminal = entity(&bytes, config.control, alt.terminal)?;
            if terminal.len() < 17
                || terminal[2] != 2
                || terminal[4..6] != [1, 1]
                || terminal[8] != 2
            {
                return Err(Error::Unsupported);
            }
            let mut clock = terminal[7];
            for _ in 0..16 {
                let d = entity(&bytes, config.control, clock)?;
                match d[2] {
                    10 if d.len() >= 8 => {
                        if !matches!(d[5] & 3, 1 | 3) {
                            return Err(Error::Unsupported);
                        }
                        alt.clock = clock;
                        alt.clock_controls = d[5];
                        break;
                    }
                    11 if d.len() >= 7 => {
                        let mut pin = [0u8; 1];
                        self.transfer(
                            slot,
                            Setup {
                                kind: 0xa1,
                                request: 1,
                                value: 0x100,
                                index: u16::from(clock) << 8 | u16::from(config.control),
                            },
                            &mut pin,
                        )?;
                        if pin[0] == 0 || pin[0] > d[4] {
                            return Err(Error::Protocol);
                        }
                        clock = *d.get(4 + usize::from(pin[0])).ok_or(Error::Protocol)?;
                    }
                    _ => return Err(Error::Unsupported),
                }
            }
            if alt.clock == 0 {
                return Err(Error::Protocol);
            }
            let index = u16::from(alt.clock) << 8 | u16::from(config.control);
            let mut count = [0u8; 2];
            self.transfer(
                slot,
                Setup {
                    kind: 0xa1,
                    request: 2,
                    value: 0x100,
                    index,
                },
                &mut count,
            )?;
            let length = 2 + usize::from(u16::from_le_bytes(count)) * 12;
            if length > 4096 {
                return Err(Error::Unsupported);
            }
            let mut ranges = Vec::new();
            ranges
                .try_reserve_exact(length)
                .map_err(|_| Error::Memory)?;
            ranges.resize(length, 0);
            self.transfer(
                slot,
                Setup {
                    kind: 0xa1,
                    request: 2,
                    value: 0x100,
                    index,
                },
                &mut ranges,
            )?;
            alt.rates = audio::rate_mask(&ranges, *alt)?;
        }
        let caps = audio::capabilities(&config.alternates);
        if caps.rates.iter().all(|mask| *mask == 0) {
            return Err(Error::Unsupported);
        }
        let name = format!(
            "USB DAC {:04x}:{:04x}",
            u16::from_le_bytes([descriptor[8], descriptor[9]]),
            u16::from_le_bytes([descriptor[10], descriptor[11]])
        );
        let output = Arc::try_new(Output {
            identity: crate::devices::model::UsbIdentity {
                bus: host.bus,
                port,
                vendor: u16::from_le_bytes([descriptor[8], descriptor[9]]),
                product: u16::from_le_bytes([descriptor[10], descriptor[11]]),
            },
            host: host.clone(),
            slot,
            generation,
            capabilities: caps,
            name,
            control: config.control,
        })
        .map_err(|_| Error::Memory)?;
        let audio = audio::Stream::new(slot, config.alternates)?;
        host.state.lock().devices[usize::from(slot)]
            .as_mut()
            .ok_or(Error::Disconnected)?
            .audio = Some(audio);
        crate::devices::model::register_usb_device(output.clone()).map_err(|_| Error::Memory)?;
        crate::devices::model::register_pcm_output(output).map_err(|_| Error::Memory)?;
        crate::info!(
            "USB Audio 2 playback registered: port={}, slot={}",
            port,
            slot
        );
        crate::execution::deferred::raise(crate::execution::deferred::DeferredWork::DRIVER_IO);
        Ok(())
    }
}
fn entity(bytes: &[u8], control: u8, id: u8) -> Result<&[u8]> {
    let mut ac = false;
    for d in audio::descriptors(bytes)? {
        if d[1] == 4 {
            ac = d.len() >= 9 && d[2] == control && d[5..8] == [1, 1, 0x20];
        }
        if ac && d[1] == 0x24 && d.len() >= 4 && d[3] == id {
            return Ok(d);
        }
    }
    Err(Error::Protocol)
}
