use alloc::{boxed::Box, sync::Arc, vec::Vec};
use spin::Mutex;

use super::{
    InputAbsInfo, InputDevice, InputDeviceError, InputId, InterruptError, InterruptHandler,
    InterruptVector, RawInputEvent, VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK,
    VIRTIO_F_VERSION_1, VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING, VirtIODevice,
    virtio_queue::VirtQueue,
};

const EVENT_QUEUE: u32 = 0;
const QUEUE_SIZE: u16 = 64;
const EVENT_SIZE: usize = 8;
const CONFIG_ID_NAME: u8 = 0x01;
const CONFIG_ID_SERIAL: u8 = 0x02;
const CONFIG_ID_DEVIDS: u8 = 0x03;
const CONFIG_PROP_BITS: u8 = 0x10;
const CONFIG_EV_BITS: u8 = 0x11;
const CONFIG_ABS_INFO: u8 = 0x12;
const CONFIG_PAYLOAD: usize = 8;
const CONFIG_PAYLOAD_MAX: usize = 128;
const EV_ABS: u16 = 0x03;
const EV_MAX: u16 = 0x1f;
const ABS_MAX: u16 = 0x3f;
const PHYSICAL_PATH_TEMPLATE: &[u8; 35] = b"virtio-mmio@0000000000000000/input0";

struct EventCapability {
    event_type: u16,
    bits: Vec<u8>,
}

struct InputMetadata {
    name: Vec<u8>,
    physical_path: [u8; PHYSICAL_PATH_TEMPLATE.len()],
    serial: Vec<u8>,
    id: InputId,
    properties: Vec<u8>,
    event_types: [u8; 4],
    capabilities: Vec<EventCapability>,
    absolute: Vec<Option<InputAbsInfo>>,
}

struct EventSlot {
    bytes: Box<[u8; EVENT_SIZE]>,
}

struct EventQueueState {
    queue: VirtQueue,
    slots: Vec<EventSlot>,
    by_head: Vec<Option<u16>>,
    reposted: bool,
}

/// @description modern MMIO VirtIO input adapter；eventq DMA 与 metadata 由实例唯一拥有。
pub(crate) struct VirtIOInputDevice {
    device: VirtIODevice,
    metadata: InputMetadata,
    // OWNER: event queue lock 唯一串行 used recycle、slot/head 映射与 repost publication。
    // 缺失该锁会让两个 consumer 把同一 DMA slot 同时重新交还 device。
    events: Mutex<EventQueueState>,
}

