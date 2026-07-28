//! @description VirtIO Console multiport adapter for the standard SPICE agent byte stream.

mod byte_ring;
mod wire;

use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

use super::{
    InterruptError, InterruptHandler, InterruptVector, VIRTIO_CONFIG_S_DRIVER_OK,
    VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1, VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING,
    VirtIODevice,
    virtio_queue::{DmaBuffer, UsedDescriptor, VirtQueue},
};
use byte_ring::ByteRing;
use wire::is_spice_port_name;

const VIRTIO_CONSOLE_F_MULTIPORT: u64 = 1 << 1;
const CONTROL_RX_QUEUE: u32 = 2;
const CONTROL_TX_QUEUE: u32 = 3;
const DATA_RX_QUEUE: u32 = 4;
const DATA_TX_QUEUE: u32 = 5;
const QUEUE_LIMIT: u16 = 32;
const RX_SLOTS: usize = 8;
const TX_SLOTS: usize = 8;
const CONTROL_SLOTS: usize = 4;
const RX_BYTES: usize = 4096;
const TX_BYTES: usize = 1024;
const CONTROL_BYTES: usize = 128;
const CONTROL_MESSAGE_BYTES: usize = 8;
const DEVICE_READY: u16 = 0;
const PORT_ADD: u16 = 1;
const PORT_REMOVE: u16 = 2;
const PORT_READY: u16 = 3;
const PORT_OPEN: u16 = 6;
const PORT_NAME: u16 = 7;

/// @description VirtIO port byte-stream operation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PortError {
    /// No byte or transmit slot is currently available.
    WouldBlock,
    /// The selected named port is closed or the transport failed.
    Disconnected,
}

/// @description One deferred VirtIO Console drain result.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PortActivity {
    /// Receive bytes or disconnect state changed.
    pub(crate) readable_changed: bool,
    /// A transmit slot became available or the port disconnected.
    pub(crate) writable_changed: bool,
    /// At least one queue still contains a completion after the bounded pass.
    pub(crate) backlog: bool,
}

struct ReceiveSlot<const SIZE: usize> {
    bytes: DmaBuffer<SIZE>,
}

struct TransmitSlot<const SIZE: usize> {
    bytes: DmaBuffer<SIZE>,
    busy: bool,
}

struct ReceiveQueue<const SIZE: usize> {
    queue: VirtQueue,
    slots: Vec<ReceiveSlot<SIZE>>,
    by_head: Vec<Option<u16>>,
}

struct TransmitQueue<const SIZE: usize> {
    queue: VirtQueue,
    slots: Vec<TransmitSlot<SIZE>>,
    by_head: Vec<Option<u16>>,
}

struct State {
    control_rx: ReceiveQueue<CONTROL_BYTES>,
    control_tx: TransmitQueue<8>,
    data_rx: ReceiveQueue<RX_BYTES>,
    data_tx: TransmitQueue<TX_BYTES>,
    stream: ByteRing,
    port_id: Option<u32>,
    name_matches: bool,
    // Records host state independently because PORT_OPEN and PORT_NAME may arrive in either order;
    // deriving openness only when PORT_OPEN arrives can leave a valid named port disconnected forever.
    host_open: bool,
    // Records the guest PORT_OPEN publication required by the multiport contract; without this
    // acknowledgement QEMU keeps the host channel present but does not deliver its byte stream.
    guest_open_announced: bool,
    open: bool,
    failed: bool,
    // Makes transport failure a single terminal reset; without it, every deferred pass can reset
    // the same failed device again and hide the original owner transition.
    reset_issued: bool,
}

/// @description Modern VirtIO Console adapter selecting `com.redhat.spice.0`.
pub(crate) struct VirtIOConsoleDevice {
    device: VirtIODevice,
    state: Mutex<State>,
}

