//! 有界 PCM staging、显式反馈与等时 TD；播放热路径不分配、不执行 USB control request。
use super::*;
use crate::devices::model::{
    MAX_PCM_PERIOD_BYTES, PCM_PERIODS, PcmCompletionObserver, PcmConfiguration,
};
use crate::drivers::usb::{
    dma::{Dma, Ring, Trb},
    xhci::Host,
};
use alloc::sync::Arc;
const PACKETS: usize = 128;
const FEEDBACK_PACKETS: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::drivers::usb) enum Lifecycle {
    Unconfigured,
    Configured,
    Prepared,
    Running,
    Stopped,
    Failed,
}
struct Period {
    memory: Dma,
    occupied: bool,
    offset: usize,
}
#[derive(Clone, Copy)]
struct Packet {
    address: u64,
    finishes: Option<usize>,
}
pub(in crate::drivers::usb) struct Stream {
    pub(in crate::drivers::usb) slot: u8,
    pub(in crate::drivers::usb) alternates: Vec<Alternate>,
    pub(in crate::drivers::usb) selected: Option<Alternate>,
    pub(in crate::drivers::usb) configuration: Option<PcmConfiguration>,
    pub(in crate::drivers::usb) state: Lifecycle,
    pub(in crate::drivers::usb) observer: Option<Arc<dyn PcmCompletionObserver>>,
    pub(in crate::drivers::usb) output: Ring,
    pub(in crate::drivers::usb) feedback: Ring,
    periods: Vec<Period>,
    buffers: Vec<Dma>,
    feedback_buffer: Dma,
    packets: [Option<Packet>; PACKETS],
    feedback_heads: [Option<u64>; FEEDBACK_PACKETS],
    packet_head: usize,
    packet_len: usize,
    period_tail: usize,
    schedule: usize,
    feedback_rate: u64,
    phase: u64,
}
impl Stream {
    pub(in crate::drivers::usb) fn new(slot: u8, alternates: Vec<Alternate>) -> Result<Self> {
        let mut periods = Vec::new();
        periods
            .try_reserve_exact(PCM_PERIODS)
            .map_err(|_| Error::Memory)?;
        for _ in 0..PCM_PERIODS {
            periods.push(Period {
                memory: Dma::allocate(MAX_PCM_PERIOD_BYTES.div_ceil(4096).next_power_of_two())?,
                occupied: false,
                offset: 0,
            });
        }
        let mut buffers = Vec::new();
        buffers
            .try_reserve_exact(PACKETS)
            .map_err(|_| Error::Memory)?;
        for _ in 0..PACKETS {
            buffers.push(Dma::allocate(1)?);
        }
        Ok(Self {
            slot,
            alternates,
            selected: None,
            configuration: None,
            state: Lifecycle::Unconfigured,
            observer: None,
            output: Ring::new(1)?,
            feedback: Ring::new(1)?,
            periods,
            buffers,
            feedback_buffer: Dma::allocate(1)?,
            packets: [None; PACKETS],
            feedback_heads: [None; FEEDBACK_PACKETS],
            packet_head: 0,
            packet_len: 0,
            period_tail: 0,
            schedule: 0,
            feedback_rate: 0,
            phase: 0,
        })
    }
    pub(in crate::drivers::usb) fn disconnect(&mut self) {
        if self.state == Lifecycle::Failed {
            return;
        }
        self.state = Lifecycle::Failed;
        if let Some(observer) = &self.observer {
            observer.disconnected();
        }
    }
    pub(in crate::drivers::usb) fn reset(&mut self) -> Result<()> {
        self.output.reset()?;
        self.feedback.reset()?;
        for period in &mut self.periods {
            period.occupied = false;
            period.offset = 0;
        }
        self.packets = [None; PACKETS];
        self.feedback_heads = [None; FEEDBACK_PACKETS];
        self.packet_head = 0;
        self.packet_len = 0;
        self.period_tail = 0;
        self.schedule = 0;
        self.phase = 0;
        let configuration = self.configuration.ok_or(Error::Protocol)?;
        self.feedback_rate = (u64::from(configuration.rate) << 16) / 8000;
        Ok(())
    }
    pub(in crate::drivers::usb) fn writable(&self) -> bool {
        self.state != Lifecycle::Failed && !self.periods[self.period_tail].occupied
    }
    pub(in crate::drivers::usb) fn submit(&mut self, host: &Host, bytes: &[u8]) -> Result<()> {
        let configuration = self.configuration.ok_or(Error::Protocol)?;
        if !matches!(
            self.state,
            Lifecycle::Prepared | Lifecycle::Running | Lifecycle::Stopped
        ) || bytes.len() != configuration.period_bytes()
        {
            return Err(Error::Protocol);
        }
        if !self.writable() {
            return Err(Error::Busy);
        }
        let period = &mut self.periods[self.period_tail];
        period.memory.write(0, bytes)?;
        period.offset = 0;
        period.occupied = true;
        self.period_tail = (self.period_tail + 1) % PCM_PERIODS;
        if self.state == Lifecycle::Running {
            self.fill(host)?;
        }
        Ok(())
    }
    pub(in crate::drivers::usb) fn start(&mut self, host: &Host) -> Result<()> {
        if !matches!(self.state, Lifecycle::Prepared | Lifecycle::Stopped) {
            return Err(Error::Protocol);
        }
        self.state = Lifecycle::Running;
        self.fill_feedback(host)?;
        self.fill(host)
    }
    fn fill(&mut self, host: &Host) -> Result<()> {
        let alt = self.selected.ok_or(Error::Protocol)?;
        let configuration = self.configuration.ok_or(Error::Protocol)?;
        let frame_bytes = configuration.frame_bytes();
        let microframes = 1u64 << (alt.output.interval - 1);
        let mut pushed = false;
        while self.packet_len < PACKETS {
            if !self.periods[self.schedule].occupied
                || self.periods[self.schedule].offset == configuration.period_bytes()
            {
                break;
            }
            let phase = self.phase + self.feedback_rate * microframes;
            let frames = (phase >> 16) as usize;
            let bytes = frames.checked_mul(frame_bytes).ok_or(Error::Protocol)?;
            if bytes > alt.output.payload() {
                return Err(Error::Protocol);
            }
            let current = self.schedule;
            let remaining = configuration.period_bytes() - self.periods[current].offset;
            let next = (current + 1) % PCM_PERIODS;
            if bytes > remaining && (!self.periods[next].occupied || self.periods[next].offset != 0)
            {
                // ALSA period 不是 USB service interval：先保留尾部，下一次 submit 继续拼包。
                // 截短这里会消耗完整时隙却少发 PCM frames；即使应用 ring 未欠载也会周期性断音。
                // 尚未发布 TD，因此 phase、period offset 和完成记账均不能提前推进。
                break;
            }
            let index = (self.packet_head + self.packet_len) % PACKETS;
            let first = bytes.min(remaining);
            // 一个 packet 最多跨越两个 period；stack 只用 3072-byte USB service payload，不放整段 PCM。
            let mut packet = [0u8; 3072];
            self.periods[current]
                .memory
                .read(self.periods[current].offset, &mut packet[..first])?;
            if bytes > first {
                self.periods[next]
                    .memory
                    .read(0, &mut packet[first..bytes])?;
            }
            // UAC Type-I 的 24-in-32 为左对齐，ALSA S24_LE 为低位对齐；仅重排有效位，不量化或重采样。
            if configuration.format == PcmFormat::S24 {
                for sample in packet[..bytes].as_chunks_mut::<4>().0 {
                    let value = u32::from_le_bytes(*sample) << 8;
                    sample.copy_from_slice(&value.to_le_bytes());
                }
            }
            self.buffers[index].write(0, &packet[..bytes])?;
            self.periods[current].offset += first;
            let finishes = if self.periods[current].offset == configuration.period_bytes() {
                self.schedule = next;
                Some(current)
            } else {
                None
            };
            if bytes > first {
                self.periods[next].offset = bytes - first;
            }
            self.phase = phase & 0xffff;
            // SIA 由 host controller 选择连续 service interval；IOC 每 8 个 packet 或 period 边界一次。
            // 每个 TD 只有一个 Isoch TRB；高带宽端点的 TLBPC 按真实 USB transaction 数编码。
            let transactions = bytes
                .div_ceil(usize::from(alt.output.packet & 0x7ff))
                .max(1);
            let ioc = finishes.is_some()
                || (self.packet_len + 1).is_multiple_of(8)
                || self.packet_len + 1 == PACKETS;
            let address = self.output.push(Trb::new(
                self.buffers[index].physical,
                bytes as u32,
                5 << 10 | 1 << 31 | u32::from(ioc) << 5 | ((transactions - 1) as u32) << 16,
            ))?;
            self.packets[index] = Some(Packet { address, finishes });
            self.packet_len += 1;
            pushed = true;
        }
        if pushed {
            host.doorbell(self.slot, alt.output.dci())?;
        }
        Ok(())
    }
    fn fill_feedback(&mut self, host: &Host) -> Result<()> {
        let Some(endpoint) = self.selected.and_then(|alt| alt.feedback) else {
            return Ok(());
        };
        let mut pushed = false;
        for (index, head) in self.feedback_heads.iter_mut().enumerate() {
            if head.is_none() {
                *head = Some(self.feedback.push(Trb::new(
                    self.feedback_buffer.physical + (index * 16) as u64,
                    4,
                    5 << 10 | 1 << 31 | 1 << 5,
                ))?);
                pushed = true;
            }
        }
        if pushed {
            host.doorbell(self.slot, endpoint.dci())?;
        }
        Ok(())
    }
    pub(in crate::drivers::usb) fn complete(&mut self, host: &Host, event: Trb) -> Result<()> {
        if self.state == Lifecycle::Failed {
            return Ok(());
        }
        let alt = self.selected.ok_or(Error::Protocol)?;
        if matches!(event.code(), 14 | 15) {
            // xHCI 1.2 4.10.3.1：空等时 ring 的通知不完成任何 TD，不能把 pointer 当作
            // 已提交 TRB 回收；QEMU 在这里填写下一个 dequeue 地址，而不是零。
            if event.endpoint() != alt.output.dci()
                && !alt.feedback.is_some_and(|ep| ep.dci() == event.endpoint())
            {
                return Err(Error::Protocol);
            }
            if self.state == Lifecycle::Running
                && event.endpoint() == alt.output.dci()
                && let Some(observer) = &self.observer
            {
                observer.xrun();
            }
            return Ok(());
        }
        if !matches!(event.code(), 1 | 13) {
            return Err(Error::Controller(event.code() as u8));
        }
        if alt.feedback.is_some_and(|ep| ep.dci() == event.endpoint()) {
            self.feedback.complete(event.address())?;
            let index = self
                .feedback_heads
                .iter()
                .position(|head| *head == Some(event.address()))
                .ok_or(Error::Protocol)?;
            let remaining = event.0[2] & 0xffffff;
            if remaining > 4 {
                return Err(Error::Protocol);
            }
            // 1. USB 转发预缓冲与设备启动期间可返回空/短反馈包；该 TD 已完成，仍须回收并续投。
            // 2. 仅完整且合理的 16.16 反馈更新速率，避免读到 DMA slot 上次留下的数据。
            // 3. 缺少有效反馈时保留上次速率（reset 时为协商的 nominal），不把单包缺失升级为断卡。
            if remaining == 0 {
                let value = u64::from(self.feedback_buffer.word(index * 16)?);
                let nominal =
                    (u64::from(self.configuration.ok_or(Error::Protocol)?.rate) << 16) / 8000;
                if value != 0 && value.abs_diff(nominal) <= nominal / 16 {
                    self.feedback_rate = value;
                }
            }
            self.feedback_heads[index] = None;
            if self.state == Lifecycle::Running {
                self.fill_feedback(host)?;
            }
        } else if event.endpoint() == alt.output.dci() {
            if event.0[2] & 0xffffff != 0 {
                return Err(Error::Protocol);
            }
            self.output.complete(event.address())?;
            let mut found = false;
            while self.packet_len != 0 {
                let packet = self.packets[self.packet_head]
                    .take()
                    .ok_or(Error::Protocol)?;
                self.packet_head = (self.packet_head + 1) % PACKETS;
                self.packet_len -= 1;
                if let Some(index) = packet.finishes {
                    self.periods[index].occupied = false;
                    if let Some(observer) = &self.observer {
                        observer.period_completed(
                            self.configuration.ok_or(Error::Protocol)?.period_frames,
                        );
                    }
                }
                if packet.address == event.address() {
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(Error::Protocol);
            }
            if self.state == Lifecycle::Running {
                self.fill(host)?;
            }
        } else {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}
