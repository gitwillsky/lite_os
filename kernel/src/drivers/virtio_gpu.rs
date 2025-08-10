use core::mem;
use core::ptr;
use alloc::{boxed::Box, sync::Arc, vec::Vec, format, string::{String, ToString}, vec};
use spin::Mutex;
use crate::memory::{KERNEL_SPACE, address::{PhysicalAddress, VirtualAddress}};
use crate::drivers::{
    Device, DeviceType, DeviceState, DeviceError, GenericDevice,
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
}

impl VirtioGpuDevice {
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
        self.write32(0x70, 0);
        
        while self.read32(0x70) != 0 {
            core::hint::spin_loop();
        }
    }

    fn negotiate_features(&mut self) -> Result<(), DeviceError> {
        info!("[VirtIO-GPU] Negotiating features");
        
        let device_features = self.read32(0x10);
        info!("[VirtIO-GPU] Device features: {:#x}", device_features);
        
        let mut driver_features = 0u32;
        if device_features & (1 << VIRTIO_GPU_F_EDID) != 0 {
            driver_features |= 1 << VIRTIO_GPU_F_EDID;
            info!("[VirtIO-GPU] Enabling EDID support");
        }
        
        self.write32(0x20, driver_features);
        self.write32(0x70, self.read32(0x70) | 8);
        
        let actual_features = self.read32(0x20);
        if actual_features != driver_features {
            warn!("[VirtIO-GPU] Feature negotiation mismatch: expected {:#x}, got {:#x}", 
                  driver_features, actual_features);
        }
        
        Ok(())
    }

    fn setup_virtqueues(&mut self) -> Result<(), DeviceError> {
        info!("[VirtIO-GPU] Setting up VirtQueues");
        
        // 创建控制队列 (队列大小128，队列标记0)
        match VirtQueue::new(128, 0) {
            Some(queue) => self.ctrl_queue = Some(queue),
            None => {
                error!("[VirtIO-GPU] Failed to create control queue");
                return Err(DeviceError::InitializationFailed);
            }
        }
        
        // 创建光标队列 (队列大小16，队列标记1)
        match VirtQueue::new(16, 1) {
            Some(queue) => self.cursor_queue = Some(queue),
            None => {
                error!("[VirtIO-GPU] Failed to create cursor queue");
                return Err(DeviceError::InitializationFailed);
            }
        }
        
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
        
        if self.state() != DeviceState::Ready {
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
        
        self.write32(0x70, self.read32(0x70) | 4);
        
        self.get_display_info()?;
        
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

