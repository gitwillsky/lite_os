//! @description VirtIO Sound device ID 25 的单 playback-stream adapter。

use alloc::{sync::Arc, vec::Vec};
use spin::{Mutex, Once};

use super::{
    InterruptError, InterruptHandler, InterruptVector, PcmCompletionObserver, PcmOutput,
    PcmOutputError, VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1,
    VirtIODevice,
    audio_output::{PCM_BUFFER_BYTES, PCM_CHANNELS, PCM_PERIOD_BYTES, PCM_PERIODS},
    virtio_completion_irq::VirtIoCompletionIrq,
    virtio_queue::{DmaBuffer, VirtQueue},
};

#[path = "virtio_sound/wire.rs"]
mod wire;
use wire::*;
#[path = "virtio_sound/lifecycle.rs"]
mod lifecycle;
use lifecycle::{DeviceState, polled_control_ack_requires_deferred, unique_slot_for};

const QUEUE_SIZE: u16 = 64;
const EVENT_SLOTS: usize = 8;
const CONTROL_SPIN_LIMIT: usize = 1 << 24;

struct EventSlot {
    buffer: DmaBuffer<EVENT_BYTES>,
    head: u16,
}

struct TxSlot {
    xfer: DmaBuffer<XFER_BYTES>,
    frames: DmaBuffer<PCM_PERIOD_BYTES>,
    status: DmaBuffer<STATUS_BYTES>,
    head: Option<u16>,
}

struct SoundQueues {
    control: VirtQueue,
    event: VirtQueue,
    tx: VirtQueue,
    _rx: VirtQueue,
    control_request: DmaBuffer<CONTROL_REQUEST_BYTES>,
    control_response: DmaBuffer<CONTROL_RESPONSE_BYTES>,
    events: Vec<EventSlot>,
    tx_slots: Vec<TxSlot>,
    state: DeviceState,
}

/// Modern VirtIO-MMIO Sound adapter；所有 DMA owner 在 reset 前保持存活。
pub(crate) struct VirtIOSoundDevice {
    device: VirtIODevice,
    queues: Mutex<SoundQueues>,
    // OWNER: serializes the complete check→control command→state publication transaction.
    // Without it, concurrent ALSA ioctls can both validate stale lifecycle state.
    command: Mutex<()>,
    observer: Once<Arc<dyn PcmCompletionObserver>>,
    completion_irq: VirtIoCompletionIrq,
}

impl VirtIOSoundDevice {
    /// @description 识别 ID25、配置四个规范队列并验证唯一 output stream 能力。
    /// @param base_addr platform 已映射的 DTB VirtIO-MMIO base。
    /// @return 完整初始化但尚未配置 PCM 参数的 adapter。
    /// @errors feature、queue、DMA、stream 数或固定 PCM 能力不满足时返回 `None`。
    pub(crate) fn new(base_addr: usize) -> Option<Arc<Self>> {
        let mut device = VirtIODevice::new(base_addr, 0x1000).ok()?;
        if device.device_id() != 25 {
            return None;
        }
        device.initialize().ok()?;
        if device.device_features().ok()? & VIRTIO_F_VERSION_1 == 0 {
            return None;
        }
        device.set_driver_features(VIRTIO_F_VERSION_1).ok()?;
        let status = device.get_status().ok()?;
        device
            .set_status(status | VIRTIO_CONFIG_S_FEATURES_OK)
            .ok()?;
        if device.get_status().ok()? & VIRTIO_CONFIG_S_FEATURES_OK == 0
            || device.read_config_u32(4).ok()? != 1
        {
            return None;
        }

        let queues = [
            Self::new_queue(&device, CONTROL_QUEUE, 4)?,
            Self::new_queue(&device, EVENT_QUEUE, (EVENT_SLOTS * 2) as u16)?,
            Self::new_queue(&device, TX_QUEUE, (PCM_PERIODS * 6) as u16)?,
            Self::new_queue(&device, RX_QUEUE, 1)?,
        ];
        let [control, event, tx, rx] = queues;
        let mut events = Vec::new();
        events.try_reserve_exact(EVENT_SLOTS).ok()?;
        for _ in 0..EVENT_SLOTS {
            events.push(EventSlot {
                buffer: DmaBuffer::try_zeroed().ok()?,
                head: 0,
            });
        }
        let mut tx_slots = Vec::new();
        tx_slots.try_reserve_exact(PCM_PERIODS).ok()?;
        for _ in 0..PCM_PERIODS {
            let mut xfer = DmaBuffer::try_zeroed().ok()?;
            write_u32(xfer.as_mut_slice(), 0, 0)?;
            tx_slots.push(TxSlot {
                xfer,
                frames: DmaBuffer::try_zeroed().ok()?,
                status: DmaBuffer::try_zeroed().ok()?,
                head: None,
            });
        }
        let mut owner = SoundQueues {
            control,
            event,
            tx,
            _rx: rx,
            control_request: DmaBuffer::try_zeroed().ok()?,
            control_response: DmaBuffer::try_zeroed().ok()?,
            events,
            tx_slots,
            state: DeviceState::Setup,
        };
        Self::populate_events(&mut owner)?;
        device.notify_queue(EVENT_QUEUE).ok()?;
        let status = device.get_status().ok()?;
        device.set_status(status | VIRTIO_CONFIG_S_DRIVER_OK).ok()?;
        let adapter = Arc::try_new(Self {
            device,
            queues: Mutex::new(owner),
            command: Mutex::new(()),
            observer: Once::new(),
            completion_irq: VirtIoCompletionIrq::new(),
        })
        .ok()?;
        adapter.query_and_validate_stream()?;
        crate::info!("[Audio] VirtIO Sound capability ready");
        Some(adapter)
    }