impl VirtIOConsoleDevice {
    /// @description Initialize the standard multiport queues and announce guest readiness.
    /// @param base_addr DTB VirtIO MMIO base.
    /// @return A complete adapter, or `None` when required features/queues are absent.
    pub(crate) fn new(base_addr: usize) -> Option<Arc<Self>> {
        let mut device = VirtIODevice::new(base_addr, 0x1000).ok()?;
        if device.device_id() != 3 {
            return None;
        }
        device.initialize().ok()?;
        let offered = device.device_features().ok()?;
        let required = VIRTIO_F_VERSION_1 | VIRTIO_CONSOLE_F_MULTIPORT;
        if offered & required != required {
            return None;
        }
        device.set_driver_features(required).ok()?;
        let status = device.get_status().ok()?;
        device
            .set_status(status | VIRTIO_CONFIG_S_FEATURES_OK)
            .ok()?;
        if device.get_status().ok()? & VIRTIO_CONFIG_S_FEATURES_OK == 0 {
            return None;
        }

        let mut control_rx = Self::receive_queue(&device, CONTROL_RX_QUEUE, CONTROL_SLOTS)?;
        let control_tx = Self::transmit_queue(&device, CONTROL_TX_QUEUE, CONTROL_SLOTS)?;
        let mut data_rx = Self::receive_queue(&device, DATA_RX_QUEUE, RX_SLOTS)?;
        let data_tx = Self::transmit_queue(&device, DATA_TX_QUEUE, TX_SLOTS)?;
        Self::post_receive_slots(&mut control_rx)?;
        Self::post_receive_slots(&mut data_rx)?;

        let status = device.get_status().ok()?;
        device.set_status(status | VIRTIO_CONFIG_S_DRIVER_OK).ok()?;
        device.notify_queue(CONTROL_RX_QUEUE).ok()?;
        device.notify_queue(DATA_RX_QUEUE).ok()?;

        let adapter = Arc::try_new(Self {
            device,
            state: Mutex::new(State {
                control_rx,
                control_tx,
                data_rx,
                data_tx,
                stream: ByteRing::new()?,
                port_id: None,
                name_matches: false,
                host_open: false,
                guest_open_announced: false,
                open: false,
                failed: false,
                reset_issued: false,
            }),
        })
        .ok()?;
        {
            let mut state = adapter.state.lock();
            adapter.submit_control(&mut state, 0, DEVICE_READY, 1)?;
        }
        adapter.device.notify_queue(CONTROL_TX_QUEUE).ok()?;
        Some(adapter)
    }

    fn receive_queue<const SIZE: usize>(
        device: &VirtIODevice,
        index: u32,
        slots: usize,
    ) -> Option<ReceiveQueue<SIZE>> {
        let maximum = device.queue_max_size(index).ok()?;
        let size = maximum.min(QUEUE_LIMIT);
        if size < slots as u16 || !size.is_power_of_two() {
            return None;
        }
        let queue = VirtQueue::new(size)?;
        device
            .configure_queue(index, size, queue.addresses())
            .ok()?;
        let mut values = Vec::new();
        values.try_reserve_exact(slots).ok()?;
        for _ in 0..slots {
            values.push(ReceiveSlot {
                bytes: DmaBuffer::try_zeroed().ok()?,
            });
        }
        let mut by_head = Vec::new();
        by_head.try_reserve_exact(size as usize).ok()?;
        by_head.resize(size as usize, None);
        Some(ReceiveQueue {
            queue,
            slots: values,
            by_head,
        })
    }

    fn transmit_queue<const SIZE: usize>(
        device: &VirtIODevice,
        index: u32,
        slots: usize,
    ) -> Option<TransmitQueue<SIZE>> {
        let maximum = device.queue_max_size(index).ok()?;
        let size = maximum.min(QUEUE_LIMIT);
        if size < slots as u16 || !size.is_power_of_two() {
            return None;
        }
        let queue = VirtQueue::new(size)?;
        device
            .configure_queue(index, size, queue.addresses())
            .ok()?;
        let mut values = Vec::new();
        values.try_reserve_exact(slots).ok()?;
        for _ in 0..slots {
            values.push(TransmitSlot {
                bytes: DmaBuffer::try_zeroed().ok()?,
                busy: false,
            });
        }
        let mut by_head = Vec::new();
        by_head.try_reserve_exact(size as usize).ok()?;
        by_head.resize(size as usize, None);
        Some(TransmitQueue {
            queue,
            slots: values,
            by_head,
        })
    }

