use crate::sync::Mutex;
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU8, Ordering};

mod control;
mod queue_pair;
mod rx_slots;
use control::ControlQueue;
use queue_pair::QueueState;

use super::{
    VIRTIO_CONFIG_S_DRIVER_OK, VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1,
    VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING, VirtIODevice,
    completion_irq::VirtIOCompletionIrq, queue::VirtQueue,
};
use crate::{interrupt::InterruptError, interrupt::InterruptHandler, interrupt::InterruptVector};
use device_model::network::{
    NetworkController, NetworkError, NetworkProvider, NetworkRssCapabilities,
    NetworkRssConfiguration, RECEIVE_HASH_TYPES,
};

const VIRTIO_NET_F_CSUM: u64 = 1;
const VIRTIO_NET_F_CTRL_VQ: u64 = 1 << 17;
const VIRTIO_NET_F_MQ: u64 = 1 << 22;
const VIRTIO_NET_F_RSS: u64 = 1 << 60;
const RSS_KEY_OFFSET: usize = 17;
const RSS_TABLE_OFFSET: usize = 18;
const RSS_TYPES_OFFSET: usize = 20;
const MAX_PAIRS_OFFSET: usize = 8;
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
const VIRTIO_NET_F_SPEED_DUPLEX: u64 = 1 << 63;
const STATUS_OFFSET: usize = 6;
const SPEED_OFFSET: usize = 12;
const VIRTIO_NET_S_LINK_UP: u16 = 1;
const QUEUE_SIZE: u16 = 64;
// VirtIO 1.x 非 legacy header 固定包含 num_buffers；沿用 10 字节 legacy 形状会让
// device 把 Ethernet frame 的前两个字节误当作 header，并使全部 TX/RX 帧错位。
const VIRTIO_NET_HEADER_SIZE: usize = 12;
const RX_BUFFER_SIZE: usize = 2048;
const MAX_ETHERNET_FRAME: usize = 1514;
// 64-byte 对齐的密集槽，保持头部/数据 cache line 边界，不为每个 1526-byte frame 独占一页。
const TX_BUFFER_SIZE: usize = (VIRTIO_NET_HEADER_SIZE + MAX_ETHERNET_FRAME).next_multiple_of(64);

