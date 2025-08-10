use core::mem;
use core::ptr;
use alloc::{boxed::Box, sync::{Arc, Weak}, vec::Vec, format, string::{String, ToString}, vec, collections::BTreeMap};
use spin::Mutex;
use crate::memory::{KERNEL_SPACE, address::{PhysicalAddress, VirtualAddress}};
use crate::drivers::{
    Device, DeviceType, DeviceState, DeviceError, GenericDevice,
    // 用于全局 Framebuffer 注册与信息
    GenericFramebuffer, FramebufferInfo, PixelFormat, set_global_framebuffer,
    // 设备查找（用于 flush 回调）
    find_devices_by_type, get_device,
    hal::{
        device::DeviceDriver,
        interrupt::{InterruptHandler, InterruptVector},
        bus::{Bus},
        resource::{Resource, ResourceManager},
    },
    virtio_queue::{VirtQueue, VirtQueueError},
};

const VIRTIO_GPU_DEVICE_ID: u32 = 0x10;  // VirtIO GPU 子系统 ID
const VIRTIO_GPU_F_VIRGL: u32 = 0;
const VIRTIO_GPU_F_EDID: u32 = 1;

// virtio-mmio offsets (modern)
const MMIO_MAGIC_VALUE: usize = 0x000;
const MMIO_VERSION: usize = 0x004;
const MMIO_DEVICE_ID: usize = 0x008;
const MMIO_VENDOR_ID: usize = 0x00c;
const MMIO_DEVICE_FEATURES: usize = 0x010;
const MMIO_DEVICE_FEATURES_SEL: usize = 0x014;
const MMIO_DRIVER_FEATURES: usize = 0x020;
const MMIO_DRIVER_FEATURES_SEL: usize = 0x024;
const MMIO_GUEST_PAGE_SIZE: usize = 0x028; // legacy only
const MMIO_QUEUE_SEL: usize = 0x030;
const MMIO_QUEUE_NUM_MAX: usize = 0x034;
const MMIO_QUEUE_NUM: usize = 0x038;
const MMIO_QUEUE_ALIGN: usize = 0x03c; // legacy only
const MMIO_QUEUE_PFN: usize = 0x040;   // legacy only (PFN = phys_addr >> 12)
const MMIO_QUEUE_READY: usize = 0x044;
const MMIO_QUEUE_NOTIFY: usize = 0x050;
const MMIO_STATUS: usize = 0x070;
const MMIO_QUEUE_DESC_LOW: usize = 0x080;
const MMIO_QUEUE_DESC_HIGH: usize = 0x084;
const MMIO_QUEUE_AVAIL_LOW: usize = 0x090;
const MMIO_QUEUE_AVAIL_HIGH: usize = 0x094;
const MMIO_QUEUE_USED_LOW: usize = 0x0a0;
const MMIO_QUEUE_USED_HIGH: usize = 0x0a4;

// Status bits
const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 1;
const VIRTIO_STATUS_DRIVER: u32 = 2;
const VIRTIO_STATUS_DRIVER_OK: u32 = 4;
const VIRTIO_STATUS_FEATURES_OK: u32 = 8;
const VIRTIO_F_VERSION_1_BIT: u32 = 32; // feature bit number

const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const VIRTIO_GPU_CMD_RESOURCE_UNREF: u32 = 0x0102;
const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0104;
const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
const VIRTIO_GPU_CMD_GET_CAPSET_INFO: u32 = 0x0108;
const VIRTIO_GPU_CMD_GET_CAPSET: u32 = 0x0109;

const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const VIRTIO_GPU_RESP_OK_CAPSET_INFO: u32 = 0x1102;
const VIRTIO_GPU_RESP_OK_CAPSET: u32 = 0x1103;

const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1;
const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;
const VIRTIO_GPU_FORMAT_A8R8G8B8_UNORM: u32 = 3;
const VIRTIO_GPU_FORMAT_X8R8G8B8_UNORM: u32 = 4;
const VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM: u32 = 67;
const VIRTIO_GPU_FORMAT_X8B8G8R8_UNORM: u32 = 68;
const VIRTIO_GPU_FORMAT_A8B8G8R8_UNORM: u32 = 121;
const VIRTIO_GPU_FORMAT_R8G8B8X8_UNORM: u32 = 134;