    fn post_receive_slots<const SIZE: usize>(receive: &mut ReceiveQueue<SIZE>) -> Option<()> {
        for slot_index in 0..receive.slots.len() {
            let output = receive.slots[slot_index].bytes.writable_all();
            let head = receive.queue.add_dma(&[output]).ok()?;
            receive.by_head[head as usize] = Some(slot_index as u16);
            receive.queue.add_to_avail(head);
        }
        Some(())
    }

    fn submit_control(&self, state: &mut State, id: u32, event: u16, value: u16) -> Option<()> {
        let slot_index = state.control_tx.slots.iter().position(|slot| !slot.busy)?;
        let slot = &mut state.control_tx.slots[slot_index];
        slot.bytes[..4].copy_from_slice(&id.to_le_bytes());
        slot.bytes[4..6].copy_from_slice(&event.to_le_bytes());
        slot.bytes[6..8].copy_from_slice(&value.to_le_bytes());
        let input = slot.bytes.readable(0..CONTROL_MESSAGE_BYTES).ok()?;
        let head = state.control_tx.queue.add_dma(&[input]).ok()?;
        if state.control_tx.by_head[head as usize]
            .replace(slot_index as u16)
            .is_some()
        {
            return None;
        }
        slot.busy = true;
        state.control_tx.queue.add_to_avail(head);
        Some(())
    }

    /// @description Read currently buffered bytes without sleeping.
    /// @param output Kernel-owned destination.
    /// @return Positive byte count, `WouldBlock`, or terminal disconnect.
    pub(crate) fn read(&self, output: &mut [u8]) -> Result<usize, PortError> {
        let mut state = self.state.lock();
        if state.failed || !state.open {
            return Err(PortError::Disconnected);
        }
        let count = state.stream.pop(output);
        if count == 0 {
            Err(PortError::WouldBlock)
        } else {
            Ok(count)
        }
    }

    /// @description Submit one bounded byte-stream fragment without sleeping.
    /// @param input Bytes for the selected named port.
    /// @return Submitted byte count, `WouldBlock`, or terminal disconnect.
    pub(crate) fn write(&self, input: &[u8]) -> Result<usize, PortError> {
        if input.is_empty() {
            return Ok(0);
        }
        let count = input.len().min(TX_BYTES);
        let mut state = self.state.lock();
        if state.failed || !state.open {
            return Err(PortError::Disconnected);
        }
        let slot_index = state
            .data_tx
            .slots
            .iter()
            .position(|slot| !slot.busy)
            .ok_or(PortError::WouldBlock)?;
        let TransmitQueue {
            queue,
            slots,
            by_head,
        } = &mut state.data_tx;
        let slot = &mut slots[slot_index];
        slot.bytes[..count].copy_from_slice(&input[..count]);
        let payload = slot
            .bytes
            .readable(0..count)
            .map_err(|_| PortError::Disconnected)?;
        let head = queue
            .add_dma(&[payload])
            .map_err(|_| PortError::WouldBlock)?;
        if by_head[head as usize].replace(slot_index as u16).is_some() {
            state.failed = true;
            state.reset_issued = true;
            drop(state);
            let _ = self.device.reset();
            return Err(PortError::Disconnected);
        }
        slot.busy = true;
        state.data_tx.queue.add_to_avail(head);
        drop(state);
        if self.device.notify_queue(DATA_TX_QUEUE).is_err() {
            let mut state = self.state.lock();
            state.failed = true;
            if !state.reset_issued {
                state.reset_issued = true;
                drop(state);
                let _ = self.device.reset();
            }
            return Err(PortError::Disconnected);
        }
        Ok(count)
    }

    pub(crate) fn readable(&self) -> bool {
        let state = self.state.lock();
        state.failed || !state.open || !state.stream.is_empty()
    }

    pub(crate) fn writable(&self) -> bool {
        let state = self.state.lock();
        !state.failed && state.open && state.data_tx.slots.iter().any(|slot| !slot.busy)
    }

    pub(crate) fn connected(&self) -> bool {
        let state = self.state.lock();
        !state.failed && state.open
    }