    fn new_queue(device: &VirtIODevice, index: u32, minimum: u16) -> Option<VirtQueue> {
        let maximum = device.queue_max_size(index).ok()?;
        if maximum < minimum {
            return None;
        }
        let size = maximum.min(QUEUE_SIZE);
        let queue = VirtQueue::new(size)?;
        device
            .configure_queue(index, size, queue.addresses())
            .ok()?;
        Some(queue)
    }

    fn populate_events(owner: &mut SoundQueues) -> Option<()> {
        for slot in &mut owner.events {
            let head = owner.event.add_dma(&[slot.buffer.writable_all()]).ok()?;
            owner.event.add_to_avail(head);
            slot.head = head;
        }
        Some(())
    }

    fn query_and_validate_stream(&self) -> Option<()> {
        {
            let mut owner = self.queues.lock();
            owner.control_request.fill(0);
            write_u32(owner.control_request.as_mut_slice(), 0, R_PCM_INFO)?;
            write_u32(owner.control_request.as_mut_slice(), 4, 0)?;
            write_u32(owner.control_request.as_mut_slice(), 8, 1)?;
            write_u32(
                owner.control_request.as_mut_slice(),
                12,
                PCM_INFO_BYTES as u32,
            )?;
        }
        self.execute_control(16, CONTROL_RESPONSE_BYTES)?;
        let owner = self.queues.lock();
        let response = owner.control_response.as_slice();
        (read_u32(response, 0)? == S_OK
            && read_u64(response, 12)? & (1 << PCM_FMT_FLOAT) != 0
            && read_u64(response, 20)? & (1 << PCM_RATE_48000) != 0
            && response[28] == D_OUTPUT
            && response[29] <= PCM_CHANNELS
            && response[30] >= PCM_CHANNELS)
            .then_some(())
    }

    fn execute_control(&self, request_bytes: usize, response_bytes: usize) -> Option<()> {
        let result = self.execute_control_inner(request_bytes, response_bytes);
        if result.is_none() {
            self.fail();
        }
        result
    }

    fn execute_control_inner(&self, request_bytes: usize, response_bytes: usize) -> Option<()> {
        let mut owner = self.queues.lock();
        owner.control_response.fill(0);
        let head = {
            let SoundQueues {
                control,
                control_request,
                control_response,
                ..
            } = &mut *owner;
            let request = control_request.readable(0..request_bytes).ok()?;
            let response = control_response.writable_all();
            control.add_dma(&[request, response]).ok()?
        };
        owner.control.add_to_avail(head);
        self.device.notify_queue(CONTROL_QUEUE).ok()?;
        let mut completed = false;
        for _ in 0..CONTROL_SPIN_LIMIT {
            match owner.control.used() {
                Ok(Some(completion))
                    if completion.head() == head
                        && completion.length() as usize == response_bytes =>
                {
                    owner.control.recycle_used(completion).ok()?;
                    completed = true;
                    break;
                }
                Ok(Some(_)) | Err(()) => return None,
                Ok(None) => core::hint::spin_loop(),
            }
        }
        if !completed {
            return None;
        }
        let status = self.device.interrupt_status().ok()?;
        if status != 0 {
            self.device.interrupt_ack(status).ok()?;
            if polled_control_ack_requires_deferred(status) {
                crate::cpu::raise_deferred(crate::cpu::DeferredWork::DriverIo);
            }
        }
        (read_u32(owner.control_response.as_slice(), 0)? == S_OK).then_some(())
    }

