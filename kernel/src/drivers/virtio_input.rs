use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use spin::Mutex;

use crate::drivers::hal::{
    bus::Bus,
    device::{Device, DeviceError, DeviceState, DeviceType},
    interrupt::{InterruptHandler, InterruptVector},
    resource::{Resource, ResourceManager},
    virtio::{
        VIRTIO_CONFIG_S_ACKNOWLEDGE, VIRTIO_CONFIG_S_DRIVER, VIRTIO_CONFIG_S_DRIVER_OK,
        VirtIODevice,
    },
};
use crate::drivers::virtio_queue::VirtQueue;
use crate::fs::{
    FileSystemError,
    inode::{Inode, InodeType},
};
use crate::memory::PAGE_SIZE;

// VirtIO device ID for input
pub const VIRTIO_ID_INPUT: u32 = 18;

// virtio_input_event (per spec): le16 type, le16 code, le32 value
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VirtioInputEvent {
    pub type_: u16,
    pub code: u16,
    pub value: u32,
}

/// /dev/input/eventX 节点，实现为只读字符设备，支持 read/poll
pub struct InputDeviceNode {
    inner: Mutex<InputDeviceInner>,
}

struct InputDeviceInner {
    buffer: VecDeque<u8>,
    poll_waiters: BTreeMap<usize, (Weak<crate::task::TaskControlBlock>, u32)>,
}

impl InputDeviceNode {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(InputDeviceInner {
                buffer: VecDeque::with_capacity(4096),
                poll_waiters: BTreeMap::new(),
            }),
        })
    }

    pub fn push_event_bytes(&self, bytes: &[u8]) {
        let mut to_wakeup: Vec<Weak<crate::task::TaskControlBlock>> = Vec::new();
        {
            let mut inner = self.inner.lock();
            for b in bytes {
                inner.buffer.push_back(*b);
            }
            // 唤醒关心可读事件的等待者
            let mut dead: Vec<usize> = Vec::new();
            for (pid, (w, interests)) in inner.poll_waiters.iter() {
                if (interests & POLLIN) != 0 {
                    to_wakeup.push(w.clone());
                }
                if w.upgrade().is_none() {
                    dead.push(*pid);
                }
            }
            for pid in dead {
                inner.poll_waiters.remove(&pid);
            }
        }
        for w in to_wakeup {
            if let Some(t) = w.upgrade() {
                t.wakeup();
            }
        }
    }
}

// poll 常量需与 sys_poll 保持一致
const POLLIN: u32 = 0x0001;
const POLLOUT: u32 = 0x0004;