impl VirtIOInputDevice {
    /// @description 初始化 VirtIO input metadata 与永久 eventq receive slots。
    /// @param base_addr DTB VirtIO MMIO base。
    /// @return 完整 adapter Arc。
    /// @errors transport、metadata、queue 或 allocation 不满足时返回 `None`。
    pub(crate) fn new(base_addr: usize) -> Option<Arc<Self>> {
        let mut device = VirtIODevice::new(base_addr, 0x1000).ok()?;
        if device.device_id() != 18 {
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
        if device.get_status().ok()? & VIRTIO_CONFIG_S_FEATURES_OK == 0 {
            return None;
        }

        let metadata = Self::read_metadata(&device, base_addr)?;
        let maximum = device.queue_max_size(EVENT_QUEUE).ok()?;
        let size = maximum.min(QUEUE_SIZE);
        if size < 2 || !size.is_power_of_two() {
            return None;
        }
        let mut queue = VirtQueue::new(size)?;
        device
            .configure_queue(EVENT_QUEUE, size, queue.addresses())
            .ok()?;

        // 任意小 buffer 最多跨两个页；预留 size/2 个 slot 可证明 descriptor capacity。
        let capacity = size / 2;
        let mut slots = Vec::new();
        slots.try_reserve_exact(capacity as usize).ok()?;
        let mut by_head = Vec::new();
        by_head.try_reserve_exact(size as usize).ok()?;
        by_head.resize(size as usize, None);
        for slot_index in 0..capacity {
            let mut bytes = Box::try_new([0u8; EVENT_SIZE]).ok()?;
            let mut outputs: [&mut [u8]; 1] = [&mut bytes[..]];
            let head = queue.add_buffer(&[], &mut outputs)?;
            queue.add_to_avail(head);
            by_head[head as usize] = Some(slot_index);
            slots.push(EventSlot { bytes });
        }

        let status = device.get_status().ok()?;
        device.set_status(status | VIRTIO_CONFIG_S_DRIVER_OK).ok()?;
        device.notify_queue(EVENT_QUEUE).ok()?;
        Arc::try_new(Self {
            device,
            metadata,
            events: Mutex::new(EventQueueState {
                queue,
                slots,
                by_head,
                reposted: false,
            }),
        })
        .ok()
    }

    fn query(device: &VirtIODevice, select: u8, subsel: u8) -> Option<Vec<u8>> {
        // 1. 规范要求两个 selector 都显式写入；只更新一个会读取上一次 query 的混合状态。
        device.write_config_u8(0, select).ok()?;
        device.write_config_u8(1, subsel).ok()?;
        let mut payload = [0u8; CONFIG_PAYLOAD_MAX];
        for _ in 0..4 {
            let before = device.config_generation().ok()?;
            let size = usize::from(device.read_config_u8(2).ok()?);
            if size > payload.len() {
                return None;
            }
            for (offset, byte) in payload[..size].iter_mut().enumerate() {
                *byte = device.read_config_u8(CONFIG_PAYLOAD + offset).ok()?;
            }
            if before == device.config_generation().ok()? {
                let mut value = Vec::new();
                value.try_reserve_exact(size).ok()?;
                value.extend_from_slice(&payload[..size]);
                return Some(value);
            }
        }
        None
    }

    fn read_metadata(device: &VirtIODevice, base_addr: usize) -> Option<InputMetadata> {
        let mut name = Self::query(device, CONFIG_ID_NAME, 0)?;
        if name.last() == Some(&0) {
            name.pop();
        }
        if name.is_empty() {
            return None;
        }
        let mut serial = Self::query(device, CONFIG_ID_SERIAL, 0)?;
        if serial.last() == Some(&0) {
            serial.pop();
        }
        let ids = Self::query(device, CONFIG_ID_DEVIDS, 0)?;
        if ids.len() != 8 {
            return None;
        }
        let id = InputId {
            bustype: u16::from_le_bytes(ids[0..2].try_into().ok()?),
            vendor: u16::from_le_bytes(ids[2..4].try_into().ok()?),
            product: u16::from_le_bytes(ids[4..6].try_into().ok()?),
            version: u16::from_le_bytes(ids[6..8].try_into().ok()?),
        };
        let properties = Self::query(device, CONFIG_PROP_BITS, 0)?;
        // Linux input core 固有发布 EV_SYN；VirtIO config 只描述 device-specific types，
        // 因此 QEMU 不会额外返回 EV_SYN config。缺失 bit 0 会让 EVIOCGBIT(0) 漏报能力。
        let mut event_types = [1u8, 0, 0, 0];
        let mut capabilities = Vec::new();
        capabilities.try_reserve_exact(4).ok()?;
        for event_type in 0..=EV_MAX {
            let bits = Self::query(device, CONFIG_EV_BITS, event_type as u8)?;
            if bits.is_empty() {
                continue;
            }
            event_types[event_type as usize / 8] |= 1 << (event_type % 8);
            if capabilities.try_reserve(1).is_err() {
                return None;
            }
            capabilities.push(EventCapability { event_type, bits });
        }
        if capabilities.is_empty() {
            return None;
        }

        let mut absolute = Vec::new();
        absolute.try_reserve_exact(usize::from(ABS_MAX) + 1).ok()?;
        absolute.resize(usize::from(ABS_MAX) + 1, None);
        if let Some(capability) = capabilities
            .iter()
            .find(|capability| capability.event_type == EV_ABS)
        {
            for code in 0..=ABS_MAX {
                if !bit_is_set(&capability.bits, code) {
                    continue;
                }
                let value = Self::query(device, CONFIG_ABS_INFO, code as u8)?;
                if value.len() != 20 {
                    return None;
                }
                let word =
                    |offset| i32::from_le_bytes(value[offset..offset + 4].try_into().unwrap());
                absolute[code as usize] = Some(InputAbsInfo {
                    minimum: word(0),
                    maximum: word(4),
                    fuzz: word(8),
                    flat: word(12),
                    resolution: word(16),
                });
            }
        }
        Some(InputMetadata {
            name,
            physical_path: physical_path(base_addr),
            serial,
            id,
            properties,
            event_types,
            capabilities,
            absolute,
        })
    }

    /// @description 构造只确认 VirtIO interrupt 并投递 input softirq 的 handler。
    /// @return 与 adapter 同生命周期的 IRQ handler Arc。
    pub(crate) fn irq_handler_for(self: &Arc<Self>) -> Arc<dyn InterruptHandler> {
        Arc::try_new(VirtIOInputIrqHandler {
            device: self.clone(),
        })
        .expect("VirtIO input IRQ handler allocation failed")
    }
}

fn physical_path(base_addr: usize) -> [u8; PHYSICAL_PATH_TEMPLATE.len()] {
    let mut path = *PHYSICAL_PATH_TEMPLATE;
    for (index, digit) in path[12..28].iter_mut().enumerate() {
        let value = (base_addr >> ((15 - index) * 4)) & 0xf;
        *digit = b"0123456789abcdef"[value];
    }
    path
}

fn bit_is_set(bits: &[u8], bit: u16) -> bool {
    bits.get(bit as usize / 8)
        .is_some_and(|byte| byte & (1 << (bit % 8)) != 0)
}

impl InputDevice for VirtIOInputDevice {
    fn name(&self) -> &[u8] {
        &self.metadata.name
    }