/// 物理 NIC 与传输 owner；活动队列按独占会话请求创建，不把 CPU 策略固化到驱动。
pub(crate) struct VirtIONetworkDevice {
    device: VirtIODevice,
    mac: [u8; 6],
    features: u64,
    maximum_pairs: u16,
    rss: Option<NetworkRssCapabilities>,
    completion_irq: VirtIOCompletionIrq,
    // OWNER: 冷配置租约与 IRQ 回调；Some(Weak) 即使已不能 upgrade，也要等旧 Drop 撤销 DMA 后清空。
    // 否则新 OPEN 可能在旧队列正在析构时重新启用设备。IRQ 不持有活动队列强引用。
    lease: Mutex<Lease>,
    // OWNER: 已 stop 的最后一个临时引用退休后唤醒重开者；避免把短暂析构窗口误报为永久安装失败。
    retired: native_runtime::Event,
}
struct Lease {
    active: Option<Weak<ActiveNetwork>>,
    activity: Vec<Option<device_model::Activity>>,
}
/// 同一独占会话的队列、DMA 与失败状态；最后一个持有者先 reset 再释放字段。
struct ActiveNetwork {
    owner: Arc<VirtIONetworkDevice>,
    // FLAG: true 表示已发布到 lease，Drop 必须回收租约；构造失败时仍持配置锁，只 reset 不重入锁。
    lease_published: bool,
    queues: Vec<Mutex<QueueState>>,
    // OWNER: 0=运行，1=请求撤离，2=唯一 reset owner，3=DMA 已停止；stop 必须等到 3。
    failed: AtomicU8,
    control: Option<Mutex<ControlQueue>>,
}
impl core::ops::Deref for ActiveNetwork {
    type Target = VirtIONetworkDevice;
    fn deref(&self) -> &VirtIONetworkDevice {
        &self.owner
    }
}
impl VirtIONetworkDevice {
    /// 从 DTB 的 MMIO 窗口发现 NIC；不创建或启动数据队列。
    /// # Returns
    /// 发现及能力协商成功的 controller，失败返回 None。
    pub(crate) fn new(base_addr: usize) -> Option<Arc<Self>> {
        Self::from_device(VirtIODevice::new(base_addr, 0x1000).ok()?)
    }
    /// 发现统一 MMIO/PCI transport 的能力，数据队列留给显式 OPEN。
    /// # Returns
    /// 必要 feature、配置或分配失败返回 None。
    pub(super) fn from_device(device: VirtIODevice) -> Option<Arc<Self>> {
        if device.device_id() != 1 {
            return None;
        }
        device.initialize().ok()?;
        let features = device.device_features().ok()?;
        let required_features = VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC;
        if features & required_features != required_features {
            return None;
        }
        let queue_features = if features & VIRTIO_NET_F_CTRL_VQ != 0
            && features & (VIRTIO_NET_F_MQ | VIRTIO_NET_F_RSS) != 0
        {
            features & (VIRTIO_NET_F_CTRL_VQ | VIRTIO_NET_F_MQ | VIRTIO_NET_F_RSS)
        } else {
            0
        };
        let features = required_features
            | queue_features
            | features & (VIRTIO_NET_F_CSUM | VIRTIO_NET_F_STATUS | VIRTIO_NET_F_SPEED_DUPLEX);
        device.set_driver_features(features).ok()?;
        let status = device.get_status().ok()?;
        device
            .set_status(status | VIRTIO_CONFIG_S_FEATURES_OK)
            .ok()?;
        if device.get_status().ok()? & VIRTIO_CONFIG_S_FEATURES_OK == 0 {
            return None;
        }
        let maximum_pairs = if features & (VIRTIO_NET_F_MQ | VIRTIO_NET_F_RSS) != 0 {
            device
                .read_config_consistent(|device| device.read_config_u16(MAX_PAIRS_OFFSET))
                .ok()?
        } else {
            1
        };
        if !(1..=0x8000).contains(&maximum_pairs) {
            return None;
        }

        let rss = if features & VIRTIO_NET_F_RSS != 0 {
            let cap = device
                .read_config_consistent(|device| {
                    Ok(NetworkRssCapabilities {
                        maximum_key_bytes: device.read_config_u8(RSS_KEY_OFFSET)?,
                        maximum_table_entries: device.read_config_u16(RSS_TABLE_OFFSET)?,
                        hash_types: device.read_config_u32(RSS_TYPES_OFFSET)? & RECEIVE_HASH_TYPES,
                    })
                })
                .ok()?;
            if cap.maximum_key_bytes < 40 || cap.maximum_table_entries < 128 {
                return None;
            }
            Some(cap)
        } else {
            None
        };
        let mac = device
            .read_config_consistent(|device| {
                let mut mac = [0; 6];
                for (offset, byte) in mac.iter_mut().enumerate() {
                    *byte = device.read_config_u8(offset)?;
                }
                Ok(mac)
            })
            .ok()?;
        let controller = Arc::try_new(Self {
            device,
            mac,
            features,
            maximum_pairs,
            rss,
            completion_irq: VirtIOCompletionIrq::new(),
            lease: Mutex::new(Lease {
                active: None,
                activity: Vec::new(),
            }),
            retired: native_runtime::Event::create().ok()?,
        })
        .ok()?;
        crate::io_completion::register_completion_source(controller.clone()).ok()?;
        Some(controller)
    }
    fn notify_activity(&self, queue: u16) {
        let callback = self
            .lease
            .lock()
            .activity
            .get(queue as usize)
            .cloned()
            .flatten();
        if let Some(callback) = callback {
            callback();
        }
    }
    fn notify_all_activity(&self) {
        let count = self.lease.lock().activity.len();
        for queue in 0..count {
            self.notify_activity(queue as u16);
        }
    }
    pub(crate) fn irq_handler_for(self: &Arc<Self>) -> Arc<dyn InterruptHandler> {
        Arc::try_new(VirtIONetworkIrqHandler {
            device: self.clone(),
        })
        .expect("VirtIO network IRQ handler allocation failed")
    }
    fn create_queue(device: &VirtIODevice, index: u32) -> Option<VirtQueue> {
        let maximum = device.queue_max_size(index).ok()?;
        let size = maximum.min(QUEUE_SIZE);
        if size == 0 || !size.is_power_of_two() {
            return None;
        }
        let queue = VirtQueue::new(size, device.bti())?;
        device
            .configure_queue(index, size, queue.addresses())
            .ok()?;
        Some(queue)
    }
}
impl NetworkController for VirtIONetworkDevice {
    fn maximum_queue_pairs(&self) -> u16 {
        self.maximum_pairs
    }
    fn transmit_checksum(&self) -> bool {
        self.features & VIRTIO_NET_F_CSUM != 0
    }
    fn receive_scaling(&self) -> Option<NetworkRssCapabilities> {
        self.rss
    }
    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }
    fn driver(&self) -> &str {
        // 名称只由物理驱动提供，活动会话按 owner 借用。
        "virtio_net"
    }
    fn open(self: Arc<Self>, count: u16) -> Result<Arc<dyn NetworkProvider>, NetworkError> {
        if count == 0 || count > self.maximum_pairs {
            return Err(NetworkError::InvalidConfiguration);
        }
        let mut lease = loop {
            let lease = self.lease.lock();
            if lease.active.is_none() {
                break lease;
            }
            // 目录会话已关闭，但错误完成线程可能仍临时保活已 stop 的队列。
            // Weak 失效也可能只是 Drop 正在 reset/撤销 pin，必须等实际退休事件。
            let current = lease.active.as_ref().and_then(Weak::upgrade);
            let retiring = current.as_ref().is_none_or(|active| {
                active.failed.load(Ordering::Acquire) == ActiveNetwork::QUIESCENT
            });
            // 可能在此归还最后一个强引用；不能持 lease 锁触发 ActiveNetwork::drop。
            drop(lease);
            drop(current);
            if !retiring {
                return Err(NetworkError::WouldBlock);
            }
            self.retired
                .wait_one(
                    native_runtime::native_abi::signals::SIGNALED,
                    native_runtime::native_abi::DEADLINE_INFINITE,
                )
                .map_err(|_| NetworkError::Device)?;
        };
        // 冷配置锁同时划分 IRQ 错误代际；旧实例的锁存不能延迟杀死重开的队列。
        self.completion_irq.take_transport_error();
        self.device.initialize().map_err(|_| NetworkError::Device)?;
        self.device
            .set_driver_features(self.features)
            .map_err(|_| NetworkError::Device)?;
        let status = self.device.get_status().map_err(|_| NetworkError::Device)?;
        self.device
            .set_status(status | VIRTIO_CONFIG_S_FEATURES_OK)
            .map_err(|_| NetworkError::Device)?;
        if self.device.get_status().map_err(|_| NetworkError::Device)? & VIRTIO_CONFIG_S_FEATURES_OK
            == 0
        {
            return Err(NetworkError::Device);
        }
        let mut queues = Vec::new();
        let mut activity = Vec::new();
        queues
            .try_reserve_exact(count as usize)
            .map_err(|_| NetworkError::NoMemory)?;
        activity
            .try_reserve_exact(count as usize)
            .map_err(|_| NetworkError::NoMemory)?;
        for index in 0..count {
            queues.push(Mutex::new(
                QueueState::try_new(&self.device, index).ok_or(NetworkError::NoMemory)?,
            ));
            activity.push(None);
        }
        let control = if self.features & VIRTIO_NET_F_CTRL_VQ != 0 {
            Some(Mutex::new(
                ControlQueue::try_new(&self.device, u32::from(self.maximum_pairs) * 2, self.rss)
                    .ok_or(NetworkError::NoMemory)?,
            ))
        } else {
            None
        };
        // OWNER: 先建立会 reset 的 owner，再 DRIVER_OK。所有 fallible 步骤结束前不发布租约。
        let mut active = Arc::try_new(ActiveNetwork {
            owner: self.clone(),
            lease_published: false,
            queues,
            failed: AtomicU8::new(ActiveNetwork::LIVE),
            control,
        })
        .map_err(|_| NetworkError::NoMemory)?;
        let status = self.device.get_status().map_err(|_| NetworkError::Device)?;
        self.device
            .set_status(status | VIRTIO_CONFIG_S_DRIVER_OK)
            .map_err(|_| NetworkError::Device)?;
        if let Some(control) = &active.control {
            if self.features & VIRTIO_NET_F_MQ != 0 {
                control
                    .lock()
                    .set_pairs(&self.device, count)
                    .map_err(|_| NetworkError::Device)?;
            } else if self.rss.is_some() {
                // RSS-only 硬件没有 PAIRS_SET。客户端安装前提交实际策略；bootstrap 只指定 TX 数和未分类队列。
                let mut indirection = Vec::new();
                indirection
                    .try_reserve_exact(1)
                    .map_err(|_| NetworkError::NoMemory)?;
                indirection.push(0);
                control.lock().set_rss(
                    &self.device,
                    count,
                    &NetworkRssConfiguration {
                        hash_types: 0,
                        unclassified_queue: 0,
                        indirection,
                        key: Vec::new(),
                    },
                )?;
            }
        }
        for index in 0..count {
            self.device
                .notify_queue(u32::from(index) * 2)
                .map_err(|_| NetworkError::Device)?;
        }
        self.retired
            .signal(native_runtime::native_abi::signals::SIGNALED, 0)
            .map_err(|_| NetworkError::Device)?;
        Arc::get_mut(&mut active)
            .expect("network lease is not yet published")
            .lease_published = true;
        lease.activity = activity;
        lease.active = Some(Arc::downgrade(&active));
        drop(lease);
        Ok(active)
    }
}
impl ActiveNetwork {
    const LIVE: u8 = 0;
    const STOP_REQUESTED: u8 = 1;
    const RESET_OWNER: u8 = 2;
    const QUIESCENT: u8 = 3;
    fn queue(&self, queue: u16) -> Result<&Mutex<QueueState>, NetworkError> {
        self.queues.get(queue as usize).ok_or(NetworkError::Device)
    }
    /// 非阻塞撤离：先阻止新 DMA，再观察各临界区退出；只有一个调用者执行 reset。
    /// 完成线程竞争时重调度，不能等控制 ACK 而停住其他设备的完成来源。
    fn try_quiesce(&self) -> bool {
        if self.failed.load(Ordering::Acquire) == Self::QUIESCENT {
            return true;
        }
        let _ = self.failed.compare_exchange(
            Self::LIVE,
            Self::STOP_REQUESTED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        if self.failed.load(Ordering::Acquire) == Self::RESET_OWNER {
            return false;
        }
        for queue in &self.queues {
            let Some(guard) = queue.try_lock() else {
                return false;
            };
            drop(guard);
        }
        if let Some(control) = &self.control {
            let Some(guard) = control.try_lock() else {
                return false;
            };
            drop(guard);
        }
        if self
            .failed
            .compare_exchange(
                Self::STOP_REQUESTED,
                Self::RESET_OWNER,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            // failed 已阻止临界区重新提交；顺序锁过各队列后，旧描述符 publication/doorbell 都已退出。
            self.device
                .reset()
                .expect("VirtIO net must stop DMA before retiring ownership");
            self.failed.store(Self::QUIESCENT, Ordering::Release);
            self.notify_all_activity();
            true
        } else {
            self.failed.load(Ordering::Acquire) == Self::QUIESCENT
        }
    }
    fn quiesce(&self) {
        while !self.try_quiesce() {
            native_runtime::system::sleep_until(native_runtime::system::deadline_after(50_000))
                .expect("wait for network DMA revocation");
        }
    }
    fn fail_device(&self) -> NetworkError {
        self.quiesce();
        NetworkError::Device
    }
}

impl Drop for ActiveNetwork {
    fn drop(&mut self) {
        if self.failed.load(Ordering::Acquire) != Self::QUIESCENT {
            self.device
                .reset()
                .expect("VirtIO net must stop DMA before retiring ownership");
        }
        // reset 已结束 DMA；先退休 pin/IOVA，再让下一个 OPEN 获得配置租约。
        // 否则新配置会与旧存储的延迟析构竞争地址空间和内存配额。
        self.queues.clear();
        drop(self.control.take());
        if self.lease_published {
            let callbacks = {
                let mut lease = self.owner.lease.lock();
                // 与 active=None 同锁发表，旧退休通知不能越过下一租约的 clear。
                self.owner
                    .retired
                    .signal(0, native_runtime::native_abi::signals::SIGNALED)
                    .expect("network lease retirement event must remain valid");
                lease.active = None;
                core::mem::take(&mut lease.activity)
            };
            drop(callbacks);
        }
    }
}
impl Drop for VirtIONetworkDevice {
    fn drop(&mut self) {
        self.device
            .reset()
            .expect("VirtIO net controller must reset before transport retirement");
    }
}
impl crate::io_completion::CompletionSource for VirtIONetworkDevice {
    fn dispatch_completions(&self) -> bool {
        if !self.completion_irq.has_transport_error() {
            return false;
        }
        let active = {
            let Some(lease) = self.lease.try_lock() else {
                return true;
            };
            let active = lease.active.as_ref().and_then(Weak::upgrade);
            if active.is_none() {
                self.completion_irq.take_transport_error();
            }
            active
        };
        let Some(active) = active else {
            return false;
        };
        if !active.try_quiesce() {
            return true;
        }
        // 本强引用仍保活旧租约，下一 OPEN 不可能先于此处清除本代锁存。
        self.completion_irq.take_transport_error();
        false
    }
}

struct VirtIONetworkIrqHandler {
    device: Arc<VirtIONetworkDevice>,
}

impl InterruptHandler for VirtIONetworkIrqHandler {
    fn handle_interrupt(&self, _vector: InterruptVector) -> Result<(), InterruptError> {
        // native IRQ 服务线程可在冷重配置期间阻塞；控制 ACK 由 DMA/used 推进，不依赖此线程。
        // 与重开事务串行 ACK/错误锁存，否则上一租约的延迟错误可能停止下一租约。
        let lease = self.device.lease.lock();
        let status = self.device.completion_irq.acknowledge(&self.device.device);
        drop(lease);
        let Some(status) = status else {
            // IRQ 不取得队列锁、不等待 reset；完成线程消费错误后停止 DMA 并唤醒全部队列。
            crate::completion::raise();
            return Ok(());
        };
        if status & (VIRTIO_MMIO_INT_VRING | VIRTIO_MMIO_INT_CONFIG) != 0 {
            self.device.notify_all_activity();
        }
        Ok(())
    }
}
