//! xHCI 1.2 单 interrupter、command/control 与周期传输的 DMA owner。
mod control;
mod enumerate;
mod output;
use super::{
    Error, Result, audio,
    dma::{Dma, Ring, Trb},
};
use crate::{
    arch::mmio::MmioBus,
    devices::model::io_completion::CompletionSource,
    execution::{
        interrupt::{InterruptError, InterruptHandler, InterruptVector},
        sync::{TaskEvent, TaskMutex},
        timer::get_time_ns,
    },
};
use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

const EVENT_COUNT: usize = 256;
const PORT_CHANGE: u32 = 0x00fe0000;
const OP_TIMEOUT_NS: u64 = 2_000_000_000;

struct Device {
    generation: u64,
    output: Dma,
    input: Dma,
    ep0: Ring,
    control: Dma,
    audio: Option<audio::Stream>,
}
struct Pending {
    address: u64,
    result: Option<Result<Trb>>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum PortState {
    Empty,
    Seen,
    Addressed(u8),
}
struct State {
    ports: Vec<PortState>,
    commands: Ring,
    events: Dma,
    event_index: usize,
    event_cycle: bool,
    // 保活 controller 永久引用的 DMA；即使硬件失效，也不让未确认停止的 DMA 指向已回收页。
    dcbaa: Dma,
    _erst: Dma,
    _scratchpads: Vec<Dma>,
    _scratchpad_array: Option<Dma>,
    devices: Vec<Option<Device>>,
    next_generation: u64,
    command: Option<Pending>,
    transfer: Option<Pending>,
}
/// 一个 PCI xHCI 控制器；IRQ/deferred、枚举线程与 PCM adapter 共用同一 DMA owner。
///
/// State 锁只串行化有界 ring 操作；operations 是 task-only command/control 事务锁。
/// 两者不嵌套等待。hardirq 只确认硬件并发布 deferred，不取 State 锁。
pub(crate) struct Host {
    bus: usize,
    op: MmioBus,
    runtime: MmioBus,
    doors: MmioBus,
    stride: usize,
    ports: usize,
    state: Mutex<State>,
    operations: TaskMutex<()>,
    command_event: TaskEvent,
    transfer_event: TaskEvent,
    port_event: TaskEvent,
    // OWNER: IRQ/MMIO/timeout 的永久错误状态；Acquire/Release 使所有入口拒绝继续发布 DMA。
    // 缺失时超时请求可与下一次重用的 ring 共享身份。注册的 IRQ 与 completion Arc 保活隔离的 DMA。
    failed: AtomicBool,
}
impl Host {
    pub(super) fn new(function: &crate::drivers::pci::Function, bus: usize) -> Result<Arc<Self>> {
        let bar = function.bars[0].ok_or(Error::Protocol)?;
        let regs = MmioBus::new(crate::platform::map_device_window(bar.address), bar.size)?;
        let length = usize::from(regs.read_u8(0)?);
        let hcs = regs.read_u32(4)?;
        let slots = (hcs & 255) as usize;
        let ports = (hcs >> 24) as usize;
        if slots == 0 || ports == 0 {
            return Err(Error::Protocol);
        }
        let hcc = regs.read_u32(0x10)?;
        let mut extended = ((hcc >> 16) * 4) as usize;
        // BIOS-owned handoff 必须在 reset 前完成；不依赖某个厂商的 xHCI 寄存器布局。
        for _ in 0..64 {
            if extended == 0 {
                break;
            }
            let cap = regs.read_u32(extended)?;
            if cap & 255 == 1 {
                regs.write_u32(extended, cap | (1 << 24))?;
                wait_register(&regs, extended, 1 << 16, 0)?;
                regs.write_u32(extended + 4, 0)?;
            }
            let next = ((cap >> 8 & 255) * 4) as usize;
            if next == 0 {
                break;
            }
            extended = extended.checked_add(next).ok_or(Error::Protocol)?;
        }
        let op = regs.subwindow(length, 0x400 + ports * 0x10)?;
        op.write_u32(0, 0)?;
        wait_register(&op, 4, 1, 1)?;
        op.write_u32(0, 2)?;
        wait_register(&op, 0, 2, 0)?;
        wait_register(&op, 4, 1 << 11, 0)?;
        if op.read_u32(8)? & 1 == 0 {
            return Err(Error::Unsupported);
        }
        let runtime = regs.subwindow((regs.read_u32(0x18)? & !31) as usize, 0x40)?;
        let doors = regs.subwindow((regs.read_u32(0x14)? & !3) as usize, (slots + 1) * 4)?;
        let dcbaa = Dma::allocate(1)?;
        let commands = Ring::new(1)?;
        let events = Dma::allocate(1)?;
        let erst = Dma::allocate(1)?;
        erst.pointer(0, events.physical)?;
        erst.set_word(8, EVENT_COUNT as u32)?;
        let scratch = regs.read_u32(8)?;
        let count = (((scratch >> 21) & 31) << 5 | (scratch >> 27)) as usize;
        let mut scratchpads = Vec::new();
        scratchpads
            .try_reserve_exact(count)
            .map_err(|_| Error::Memory)?;
        let scratchpad_array = if count != 0 {
            let array = Dma::allocate((count * 8).div_ceil(4096))?;
            for index in 0..count {
                let page = Dma::allocate(1)?;
                array.pointer(index * 8, page.physical)?;
                scratchpads.push(page);
            }
            dcbaa.pointer(0, array.physical)?;
            Some(array)
        } else {
            None
        };
        let mut port_states = Vec::new();
        port_states
            .try_reserve_exact(ports)
            .map_err(|_| Error::Memory)?;
        port_states.resize(ports, PortState::Empty);
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(slots + 1)
            .map_err(|_| Error::Memory)?;
        devices.resize_with(slots + 1, || None);
        // 所有 fallible 分配先完成，控制器接收 DMA 地址后由永久注册 Arc 保活。
        let host = Arc::try_new(Self {
            bus,
            op,
            runtime,
            doors,
            stride: if hcc & 4 != 0 { 64 } else { 32 },
            ports,
            state: Mutex::new(State {
                ports: port_states,
                commands,
                events,
                event_index: 0,
                event_cycle: true,
                dcbaa,
                _erst: erst,
                _scratchpads: scratchpads,
                _scratchpad_array: scratchpad_array,
                devices,
                next_generation: 0,
                command: None,
                transfer: None,
            }),
            operations: TaskMutex::new(()),
            command_event: TaskEvent::new(),
            transfer_event: TaskEvent::new(),
            port_event: TaskEvent::new(),
            failed: AtomicBool::new(false),
        })
        .map_err(|_| Error::Memory)?;
        let irq = function.interrupt.ok_or(Error::Unsupported)?;
        crate::devices::model::register_completion_source(host.clone())
            .map_err(|_| Error::Memory)?;
        crate::platform::register_device_interrupt(irq, host.clone(), "xHCI");
        let state = host.state.lock();
        host.op.write_u64(0x30, state.dcbaa.physical)?;
        host.op
            .write_u64(0x18, state.commands.memory.physical | 1)?;
        host.op.write_u32(0x38, slots as u32)?;
        host.runtime.write_u32(0x28, 1)?;
        host.runtime.write_u64(0x30, state._erst.physical)?;
        host.runtime.write_u64(0x38, state.events.physical)?;
        drop(state);
        host.runtime.write_u32(0x24, 1000)?; // 250us IRQ moderation；没有它会在每个 125us packet 中断 CPU。
        host.runtime.write_u32(0x20, 3)?;
        crate::arch::before_mmio_write();
        host.op.write_u32(0, 1 | 4)?;
        wait_register(&host.op, 4, 1, 0)?;
        crate::info!("USB xHCI ready: ports={}, slots={}", ports, slots);
        Ok(host)
    }
    pub(super) fn doorbell(&self, slot: u8, endpoint: u8) -> Result<()> {
        if self.failed.load(Ordering::Acquire) {
            return Err(Error::Disconnected);
        }
        crate::arch::before_mmio_write();
        self.doors
            .write_u32(usize::from(slot) * 4, u32::from(endpoint))?;
        Ok(())
    }
    fn fail(&self) {
        if !self.failed.swap(true, Ordering::AcqRel) {
            let _ = self.op.write_u32(0, 0);
            let mut state = self.state.lock();
            for device in state.devices.iter_mut().flatten() {
                if let Some(audio) = &mut device.audio {
                    audio.disconnect();
                }
            }
            crate::error!("USB xHCI disconnected: controller/transfer failure");
        }
        self.command_event.signal();
        self.transfer_event.signal();
        self.port_event.signal();
    }
}
fn wait_register(bus: &MmioBus, offset: usize, mask: u32, wanted: u32) -> Result<()> {
    let deadline = get_time_ns().saturating_add(OP_TIMEOUT_NS);
    while bus.read_u32(offset)? & mask != wanted {
        if get_time_ns() >= deadline {
            return Err(Error::Timeout);
        }
        core::hint::spin_loop();
    }
    Ok(())
}
impl InterruptHandler for Host {
    fn handle_interrupt(&self, _: InterruptVector) -> core::result::Result<(), InterruptError> {
        let failed = (|| -> Result<bool> {
            let status = self.op.read_u32(4)?;
            self.op.write_u32(4, status & ((1 << 3) | (1 << 4)))?;
            self.runtime.write_u32(0x20, 3)?;
            Ok(status & ((1 << 2) | (1 << 12)) != 0)
        })();
        if !matches!(failed, Ok(false)) {
            self.failed.store(true, Ordering::Release);
        }
        crate::execution::deferred::raise(crate::execution::deferred::DeferredWork::DRIVER_IO);
        Ok(())
    }
}
impl CompletionSource for Host {
    fn dispatch_completions(&self) -> bool {
        if self.failed.load(Ordering::Acquire) {
            // fail() 不能在 IRQ 取 State；在这里完成通知和停止请求。
            let _ = self.op.write_u32(0, 0);
            for device in self.state.lock().devices.iter_mut().flatten() {
                if let Some(audio) = &mut device.audio {
                    audio.disconnect();
                }
            }
            self.command_event.signal();
            self.transfer_event.signal();
            self.port_event.signal();
            return false;
        }
        let result = self.drain();
        match result {
            Ok(more) => more,
            Err(error) => {
                crate::error!("USB xHCI event processing failed: {:?}", error);
                self.fail();
                false
            }
        }
    }
}
impl Host {
    fn drain(&self) -> Result<bool> {
        let mut command = false;
        let mut transfer = false;
        let mut port = false;
        let mut state = self.state.lock();
        let mut count = 0;
        while count < 128 {
            let offset = state.event_index * 16;
            let control = state.events.word(offset + 12)?;
            if control & 1 != u32::from(state.event_cycle) {
                break;
            }
            let event = Trb([
                state.events.word(offset)?,
                state.events.word(offset + 4)?,
                state.events.word(offset + 8)?,
                control,
            ]);
            state.event_index += 1;
            if state.event_index == EVENT_COUNT {
                state.event_index = 0;
                state.event_cycle = !state.event_cycle;
            }
            count += 1;
            match event.kind() {
                33 => {
                    state.commands.complete(event.address())?;
                    let pending = state.command.as_mut().ok_or(Error::Protocol)?;
                    if pending.address != event.address() || pending.result.is_some() {
                        return Err(Error::Protocol);
                    }
                    pending.result = Some(if event.code() == 1 {
                        Ok(event)
                    } else {
                        Err(Error::Controller(event.code() as u8))
                    });
                    command = true;
                }
                32 => {
                    // Stop Endpoint 可在真实在途 TD 上产生 Stopped/Stopped Length Invalid；不是传输失败。
                    if matches!(event.code(), 26..=28) {
                        continue;
                    }
                    if event.endpoint() == 1 {
                        let device = state
                            .devices
                            .get_mut(usize::from(event.slot()))
                            .and_then(Option::as_mut)
                            .ok_or(Error::Protocol)?;
                        device.ep0.complete(event.address())?;
                        let pending = state.transfer.as_mut().ok_or(Error::Protocol)?;
                        if (matches!(event.code(), 1 | 13) && pending.address != event.address())
                            || pending.result.is_some()
                        {
                            return Err(Error::Protocol);
                        }
                        pending.result = Some(if matches!(event.code(), 1 | 13) {
                            Ok(event)
                        } else {
                            Err(Error::Controller(event.code() as u8))
                        });
                        transfer = true;
                    } else {
                        let device = state
                            .devices
                            .get_mut(usize::from(event.slot()))
                            .and_then(Option::as_mut)
                            .ok_or(Error::Protocol)?;
                        if let Some(audio) = device.audio.as_mut()
                            && let Err(error) = audio.complete(self, event)
                        {
                            crate::error!(
                                "USB audio transfer failed: slot={} endpoint={} code={} pointer={:#x} remaining={} error={:?}",
                                event.slot(),
                                event.endpoint(),
                                event.code(),
                                event.address(),
                                event.0[2] & 0xffffff,
                                error,
                            );
                            if matches!(error, Error::Controller(_)) {
                                audio.disconnect();
                                port = true;
                            } else {
                                return Err(error);
                            }
                        }
                    }
                }
                34 => port = true,
                _ => return Err(Error::Protocol),
            }
        }
        self.runtime.write_u64(
            0x38,
            (state.events.physical + (state.event_index * 16) as u64) | 8,
        )?;
        let more =
            state.events.word(state.event_index * 16 + 12)? & 1 == u32::from(state.event_cycle);
        drop(state);
        if command {
            self.command_event.signal();
        }
        if transfer {
            self.transfer_event.signal();
        }
        if port {
            self.port_event.signal();
        }
        Ok(more)
    }
}