    fn physical_path(&self) -> &[u8] {
        &self.metadata.physical_path
    }

    fn serial(&self) -> &[u8] {
        &self.metadata.serial
    }

    fn id(&self) -> InputId {
        self.metadata.id
    }

    fn properties(&self) -> &[u8] {
        &self.metadata.properties
    }

    fn event_types(&self) -> &[u8] {
        &self.metadata.event_types
    }

    fn event_codes(&self, event_type: u16) -> &[u8] {
        self.metadata
            .capabilities
            .iter()
            .find_map(|capability| {
                (capability.event_type == event_type).then_some(capability.bits.as_slice())
            })
            .unwrap_or(&[])
    }

    fn abs_info(&self, code: u16) -> Option<InputAbsInfo> {
        self.metadata.absolute.get(code as usize).copied().flatten()
    }

    fn receive_event(&self) -> Result<Option<RawInputEvent>, InputDeviceError> {
        let mut events = self.events.lock();
        let Some((head, used_length)) =
            events.queue.used().map_err(|()| InputDeviceError::Device)?
        else {
            return Ok(None);
        };
        if used_length as usize != EVENT_SIZE {
            return Err(InputDeviceError::Device);
        }
        let slot_index = events.by_head[head as usize]
            .take()
            .ok_or(InputDeviceError::Device)?;
        let bytes = *events.slots[slot_index as usize].bytes;
        let event = RawInputEvent {
            event_type: u16::from_le_bytes(bytes[0..2].try_into().unwrap()),
            code: u16::from_le_bytes(bytes[2..4].try_into().unwrap()),
            value: i32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        };

        // 1. used() 已归还旧 chain；2. 原 slot 立即重新发布；3. batch 末尾统一 notify。
        // 缺失第 2 步会让 device 在持续指针事件中耗尽 eventq 并静默丢包。
        let new_head = {
            let EventQueueState { queue, slots, .. } = &mut *events;
            let mut outputs: [&mut [u8]; 1] = [&mut slots[slot_index as usize].bytes[..]];
            queue
                .add_buffer(&[], &mut outputs)
                .ok_or(InputDeviceError::Device)?
        };
        if events.by_head[new_head as usize]
            .replace(slot_index)
            .is_some()
        {
            return Err(InputDeviceError::Device);
        }
        events.queue.add_to_avail(new_head);
        events.reposted = true;
        Ok(Some(event))
    }

    fn finish_receive_batch(&self) -> Result<(), InputDeviceError> {
        let notify = core::mem::take(&mut self.events.lock().reposted);
        if notify {
            self.device
                .notify_queue(EVENT_QUEUE)
                .map_err(|_| InputDeviceError::Device)?;
        }
        Ok(())
    }

    fn has_pending_event(&self) -> bool {
        self.events.lock().queue.has_used()
    }
}

struct VirtIOInputIrqHandler {
    device: Arc<VirtIOInputDevice>,
}

impl InterruptHandler for VirtIOInputIrqHandler {
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
        if status & VIRTIO_MMIO_INT_VRING != 0 {
            crate::cpu::raise_deferred(crate::cpu::DeferredWork::Input);
        }
        Ok(())
    }
}