const VIRTIO_GPU_MAX_SCANOUTS: usize = 16;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuCtrlHeader {
    command_type: u32,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuDisplayOne {
    r: VirtioGpuRect,
    enabled: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuRespDisplayInfo {
    hdr: VirtioGpuCtrlHeader,
    pmodes: [VirtioGpuDisplayOne; VIRTIO_GPU_MAX_SCANOUTS],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuResourceCreate2d {
    hdr: VirtioGpuCtrlHeader,
    resource_id: u32,
    format: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuSetScanout {
    hdr: VirtioGpuCtrlHeader,
    r: VirtioGpuRect,
    scanout_id: u32,
    resource_id: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuResourceFlush {
    hdr: VirtioGpuCtrlHeader,
    r: VirtioGpuRect,
    resource_id: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuTransferToHost2d {
    hdr: VirtioGpuCtrlHeader,
    r: VirtioGpuRect,
    offset: u64,
    resource_id: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuMemEntry {
    addr: u64,
    length: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuResourceAttachBacking {
    hdr: VirtioGpuCtrlHeader,
    resource_id: u32,
    nr_entries: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtioGpuCtrlResponse {
    response_type: u32,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    padding: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub enabled: bool,
}

pub struct VirtioGpuDevice {
    base: GenericDevice,
    base_addr: usize,
    ctrl_queue: Option<VirtQueue>,
    cursor_queue: Option<VirtQueue>,
    display_modes: Vec<DisplayMode>,
    resource_id_counter: u32,
    framebuffer_resource_id: Option<u32>,
    framebuffer_phys_addr: Option<PhysicalAddress>,
    framebuffer_virt_addr: Option<VirtualAddress>,
    framebuffer_size: usize,
    current_width: u32,
    current_height: u32,
    current_format: u32,
    pending_waiters: BTreeMap<u16, Weak<crate::task::TaskControlBlock>>,
}

impl VirtioGpuDevice {
    fn irq_ack_and_drain(&mut self) {
        // acknowledge by reading status if needed (MMIO interrupt status not exposed here)
        if let Some(ref mut q) = self.ctrl_queue {
            while let Some((id, _len)) = q.used() {
                // 精准唤醒等待的任务
                if let Some(weak) = self.pending_waiters.remove(&id) {
                    if let Some(task) = weak.upgrade() {
                        task.wakeup();
                    }
                }
            }
        }
    }
    pub fn new(base_addr: usize, interrupt_vector: InterruptVector) -> Result<Self, DeviceError> {
        info!("[VirtIO-GPU] Creating new VirtIO GPU device at {:#x}", base_addr);

        let device = VirtioGpuDevice {
            base: GenericDevice::new(
                DeviceType::Display,
                VIRTIO_GPU_DEVICE_ID,
                0x1AF4, // Red Hat VirtIO vendor ID
                format!("virtio-gpu@{:#x}", base_addr),
                "virtio-gpu".to_string(),
                Arc::new(DummyBus::new()),
            ),
            base_addr,
            ctrl_queue: None,
            cursor_queue: None,
            display_modes: Vec::new(),
            resource_id_counter: 1,
            framebuffer_resource_id: None,
            framebuffer_phys_addr: None,
            framebuffer_virt_addr: None,
            framebuffer_size: 0,
            current_width: 0,
            current_height: 0,
            current_format: VIRTIO_GPU_FORMAT_X8R8G8B8_UNORM,
            pending_waiters: BTreeMap::new(),
        };

        Ok(device)
    }

    fn read32(&self, offset: usize) -> u32 {
        unsafe {
            ptr::read_volatile((self.base_addr + offset) as *const u32)
        }
    }

    fn write32(&self, offset: usize, value: u32) {
        unsafe {
            ptr::write_volatile((self.base_addr + offset) as *mut u32, value);
        }
    }

    fn check_device_id(&self) -> bool {
        let device_id = self.read32(0x08);
        device_id == VIRTIO_GPU_DEVICE_ID
    }

    fn reset_device(&mut self) {
        info!("[VirtIO-GPU] Resetting device");
        self.write32(MMIO_STATUS, 0);

        while self.read32(MMIO_STATUS) != 0 {
            core::hint::spin_loop();
        }
    }

    fn set_status_bit(&mut self, bit: u32) {
        let current = self.read32(MMIO_STATUS);
        self.write32(MMIO_STATUS, current | bit);
    }

    fn negotiate_features(&mut self) -> Result<(), DeviceError> {
        info!("[VirtIO-GPU] Negotiating features");
        // 读取设备特性（低/高32位）
        self.write32(MMIO_DEVICE_FEATURES_SEL, 0);
        let dev_feat_low = self.read32(MMIO_DEVICE_FEATURES);
        self.write32(MMIO_DEVICE_FEATURES_SEL, 1);
        let dev_feat_high = self.read32(MMIO_DEVICE_FEATURES);

        info!("[VirtIO-GPU] Device features: high={:#x} low={:#x}", dev_feat_high, dev_feat_low);

        // 选择我们支持的特性
        let mut drv_feat_low: u32 = 0;
        let mut drv_feat_high: u32 = 0;

        // 设备特性：EDID（设备自定义bit 1）
        if (dev_feat_low & (1 << VIRTIO_GPU_F_EDID)) != 0 {
            drv_feat_low |= 1 << VIRTIO_GPU_F_EDID;
            info!("[VirtIO-GPU] Enabling EDID support");
        }

        // 通用特性：VERSION_1（bit 32）
        if (dev_feat_high & (1 << (VIRTIO_F_VERSION_1_BIT - 32))) != 0 {
            drv_feat_high |= 1 << (VIRTIO_F_VERSION_1_BIT - 32);
            info!("[VirtIO-GPU] Enabling VERSION_1");
        }

        // 必须在设置FEATURES_OK前设置ACKNOWLEDGE和DRIVER
        self.set_status_bit(VIRTIO_STATUS_ACKNOWLEDGE);
        self.set_status_bit(VIRTIO_STATUS_DRIVER);

        // 写驱动特性（低/高32位）
        self.write32(MMIO_DRIVER_FEATURES_SEL, 0);
        self.write32(MMIO_DRIVER_FEATURES, drv_feat_low);
        self.write32(MMIO_DRIVER_FEATURES_SEL, 1);
        self.write32(MMIO_DRIVER_FEATURES, drv_feat_high);
        self.set_status_bit(VIRTIO_STATUS_FEATURES_OK);

        // 验证设备是否接受FEATURES_OK
        let status = self.read32(MMIO_STATUS);
        if (status & VIRTIO_STATUS_FEATURES_OK) == 0 {
            warn!("[VirtIO-GPU] Device did not accept FEATURES_OK, status={:#x}", status);
        }

        Ok(())
    }

    fn setup_virtqueues(&mut self) -> Result<(), DeviceError> {
        info!("[VirtIO-GPU] Setting up VirtQueues");

        let version = self.read32(MMIO_VERSION);
        info!("[VirtIO-GPU] MMIO version={}", version);

        // 工具：向下取2的幂（返回<=x的最大2次幂，x>0）
        fn pow2_down(mut x: u16) -> u16 {
            if x == 0 { return 0; }
            x |= x >> 1;
            x |= x >> 2;
            x |= x >> 4;
            x |= x >> 8;
            (x + 1) >> 1
        }

        // 队列0：控制队列
        self.write32(MMIO_QUEUE_SEL, 0);
        let max0 = self.read32(MMIO_QUEUE_NUM_MAX) as u16;
        if max0 == 0 {
            error!("[VirtIO-GPU] Control queue unsupported (NUM_MAX=0)");
            return Err(DeviceError::InitializationFailed);
        }
        let req0 = 128u16;
        let size0 = pow2_down(core::cmp::min(req0, max0));
        let q0 = VirtQueue::new(size0, 0).ok_or(DeviceError::InitializationFailed)?;
        self.write32(MMIO_QUEUE_NUM, size0 as u32);
        // 使用与块设备一致的 legacy 队列编程路径
        self.write32(MMIO_GUEST_PAGE_SIZE, 4096);
        self.write32(MMIO_QUEUE_ALIGN, 4096);
        let (d0, _a0, _u0) = q0.mmio_addresses();
        let pfn0 = (d0 as usize) >> 12;
        self.write32(MMIO_QUEUE_PFN, pfn0 as u32);
        self.write32(MMIO_QUEUE_READY, 1);
        self.ctrl_queue = Some(q0);

        // 队列1：光标队列
        self.write32(MMIO_QUEUE_SEL, 1);
        let max1 = self.read32(MMIO_QUEUE_NUM_MAX) as u16;
        if max1 == 0 {
            error!("[VirtIO-GPU] Cursor queue unsupported (NUM_MAX=0)");
            return Err(DeviceError::InitializationFailed);
        }
        let req1 = 16u16;
        let size1 = pow2_down(core::cmp::min(req1, max1));
        let q1 = VirtQueue::new(size1, 1).ok_or(DeviceError::InitializationFailed)?;
        self.write32(MMIO_QUEUE_NUM, size1 as u32);
        self.write32(MMIO_GUEST_PAGE_SIZE, 4096);
        self.write32(MMIO_QUEUE_ALIGN, 4096);
        let (d1, _a1, _u1) = q1.mmio_addresses();
        let pfn1 = (d1 as usize) >> 12;
        self.write32(MMIO_QUEUE_PFN, pfn1 as u32);
        self.write32(MMIO_QUEUE_READY, 1);
        self.cursor_queue = Some(q1);

        Ok(())
    }

    fn send_command<T: Copy>(&mut self, cmd: &T) -> Result<VirtioGpuCtrlResponse, DeviceError> {
        let cmd_bytes = unsafe {
            core::slice::from_raw_parts(
                cmd as *const T as *const u8,
                mem::size_of::<T>()
            )
        };

        let mut response = VirtioGpuCtrlResponse {
            response_type: 0,
            flags: 0,
            fence_id: 0,
            ctx_id: 0,
            padding: 0,
        };

        let response_bytes = unsafe {
            core::slice::from_raw_parts_mut(
                &mut response as *mut VirtioGpuCtrlResponse as *mut u8,
                mem::size_of::<VirtioGpuCtrlResponse>()
            )
        };

        let desc_head = {
            let ctrl_queue = self.ctrl_queue.as_mut()
                .ok_or(DeviceError::InvalidState)?;
            let head = ctrl_queue.add_buffer(&[cmd_bytes], &mut [response_bytes]);
            if head.is_none() {
                error!("[VirtIO-GPU] Failed to add buffer to queue");
                return Err(DeviceError::OperationFailed);
            }
            // Add to available ring
            ctrl_queue.add_to_avail(head.unwrap());
            let h = head.unwrap();
            // 记录等待者（弱引用），用于精准唤醒
            if let Some(current) = crate::task::current_task() {
                self.pending_waiters.insert(h, Arc::downgrade(&current));
            }
            h
        };

        // Notify device (control queue id = 0)
        self.write32(MMIO_QUEUE_NOTIFY, 0);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        // 阻塞式等待：避免长时间忙等，自旋-让出结合
        let mut attempts = 5000;
        loop {
            let ctrl_queue = self.ctrl_queue.as_mut().unwrap();
            if let Some((desc_id, _len)) = ctrl_queue.used() {
                if desc_id == desc_head { break; }
            }
            if attempts == 0 {
                error!("[VirtIO-GPU] Timeout waiting for control response");
                return Err(DeviceError::OperationFailed);
            }
            attempts -= 1;
            // 让出CPU等待外部中断推进
            crate::task::block_current_and_run_next();
        }

        // 请求完成，清理等待者
        self.pending_waiters.remove(&desc_head);

        if response.response_type != VIRTIO_GPU_RESP_OK_NODATA &&
           response.response_type != VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
            error!("[VirtIO-GPU] Command failed with response: {:#x}", response.response_type);
            return Err(DeviceError::OperationFailed);
        }

        Ok(response)
    }

    fn get_display_info(&mut self) -> Result<(), DeviceError> {
        info!("[VirtIO-GPU] Getting display information");

        let cmd = VirtioGpuCtrlHeader {
            command_type: VIRTIO_GPU_CMD_GET_DISPLAY_INFO,
            flags: 0,
            fence_id: 0,
            ctx_id: 0,
            padding: 0,
        };

        let ctrl_queue = self.ctrl_queue.as_mut()
            .ok_or(DeviceError::InvalidState)?;

        let cmd_bytes = unsafe {
            core::slice::from_raw_parts(
                &cmd as *const VirtioGpuCtrlHeader as *const u8,
                mem::size_of::<VirtioGpuCtrlHeader>()
            )
        };

        let mut response = VirtioGpuRespDisplayInfo {
            hdr: VirtioGpuCtrlHeader {
                command_type: 0,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            pmodes: [VirtioGpuDisplayOne {
                r: VirtioGpuRect { x: 0, y: 0, width: 0, height: 0 },
                enabled: 0,
                flags: 0,
            }; VIRTIO_GPU_MAX_SCANOUTS],
        };

        let response_bytes = unsafe {
            core::slice::from_raw_parts_mut(
                &mut response as *mut VirtioGpuRespDisplayInfo as *mut u8,
                mem::size_of::<VirtioGpuRespDisplayInfo>()
            )
        };

        let desc_head = {
            let head = ctrl_queue.add_buffer(&[cmd_bytes], &mut [response_bytes]);
            if head.is_none() {
                error!("[VirtIO-GPU] Failed to add buffer to queue");
                return Err(DeviceError::OperationFailed);
            }
            // Add to available ring
            ctrl_queue.add_to_avail(head.unwrap());
            head.unwrap()
        };

        // Notify device (control queue id = 0)
        self.write32(MMIO_QUEUE_NOTIFY, 0);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        // Wait for response with timeout
        let mut spins: usize = 0;
        loop {
            let ctrl_queue = self.ctrl_queue.as_mut().unwrap();
            if let Some((desc_id, _len)) = ctrl_queue.used() {
                if desc_id == desc_head {
                    break;
                }
            }
            spins += 1;
            if spins % 1_000_000 == 0 {
                let (used_idx, avail_idx) = ctrl_queue.indices();
                warn!("[VirtIO-GPU] Waiting for display info... used_idx={}, avail_idx={}", used_idx, avail_idx);
            }
            if spins > 20_000_000 {
                error!("[VirtIO-GPU] Timeout waiting for display info response");
                return Err(DeviceError::OperationFailed);
            }
            core::hint::spin_loop();
        }

        if response.hdr.command_type != VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
            error!("[VirtIO-GPU] Get display info failed with response: {:#x}", response.hdr.command_type);
            return Err(DeviceError::OperationFailed);
        }

        self.display_modes.clear();
        for i in 0..VIRTIO_GPU_MAX_SCANOUTS {
            let mode = &response.pmodes[i];
            if mode.enabled != 0 && mode.r.width > 0 && mode.r.height > 0 {
                info!("[VirtIO-GPU] Display {}: {}x{} at ({},{})",
                      i, mode.r.width, mode.r.height, mode.r.x, mode.r.y);
                self.display_modes.push(DisplayMode {
                    width: mode.r.width,
                    height: mode.r.height,
                    enabled: mode.enabled != 0,
                });
            }
        }

        if self.display_modes.is_empty() {
            warn!("[VirtIO-GPU] No enabled displays found, using default 1024x768");
            self.display_modes.push(DisplayMode {
                width: 1024,
                height: 768,
                enabled: true,
            });
        }

        Ok(())
    }

    pub fn get_display_modes(&self) -> &[DisplayMode] {
        &self.display_modes
    }

    pub fn get_current_resolution(&self) -> (u32, u32) {
        (self.current_width, self.current_height)
    }

    pub fn get_framebuffer_info(&self) -> Option<(VirtualAddress, usize)> {
        if let Some(virt_addr) = self.framebuffer_virt_addr {
            Some((virt_addr, self.framebuffer_size))
        } else {
            None
        }
    }

    fn allocate_next_resource_id(&mut self) -> u32 {
        let id = self.resource_id_counter;
        self.resource_id_counter += 1;
        id
    }

    pub fn setup_framebuffer(&mut self, width: u32, height: u32) -> Result<(), DeviceError> {
        info!("[VirtIO-GPU] Setting up framebuffer {}x{}", width, height);

        let state = self.state();
        if !(state == DeviceState::Ready || state == DeviceState::Initializing) {
            return Err(DeviceError::InvalidState);
        }

        let bytes_per_pixel = 4;
        let framebuffer_size = (width * height * bytes_per_pixel) as usize;

        let resource_id = self.allocate_next_resource_id();

        let create_cmd = VirtioGpuResourceCreate2d {
            hdr: VirtioGpuCtrlHeader {
                command_type: VIRTIO_GPU_CMD_RESOURCE_CREATE_2D,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            resource_id,
            format: self.current_format,
            width,
            height,
        };

        let response = self.send_command(&create_cmd)?;
        if response.response_type != VIRTIO_GPU_RESP_OK_NODATA {
            error!("[VirtIO-GPU] Failed to create 2D resource");
            return Err(DeviceError::OperationFailed);
        }

        // 分配DMA内存
        let page_count = (framebuffer_size + 4095) / 4096;
        let phys_addr = KERNEL_SPACE.get().unwrap().lock()
            .alloc_dma_pages(page_count)
            .map_err(|e| {
                error!("[VirtIO-GPU] Failed to allocate DMA memory: {:?}", e);
                DeviceError::OperationFailed
            })?;

        let virt_addr = KERNEL_SPACE.get().unwrap().lock()
            .map_dma(phys_addr, framebuffer_size)
            .map_err(|e| {
                error!("[VirtIO-GPU] Failed to map DMA memory: {:?}", e);
                DeviceError::OperationFailed
            })?;

        // 清零framebuffer内存
        unsafe {
            ptr::write_bytes(virt_addr.as_usize() as *mut u8, 0, framebuffer_size);
        }

        let mem_entry = VirtioGpuMemEntry {
            addr: phys_addr.as_usize() as u64,
            length: framebuffer_size as u32,
            padding: 0,
        };

        let attach_cmd = VirtioGpuResourceAttachBacking {
            hdr: VirtioGpuCtrlHeader {
                command_type: VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            resource_id,
            nr_entries: 1,
        };

        let ctrl_queue = self.ctrl_queue.as_mut()
            .ok_or(DeviceError::InvalidState)?;

        let attach_cmd_bytes = unsafe {
            core::slice::from_raw_parts(
                &attach_cmd as *const VirtioGpuResourceAttachBacking as *const u8,
                mem::size_of::<VirtioGpuResourceAttachBacking>()
            )
        };

        let mem_entry_bytes = unsafe {
            core::slice::from_raw_parts(
                &mem_entry as *const VirtioGpuMemEntry as *const u8,
                mem::size_of::<VirtioGpuMemEntry>()
            )
        };

        let mut response = VirtioGpuCtrlResponse {
            response_type: 0,
            flags: 0,
            fence_id: 0,
            ctx_id: 0,
            padding: 0,
        };

        let response_bytes = unsafe {
            core::slice::from_raw_parts_mut(
                &mut response as *mut VirtioGpuCtrlResponse as *mut u8,
                mem::size_of::<VirtioGpuCtrlResponse>()
            )
        };

        let desc_head = {
            let head = ctrl_queue.add_buffer(&[attach_cmd_bytes, mem_entry_bytes], &mut [response_bytes]);
            if head.is_none() {
                error!("[VirtIO-GPU] Failed to add buffer to queue");
                return Err(DeviceError::OperationFailed);
            }
            // Add to available ring
            ctrl_queue.add_to_avail(head.unwrap());
            head.unwrap()
        };

        // Notify device
        self.write32(0x50, 0);

        // Wait for response
        loop {
            let ctrl_queue = self.ctrl_queue.as_mut().unwrap();
            if let Some((desc_id, _len)) = ctrl_queue.used() {
                if desc_id == desc_head {
                    break;
                }
            }
        }

        if response.response_type != VIRTIO_GPU_RESP_OK_NODATA {
            error!("[VirtIO-GPU] Failed to attach backing to resource");
            return Err(DeviceError::OperationFailed);
        }

        let scanout_cmd = VirtioGpuSetScanout {
            hdr: VirtioGpuCtrlHeader {
                command_type: VIRTIO_GPU_CMD_SET_SCANOUT,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            r: VirtioGpuRect {
                x: 0,
                y: 0,
                width,
                height,
            },
            scanout_id: 0,
            resource_id,
        };

        let response = self.send_command(&scanout_cmd)?;
        if response.response_type != VIRTIO_GPU_RESP_OK_NODATA {
            error!("[VirtIO-GPU] Failed to set scanout");
            return Err(DeviceError::OperationFailed);
        }

        self.framebuffer_resource_id = Some(resource_id);
        self.framebuffer_phys_addr = Some(phys_addr);
        self.framebuffer_virt_addr = Some(virt_addr);
        self.framebuffer_size = framebuffer_size;
        self.current_width = width;
        self.current_height = height;

        info!("[VirtIO-GPU] Framebuffer setup complete: {}x{}, {} bytes",
              width, height, framebuffer_size);

        // 在 GPU 完成 framebuffer 建立后，创建并注册全局 Framebuffer，供 GUI 系统调用使用
        // 将格式映射为通用 Framebuffer 的像素格式
        let fb_format = match self.current_format {
            VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM | VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM => PixelFormat::BGRA8888,
            VIRTIO_GPU_FORMAT_A8R8G8B8_UNORM | VIRTIO_GPU_FORMAT_X8R8G8B8_UNORM => PixelFormat::BGRA8888,
            VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM | VIRTIO_GPU_FORMAT_R8G8B8X8_UNORM => PixelFormat::RGBA8888,
            _ => PixelFormat::RGBA8888,
        };

        let fb_info = FramebufferInfo::new(self.current_width, self.current_height, fb_format);
        let fb_buffer = virt_addr.as_usize();

        // flush 回调：找到 GPU 设备并触发 flush 到宿主端（支持可选矩形列表）
        let flush_cb: Option<Box<dyn Fn(Option<&[crate::drivers::framebuffer::Rect]>) -> Result<(), DeviceError> + Send + Sync>> = Some(Box::new(|rects_opt| {
            let display_ids = find_devices_by_type(DeviceType::Display);
            for id in display_ids {
                if let Some(dev_arc) = get_device(id) {
                    let mut dev = dev_arc.lock();
                    if let Some(gpu) = dev.as_any_mut().downcast_mut::<VirtioGpuDevice>() {
                        return match rects_opt {
                            Some(rects) => gpu.flush_framebuffer_rects(rects),
                            None => gpu.flush_framebuffer(),
                        };
                    }
                }
            }
            Err(DeviceError::DeviceNotFound)
        }));

        let fb = GenericFramebuffer::new(fb_info, fb_buffer, flush_cb);
        set_global_framebuffer(Arc::new(Mutex::new(fb)));

        Ok(())
    }

    pub fn flush_framebuffer(&mut self) -> Result<(), DeviceError> {
        let resource_id = self.framebuffer_resource_id
            .ok_or(DeviceError::InvalidState)?;

        let transfer_cmd = VirtioGpuTransferToHost2d {
            hdr: VirtioGpuCtrlHeader {
                command_type: VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            r: VirtioGpuRect {
                x: 0,
                y: 0,
                width: self.current_width,
                height: self.current_height,
            },
            offset: 0,
            resource_id,
            padding: 0,
        };

        let response = self.send_command(&transfer_cmd)?;
        if response.response_type != VIRTIO_GPU_RESP_OK_NODATA {
            error!("[VirtIO-GPU] Failed to transfer to host");
            return Err(DeviceError::OperationFailed);
        }

        let flush_cmd = VirtioGpuResourceFlush {
            hdr: VirtioGpuCtrlHeader {
                command_type: VIRTIO_GPU_CMD_RESOURCE_FLUSH,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            r: VirtioGpuRect {
                x: 0,
                y: 0,
                width: self.current_width,
                height: self.current_height,
            },
            resource_id,
            padding: 0,
        };

        let response = self.send_command(&flush_cmd)?;
        if response.response_type != VIRTIO_GPU_RESP_OK_NODATA {
            error!("[VirtIO-GPU] Failed to flush resource");
            return Err(DeviceError::OperationFailed);
        }

        Ok(())
    }

    pub fn flush_framebuffer_rects(&mut self, rects: &[crate::drivers::framebuffer::Rect]) -> Result<(), DeviceError> {
        let resource_id = self.framebuffer_resource_id
            .ok_or(DeviceError::InvalidState)?;

        // 对每个矩形执行 transfer + flush；可以进一步合并/裁剪
        for r in rects.iter() {
            let transfer_cmd = VirtioGpuTransferToHost2d {
                hdr: VirtioGpuCtrlHeader { command_type: VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D, flags: 0, fence_id: 0, ctx_id: 0, padding: 0 },
                r: VirtioGpuRect { x: r.x as u32, y: r.y as u32, width: r.width as u32, height: r.height as u32 },
                offset: 0,
                resource_id,
                padding: 0,
            };
            let response = self.send_command(&transfer_cmd)?;
            if response.response_type != VIRTIO_GPU_RESP_OK_NODATA {
                error!("[VirtIO-GPU] Failed to transfer rect to host");
                return Err(DeviceError::OperationFailed);
            }

            let flush_cmd = VirtioGpuResourceFlush {
                hdr: VirtioGpuCtrlHeader { command_type: VIRTIO_GPU_CMD_RESOURCE_FLUSH, flags: 0, fence_id: 0, ctx_id: 0, padding: 0 },
                r: VirtioGpuRect { x: r.x as u32, y: r.y as u32, width: r.width as u32, height: r.height as u32 },
                resource_id,
                padding: 0,
            };
            let response = self.send_command(&flush_cmd)?;
            if response.response_type != VIRTIO_GPU_RESP_OK_NODATA {
                error!("[VirtIO-GPU] Failed to flush rect");
                return Err(DeviceError::OperationFailed);
            }
        }

        Ok(())
    }

    pub fn write_pixel(&mut self, x: u32, y: u32, color: u32) -> Result<(), DeviceError> {
        if x >= self.current_width || y >= self.current_height {
            return Err(DeviceError::OperationFailed);
        }

        let virt_addr = self.framebuffer_virt_addr
            .ok_or(DeviceError::InvalidState)?;

        let offset = ((y * self.current_width + x) * 4) as usize;

        unsafe {
            let pixel_ptr = (virt_addr.as_usize() + offset) as *mut u32;
            ptr::write_volatile(pixel_ptr, color);
        }

        Ok(())
    }

    pub fn fill_rect(&mut self, x: u32, y: u32, width: u32, height: u32, color: u32) -> Result<(), DeviceError> {
        for dy in 0..height {
            for dx in 0..width {
                if let Err(e) = self.write_pixel(x + dx, y + dy, color) {
                    if x + dx >= self.current_width || y + dy >= self.current_height {
                        break;
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    pub fn clear_screen(&mut self, color: u32) -> Result<(), DeviceError> {
        self.fill_rect(0, 0, self.current_width, self.current_height, color)
    }
}

impl Device for VirtioGpuDevice {
    fn device_type(&self) -> DeviceType {
        DeviceType::Display
    }

    fn device_id(&self) -> u32 {
        VIRTIO_GPU_DEVICE_ID
    }

    fn vendor_id(&self) -> u32 {
        0x1AF4 // Red Hat, Inc. (VirtIO vendor)
    }

    fn device_name(&self) -> String {
        format!("VirtIO GPU @ {:#x}", self.base_addr)
    }

    fn driver_name(&self) -> String {
        "virtio-gpu".to_string()
    }

    fn state(&self) -> DeviceState {
        self.base.state()
    }

    fn probe(&mut self) -> Result<bool, DeviceError> {
        info!("[VirtIO-GPU] Probing VirtIO GPU device");

        self.base.set_state(DeviceState::Probing);

        if !self.check_device_id() {
            error!("[VirtIO-GPU] Device ID mismatch");
            self.base.set_state(DeviceState::Failed);
            return Ok(false);
        }

        info!("[VirtIO-GPU] VirtIO GPU device detected");
        Ok(true)
    }

    fn initialize(&mut self) -> Result<(), DeviceError> {
        info!("[VirtIO-GPU] Initializing VirtIO GPU device");

        self.base.set_state(DeviceState::Initializing);

        self.reset_device();
        self.negotiate_features()?;
        self.setup_virtqueues()?;
        // 设置DRIVER_OK
        self.set_status_bit(VIRTIO_STATUS_DRIVER_OK);

        self.get_display_info()?;

        // 选择一个分辨率并建立framebuffer，激活显示
        let (width, height) = if let Some(mode) = self.display_modes.first() {
            (mode.width, mode.height)
        } else {
            (1024, 768)
        };

        if let Err(e) = self.setup_framebuffer(width, height) {
            error!("[VirtIO-GPU] Failed to setup framebuffer: {:?}", e);
            return Err(e);
        }

        // 清屏并刷新到host，确保窗口激活
        let _ = self.clear_screen(0xFF000000); // 黑色
        let _ = self.flush_framebuffer();

        self.base.set_state(DeviceState::Ready);
        info!("[VirtIO-GPU] VirtIO GPU device initialized successfully");

        Ok(())
    }

    fn reset(&mut self) -> Result<(), DeviceError> {
        info!("[VirtIO-GPU] Resetting device");
        self.reset_device();
        self.base.set_state(DeviceState::Uninitialized);
        Ok(())
    }

    fn shutdown(&mut self) -> Result<(), DeviceError> {
        info!("[VirtIO-GPU] Shutting down device");
        self.reset_device();
        Ok(())
    }

    fn remove(&mut self) -> Result<(), DeviceError> {
        info!("[VirtIO-GPU] Removing device");

        // 清理framebuffer内存
        if let (Some(virt_addr), Some(_phys_addr)) = (self.framebuffer_virt_addr, self.framebuffer_phys_addr) {
            if let Err(e) = KERNEL_SPACE.get().unwrap().lock().unmap_dma(virt_addr, self.framebuffer_size) {
                error!("[VirtIO-GPU] Failed to unmap DMA memory: {:?}", e);
            }
        }

        // 清理队列
        self.ctrl_queue = None;
        self.cursor_queue = None;
        self.display_modes.clear();

        self.reset_device();
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), DeviceError> {
        info!("[VirtIO-GPU] Suspending device");
        self.base.set_state(DeviceState::Suspended);
        Ok(())
    }

    fn resume(&mut self) -> Result<(), DeviceError> {
        info!("[VirtIO-GPU] Resuming device");
        self.base.set_state(DeviceState::Ready);
        Ok(())
    }

    fn bus(&self) -> Arc<dyn Bus> {
        // 返回一个虚拟总线实现
        Arc::new(DummyBus::new())
    }

    fn resources(&self) -> Vec<Resource> {
        use crate::drivers::hal::resource::MemoryRange;
        vec![
            Resource::Memory(MemoryRange::with_attributes(
                self.base_addr,
                0x1000,
                false,  // not cached
                true,   // writable
                false,  // not executable
            ))
        ]
    }

    fn request_resources(&mut self, _resource_manager: &mut dyn ResourceManager) -> Result<(), DeviceError> {
        Ok(())
    }

    fn release_resources(&mut self, _resource_manager: &mut dyn ResourceManager) -> Result<(), DeviceError> {
        Ok(())
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

// 简单的GPU中断处理器：确认中断并尝试推进控制队列 used ring，用于唤醒等待路径
pub struct VirtioGpuIrqHandler;
impl InterruptHandler for VirtioGpuIrqHandler {
    fn handle_interrupt(&self, _vector: InterruptVector) -> Result<(), crate::drivers::hal::interrupt::InterruptError> {
        // 遍历显示类设备，找到GPU并让其 drain used ring
        let display_ids = find_devices_by_type(DeviceType::Display);
        for id in display_ids {
            if let Some(dev_arc) = get_device(id) {
                let mut dev = dev_arc.lock();
                if let Some(gpu) = dev.as_any_mut().downcast_mut::<VirtioGpuDevice>() {
                    gpu.irq_ack_and_drain();
                }
            }
        }
        Ok(())
    }
    fn can_handle(&self, _vector: InterruptVector) -> bool { true }
    fn name(&self) -> &str { "virtio-gpu-irq" }
}

// 简单的 Bus 实现用于 VirtIO GPU
struct DummyBus {
    bus_type: crate::drivers::hal::bus::BusType,
}

impl DummyBus {
    pub fn new() -> Self {
        DummyBus {
            bus_type: crate::drivers::hal::bus::BusType::VirtIO,
        }
    }
}

impl Bus for DummyBus {
    fn bus_type(&self) -> crate::drivers::hal::bus::BusType {
        self.bus_type
    }

    fn read_u8(&self, _offset: usize) -> Result<u8, crate::drivers::hal::bus::BusError> {
        Err(crate::drivers::hal::bus::BusError::NotSupported)
    }

    fn read_u16(&self, _offset: usize) -> Result<u16, crate::drivers::hal::bus::BusError> {
        Err(crate::drivers::hal::bus::BusError::NotSupported)
    }

    fn read_u32(&self, _offset: usize) -> Result<u32, crate::drivers::hal::bus::BusError> {
        Err(crate::drivers::hal::bus::BusError::NotSupported)
    }

    fn read_u64(&self, _offset: usize) -> Result<u64, crate::drivers::hal::bus::BusError> {
        Err(crate::drivers::hal::bus::BusError::NotSupported)
    }

    fn write_u8(&self, _offset: usize, _value: u8) -> Result<(), crate::drivers::hal::bus::BusError> {
        Err(crate::drivers::hal::bus::BusError::NotSupported)
    }

    fn write_u16(&self, _offset: usize, _value: u16) -> Result<(), crate::drivers::hal::bus::BusError> {
        Err(crate::drivers::hal::bus::BusError::NotSupported)
    }

    fn write_u32(&self, _offset: usize, _value: u32) -> Result<(), crate::drivers::hal::bus::BusError> {
        Err(crate::drivers::hal::bus::BusError::NotSupported)
    }

    fn write_u64(&self, _offset: usize, _value: u64) -> Result<(), crate::drivers::hal::bus::BusError> {
        Err(crate::drivers::hal::bus::BusError::NotSupported)
    }

    fn base_address(&self) -> usize {
        0
    }

    fn size(&self) -> usize {
        0
    }

    fn is_accessible(&self) -> bool {
        false
    }
}