    /// @description Drain a bounded batch from all four queues at a safe point.
    /// @return Read/write level transitions and remaining backlog.
    pub(crate) fn dispatch(&self) -> PortActivity {
        let before_readable = self.readable();
        let before_writable = self.writable();
        let mut state = self.state.lock();
        if state.failed {
            let reset = !state.reset_issued;
            state.reset_issued = true;
            drop(state);
            if reset {
                let _ = self.device.reset();
            }
            return PortActivity::default();
        }
        let mut notify_control_rx = false;
        let mut notify_data_rx = false;
        // 1. Reclaim only a bounded number of completions in deferred context.
        // 2. Remember each RX repost because avail publication, not residual used entries, requires
        //    a device notification; omitting this can permanently stall a drained clipboard queue.
        // 3. Reset once after releasing the state lock when any queue invariant fails.
        for _ in 0..32 {
            let progressed = self.reclaim_control_tx(&mut state)
                | self.reclaim_control_rx(&mut state, &mut notify_control_rx)
                | self.reclaim_data_tx(&mut state)
                | self.reclaim_data_rx(&mut state, &mut notify_data_rx);
            if state.failed || !progressed {
                break;
            }
        }
        let backlog = state.control_rx.queue.has_used()
            || state.control_tx.queue.has_used()
            || state.data_rx.queue.has_used()
            || state.data_tx.queue.has_used();
        let after_readable = state.failed || !state.open || !state.stream.is_empty();
        let after_writable =
            !state.failed && state.open && state.data_tx.slots.iter().any(|slot| !slot.busy);
        let reset = state.failed && !state.reset_issued;
        if reset {
            state.reset_issued = true;
        }
        drop(state);
        if notify_control_rx {
            let _ = self.device.notify_queue(CONTROL_RX_QUEUE);
        }
        if notify_data_rx {
            let _ = self.device.notify_queue(DATA_RX_QUEUE);
        }
        if reset {
            let _ = self.device.reset();
        }
        PortActivity {
            readable_changed: before_readable != after_readable,
            writable_changed: before_writable != after_writable,
            backlog,
        }
    }

    fn reclaim_control_tx(&self, state: &mut State) -> bool {
        match Self::reclaim_transmit(&mut state.control_tx) {
            Ok(progressed) => progressed,
            Err(()) => {
                state.failed = true;
                false
            }
        }
    }

    fn reclaim_data_tx(&self, state: &mut State) -> bool {
        match Self::reclaim_transmit(&mut state.data_tx) {
            Ok(progressed) => progressed,
            Err(()) => {
                state.failed = true;
                false
            }
        }
    }

    fn reclaim_transmit<const SIZE: usize>(transmit: &mut TransmitQueue<SIZE>) -> Result<bool, ()> {
        let completion = match transmit.queue.used() {
            Ok(Some(value)) => value,
            Ok(None) => return Ok(false),
            Err(()) => return Err(()),
        };
        let head = completion.head();
        let Some(slot_index) = transmit.by_head[head as usize].take() else {
            return Err(());
        };
        if completion.length() != 0 || transmit.queue.recycle_used(completion).is_err() {
            return Err(());
        }
        transmit.slots[slot_index as usize].busy = false;
        Ok(true)
    }