    fn pcm_command(&self, command: u32) -> Result<(), PcmOutputError> {
        {
            let mut owner = self.queues.lock();
            owner.control_request.fill(0);
            write_u32(owner.control_request.as_mut_slice(), 0, command)
                .ok_or(PcmOutputError::Device)?;
            write_u32(owner.control_request.as_mut_slice(), 4, 0).ok_or(PcmOutputError::Device)?;
        }
        self.execute_control(8, 4).ok_or(PcmOutputError::Device)
    }

    fn fail(&self) {
        let observer = self.observer.get().cloned();
        let first = {
            let mut owner = self.queues.lock();
            owner.state.fail()
        };
        if first {
            let _ = self.device.reset();
            if let Some(observer) = observer {
                observer.disconnected();
            }
        }
    }

    fn reclaim(&self) -> bool {
        if self.completion_irq.take_transport_error() {
            self.fail();
            return false;
        }
        let mut completed = 0usize;
        let mut saw_xrun = false;
        let mut reposted_event = false;
        let mut failed = false;
        let backlog = {
            let mut owner = self.queues.lock();
            for _ in 0..QUEUE_SIZE {
                let completion = match owner.tx.used() {
                    Ok(Some(completion)) => completion,
                    Ok(None) => break,
                    Err(()) => {
                        failed = true;
                        break;
                    }
                };
                let head = completion.head();
                let Some(index) = unique_slot_for(head, owner.tx_slots.len(), |index| {
                    owner.tx_slots[index].head
                }) else {
                    failed = true;
                    break;
                };
                if completion.length() as usize != STATUS_BYTES
                    || read_u32(owner.tx_slots[index].status.as_slice(), 0) != Some(S_OK)
                    || owner.tx.recycle_used(completion).is_err()
                {
                    failed = true;
                    break;
                }
                owner.tx_slots[index].head = None;
                completed += 1;
            }
            for _ in 0..QUEUE_SIZE {
                let completion = match owner.event.used() {
                    Ok(Some(completion)) => completion,
                    Ok(None) => break,
                    Err(()) => {
                        failed = true;
                        break;
                    }
                };
                let head = completion.head();
                let Some(index) = owner.events.iter().position(|slot| slot.head == head) else {
                    failed = true;
                    break;
                };
                if completion.length() as usize != EVENT_BYTES {
                    failed = true;
                    break;
                }
                let event = read_u32(owner.events[index].buffer.as_slice(), 0);
                if event == Some(EVT_PCM_XRUN) {
                    if read_u32(owner.events[index].buffer.as_slice(), 4) != Some(0) {
                        failed = true;
                        break;
                    }
                    saw_xrun = true;
                }
                if owner.event.recycle_used(completion).is_err() {
                    failed = true;
                    break;
                }
                let SoundQueues { event, events, .. } = &mut *owner;
                let buffer = events[index].buffer.writable_all();
                let Ok(new_head) = event.add_dma(&[buffer]) else {
                    failed = true;
                    break;
                };
                event.add_to_avail(new_head);
                events[index].head = new_head;
                reposted_event = true;
            }
            owner.tx.has_used() || owner.event.has_used()
        };
        if failed {
            self.fail();
            return false;
        }
        if reposted_event && self.device.notify_queue(EVENT_QUEUE).is_err() {
            self.fail();
            return false;
        }
        if (completed != 0 || saw_xrun)
            && let Some(observer) = self.observer.get()
        {
            for _ in 0..completed {
                observer.period_completed(super::audio_output::PCM_PERIOD_FRAMES);
            }
            if saw_xrun {
                observer.xrun();
            }
        }
        backlog
    }

    pub(crate) fn irq_handler_for(self: &Arc<Self>) -> Arc<dyn InterruptHandler> {
        Arc::try_new(VirtIOSoundIrqHandler {
            device: self.clone(),
        })
        .expect("VirtIO Sound IRQ handler allocation failed")
    }
}

impl PcmOutput for VirtIOSoundDevice {
    fn set_observer(&self, observer: Arc<dyn PcmCompletionObserver>) -> Result<(), PcmOutputError> {
        if self.observer.get().is_some() || self.queues.lock().state == DeviceState::Failed {
            return Err(PcmOutputError::Device);
        }
        self.observer.call_once(|| observer);
        Ok(())
    }