impl Inode for InputDeviceNode {
    fn inode_type(&self) -> InodeType {
        InodeType::Device
    }
    fn size(&self) -> u64 {
        0
    }
    fn read_at(&self, _off: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        if buf.is_empty() {
            return Ok(0);
        }
        // 非阻塞读取：若无数据立即返回0，交由上层通过 poll 等待
        let mut read_len = 0usize;
        {
            let mut inner = self.inner.lock();
            while read_len < buf.len() {
                if let Some(b) = inner.buffer.pop_front() {
                    buf[read_len] = b;
                    read_len += 1;
                } else {
                    break;
                }
            }
        }
        Ok(read_len)
    }
    fn write_at(&self, _off: u64, _buf: &[u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    fn list_dir(&self) -> Result<Vec<String>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn find_child(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn create_file(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn create_directory(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn remove(&self, _name: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::NotDirectory)
    }
    fn truncate(&self, _size: u64) -> Result<(), FileSystemError> {
        Ok(())
    }
    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
    fn poll_mask(&self) -> u32 {
        let inner = self.inner.lock();
        if inner.buffer.is_empty() { 0 } else { POLLIN }
    }
    fn register_poll_waiter(&self, interests: u32, task: Arc<crate::task::TaskControlBlock>) {
        let mut inner = self.inner.lock();
        inner
            .poll_waiters
            .insert(task.pid(), (Arc::downgrade(&task), interests));
    }
    fn clear_poll_waiter(&self, task_pid: usize) {
        let mut inner = self.inner.lock();
        inner.poll_waiters.remove(&task_pid);
    }
}

/// 全局输入设备注册表：路径 -> 节点
static INPUT_REGISTRY: Mutex<BTreeMap<String, Arc<InputDeviceNode>>> = Mutex::new(BTreeMap::new());
static NEXT_EVENT_INDEX: spin::Mutex<u32> = spin::Mutex::new(0);

pub fn register_input_node_auto(node: Arc<InputDeviceNode>) -> String {
    let mut idx = NEXT_EVENT_INDEX.lock();
    let name = alloc::format!("/dev/input/event{}", *idx);
    *idx += 1;
    let mut reg = INPUT_REGISTRY.lock();
    reg.insert(name.clone(), node);
    name
}

pub fn open_input_device(path: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
    let reg = INPUT_REGISTRY.lock();
    reg.get(path)
        .map(|n| n.clone() as Arc<dyn Inode>)
        .ok_or(FileSystemError::NotFound)
}

pub fn list_input_nodes() -> alloc::vec::Vec<alloc::string::String> {
    let reg = INPUT_REGISTRY.lock();
    reg.keys().cloned().collect()
}

/// Virtio 输入设备
pub struct VirtioInputDevice {
    device: VirtIODevice,
    event_queue: alloc::sync::Arc<spin::Mutex<VirtQueue>>,
    /// 每个描述符 head 索引到缓冲区下标的映射
    desc_to_buf_index: Mutex<BTreeMap<u16, usize>>,
    /// 固定事件缓冲区
    buffers: Mutex<Vec<Box<[u8; 8]>>>,
    /// 用户可见的输入节点
    pub node: Arc<InputDeviceNode>,
}

impl VirtioInputDevice {
    pub fn new(base_addr: usize) -> Option<Arc<Self>> {
        info!("[VirtIO-Input] Probing device at {:#x}", base_addr);
        // 创建 VirtIODevice 包装
        let mut virt = VirtIODevice::new(base_addr, 0x1000).ok()?;
        info!("[VirtIO-Input] VirtIODevice created");
        // 基本初始化流程对齐 console 驱动
        virt.initialize().ok()?;
        info!("[VirtIO-Input] Device initialized");
        let device_features = virt.device_features().ok().unwrap_or(0);
        debug!("[VirtIO-Input] Device features: {:#x}", device_features);
        // 简化：暂不启用任何特性
        virt.set_driver_features(0).ok()?;
        debug!("[VirtIO-Input] Driver features set to 0");
        let status = virt.get_status().ok()?;
        virt.set_status(status | crate::drivers::hal::virtio::VIRTIO_CONFIG_S_FEATURES_OK)
            .ok()?;
        debug!("[VirtIO-Input] FEATURES_OK set");
        // 可选校验
        let _ = virt.get_status().ok();
        // 设置 guest page size（legacy 设备必需）
        virt.set_guest_page_size(4096).ok()?;
        debug!("[VirtIO-Input] Guest page size set");

        // 设置事件接收队列0
        virt.select_queue(0).ok()?;
        debug!("[VirtIO-Input] Queue 0 selected");
        let max = virt.queue_max_size().ok()?;
        if max == 0 {
            return None;
        }
        let qsize = core::cmp::min(max, 128);
        debug!("[VirtIO-Input] Queue max size={} use size={}", max, qsize);
        let queue = alloc::sync::Arc::new(spin::Mutex::new(VirtQueue::new(qsize as u16, 0)?));
        virt.set_queue_size(qsize).ok()?;
        virt.set_queue_align(4096).ok()?;
        // 设置队列物理页帧号（PFN） - 与 console 驱动一致，从队列互斥体计算
        let pfn = (queue.lock().physical_address().as_usize() >> 12) as u32;
        virt.set_queue_pfn(pfn).ok()?;
        virt.set_queue_ready(1).ok()?;
        debug!("[VirtIO-Input] Queue ready (pfn={:#x})", pfn);

        let node = InputDeviceNode::new();
        let dev = Arc::new(Self {
            device: virt,
            event_queue: queue,
            desc_to_buf_index: Mutex::new(BTreeMap::new()),
            buffers: Mutex::new(Vec::new()),
            node,
        });

        // 预投递接收缓冲区
        dev.setup_receive_buffers();
        // 驱动就绪
        let status = dev.device.get_status().ok()?;
        dev.device
            .set_status(status | VIRTIO_CONFIG_S_DRIVER_OK)
            .ok()?;
        info!("[VirtIO-Input] Driver OK set");
        Some(dev)
    }

    fn setup_receive_buffers(self: &Arc<Self>) {
        // 固定锁顺序：event_queue -> desc_to_buf_index -> buffers
        let qsize = { self.event_queue.lock().size };
        let mut posted_any = false;
        for _ in 0..qsize {
            // 先准备缓冲
            let mut b = Box::new([0u8; 8]);
            // 限定锁作用域，避免在持锁时触发中断后死锁
            let head_opt = {
                let mut q = self.event_queue.lock();
                let mut outs: [&mut [u8]; 1] = [&mut b[..]];
                q.add_buffer(&[], &mut outs)
            };
            if let Some(head) = head_opt {
                // 记录映射并存储缓冲区
                {
                    let mut map = self.desc_to_buf_index.lock();
                    let mut bufs = self.buffers.lock();
                    let idx = bufs.len();
                    bufs.push(b);
                    map.insert(head, idx);
                }
                // 再次短锁加入 avail
                {
                    let mut q = self.event_queue.lock();
                    q.add_to_avail(head);
                }
                posted_any = true;
            }
        }
        if posted_any {
            // 首次通知延后到IRQ注册完成后
        }
    }

    pub fn enable_notifications(&self) {
        let _ = self.device.notify_queue(0);
    }

    fn recycle_and_renew(&self, id: u16) {
        // 将对应缓冲重新投递
        let mut q = self.event_queue.lock();
        let idx_opt = { self.desc_to_buf_index.lock().get(&id).cloned() };
        if let Some(idx) = idx_opt {
            let mut bufs = self.buffers.lock();
            let mut outs: [&mut [u8]; 1] = [&mut bufs[idx][..]];
            if let Some(new_head) = q.add_buffer(&[], &mut outs) {
                // 更新映射
                self.desc_to_buf_index.lock().remove(&id);
                self.desc_to_buf_index.lock().insert(new_head, idx);
                q.add_to_avail(new_head);
            }
        }
        let _ = self.device.notify_queue(0);
    }

    pub fn drain_used_and_push_events(&self) {
        let mut produced_any = false;
        loop {
            let next_used = { self.event_queue.lock().used() };
            let (id, len) = match next_used {
                Some(x) => x,
                None => break,
            };
            if len >= 8 {
                let idx_opt = { self.desc_to_buf_index.lock().get(&id).cloned() };
                if let Some(idx) = idx_opt {
                    let buf = self.buffers.lock();
                    let bytes: &[u8] = &buf[idx][..8];
                    // 推送到用户缓冲
                    self.node.push_event_bytes(bytes);
                    produced_any = true;
                }
            }
            // 重新投递该缓冲
            if let Some(idx) = { self.desc_to_buf_index.lock().remove(&id) } {
                let mut bufs = self.buffers.lock();
                let mut outs: [&mut [u8]; 1] = [&mut bufs[idx][..]];
                let head_opt = { self.event_queue.lock().add_buffer(&[], &mut outs) };
                if let Some(new_head) = head_opt {
                    {
                        let mut map = self.desc_to_buf_index.lock();
                        map.insert(new_head, idx);
                    }
                    {
                        self.event_queue.lock().add_to_avail(new_head);
                    }
                }
            }
        }
        if produced_any {
            // 已在 push 时唤醒 poller
        }
    }
}

impl Device for VirtioInputDevice {
    fn device_type(&self) -> DeviceType {
        DeviceType::Input
    }
    fn device_id(&self) -> u32 {
        VIRTIO_ID_INPUT
    }
    fn vendor_id(&self) -> u32 {
        0x1af4
    }
    fn device_name(&self) -> String {
        "VirtIO-Input".to_string()
    }
    fn driver_name(&self) -> String {
        "virtio-input".to_string()
    }
    fn state(&self) -> DeviceState {
        DeviceState::Ready
    }
    fn probe(&mut self) -> Result<bool, DeviceError> {
        Ok(true)
    }
    fn initialize(&mut self) -> Result<(), DeviceError> {
        Ok(())
    }
    fn reset(&mut self) -> Result<(), DeviceError> {
        Ok(())
    }
    fn shutdown(&mut self) -> Result<(), DeviceError> {
        Ok(())
    }
    fn remove(&mut self) -> Result<(), DeviceError> {
        Ok(())
    }
    fn suspend(&mut self) -> Result<(), DeviceError> {
        Ok(())
    }
    fn resume(&mut self) -> Result<(), DeviceError> {
        Ok(())
    }
    fn bus(&self) -> alloc::sync::Arc<dyn Bus> {
        self.device.bus()
    }
    fn resources(&self) -> alloc::vec::Vec<Resource> {
        alloc::vec::Vec::new()
    }
    fn request_resources(&mut self, _rm: &mut dyn ResourceManager) -> Result<(), DeviceError> {
        Ok(())
    }
    fn release_resources(&mut self, _rm: &mut dyn ResourceManager) -> Result<(), DeviceError> {
        Ok(())
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

pub struct VirtioInputIrqHandler(pub alloc::sync::Arc<VirtioInputDevice>);
impl InterruptHandler for VirtioInputIrqHandler {
    fn handle_interrupt(
        &self,
        _vector: InterruptVector,
    ) -> Result<(), crate::drivers::hal::interrupt::InterruptError> {
        // 先读取并清除设备侧中断状态
        if let Ok(isr) = self.0.device.interrupt_status() {
            if isr != 0 {
                let _ = self.0.device.interrupt_ack(isr);
            }
        }
        self.0.drain_used_and_push_events();
        Ok(())
    }
    fn can_handle(&self, _vector: InterruptVector) -> bool {
        true
    }
    fn name(&self) -> &str {
        "virtio-input-irq"
    }
}