    fn reclaim_control_rx(&self, state: &mut State, reposted: &mut bool) -> bool {
        let (slot, completion) = match Self::claim_receive(&mut state.control_rx) {
            Ok(Some(value)) => value,
            Ok(None) => return false,
            Err(()) => {
                state.failed = true;
                return false;
            }
        };
        let length = completion.length() as usize;
        if !(8..=CONTROL_BYTES).contains(&length) {
            state.failed = true;
            return false;
        }
        let message = &state.control_rx.slots[slot].bytes[..length];
        let id = u32::from_le_bytes(message[..4].try_into().unwrap());
        let event = u16::from_le_bytes(message[4..6].try_into().unwrap());
        let value = u16::from_le_bytes(message[6..8].try_into().unwrap());
        let name_matches = is_spice_port_name(&message[8..]);
        if state.control_rx.queue.recycle_used(completion).is_err()
            || Self::repost_receive(&mut state.control_rx, slot).is_none()
        {
            state.failed = true;
            return true;
        }
        *reposted = true;
        match event {
            PORT_ADD => {
                state.port_id = Some(id);
                state.name_matches = false;
                state.host_open = false;
                state.guest_open_announced = false;
                state.open = false;
                if self.submit_control(state, id, PORT_READY, 1).is_none() {
                    state.failed = true;
                }
            }
            PORT_REMOVE if state.port_id == Some(id) => {
                state.port_id = None;
                state.name_matches = false;
                state.host_open = false;
                state.guest_open_announced = false;
                state.open = false;
            }
            PORT_NAME if state.port_id == Some(id) => {
                state.name_matches = name_matches;
                if state.name_matches
                    && !state.guest_open_announced
                    && self.submit_control(state, id, PORT_OPEN, 1).is_some()
                {
                    state.guest_open_announced = true;
                } else if state.name_matches && !state.guest_open_announced {
                    state.failed = true;
                }
                state.open = state.name_matches && state.host_open;
            }
            PORT_OPEN if state.port_id == Some(id) => {
                state.host_open = value != 0;
                state.open = state.name_matches && state.host_open;
            }
            _ => {}
        }
        let _ = self.device.notify_queue(CONTROL_TX_QUEUE);
        true
    }

    fn reclaim_data_rx(&self, state: &mut State, reposted: &mut bool) -> bool {
        let (slot, completion) = match Self::claim_receive(&mut state.data_rx) {
            Ok(Some(value)) => value,
            Ok(None) => return false,
            Err(()) => {
                state.failed = true;
                return false;
            }
        };
        let length = completion.length() as usize;
        if length == 0 || length > RX_BYTES {
            state.failed = true;
            return false;
        }
        let bytes = &state.data_rx.slots[slot].bytes[..length];
        if !state.stream.push(bytes) {
            state.failed = true;
            return false;
        }
        if state.data_rx.queue.recycle_used(completion).is_err()
            || Self::repost_receive(&mut state.data_rx, slot).is_none()
        {
            state.failed = true;
        } else {
            *reposted = true;
        }
        true
    }

    fn claim_receive<const SIZE: usize>(
        receive: &mut ReceiveQueue<SIZE>,
    ) -> Result<Option<(usize, UsedDescriptor)>, ()> {
        let completion = match receive.queue.used() {
            Ok(Some(value)) => value,
            Ok(None) => return Ok(None),
            Err(()) => return Err(()),
        };
        let head = completion.head();
        let Some(slot) = receive.by_head[head as usize].take() else {
            return Err(());
        };
        Ok(Some((slot as usize, completion)))
    }

    fn repost_receive<const SIZE: usize>(
        receive: &mut ReceiveQueue<SIZE>,
        slot: usize,
    ) -> Option<()> {
        let output = receive.slots[slot].bytes.writable_all();
        let head = receive.queue.add_dma(&[output]).ok()?;
        if receive.by_head[head as usize]
            .replace(slot as u16)
            .is_some()
        {
            return None;
        }
        receive.queue.add_to_avail(head);
        Some(())
    }

    pub(crate) fn irq_handler_for(self: &Arc<Self>) -> Arc<dyn InterruptHandler> {
        Arc::new(VirtIOConsoleIrqHandler {
            device: self.clone(),
        })
    }
}

impl Drop for VirtIOConsoleDevice {
    fn drop(&mut self) {
        let _ = self.device.reset();
    }
}

struct VirtIOConsoleIrqHandler {
    device: Arc<VirtIOConsoleDevice>,
}

impl InterruptHandler for VirtIOConsoleIrqHandler {
    fn handle_interrupt(&self, _vector: InterruptVector) -> Result<(), InterruptError> {
        let status = self
            .device
            .device
            .interrupt_status()
            .map_err(|_| InterruptError::DeviceFailure)?;
        self.device
            .device
            .interrupt_ack(status & (VIRTIO_MMIO_INT_VRING | VIRTIO_MMIO_INT_CONFIG))
            .map_err(|_| InterruptError::DeviceFailure)?;
        crate::cpu::raise_deferred(crate::cpu::DeferredWork::VirtioPort);
        Ok(())
    }
}