    fn configure(&self) -> Result<(), PcmOutputError> {
        let _command = self.command.lock();
        let mut owner = self.queues.lock();
        let next = owner
            .state
            .after_configure()
            .ok_or(PcmOutputError::InvalidState)?;
        owner.control_request.fill(0);
        write_u32(owner.control_request.as_mut_slice(), 0, R_PCM_SET_PARAMS)
            .ok_or(PcmOutputError::Device)?;
        write_u32(owner.control_request.as_mut_slice(), 4, 0).ok_or(PcmOutputError::Device)?;
        write_u32(
            owner.control_request.as_mut_slice(),
            8,
            PCM_BUFFER_BYTES as u32,
        )
        .ok_or(PcmOutputError::Device)?;
        write_u32(
            owner.control_request.as_mut_slice(),
            12,
            PCM_PERIOD_BYTES as u32,
        )
        .ok_or(PcmOutputError::Device)?;
        owner.control_request[20] = PCM_CHANNELS;
        owner.control_request[21] = PCM_FMT_FLOAT;
        owner.control_request[22] = PCM_RATE_48000;
        drop(owner);
        self.execute_control(24, 4).ok_or(PcmOutputError::Device)?;
        self.queues.lock().state = next;
        Ok(())
    }

    fn prepare(&self) -> Result<(), PcmOutputError> {
        let _command = self.command.lock();
        let state = self.queues.lock().state;
        let next = state.after_prepare().ok_or(PcmOutputError::InvalidState)?;
        self.pcm_command(R_PCM_PREPARE)?;
        self.queues.lock().state = next;
        Ok(())
    }

    fn start(&self) -> Result<(), PcmOutputError> {
        let _command = self.command.lock();
        let next = self
            .queues
            .lock()
            .state
            .after_start()
            .ok_or(PcmOutputError::InvalidState)?;
        self.pcm_command(R_PCM_START)?;
        self.queues.lock().state = next;
        Ok(())
    }

    fn stop(&self) -> Result<(), PcmOutputError> {
        let _command = self.command.lock();
        let next = self
            .queues
            .lock()
            .state
            .after_stop()
            .ok_or(PcmOutputError::InvalidState)?;
        self.pcm_command(R_PCM_STOP)?;
        self.queues.lock().state = next;
        Ok(())
    }

    fn release(&self) -> Result<(), PcmOutputError> {
        let _command = self.command.lock();
        let state = self.queues.lock().state;
        let next = state.after_release().ok_or(PcmOutputError::InvalidState)?;
        self.pcm_command(R_PCM_RELEASE)?;
        self.queues.lock().state = next;
        Ok(())
    }

    fn submit_period(&self, bytes: &[u8]) -> Result<(), PcmOutputError> {
        if bytes.len() != PCM_PERIOD_BYTES {
            return Err(PcmOutputError::InvalidState);
        }
        self.reclaim();
        let mut owner = self.queues.lock();
        if owner.state == DeviceState::Failed {
            return Err(PcmOutputError::Device);
        }
        if !matches!(owner.state, DeviceState::Prepared | DeviceState::Running) {
            return Err(PcmOutputError::InvalidState);
        }
        let Some(index) = owner.tx_slots.iter().position(|slot| slot.head.is_none()) else {
            return Err(PcmOutputError::WouldBlock);
        };
        owner.tx_slots[index].frames.copy_from_slice(bytes);
        owner.tx_slots[index].status.fill(0);
        let head = {
            let SoundQueues { tx, tx_slots, .. } = &mut *owner;
            let slot = &tx_slots[index];
            tx.add_dma(&[
                slot.xfer.readable_all(),
                slot.frames.readable_all(),
                slot.status.writable_all(),
            ])
            .map_err(|_| PcmOutputError::Device)?
        };
        owner.tx_slots[index].head = Some(head);
        owner.tx.add_to_avail(head);
        drop(owner);
        if self.device.notify_queue(TX_QUEUE).is_err() {
            self.fail();
            return Err(PcmOutputError::Device);
        }
        Ok(())
    }

    fn writable(&self) -> bool {
        self.reclaim();
        let owner = self.queues.lock();
        owner.state != DeviceState::Failed && owner.tx_slots.iter().any(|slot| slot.head.is_none())
    }
}

impl Drop for VirtIOSoundDevice {
    fn drop(&mut self) {
        // Reset 是释放 control/event/tx/rx DMA owner 前的唯一 device revocation barrier。
        let _ = self.device.reset();
    }
}

struct VirtIOSoundIrqHandler {
    device: Arc<VirtIOSoundDevice>,
}

impl InterruptHandler for VirtIOSoundIrqHandler {
    fn handle_interrupt(&self, _vector: InterruptVector) -> Result<(), InterruptError> {
        self.device
            .completion_irq
            .acknowledge_and_defer(&self.device.device);
        Ok(())
    }
}

/// @description 在统一 safe point 回收 bounded audio completions。
/// @return adapter 仍有 backlog 时返回 true。
pub(super) fn dispatch_completion_work(device: &VirtIOSoundDevice) -> bool {
    device.reclaim()
}
