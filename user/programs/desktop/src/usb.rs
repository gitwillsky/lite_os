//! USB 通知区状态；后台读取标准 sysfs，不让 UI 线程等待设备或文件系统。
use linux_uapi::usb::{self, Device};
use std::{io, sync::mpsc, thread, time::Duration};
pub(super) struct Usb {
    devices: Vec<Device>,
    updates: mpsc::Receiver<io::Result<Vec<Device>>>,
    // OWNER: 停止通道与线程只属于本对象；Drop 唤醒并 join，避免桌面退出后继续扫描。
    stop: Option<mpsc::SyncSender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Usb {
    pub fn new() -> io::Result<Self> {
        let (sender, updates) = mpsc::sync_channel(1);
        let (stop, stopped) = mpsc::sync_channel(0);
        let worker = thread::Builder::new()
            .name("usb-tray".into())
            .spawn(move || {
                loop {
                    if matches!(
                        sender.try_send(usb::devices()),
                        Err(mpsc::TrySendError::Disconnected(_))
                    ) {
                        break;
                    }
                    if !matches!(
                        stopped.recv_timeout(Duration::from_millis(500)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        break;
                    }
                }
            })?;
        Ok(Self {
            devices: Vec::new(),
            updates,
            stop: Some(stop),
            worker: Some(worker),
        })
    }
    pub fn poll(&mut self) -> bool {
        let Ok(Ok(devices)) = self.updates.try_recv() else {
            return false;
        };
        let changed = devices != self.devices;
        self.devices = devices;
        changed
    }
    pub fn connected(&self) -> bool {
        !self.devices.is_empty()
    }
    pub fn tooltip(&self) -> Option<String> {
        self.connected()
            .then(|| format!("拔出或弹出硬件（{} 个设备）", self.devices.len()))
    }
}
impl Drop for Usb {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
